"""唯一 canonical export 生成器（指导 §8 去重保证 / §6 节点重命名）。

所有订阅出口（Gist、Sub-Store 人工推送、订阅 API）只读取这里产出的
`data/exports/mihomo-alive.yaml`。每个探测点 / 每个来源 / 每种模式各自产出
订阅是本轮要删除的行为，所以输入是「多个来源的 (proxies, groups)」，输出是
**一份**去重、重命名、排序后的 YAML。

设计要点：

* 身份始终是连接参数指纹（`core.fingerprint_proxy`，忽略 `name`），改名只影响
  显示名，绝不触碰 server/port/uuid/password/reality/tls/dialer-proxy。
* 同一 fingerprint 从多个来源出现时只保留 canonical 记录，元数据里记
  `source_aliases`（首次出现的来源为主，其余来源记为别名）。
* 输出顺序固定：country → protocol → normalized original name → fingerprint。
  每轮稳定，不抖动。
* YAML 正文里**不写** generated_at / round_id 等每轮易变字段。内容不变则
  sha 不变，Gist 幂等（不发 PATCH、不产生 revision）依赖这一点。
"""
import hashlib
import re

import yaml

from . import core as coremod
from .engine import DIALER_FIELD, _ExportDumper, _LEADING_TAG

# 改名只允许触碰 `name`。其余一律照抄上游，一个字段都不动。
_NAME_KEY = "name"

# normalized original name：去掉前导地区 tag / 旗帜 emoji 后、去空白、小写，
# 用于排序（不动显示名本身）。
_COUNTRY_TAG = re.compile(r"^\[[A-Z]{2}(?:-[A-Za-z0-9]+)?\]\s*")
_FLAG_EMOJI = re.compile(r"^[\U0001F1E6-\U0001F1FF]{2}\s*")
_UNSET_COUNTRY = "UN"


def normalize_for_sort(name):
    text = _LEADING_TAG.sub("", str(name or "")).strip()
    return text.lower()


def _strip_region_tag(name):
    """Display name with any measured region tag / flag removed."""
    text = str(name or "")
    for _ in range(4):
        stripped = _COUNTRY_TAG.sub("", text, count=1)
        stripped = _FLAG_EMOJI.sub("", stripped, count=1)
        if stripped == text:
            break
        text = stripped.strip()
    return text.strip()


def _fingerprint(proxy):
    return coremod.fingerprint_proxy(proxy)


def _country_of(proxy, meta):
    """Measured exit country for the rename template; `UN` when unknown."""
    name = proxy.get(_NAME_KEY)
    direct = (meta or {}).get("egress_country")
    if direct:
        return str(direct).upper()
    proxy_country = (proxy.get("_country") or (proxy.get("meta") or {}).get("country"))
    if proxy_country:
        return str(proxy_country).upper()
    if isinstance(name, str):
        match = re.match(r"^\[([A-Z]{2})(?:-[A-Za-z0-9]+)?\]", name)
        if match:
            return match.group(1)
    return _UNSET_COUNTRY


def _collision_suffix(template, n):
    """`collision_suffix` with `{n}` (or a bare number) filled in."""
    text = str(template)
    if "{n}" in text:
        return text.replace("{n}", str(n))
    if "{N}" in text:
        return text.replace("{N}", str(n))
    return f"{text}{n}"


def _render_name(template, fields):
    try:
        return str(template).format(**fields)
    except (KeyError, IndexError, ValueError):
        return str(fields.get("display_name") or "")


class _Renamer:
    """One rename pass over a fixed sort order."""

    def __init__(self, cfg):
        rename = cfg.get("rename") or {}
        self.enabled = bool(rename.get("enabled", True))
        self.template = str(rename.get("template") or "{country} | {display_name}")
        self.collision_suffix = str(rename.get("collision_suffix") or " #{n}")
        self.preserve = bool(rename.get("preserve_original_name", True))

    def apply(self, ordered):
        """`ordered` is [(fingerprint, proxy, meta)] in final export order.

        Returns (proxies, renames). `proxies` carries the final display name;
        every connection field is copied verbatim from the input proxy.
        """
        proxies, renames = [], []
        used = {}
        for index, (fingerprint, proxy, meta) in enumerate(ordered, start=1):
            out = dict(proxy)
            original = str(proxy.get(_NAME_KEY) or "")
            meta = meta or {}
            if self.preserve and original:
                meta = dict(meta)
                meta.setdefault("original_name", original)
            final = original
            if self.enabled:
                display = _strip_region_tag(original) or original
                fields = {
                    "display_name": display,
                    "country": _country_of(proxy, meta),
                    "probe": str(meta.get("best_probe") or ""),
                    "delay": ("" if meta.get("delay") is None
                              else str(meta.get("delay"))),
                    "protocol": str(proxy.get("type") or ""),
                    "index": index,
                }
                final = _render_name(self.template, fields)
                if final in used and used[final] != fingerprint:
                    # 同名不同 fingerprint：追加 #2 / #3，直到唯一。
                    suffix_n = 2
                    while True:
                        candidate = final + _collision_suffix(self.collision_suffix,
                                                              suffix_n)
                        if used.get(candidate, fingerprint) == fingerprint:
                            final = candidate
                            break
                        suffix_n += 1
                used[final] = fingerprint
                out[_NAME_KEY] = final
            else:
                # enabled=false：保留原名，但仍要防止同名不同 fingerprint 在
                # 客户端里撞名（mihomo 按 name 索引）。仅在真正冲突时追加。
                if final in used and used[final] != fingerprint:
                    suffix_n = 2
                    while True:
                        candidate = final + _collision_suffix(self.collision_suffix,
                                                              suffix_n)
                        if used.get(candidate, fingerprint) == fingerprint:
                            final = candidate
                            break
                        suffix_n += 1
                used[final] = fingerprint
                out[_NAME_KEY] = final
            renames.append({
                "fingerprint": fingerprint,
                "original_name": original,
                "final_name": final,
                "renamed": self.enabled and final != original,
            })
            proxies.append(out)
        return proxies, renames


