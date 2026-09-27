"""IP 回显探测(ipmap):经每个节点请求 IP 回显端点,实测它的真实出口。

测活回答「节点活不活」,本模块回答「节点到底是谁」。节点配置里写明的 server
只是客户端拨号的入口;流量从哪台机器出去、入口是不是中转、落地机还有没有
写明之外的地址,配置里一个字都没有。经节点请求一个回显端点,回显正文就是
真实出口地址:v4 回显端点(仅 A 记录)与 v6 回显端点(仅 AAAA)分开测,得到
落地机两个协议族的地址;再用 Cloudflare trace 的 loc=/colo= 交叉印证国别。
出口 v6 与写明的入口一致 → 直落;不一致 → 中转,落地机才是要的那台。

实现完全复用测活的车道机制(core.build_config 的 lane):每条车道一个
select 组 + 一个 loopback 入站,IN-NAME 规则钉住,于是可以并发地
「选中节点 → 经它 curl」。与测活轮次彻底隔离:独立容器 `mihomo-ipmap`、
独立 API 端口(19191)、独立车道端口段(19300 起),不碰 mihomo-probe 的
配置——一轮 ipmap 跑再久也不会打断正在收敛的测活轮次。

内核目录放在 `data/ipmap-core/`:compose 把 ./data bind 进应用容器,所以
无论从 vps 宿主机还是从应用容器里执行,docker bind-mount 的宿主路径都成立
(放 `core/` 之外的自建目录则只有宿主机跑得通,容器里写的配置宿主看不到)。

节点来源三选一:
  --url URL     订阅地址(裸 Clash YAML,或 base64 包着的 Clash YAML)。
                分享链接文本不支持——链接解析交给 Sub-Store,这是它已经做
                对的事,本模块不写第二个更差的解析器:把链接录入 Sub-Store
                后用 --source 引用即可。
  --file PATH   本地 Clash YAML。
  --source NAME Sub-Store 资源名(--source-kind collection 选组合订阅)。

用法(vps 上,宿主机或应用容器均可):

    python3 -m mihomo_test ipmap --url https://... --family v6 \\
        --key demo-v6 --push-sub ipmap-demo-v6

产出:data/ipmap/<key>.json + <key>.md(节点 ↔ 实测出口映射);--push-sub
把带实测标注的节点写回 Sub-Store 本地订阅。
"""
import base64
import binascii
import ipaddress
import json
import re
import subprocess
import threading
import time
import urllib.request
from pathlib import Path

import yaml

from . import config as cfgmod
from . import core as coremod

IPMAP_CONTAINER = "mihomo-ipmap"
# 内核目录在 data/ 下(见模块 docstring 的 bind-mount 讨论)。
CORE_DIR_NAME = "ipmap-core"
IMAGE = "metacubex/mihomo:latest"
DEFAULT_API_PORT = 19191
DEFAULT_BASE_PORT = 19300
DEFAULT_MIXED_PORT = 19494

# 订阅站普遍按 UA 分流:浏览器 UA 拿到 403,clash 系 UA 拿到正文
# (sub.example.com 实测如此)。浏览器 UA 兜底是为了 Sub-Store 这类不挑 UA 的源。
CLASH_UA = "clash-verge/v1.7.7"
BROWSER_UA = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/125.0 Safari/537.36"
)

# 回显端点按协议族分组,每组两个、按序取第一个成功者:v4 组只有 A 记录、
# v6 组只有 AAAA,内核拨哪个族由端点决定,不依赖任何内核侧的拨号选项。
# 全走明文 HTTP:回显没有可伪造的价值,省掉 TLS 再引入一层 SNI 变数。
ECHO_TARGETS = {
    "v4": ["http://api.ipify.org/", "http://ipv4.icanhazip.com/"],
    "v6": ["http://api6.ipify.org/", "http://ipv6.icanhazip.com/"],
}
# 与测活出口验证同一目标,国别口径与面板一致。
TRACE_URL = "https://www.cloudflare.com/cdn-cgi/trace"


class ImapError(RuntimeError):
    pass


def server_family(server):
    """v4 | v6 | domain。判定依据是字面量,域名不去猜它的解析结果。"""
    try:
        ip = ipaddress.ip_address(str(server or "").strip())
    except ValueError:
        return "domain"
    return "v6" if ip.version == 6 else "v4"


