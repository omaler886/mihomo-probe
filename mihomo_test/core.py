"""mihomo kernel control: config build, load, and real HTTP testing.

Testing goes through the kernel's own REST API, so every verdict is produced
by an actual mihomo outbound and real traffic -- not a TCP handshake guess.

Failure bodies are preserved. mihomo answers HTTP 503 with
{"message": "An error occurred in the delay test"} for an internal failure and
HTTP 504 {"message": "Timeout"} for a genuine timeout; collapsing both into
"HTTP Error 503" is what made the previous pipeline's dead list unreadable.
"""
import json
import shutil
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request

from . import config as cfgmod

# Fields mihomo rejects outright, and the ones that only make sense with a
# front proxy this core does not carry.
DROP_FIELDS = ("dialer-proxy", "interface-name", "routing-mark")
REQUIRED = ("name", "type", "server", "port")


def parse_port(url, default):
    """Return the port from an API URL like http://127.0.0.1:19190."""
    try:
        parsed = urllib.parse.urlparse(str(url))
        return parsed.port or default
    except (ValueError, AttributeError):
        return default


def fingerprint_proxy(proxy):
    """Stable identity for a node, independent of its display name.

    Upstream lists contain duplicate names, and the position-derived "#2"
    suffix can swap between rounds. Keying convergence state by connection
    parameters instead keeps one node's failure streak from being shared with
    (and cancelled out by) a differently-named twin.
    """
    import hashlib

    material = {}
    for key, value in proxy.items():
        if key == "name" or key.startswith("_"):
            continue
        material[key] = value
    blob = json.dumps(material, sort_keys=True, ensure_ascii=False, default=str)
    return hashlib.sha256(blob.encode("utf-8")).hexdigest()[:16]


class CoreError(RuntimeError):
    pass


def _auth_headers(secret):
    return {"Authorization": f"Bearer {secret}"} if secret else {}


def _req(method, url, secret, payload=None, timeout=20):
    body = json.dumps(payload).encode("utf-8") if payload is not None else None
    req = urllib.request.Request(url, data=body, method=method)
    req.add_header("Accept", "application/json")
    for key, value in _auth_headers(secret).items():
        req.add_header(key, value)
    if body is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            text = resp.read().decode("utf-8", "replace")
            return resp.status, text
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read().decode("utf-8", "replace")
    except Exception as exc:
        raise CoreError(f"{type(exc).__name__}: {exc}") from None


def _reason_from(status, body):
    """Classify a failed delay call into a stable, greppable reason."""
    message = ""
    try:
        parsed = json.loads(body)
        if isinstance(parsed, dict):
            message = str(parsed.get("message") or parsed.get("error") or "")
    except ValueError:
        message = (body or "").strip()[:120]
    low = message.lower()
    if "timeout" in low or status == 504:
        return "timeout", message
    if "delay test" in low or status == 503:
        return "kernel_error", message
    if status == 400:
        return "bad_request", message
    if status == 0 or "refused" in low:
        return "unreachable", message
    return f"http_{status}", message


def prepare(entries, strip_ech=False):
    """Return config-ready proxies plus a mapping back to source entries.

    Names are made unique because mihomo keys proxies by name and silently
    collapses duplicates; the previous pipeline hit this with two BageVM nodes.
    """
    proxies, mapping, dropped = [], [], []
    seen = {}
    for entry in entries:
        proxy = {k: v for k, v in entry["proxy"].items() if k not in DROP_FIELDS}
        if strip_ech:
            proxy.pop("ech-opts", None)
        missing = [f for f in REQUIRED if proxy.get(f) in (None, "")]
        if missing:
            dropped.append({"name": entry.get("name"), "why": "missing " + ",".join(missing)})
            continue
        if not isinstance(proxy.get("port"), int):
            try:
                proxy["port"] = int(proxy["port"])
            except (TypeError, ValueError):
                dropped.append({"name": entry.get("name"), "why": "bad port"})
                continue
        base = str(proxy["name"]).strip() or f"node-{entry['index']}"
        name = base
        if base in seen:
            seen[base] += 1
            name = f"{base} #{seen[base]}"
            while name in seen:
                seen[base] += 1
                name = f"{base} #{seen[base]}"
        seen[name] = 1
        proxy["name"] = name
        proxies.append(proxy)
        # Per-address variants carry the ORIGINAL node's fingerprint: the
        # ledger and the export speak the domain form -- only this round's
        # kernel config is expanded to addresses.
        fp = entry.get("fp") or fingerprint_proxy(proxy)
        mapping.append(
            {
                "source": entry["source"],
                "original": entry["name"],
                "mihomo": name,
                "index": entry["index"],
                "orig_proxy": entry.get("orig_proxy") or dict(proxy),
                "test_ip": entry.get("test_ip"),
                "fp": fp,
                "proto": proxy.get("type"),
                "server": proxy.get("server"),
            }
        )
    return proxies, mapping, dropped


