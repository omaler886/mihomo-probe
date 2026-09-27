// probe_filter.script.js — Sub-Store Script Operator：按 probe 测活账本过滤/标注节点
//
// 用途：消费独立 probe 服务（mihomo_test）的只读账本端点 GET <probe_url>，
//   按 name（精确）→ server（兜底，多记录状态矛盾视为无记录）匹配当前订阅节点。
//   mode=filter   仅删除 status=dead 的节点（pending/unknown/excluded 保留）
//   mode=annotate 存活节点名尾追加 " [CC]"（实测国别），dead 追加 " ·dead"，幂等
//   mode=both     先 filter 后 annotate
//   missing=keep  probe 不可达/节点无记录时保留（默认，失败开放不误杀）
//   missing=drop  无记录即删（严格模式，慎用）
//   probe_url 缺失为配置错误：直接抛错让本次 produce 显式失败（不静默失效）。
//
// 用法：Sub-Store 订阅 → 操作(process) → 添加「脚本操作」，粘贴本文件全文，传参：
//   probe_url=https://probe.example.com/api/probe/nodes
//   probe_token=<probe 的 publish.token>   （以 X-Auth-Token 头发送，勿拼进 URL）
//   mode=filter&missing=keep
// 契约细节：仓库 ARCHITECTURE.md §3/§4。

async function operator(proxies, targetPlatform, context) {
  const args = (typeof $arguments === "object" && $arguments) || {};
  const mode = ["filter", "annotate", "both"].includes(args.mode) ? args.mode : "filter";
  const missing = args.missing === "drop" ? "drop" : "keep";
  const url = String(args.probe_url || "").trim();
  if (!url) throw new Error("probe_filter: $arguments.probe_url 缺失");

  // 空入站数组直接原样返回（放在 probe_url 校验之后：配置错误即使在空订阅上
  // 也要红掉，不能借「没节点」溜过去；但空订阅不值得再付一次 fetch）。
  if (!Array.isArray(proxies) || proxies.length === 0) return proxies;

  let nodes = null;                       // null = 「按 missing 策略」的失败/无记录态
  try {
    const ctrl = typeof AbortController === "function" ? new AbortController() : null;
    const timer = ctrl ? setTimeout(() => ctrl.abort(), 5000) : null;
    try {
      // token 只走请求头（服务端三传法之一），不进 URL，避免落反代 access log。
      const headers = args.probe_token ? { "X-Auth-Token": String(args.probe_token) } : {};
      const resp = await fetch(url, { headers, signal: ctrl ? ctrl.signal : undefined });
      if (resp.ok) {
        const body = JSON.parse(await resp.text());
        if (body && body.ok === true && Array.isArray(body.nodes)) nodes = body.nodes;
      }
      if (!nodes) console.log(`[probe_filter] probe 应答不可用（HTTP ${resp.status}）→ missing=${missing}`);
    } finally {
      if (timer) clearTimeout(timer);
    }
  } catch (e) {
    console.log(`[probe_filter] probe 不可达（${(e && e.name) || e}）→ missing=${missing}`);
  }

  const keepMissing = missing === "keep";
  if (!nodes) return keepMissing ? proxies : [];   // missing=drop：probe 不可用即清空（严格模式的代价）

  const byName = new Map(nodes.map(n => [n.name, n]));
  const byServer = new Map();
  for (const n of nodes) {
    if (!n.server) continue;
    const k = String(n.server).toLowerCase().replace(/\.$/, "");
    let bucket = byServer.get(k);
    if (!bucket) { bucket = []; byServer.set(k, bucket); }
    bucket.push(n);
  }
  // name 精确 → server 兜底；同 host 多记录状态互相矛盾 → 视为无记录
  //（账本无 port 列，无法区分同 host 多端口节点，宁可不裁，防误杀）。
  const hit = (p) => {
    const exact = byName.get(p.name);
    if (exact) return exact;
    if (!p.server) return null;
    const cands = byServer.get(String(p.server).toLowerCase().replace(/\.$/, "")) || [];
    if (cands.length === 0) return null;
    const first = cands[0].status;
    if (cands.every(c => c.status === first)) return cands[0];
    return null;
  };
  const aliveTag = (n) => (n.country && /^[A-Za-z]{2}$/.test(String(n.country)))
    ? ` [${String(n.country).toUpperCase()}]` : "";
  // 幂等：追加前先剥掉本脚本上次留下的尾缀（各一次），防二次处理叠加成
  // `foo ·dead ·dead`。用尾缀而非前缀，避免与 probe 导出的既有前缀标签 `[CC] ` 打架。
  const strip = (s) => s.replace(/ ·dead$/, "").replace(/ \[[A-Z]{2}\]$/, "");

  if (mode === "annotate") {
    for (const p of proxies) {
      const n = hit(p);
      if (!n) continue;
      if (n.status === "dead") p.name = strip(p.name) + " ·dead";
      else if (n.status === "alive") p.name = strip(p.name) + aliveTag(n);
    }
    return proxies;
  }
  // filter / both：删除且仅删除 dead（pending/unknown/excluded 不裁）；missing=keep 时无记录保留
  return proxies.filter(p => {
    const n = hit(p);
    if (!n) return keepMissing;
    if (n.status === "dead") return false;
    if (mode === "both" && n.status === "alive") p.name = strip(p.name) + aliveTag(n);
    return true;
  });
}