def filter_family(proxies, family):
    """family ∈ {all, v4, v6}:按写明 server 的字面量族过滤。

    域名节点只在 all 下保留:v4/v6 两档的语义是「只测写明为该族的字面量
    入口」,对域名先解析再过滤会把 DoH 抖动带进挑选口径,得不偿失。
    """
    if family == "all":
        return list(proxies)
    return [p for p in proxies if server_family(p.get("server")) == family]


def parse_echo_ip(text, family):
    """回显正文 → 规范化 IP;不是目标族的答案一律不算数。

    回显端点偶尔回错误页或 CDN 的 v4 地址(v6 端点前面的双栈前置就是),
    拿来当「实测出口」会污染映射,所以正文必须解析成 IP 且族匹配。
    `family=None` 接受任意族(trace 的 ip= 字段两个族都合法)。
    """
    if not text:
        return None
    candidate = text.strip().splitlines()[0].strip() if text.strip() else ""
    try:
        ip = ipaddress.ip_address(candidate)
    except ValueError:
        return None
    if family == "v4" and ip.version != 4:
        return None
    if family == "v6" and ip.version != 6:
        return None
    return str(ip)


def fetch_via(port, url, timeout_s):
    """经一条车道的 loopback 入站请求 URL,返回响应正文。

    与 core.egress 同一通道(车道入站 → IN-NAME 钉住的 select 组 → 节点),
    区别只在返回原始正文:回显正文本身就是答案,没有 key=value 可解析。
    """
    handler = urllib.request.ProxyHandler(
        {"http": f"http://127.0.0.1:{int(port)}", "https": f"http://127.0.0.1:{int(port)}"}
    )
    opener = urllib.request.build_opener(handler)
    with opener.open(url, timeout=timeout_s) as resp:
        return resp.read().decode("utf-8", "replace")


def _parse_trace(text):
    fields = {}
    for line in text.splitlines():
        if "=" in line:
            key, _, value = line.partition("=")
            fields[key.strip()] = value.strip()
    return fields


def probe_one(core, port, group, kernel_name, timeout_s):
    """对一个已加载进内核的节点做全套回显;返回结果行。

    select 失败直接抛 CoreError(配置层面的错,不是节点答不上来),由
    probe_all 记成 select 错误;回显失败逐族记录,坏在哪一步在映射里可见。
    """
    core.select(group, kernel_name)
    row = {"exit_v4": None, "exit_v6": None, "country": None, "colo": None,
           "trace_ip": None, "errors": {}}
    for family in ("v4", "v6"):
        last_err = None
        for target in ECHO_TARGETS[family]:
            try:
                text = fetch_via(port, target, timeout_s)
            except Exception as exc:  # noqa: BLE001 - urllib 异常类型很多
                last_err = f"{target} {type(exc).__name__}"
                continue
            ip = parse_echo_ip(text, family)
            if ip:
                row[f"exit_{family}"] = ip
                last_err = None
                break
            last_err = f"{target} 回显非{family}地址: {text.strip()[:40]!r}"
        if last_err:
            row["errors"][family] = last_err
    try:
        fields = _parse_trace(fetch_via(port, TRACE_URL, timeout_s))
        row["country"] = fields.get("loc") or None
        row["colo"] = fields.get("colo") or None
        row["trace_ip"] = parse_echo_ip(fields.get("ip", ""), None)
    except Exception as exc:  # noqa: BLE001
        row["errors"]["trace"] = f"{TRACE_URL} {type(exc).__name__}"
    return row


def probe_all(core, core_cfg, mapping, lanes, timeout_s, log):
    """按车道并发探测;mapping 是 core.prepare() 的产物。

    分桶方式与 engine._verify_egress 相同(第 i 条车道拿 i::lanes),进度
    打点合并到 5 的倍数,几百个节点时不会把日志刷成瀑布。
    """
    from concurrent.futures import ThreadPoolExecutor

    lanes = max(1, int(lanes))
    ports = coremod.lane_ports(core_cfg, lanes)
    buckets = {i: mapping[i::lanes] for i in range(lanes)}
    out, lock, progress = {}, threading.Lock(), [0]

    def run_lane(index):
        group, port = coremod.lane_group(index), ports[index]
        for m in buckets.get(index, []):
            try:
                row = probe_one(core, port, group, m["mihomo"], timeout_s)
            except coremod.CoreError as exc:
                row = {"exit_v4": None, "exit_v6": None, "country": None,
                       "colo": None, "trace_ip": None,
                       "errors": {"select": str(exc)[:120]}}
            out[m["mihomo"]] = row
            with lock:
                progress[0] += 1
                done = progress[0]
                if done % 5 == 0 or done == len(mapping):
                    log("info", f"ipmap 进度 {done}/{len(mapping)}")
        return None

    with ThreadPoolExecutor(max_workers=lanes) as pool:
        list(pool.map(run_lane, range(lanes)))
    return out