def build_config(proxies, core_cfg, secret):
    """Write the mihomo config; return the path.

    Everything binds to loopback. The probe container runs with host
    networking (required for IPv6), so a 0.0.0.0 listener here would put the
    kernel's API and its proxy port on the public internet.
    """
    cfgmod.CORE_DIR.mkdir(parents=True, exist_ok=True)
    path = cfgmod.CORE_DIR / "config.yaml"
    names = [p["name"] for p in proxies]
    api_port = parse_port(core_cfg.get("api"), 19190)
    lines = [
        "mixed-port: %d" % int(core_cfg["mixed_port"]),
        # allow-lan must be true for bind-address to take effect; with it false
        # the kernel binds the wildcard address and relies on a source-IP check,
        # which still leaves the port reachable on the host's public IP.
        "allow-lan: true",
        "bind-address: 127.0.0.1",
        "mode: rule",
        "log-level: warning",
        # IPv6 has to be on for both the kernel and its resolver: a chunk of the
        # upstream nodes are IPv6-only, and with this off they fail forever.
        "ipv6: true",
        "unified-delay: true",
        "tcp-concurrent: true",
        "find-process-mode: off",
        f"external-controller: 127.0.0.1:{api_port}",
        f'secret: "{secret}"',
        "profile:",
        "  store-selected: false",
        "  store-fake-ip: false",
        "dns:",
        "  enable: true",
        "  ipv6: true",
        "  enhanced-mode: fake-ip",
        "  fake-ip-range: 198.18.0.1/16",
        "  nameserver:",
        "    - 223.5.5.5",
        "    - 1.1.1.1",
        "proxies:",
    ]
    for proxy in proxies:
        lines.append("  - " + json.dumps(proxy, ensure_ascii=False))

    # One select group + one inbound listener per lane. Egress verification
    # needs to hold a node selected while traffic flows through it, and the
    # selector is global state -- so without lanes that phase is serial, which
    # at 174 live nodes is ~330s of the round. Each lane gets its own group and
    # its own port, and an IN-NAME rule pins that listener to that group, so
    # they can run concurrently.
    lanes = lane_count(core_cfg)
    ports = lane_ports(core_cfg, lanes)
    lines += ["proxy-groups:"]
    for i in range(lanes):
        lines += [f'  - name: "{lane_group(i)}"', "    type: select", "    proxies:"]
        for name in names:
            lines.append("      - " + json.dumps(name, ensure_ascii=False))
        if not names:
            lines.append("      - DIRECT")
    lines += ["listeners:"]
    for i in range(lanes):
        lines += [f'  - name: "{lane_name(i)}"', "    type: mixed",
                  f"    port: {ports[i]}", "    listen: 127.0.0.1"]
    lines += ["rules:"]
    for i in range(lanes):
        lines.append(f"  - IN-NAME,{lane_name(i)},{lane_group(i)}")
    lines += [f"  - MATCH,{lane_group(0)}", ""]
    path.write_text("\n".join(lines), encoding="utf-8")
    return path


LANE_PREFIX = "__LANE"


def lane_group(i):
    return f"{LANE_PREFIX}{i}__"


def lane_name(i):
    return f"lane{i}"


def lane_count(core_cfg):
    """How many parallel verification lanes to build."""
    try:
        return max(1, min(32, int(core_cfg.get("lanes", 8))))
    except (TypeError, ValueError):
        return 8


def lane_ports(core_cfg, count=None):
    """Loopback ports for each lane's inbound listener."""
    if count is None:
        count = lane_count(core_cfg)
    try:
        base = int(core_cfg.get("base_port", 19200))
    except (TypeError, ValueError):
        base = 19200
    return [base + i for i in range(count)]


