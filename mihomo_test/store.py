"""Sub-Store client.

Sub-Store answers HTTP 500 for a resource that does not exist: collections
come back as {"code": "SUBSCRIPTION_NOT_FOUND", "details": 404}, and single
subs hit an unhandled TypeError and return an HTML error page. Both mean
"absent", so both are normalised to None here -- treating them as failures is
what silently killed the previous pipelines.
"""
import json
import time
import urllib.error
import urllib.parse
import urllib.request

import yaml

BROWSER_UA = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/125.0 Safari/537.36"
)


class StoreError(RuntimeError):
    pass


class NotFound(StoreError):
    pass


class Client:
    def __init__(self, backend, timeout=60, retries=2):
        self.backend = backend.rstrip("/")
        self.timeout = timeout
        self.retries = retries

    def _request(self, method, path, payload=None, raw=False):
        url = path if path.startswith("http") else self.backend + path
        body = None
        if payload is not None:
            body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        last = None
        for attempt in range(self.retries + 1):
            req = urllib.request.Request(url, data=body, method=method)
            req.add_header("User-Agent", BROWSER_UA)
            req.add_header("Accept", "application/json, text/yaml, text/plain, */*")
            if body is not None:
                req.add_header("Content-Type", "application/json")
            try:
                with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                    text = resp.read().decode("utf-8", "replace")
                    return resp.status, text
            except urllib.error.HTTPError as exc:
                text = exc.read().decode("utf-8", "replace")
                if exc.code == 403 and "1010" in text:
                    raise StoreError("Cloudflare blocked the request (UA signature)") from None
                if exc.code in (404,) or self._is_missing(exc.code, text):
                    raise NotFound(f"{method} {path}: not found")
                if exc.code < 500:
                    raise StoreError(f"HTTP {exc.code} {method} {path}: {text[:200]}") from None
                last = StoreError(f"HTTP {exc.code} {method} {path}: {text[:200]}")
            except Exception as exc:  # network hiccup: retry
                last = StoreError(f"{type(exc).__name__} {method} {path}: {exc}")
            if attempt < self.retries:
                time.sleep(0.8 * (attempt + 1))
        raise last

    @staticmethod
    def _is_missing(status, text):
        """Return True when a 500 actually reports a missing resource."""
        if status != 500:
            return False
        if "SUBSCRIPTION_NOT_FOUND" in text or "RESOURCE_NOT_FOUND" in text:
            return True
        if "Cannot convert undefined or null to object" in text:
            return True
        try:
            payload = json.loads(text)
        except ValueError:
            return False
        error = payload.get("error") if isinstance(payload, dict) else None
        if isinstance(error, dict):
            return "NOT_FOUND" in str(error.get("code", "")) or error.get("details") == 404
        return False

    def get_json(self, path):
        status, text = self._request("GET", path)
        try:
            payload = json.loads(text)
        except ValueError:
            raise StoreError(f"non-JSON response from {path}: {text[:160]}") from None
        if payload.get("status") != "success":
            raise StoreError(f"Sub-Store rejected {path}: {str(payload)[:200]}")
        return payload.get("data")

    def collection(self, name):
        try:
            return self.get_json("/api/collection/" + urllib.parse.quote(name, safe=""))
        except NotFound:
            return None

    def sub(self, name):
        try:
            return self.get_json("/api/sub/" + urllib.parse.quote(name, safe=""))
        except NotFound:
            return None

    def download_collection(self, name, target="ClashMeta"):
        """Fetch a collection rendered for target; return the raw text."""
        return self._download(f"/download/collection/{urllib.parse.quote(name, safe='')}", target)

    def download_sub(self, name, target="ClashMeta"):
        """Fetch a single subscription rendered for target; return the raw text.

        Goes through the download route rather than reading the record's
        `content`, because a remote sub keeps its nodes behind `url` and has an
        empty `content` field.
        """
        return self._download(f"/download/{urllib.parse.quote(name, safe='')}", target)

    def _download(self, path, target):
        status, text = self._request(
            "GET", f"{path}?target={urllib.parse.quote(target, safe='')}"
        )
        return text

    @staticmethod
    def _parse_proxies(text, label):
        try:
            parsed = yaml.safe_load(text) or {}
        except yaml.YAMLError as exc:
            raise StoreError(f"{label} is not valid YAML: {exc}") from None
        proxies = parsed.get("proxies") if isinstance(parsed, dict) else None
        if not isinstance(proxies, list):
            raise StoreError(f"{label} returned no proxies list")
        return [p for p in proxies if isinstance(p, dict)]

    def fetch_proxies(self, name, target="ClashMeta"):
        """Fetch a collection and return its proxy dicts."""
        return self._parse_proxies(self.download_collection(name, target), f"collection {name}")

    def fetch_sub_proxies(self, name, target="ClashMeta"):
        """Fetch a single subscription and return its proxy dicts."""
        return self._parse_proxies(self.download_sub(name, target), f"sub {name}")

    def fetch_source(self, kind, name, target="ClashMeta"):
        """Fetch either resource kind; return its proxy dicts."""
        if kind == "sub":
            return self.fetch_sub_proxies(name, target)
        return self.fetch_proxies(name, target)

    def list_resources(self):
        """Return every collection and single sub, for the source picker."""
        out, errors = [], []
        for kind, plural in (("collection", "collections"), ("sub", "subs")):
            try:
                items = self.get_json(f"/api/{plural}") or []
            except StoreError as exc:
                errors.append(f"{plural}: {exc}")
                continue
            for item in items:
                name = item.get("name")
                if not name:
                    continue
                out.append({
                    "kind": kind,
                    "name": name,
                    "members": len(item.get("subscriptions") or []) if kind == "collection" else None,
                    "source_type": item.get("source"),
                })
        return out, errors

    def upsert(self, kind, name, payload):
        """Create or replace a sub/collection; return the resulting record."""
        plural = {"sub": "subs", "collection": "collections"}[kind]
        if self._exists(kind, name):
            self._request("PATCH", f"/api/{kind}/{urllib.parse.quote(name, safe='')}", payload)
            return "updated"
        try:
            self._request("POST", f"/api/{plural}", payload)
            return "created"
        except StoreError:
            # a concurrent create or a partially-registered name: fall back to PATCH
            self._request("PATCH", f"/api/{kind}/{urllib.parse.quote(name, safe='')}", payload)
            return "patched"

    def _exists(self, kind, name):
        return (self.sub(name) if kind == "sub" else self.collection(name)) is not None

    def preview(self, kind, resource):
        """Run a resource through Sub-Store's processors; return data."""
        return self.get_json_post(f"/api/preview/{kind}", resource)

    def get_json_post(self, path, payload):
        status, text = self._request("POST", path, payload)
        try:
            body = json.loads(text)
        except ValueError:
            raise StoreError(f"non-JSON response from {path}: {text[:160]}") from None
        if body.get("status") != "success":
            raise StoreError(f"Sub-Store rejected {path}: {str(body)[:200]}")
        return body.get("data")