def annotate_name(name, row):
    """把实测结果钉到节点名上;这是写回 Sub-Store 的载体。

    实测出口按 v6、v4 的顺序全量写进名字:这份订阅是「映射参考」而不是
    日常驱动,长就长在信息上——名字被客户端截断也比查 JSON 方便。
    """
    row = row or {}
    ips = [row[k] for k in ("exit_v6", "exit_v4") if row.get(k)]
    if not ips:
        errs = row.get("errors") or {}
        detail = (errs.get("select") or errs.get("v6") or errs.get("v4")
                  or errs.get("trace") or "无结果")
        return f"{name} · 实测失败({str(detail)[:36]})"
    bits = ([f"[{row['country']}]"] if row.get("country") else []) + ips
    return f"{name} · {' '.join(bits)}"


def annotated_proxies(mapping, rows):
    """按映射改名后的新 proxy 列表;原对象不动,写明的字段原样保留。

    用 mapping 里的 original 名(上游原名)而不是去重后的内核名,这样
    `A #2` 这种去重痕迹不会写回 Sub-Store。
    """
    out = []
    for m in mapping:
        proxy = dict(m.get("orig_proxy") or {})
        if not proxy:
            continue
        proxy["name"] = annotate_name(str(m.get("original") or proxy.get("name") or ""),
                                      rows.get(m["mihomo"]) or {})
        out.append(proxy)
    return out


def _norm_ip(text):
    """地址比较用的规范化形式;解析不了就原样小写(如域名)。

    ip_address 的 compressed 会去掉 v6 每段的**前导零**:订阅写
    `…f66:04fa:…`,回显正文规范成 `…f66:4fa:…`——同一个地址,字符串直等
    会把它误判成「出口≠入口(疑似中转)」。
    """
    try:
        return ipaddress.ip_address(str(text).strip()).compressed.lower()
    except ValueError:
        return str(text or "").strip().lower()


def landing_note(mapping_row, row):
    """映射表「备注」列:出口和写明的入口是不是同一台。"""
    server = str(mapping_row.get("server") or "")
    family = server_family(server)
    if family == "domain":
        return "入口是域名"
    v6, v4 = (row or {}).get("exit_v6"), (row or {}).get("exit_v4")
    if not v6 and not v4:
        errs = (row or {}).get("errors") or {}
        return "实测失败:" + (errs.get("select") or errs.get("v6") or errs.get("v4") or "无回显")[:40]
    exit_ip = v6 if family == "v6" else (v4 or v6)
    if exit_ip and _norm_ip(exit_ip) == _norm_ip(server):
        return "出口=写明的入口(直落)"
    if exit_ip:
        return "出口≠写明的入口(入口疑似中转)"
    return f"出口只有另一族(v{4 if family == 'v6' else 6})"


def render_report(title, mapping, rows):
    """节点 ↔ 实测出口 的 Markdown 映射表。"""
    stamp = time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime()) + " UTC"
    lines = [
        f"# IP 回显实测映射 — {title}",
        "",
        f"生成时间: {stamp}",
        "",
        "| 节点 | 写明的 server | 实测出口 v6 | 实测出口 v4 | 国别/colo | 备注 |",
        "|---|---|---|---|---|---|",
    ]
    for m in mapping:
        row = rows.get(m["mihomo"]) or {}
        cc = "/".join(x for x in (row.get("country"), row.get("colo")) if x) or "—"
        lines.append(
            "| {n} | {s} | {v6} | {v4} | {cc} | {note} |".format(
                n=str(m.get("original") or "").replace("|", "\\|"),
                s=str(m.get("server") or "—"),
                v6=row.get("exit_v6") or "—", v4=row.get("exit_v4") or "—",
                cc=cc, note=landing_note(m, row).replace("|", "\\|"),
            )
        )
    return "\n".join(lines) + "\n"


def summarize(mapping, rows):
    """面板/日志用的三行汇总:直落、中转、失败各多少。"""
    direct = relayed = failed = 0
    for m in mapping:
        note = landing_note(m, rows.get(m["mihomo"]))
        if note.startswith("出口=写明的入口"):
            direct += 1
        elif note.startswith("出口≠写明的入口"):
            relayed += 1
        else:
            failed += 1
    return direct, relayed, failed