def docker_cli():
    """Path to a usable docker CLI, or None. The CLI talks to the host daemon
    through a mounted socket, so the app can steer its own kernel container."""
    which = shutil.which("docker")
    if not which:
        return None
    probe = subprocess.run([which, "version", "--format", "{{.Server.Version}}"],
                           capture_output=True, text=True, timeout=20)
    return which if probe.returncode == 0 else None


def config_test(core_cfg):
    """Validate the config with the kernel itself; return (ok, error_text).

    The volume path must be the HOST path: this `docker run` is executed by the
    host daemon, which cannot see paths that only exist inside this container.
    """
    cli = docker_cli()
    if cli is None:
        # No usable daemon: skip the check rather than fail the round. The
        # kernel still rejects a bad config at load time, just less precisely.
        return True, "docker unavailable; config validation skipped"
    host_core = cfgmod.HOST_ROOT / "core"
    cmd = [
        cli, "run", "--rm",
        "-v", f"{host_core}:/root/.config/mihomo",
        "metacubex/mihomo:latest",
        "-t", "-d", "/root/.config/mihomo", "-f", "/root/.config/mihomo/config.yaml",
    ]
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    except subprocess.SubprocessError as exc:
        return False, f"config test could not run: {exc}"
    output = (proc.stdout or "") + (proc.stderr or "")
    return proc.returncode == 0, output.strip()


def make_testable(entries, core_cfg, secret, max_prune=5, log=None, strip_ech=False):
    """Build a config the kernel accepts, pruning offending nodes if needed.

    Returns (proxies, mapping, dropped). A single malformed node would
    otherwise fail the whole round, which is how one bad upstream entry used
    to take down every source at once.
    """
    dropped = []
    working = list(entries)
    for attempt in range(max_prune + 1):
        proxies, mapping, invalid = prepare(working, strip_ech=strip_ech)
        dropped.extend(invalid)
        if invalid:
            working = [e for e in working if e.get("name") not in {d["name"] for d in invalid}]
        build_config(proxies, core_cfg, secret)
        ok, output = config_test(core_cfg)
        if ok:
            return proxies, mapping, dropped
        culprit = _culprit_from(output, proxies)
        if log:
            log("warn", f"config test failed (attempt {attempt + 1}): {output.splitlines()[-1][:200]}")
        if culprit is None:
            raise CoreError(f"mihomo rejected the config: {output.strip()[-400:]}")
        dropped.append({"name": culprit, "why": "kernel config error"})
        working = [e for e in working if e.get("name") != culprit]
    raise CoreError("could not produce a config the kernel accepts")


def _culprit_from(output, proxies):
    """Find which proxy the kernel's error text points at."""
    for line in output.splitlines():
        for proxy in proxies:
            if proxy["name"] and proxy["name"] in line:
                return proxy["name"]
    lowered = output.lower()
    for proxy in proxies:
        server = str(proxy.get("server") or "")
        if server and server.lower() in lowered:
            return proxy["name"]
    return None