def build(sources, cfg):
    """Deduplicate + rename + sort per-source exports into one canonical payload.

    `sources` is an iterable of dicts:
        {"key": "hk3-a",
         "proxies": [...],        # 复用 engine._export_proxies 的产物（含 chain
                                  # dialer 解析后的最终形态）
         "groups": [...],         # 该源的 proxy-groups（通常来自链式解析）
         "meta": {display_name_or_fp: {alive, delay, egress_country, best_probe}}}

    Returns (payload, meta):
        payload = {"proxies": [...], "proxy-groups": [...]?}
        meta = {"count", "digest", "content_sha256", "renames", "source_aliases"}
    """
    renamer = _Renamer(cfg)
    canonical = {}      # fingerprint -> (proxy, meta, primary_source)
    source_aliases = {}  # fingerprint -> [source keys beyond the first]
    ordered_sources = list(sources or [])

    for source in ordered_sources:
        key = str((source or {}).get("key") or "")
        meta_map = (source or {}).get("meta") or {}
        for proxy in (source or {}).get("proxies") or []:
            proxy = dict(proxy)
            name = proxy.get(_NAME_KEY)
            fingerprint = _fingerprint(proxy)
            # meta 按显示名或指纹都能查到（调用方任选其一）。
            node_meta = (meta_map.get(name) or meta_map.get(fingerprint) or {})
            if fingerprint in canonical:
                existing = canonical[fingerprint]
                aliases = source_aliases.setdefault(fingerprint, [])
                if key and key not in aliases and key != existing["source"]:
                    aliases.append(key)
                continue
            canonical[fingerprint] = {"proxy": proxy, "meta": dict(node_meta),
                                      "source": key}
            if key:
                source_aliases.setdefault(fingerprint, [])

    def sort_key(item):
        fingerprint, entry = item
        proxy, meta = entry["proxy"], entry["meta"]
        return (
            _country_of(proxy, meta),
            str(proxy.get("type") or ""),
            normalize_for_sort(proxy.get(_NAME_KEY)),
            fingerprint,
        )

    ordered = sorted(canonical.items(), key=sort_key)
    proxies, renames = renamer.apply(
        [(fp, entry["proxy"], entry["meta"]) for fp, entry in ordered])

    payload = {"proxies": proxies}
    groups = _merge_groups(ordered_sources, {p.get(_NAME_KEY) for p in proxies})
    if groups:
        payload["proxy-groups"] = groups

    content = yaml.dump(payload, Dumper=_ExportDumper, allow_unicode=True,
                        sort_keys=False, default_flow_style=False, width=4096)
    meta = {
        "count": len(proxies),
        "digest": _proxy_digest(proxies),
        "content_sha256": hashlib.sha256(
            content.encode("utf-8")).hexdigest(),
        "renames": renames,
        "source_aliases": {fp: keys for fp, keys in source_aliases.items() if keys},
    }
    return content.encode("utf-8"), meta


def _merge_groups(sources, published_names):
    """Union the per-source groups, dropping members the canonical file lacks.

    Groups are the chain topology (`_resolve_chain_dialers`); dedup keeps the
    file valid when two sources carried the same group name.
    """
    merged, seen = [], set()
    for source in sources:
        for group in (source or {}).get("groups") or []:
            if not isinstance(group, dict):
                continue
            name = str(group.get("name") or "")
            if not name or name in seen:
                continue
            seen.add(name)
            group = dict(group)
            if isinstance(group.get("proxies"), list):
                group["proxies"] = [p for p in group["proxies"]
                                    if p in published_names]
            merged.append(group)
    return merged


def _proxy_digest(proxies):
    """Stable digest over the connection fields only (name-independent)."""
    material = []
    for proxy in proxies:
        material.append({
            k: v for k, v in proxy.items()
            if k != _NAME_KEY and not str(k).startswith("_")
        })
    blob = _json_blob(material)
    return hashlib.sha256(blob).hexdigest()[:32]


def _json_blob(obj):
    import json
    return json.dumps(obj, sort_keys=True, ensure_ascii=False,
                      default=str).encode("utf-8")