def fetch_subscription(url, timeout_s=30):
    """拉订阅正文;clash UA 优先(多数订阅站认它),浏览器 UA 兜底。"""
    last = None
    for ua in (CLASH_UA, BROWSER_UA):
        req = urllib.request.Request(url, headers={"User-Agent": ua})
        try:
            with urllib.request.urlopen(req, timeout=timeout_s) as resp:
                return resp.read().decode("utf-8", "replace")
        except Exception as exc:  # noqa: BLE001
            last = exc
    raise ImapError(f"订阅拉取失败 {url}: {type(last).__name__}: {last}") from None


def _maybe_base64(text):
    compact = re.sub(r"\s+", "", text)
    if len(compact) < 32:
        return None
    try:
        raw = base64.b64decode(compact + "=" * (-len(compact) % 4))
    except (binascii.Error, ValueError):
        return None
    try:
        return raw.decode("utf-8")
    except UnicodeDecodeError:
        return None


def extract_proxies(body):
    """Clash YAML(裸的或 base64 包着的)→ proxies 列表。

    只认 YAML:分享链接文本明确报错并指向 Sub-Store,而不是半解析出一批
    必然测不通的节点——那种失败和「节点是死的」在结果里无法区分。
    """
    candidates = [body, _maybe_base64(body)]
    for candidate in candidates:
        if not candidate:
            continue
        try:
            data = yaml.safe_load(candidate)
        except yaml.YAMLError:
            continue
        if isinstance(data, dict) and isinstance(data.get("proxies"), list) and data["proxies"]:
            return data["proxies"]
    if body.strip().split(":")[0] in ("vless", "vmess", "ss", "trojan",
                                      "hysteria2", "hy2", "tuic", "ssr"):
        raise ImapError(
            "订阅正文是分享链接文本,本命令不自带链接解析器:"
            "把链接录入 Sub-Store 后用 --source 引用,或用 --file 提供 Clash YAML")
    raise ImapError("订阅正文里没有解析出 proxies(既不是 Clash YAML 也不是它的 base64)")


def start_core(core_cfg, secret, host_dir, log):
    """起一个一次性的 ipmap 内核容器(host 网络,v6 才有路)。"""
    cli = coremod.docker_cli()
    if cli is None:
        raise ImapError("docker 不可用:ipmap 需要一台能跑 mihomo 容器的机器")
    # 同名残留(上次 --keep-core、或中途异常)一律清掉,run -d 才不会撞名。
    subprocess.run([cli, "rm", "-f", core_cfg["container"]],
                   capture_output=True, timeout=60)
    cmd = [
        cli, "run", "-d",
        "--name", core_cfg["container"],
        "--network", "host",
        "--cap-add", "NET_ADMIN", "--cap-add", "NET_RAW",
        "--security-opt", "no-new-privileges:true",
        "--memory", "384m",
        "-v", f"{host_dir}:/root/.config/mihomo",
        IMAGE,
        "-d", "/root/.config/mihomo",
        "-f", "/root/.config/mihomo/config.yaml",
    ]
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=180)
    if proc.returncode != 0:
        raise ImapError(
            "ipmap 内核容器启动失败: " + ((proc.stderr or proc.stdout) or "")[-300:])
    kernel = coremod.Core(core_cfg, secret)
    if not kernel.wait_ready(timeout_s=45):
        raise ImapError("ipmap 内核未就绪: " + kernel.logs(tail=30)[-400:])
    log("info", f"ipmap 内核已启动({core_cfg['container']}, API {core_cfg['api']})")
    return kernel


def stop_core(container, log):
    cli = coremod.docker_cli()
    if cli is None:
        return
    subprocess.run([cli, "rm", "-f", container], capture_output=True, timeout=60)
    log("info", f"ipmap 内核容器已移除({container})")


def push_to_substore(cfg, sub_name, display_name, proxies):
    """把带实测标注的节点 upsert 成 Sub-Store 本地订阅;返回动作。"""
    from .store import Client

    payload = {
        "name": sub_name,
        "displayName": display_name,
        "source": "local",
        "url": "",
        "content": yaml.safe_dump({"proxies": proxies}, allow_unicode=True,
                                  sort_keys=False),
        "mergeSources": "",
        "ignoreFailedRemoteSub": "quiet",
        "passThroughUA": False,
        "process": [],
    }
    store = Client(cfg["substore"]["backend"])
    return store.upsert("sub", sub_name, payload)