class Core:
    """Thin client for one mihomo instance."""

    def __init__(self, core_cfg, secret, project_root=None):
        self.cfg = core_cfg
        self.api = core_cfg["api"].rstrip("/")
        self.secret = secret

    def _docker(self, *args, timeout=180):
        cli = docker_cli()
        if cli is None:
            return subprocess.CompletedProcess(["docker"], 1, "",
                                               "docker CLI/daemon unavailable")
        return subprocess.run([cli, *args], capture_output=True, text=True,
                              timeout=timeout)

    def up(self, recreate=False):
        """Start the kernel; recreate=True restarts it so it re-reads config.yaml.

        The compose file owns the container's lifecycle (and its restart
        policy); this only nudges an existing container, which keeps the app
        from needing the compose plugin -- just the CLI and a mounted socket.
        """
        if recreate:
            return self._docker("restart", self.cfg["container"])
        running = self._docker("inspect", "-f", "{{.State.Running}}",
                               self.cfg["container"])
        if running.returncode != 0:
            # not created yet or the daemon cannot see it
            return self._docker("start", self.cfg["container"])
        if running.stdout.strip() != "true":
            return self._docker("start", self.cfg["container"])
        return running

    def stop(self):
        return self._docker("stop", self.cfg["container"])

    def logs(self, tail=60):
        proc = self._docker("logs", "--tail", str(tail), self.cfg["container"])
        return (proc.stdout or "") + (proc.stderr or "")

    def reload(self):
        """Ask the kernel to re-read the config file; restart if it refuses."""
        status, body = _req(
            "PUT", f"{self.api}/configs?force=true", self.secret,
            {"path": self.cfg["container_config_path"]},
        )
        if status in (200, 204):
            return True
        self.up()
        return False

    def wait_ready(self, timeout_s=45):
        deadline = time.time() + timeout_s
        while time.time() < deadline:
            try:
                status, _ = _req("GET", f"{self.api}/version", self.secret, timeout=4)
                if status == 200:
                    return True
            except CoreError:
                pass
            time.sleep(1.5)
        return False

    def start_and_load(self, log=None):
        """Load the freshly generated config, restarting the kernel if needed."""
        if not self._alive():
            # Not running (or running without a controller): recreate so the
            # container definitely picks up the config file just written.
            self.up(recreate=True)
            if not self.wait_ready():
                if log:
                    log("error", "内核启动失败: " + self.logs(tail=20)[-400:])
                raise CoreError("mihomo did not become ready")
            return "started"
        if self.reload():
            return "reloaded"
        # A reload can fail when the controller address itself changed; only a
        # container recreation re-reads the file in that case.
        self.up(recreate=True)
        if not self.wait_ready():
            raise CoreError("mihomo did not become ready after restart")
        return "restarted"

    def _alive(self):
        try:
            _req("GET", f"{self.api}/version", self.secret, timeout=3)
            return True
        except CoreError:
            return False

    def delay(self, name, url, timeout_ms, expected="204"):
        """Test one proxy with real traffic through the kernel.

        Returns (delay_ms, reason, detail); reason is None on success.
        """
        query = urllib.parse.urlencode(
            {"timeout": timeout_ms, "url": url, "expected": expected}
        )
        path = f"{self.api}/proxies/{urllib.parse.quote(name, safe='')}/delay?{query}"
        try:
            status, body = _req("GET", path, self.secret,
                               timeout=timeout_ms / 1000 + 8)
        except CoreError as exc:
            # The controller itself is unreachable -- this says nothing about the
            # node. It used to be reported as `unreachable`, the same string
            # `_reason_from(0, ...)` produces for a node-side failure, so the
            # ledger could not tell "our kernel is down" from "this node is
            # dead", and a controller outage advanced every node's failure
            # streak. Deliberately not in engine.TERMINAL_REASONS: a controller
            # blip is exactly the failure a retry can overturn.
            return None, "controller_error", str(exc)[:160]
        if status == 200:
            try:
                payload = json.loads(body)
            except ValueError:
                return None, "bad_response", body[:120]
            delay = payload.get("delay")
            if isinstance(delay, (int, float)) and not isinstance(delay, bool) and delay >= 0:
                return int(delay), None, ""
            return None, "bad_delay", str(payload)[:120]
        reason, message = _reason_from(status, body)
        return None, reason, message

    def select(self, group, name):
        status, body = _req("PUT", f"{self.api}/proxies/{urllib.parse.quote(group, safe='')}",
                            self.secret, {"name": name})
        if status not in (200, 204):
            raise CoreError(f"could not select {name}: HTTP {status} {body[:120]}")

    def egress(self, port, trace_url, timeout_s=15):
        """Fetch a trace URL through a lane's HTTP port; return parsed fields.

        This is the part that actually proves a node carries traffic: the
        request leaves through the kernel, through the node selected on that
        lane, and the exit reports its own country.
        """
        port = int(port)
        handler = urllib.request.ProxyHandler(
            {"http": f"http://127.0.0.1:{port}", "https": f"http://127.0.0.1:{port}"}
        )
        opener = urllib.request.build_opener(handler)
        try:
            with opener.open(trace_url, timeout=timeout_s) as resp:
                text = resp.read().decode("utf-8", "replace")
        except Exception as exc:
            return None, f"{type(exc).__name__}: {exc}"[:160]
        fields = {}
        for line in text.splitlines():
            if "=" in line:
                key, _, value = line.partition("=")
                fields[key.strip()] = value.strip()
        if not fields:
            return None, "empty trace response"
        return fields, None