def run(args, cfg, log):
    """执行一次 ipmap 探测;返回结果行字典(kernel 名 → row)。"""
    started = time.time()
    if getattr(args, "url", None):
        title = args.url
        proxies = extract_proxies(fetch_subscription(args.url))
    elif getattr(args, "file", None):
        title = str(args.file)
        proxies = extract_proxies(Path(args.file).read_text(encoding="utf-8"))
    elif getattr(args, "source", None):
        from .store import Client

        title = f"substore:{args.source}"
        store = Client(cfg["substore"]["backend"])
        kind = getattr(args, "source_kind", None) or "sub"
        proxies = store.fetch_source(kind, args.source)
    else:
        raise ImapError("需要 --url / --file / --source 之一指定节点来源")

    total = len(proxies)
    proxies = filter_family(proxies, args.family)
    log("info", f"来源「{title}」共 {total} 个节点,family={args.family} 过滤后 "
                f"{len(proxies)} 个待测")
    if not proxies:
        raise ImapError(f"过滤后没有节点:来源 {total} 个,family={args.family}")
    if args.limit:
        proxies = proxies[:args.limit]

    core_dir = cfgmod.DATA / CORE_DIR_NAME
    core_dir.mkdir(parents=True, exist_ok=True)
    core_cfg = {
        "api": f"http://127.0.0.1:{args.api_port}",
        "lanes": args.lanes,
        "base_port": args.base_port,
        "mixed_port": args.mixed_port,
        "container": IPMAP_CONTAINER,
        "container_config_path": "/root/.config/mihomo/config.yaml",
    }
    entries = [{"source": "ipmap", "name": str(p.get("name") or f"node-{i}"),
                "proxy": p, "index": i} for i, p in enumerate(proxies)]
    secret = cfgmod.core_secret()
    kproxies, mapping, dropped = coremod.make_testable(
        entries, core_cfg, secret, log=log, core_dir=core_dir,
        host_dir=cfgmod.HOST_ROOT / "data" / CORE_DIR_NAME)
    for item in dropped:
        log("warn", f"剔除不可用配置的节点 {item['name']}: {item['why']}")
    if not mapping:
        raise ImapError("没有节点能通过内核配置校验")
    log("info", f"内核配置就绪: {len(mapping)} 节点(剔除 {len(dropped)})")

    try:
        kernel = start_core(core_cfg, secret,
                            cfgmod.HOST_ROOT / "data" / CORE_DIR_NAME, log)
    except Exception:
        if not args.keep_core:
            stop_core(IPMAP_CONTAINER, log)
        raise

    try:
        rows = probe_all(kernel, core_cfg, mapping, args.lanes, args.timeout_s, log)
    finally:
        if not args.keep_core:
            stop_core(IPMAP_CONTAINER, log)

    out_dir = cfgmod.DATA / "ipmap"
    out_dir.mkdir(parents=True, exist_ok=True)
    key = args.key or "ipmap"
    payload = {
        "title": title,
        "ts": time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime()) + "Z",
        "family": args.family,
        "nodes": [
            {**m, "row": rows.get(m["mihomo"]) or {}} for m in mapping
        ],
    }
    json_path = Path(args.out) if args.out else out_dir / f"{key}.json"
    json_path.parent.mkdir(parents=True, exist_ok=True)
    json_path.write_text(json.dumps(payload, ensure_ascii=False, indent=1),
                         encoding="utf-8")
    md_path = json_path.with_suffix(".md")
    md_path.write_text(render_report(title, mapping, rows), encoding="utf-8")

    direct, relayed, failed = summarize(mapping, rows)
    log("info", f"ipmap 完成: {len(mapping)} 节点,直落 {direct},中转 {relayed},"
                f"失败 {failed},耗时 {time.time() - started:.0f}s")
    log("info", f"映射已写入 {json_path} 与 {md_path}")
    print(render_report(title, mapping, rows))

    if args.push_sub:
        annotated = annotated_proxies(mapping, rows)
        action = push_to_substore(cfg, args.push_sub,
                                  f"ipmap 实测映射({key})", annotated)
        log("info", f"已写回 Sub-Store 本地订阅 {args.push_sub}({action},"
                    f"{len(annotated)} 个节点,名称带实测出口)")
    return rows
