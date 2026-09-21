"""Single-page dashboard. Rendered server-side so the UI token ships with it."""

PAGE = """<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>__TITLE__</title>
<style>
:root{--bg:#0f1115;--panel:#171a21;--line:#262b36;--fg:#e6e9ef;--dim:#8b93a7;
--ok:#3fb950;--warn:#d29922;--bad:#f85149;--accent:#388bfd;}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);font:14px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI","Noto Sans CJK SC",sans-serif}
header{padding:16px 20px;border-bottom:1px solid var(--line);display:flex;align-items:center;gap:14px;flex-wrap:wrap}
h1{font-size:16px;margin:0;font-weight:600}
.pill{padding:3px 9px;border-radius:20px;border:1px solid var(--line);font-size:12px;color:var(--dim)}
.pill.ok{color:var(--ok);border-color:#1f4d28}
.pill.bad{color:var(--bad);border-color:#5a1f1c}
.pill.warn{color:var(--warn);border-color:#57431a}
main{padding:16px 20px;display:grid;gap:16px}
.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(130px,1fr));gap:10px}
.card{background:var(--panel);border:1px solid var(--line);border-radius:10px;padding:12px 14px}
.card .k{color:var(--dim);font-size:12px}
.card .v{font-size:22px;font-weight:600;margin-top:4px}
.panel{background:var(--panel);border:1px solid var(--line);border-radius:10px;overflow:hidden}
.panel h2{margin:0;padding:11px 14px;font-size:13px;border-bottom:1px solid var(--line);font-weight:600;display:flex;justify-content:space-between;align-items:center;gap:10px}
button{background:#21262d;color:var(--fg);border:1px solid var(--line);border-radius:7px;padding:6px 12px;cursor:pointer;font-size:13px}
button:hover{border-color:var(--accent);color:#fff}
button.primary{background:#1f6feb;border-color:#1f6feb}
button:disabled{opacity:.5;cursor:default}
table{width:100%;border-collapse:collapse;font-size:13px}
th,td{text-align:left;padding:7px 12px;border-bottom:1px solid var(--line);white-space:nowrap}
th{color:var(--dim);font-weight:500;font-size:12px;position:sticky;top:0;background:var(--panel)}
tr:last-child td{border-bottom:0}
.scroll{max-height:460px;overflow:auto}
td.name{max-width:340px;overflow:hidden;text-overflow:ellipsis}
.mono{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:12px}
.s-ok{color:var(--ok)}.s-dead{color:var(--bad)}.s-pending{color:var(--warn)}.s-unknown{color:var(--dim)}
.trend{letter-spacing:1px;font-size:12px}
.trend i{font-style:normal}
.detail{color:var(--dim);font-size:12px;max-width:280px;overflow:hidden;text-overflow:ellipsis}
#log{max-height:230px;overflow:auto;padding:10px 14px;font-family:ui-monospace,monospace;font-size:12px;color:var(--dim)}
#log div{padding:1px 0}
#log .warn{color:var(--warn)}#log .error{color:var(--bad)}
.urlbox{display:flex;gap:8px;align-items:center;padding:8px 14px;flex-wrap:wrap}
.urlbox input{flex:1;min-width:260px;background:#0d1017;border:1px solid var(--line);color:var(--fg);border-radius:7px;padding:6px 10px;font-family:ui-monospace,monospace;font-size:12px}
.grid2{display:grid;grid-template-columns:repeat(auto-fit,minmax(210px,1fr));gap:10px;padding:12px 14px}
label{display:flex;flex-direction:column;gap:4px;color:var(--dim);font-size:12px}
input,select{background:#0d1017;border:1px solid var(--line);color:var(--fg);border-radius:7px;padding:6px 9px;font-size:13px}
.row{display:flex;gap:8px;align-items:center}
.hide{display:none}
.srcbar{display:flex;gap:8px;align-items:center;padding:10px 14px;border-bottom:1px solid var(--line);flex-wrap:wrap}
.srcbar input{flex:1;min-width:180px}
label.inline{flex-direction:row;align-items:center;gap:6px;white-space:nowrap}
#src-table td{vertical-align:middle}
#src-table tr.off td.name{opacity:.5}
#src-table input[type=checkbox]{width:16px;height:16px;cursor:pointer}
.keyinput{width:130px;background:#0d1017;border:1px solid var(--line);color:var(--fg);border-radius:6px;padding:4px 7px;font-family:ui-monospace,monospace;font-size:12px}
.badge{display:inline-block;padding:1px 7px;border-radius:999px;font-size:11px;border:1px solid var(--line);color:var(--dim)}
.badge.collection{color:#79c0ff;border-color:#1f3d5c}
.badge.sub{color:#d2a8ff;border-color:#3d2a5c}
.badge.on{color:var(--ok);border-color:#1f4d28}
.tiny{font-size:12px;color:var(--dim)}
</style>
</head>
<body>
<header>
  <h1>__TITLE__</h1>
  <span class="pill" id="p-round">—</span>
  <span class="pill" id="p-sched">—</span>
  <span class="pill" id="p-busy"></span>
  <span style="flex:1"></span>
  <button class="primary" id="btn-run">立即测试</button>
  <button id="btn-push">推送 Sub-Store</button>
  <button id="btn-settings">设置</button>
</header>
<main>
  <div id="suspect" class="panel hide"><h2 style="color:var(--warn)">⚠️ 护栏提示</h2><div class="urlbox"><span id="suspect-text"></span></div></div>
  <div class="cards" id="cards"></div>

  <div class="panel" id="sources-panel">
    <h2>数据源
      <span class="row">
        <button class="primary" id="btn-src-save">保存并应用</button>
        <button id="btn-link">同步 Sub-Store 联动</button>
        <button id="btn-refresh-src">刷新列表</button>
      </span>
    </h2>
    <div class="srcbar">
      <input id="src-search" placeholder="搜索名称…">
      <select id="src-kind"><option value="">全部类型</option><option value="collection">组合订阅</option><option value="sub">单条订阅</option></select>
      <label class="inline"><input type="checkbox" id="src-onlyon"> 只看已启用</label>
      <button id="btn-manual">＋ 手动添加</button>
    </div>
    <div id="src-manual" class="grid2 hide">
      <label>类型<select id="m-kind"><option value="collection">组合订阅</option><option value="sub">单条订阅</option></select></label>
      <label>Sub-Store 里的名称<input id="m-name" placeholder="例如 air"></label>
      <label>标识 key（决定导出文件名与 URL）<input id="m-key" placeholder="留空按名称自动生成"></label>
      <label>显示名<input id="m-label" placeholder="留空同名称"></label>
      <div class="row" style="grid-column:1/-1">
        <button class="primary" id="btn-manual-add">添加</button>
        <button id="btn-manual-cancel">取消</button>
        <span id="m-err" class="detail"></span>
      </div>
    </div>
    <div class="scroll" style="max-height:330px"><table id="src-table">
      <thead><tr><th style="width:52px">启用</th><th>名称</th><th style="width:88px">类型</th><th>成员/来源</th><th style="width:150px">key</th><th style="width:120px">操作</th></tr></thead>
      <tbody></tbody></table></div>
    <div class="urlbox"><span id="src-summary" class="detail"></span></div>
  </div>

  <div class="panel">
    <h2>订阅输出 <span class="pill">Sub-Store 直接拉这个链接</span></h2>
    <div id="urls"></div>
  </div>

  <div class="panel">
    <h2>节点状态 <span class="pill" id="node-count">—</span></h2>
    <div class="scroll"><table id="nodes">
      <thead><tr><th>源</th><th>节点</th><th>协议</th><th>出口</th><th>IP</th><th>延迟</th><th>连败</th><th>状态</th><th>近况</th><th>最近原因</th></tr></thead>
      <tbody></tbody></table></div>
  </div>

  <div class="panel hide" id="settings-panel">
    <h2>设置 <button id="btn-save">保存</button></h2>
    <div class="grid2" id="settings"></div>
  </div>

  <div class="panel">
    <h2>日志 <button id="btn-clear-log">刷新</button></h2>
    <div id="log"></div>
  </div>
</main>
<script>
const TOKEN = "__TOKEN__";
const api = (p, opt) => fetch(p + (p.includes("?") ? "&" : "?") + "token=" + TOKEN,
  Object.assign({headers:{"Content-Type":"application/json"}}, opt||{}));
const esc = s => String(s==null?"":s).replace(/[&<>"]/g, c => ({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;"}[c]));
/* Bind a handler by id, tolerating a missing element. A top-level
   `getElementById(x).onclick = ...` on an element that only exists after a
   later render throws a TypeError and kills the entire script -- which is
   exactly how this dashboard ended up permanently blank. Warn instead. */
function bind(id, fn){
  const el = document.getElementById(id);
  if (!el){ console.warn("缺少元素 #" + id + "，跳过绑定"); return null; }
  el.onclick = fn;
  return el;
}
const KEY_HINT = "key 含非法字符，只能用字母数字、-、_、. 以及中文";
const BACKSLASH = String.fromCharCode(92);
const KEY_BAD = ["/", ":", "*", "?", '"', "<", ">", "|", BACKSLASH];
function badKey(text){ return KEY_BAD.some(c => String(text).indexOf(c) >= 0); }
const STATUS = {alive:["存活","s-ok"],pending:["观察中","s-pending"],dead:["已死","s-dead"],unknown:["未知","s-unknown"],excluded:["不可测","s-unknown"]};

/* Only repaint a panel when its data actually changed. Replacing these large
   innerHTML blocks every poll made the page relayout constantly, which is
   wasteful on a small VPS and left the layout never stable enough to click. */
const SIGNATURE = {};
function changed(key, value){
  const sig = JSON.stringify(value);
  if (SIGNATURE[key] === sig) return false;
  SIGNATURE[key] = sig;
  return true;
}

async function load(){
  let st, nd, lg;
  try{
    [st, nd, lg] = await Promise.all([
      api("/api/status").then(r=>r.json()),
      api("/api/nodes").then(r=>r.json()),
      api("/api/logs").then(r=>r.json()),
    ]);
  }catch(err){
    // A single failed request must not blank the whole dashboard silently.
    console.error("load failed", err);
    showLoadError("加载失败：" + (err && err.message || err));
    return;
  }
  if (st.error && !st.stats){ showLoadError("接口返回错误：" + st.error); return; }
  if (changed("status", [st.stats, st.last_round, st.busy, st.next_run, st.exports]))
    renderStatus(st);
  if (changed("nodes", nd.nodes)) renderNodes(nd);
  if (changed("logs", lg.events)) renderLogs(lg);
  if (st.config && changed("config", st.config)) { renderSettings(st.config); renderSources(); }
  if (!SOURCES_LOADED) loadSources();
}

function showLoadError(msg){
  const box = document.getElementById("log");
  if (box) box.innerHTML = `<div class="error">${esc(msg)}</div>`;
}

/* ---------------- 数据源面板 ---------------- */
let RESOURCES = [], SOURCES_LOADED = false;

async function loadSources(){
  const box = document.querySelector("#src-table tbody");
  box.innerHTML = `<tr><td colspan="6" class="tiny">读取中…</td></tr>`;
  const r = await api("/api/substore-resources").then(r=>r.json());
  if (r.error){
    box.innerHTML = `<tr><td colspan="6" class="detail s-dead">${esc(r.error)}</td></tr>`;
    return;
  }
  RESOURCES = r.available || [];
  SOURCES_LOADED = true;
  renderSources();
}

function configuredMap(){
  const map = {};
  (CONFIG && CONFIG.sources || []).forEach(s => map[s.kind + "|" + s.name] = s);
  return map;
}

function renderSources(){
  SIGNATURE.sources = null;
  const cfgMap = configuredMap();
  const q = document.getElementById("src-search").value.trim().toLowerCase();
  const kindFilter = document.getElementById("src-kind").value;
  const onlyOn = document.getElementById("src-onlyon").checked;
  const box = document.querySelector("#src-table tbody");
  const rows = RESOURCES.filter(r => {
    const sel = cfgMap[r.kind + "|" + r.name];
    if (q && !r.name.toLowerCase().includes(q)) return false;
    if (kindFilter && r.kind !== kindFilter) return false;
    if (onlyOn && !(sel && sel.enabled)) return false;
    return true;
  });
  if (!rows.length){
    box.innerHTML = `<tr><td colspan="6" class="tiny">没有匹配的订阅。用「＋ 手动添加」直接填名称。</td></tr>`;
  } else {
    box.innerHTML = rows.map(r => {
      const sel = cfgMap[r.kind + "|" + r.name];
      const on = !!(sel && sel.enabled);
      const key = sel ? sel.key : "";
      const meta = r.kind === "collection"
        ? `成员 ${r.members}`
        : `来源 ${esc(r.source_type || "local")}`;
      return `<tr class="${on ? "" : "off"}">
        <td><input type="checkbox" data-on="${esc(r.kind + "|" + r.name)}" ${on ? "checked" : ""}></td>
        <td class="name" title="${esc(r.name)}">${esc(r.name)}</td>
        <td><span class="badge ${r.kind}">${r.kind === "collection" ? "组合" : "单条"}</span></td>
        <td class="tiny">${meta}</td>
        <td>${sel ? `<input class="keyinput" data-key="${esc(r.kind + "|" + r.name)}" value="${esc(key)}">` : '<span class="tiny">—</span>'}</td>
        <td class="tiny">${on ? `导出 /api/export/${esc(key)}.yaml` : ""}</td>
      </tr>`;
    }).join("");
  }
  // sources configured but not present in Sub-Store any more (or added by hand)
  const missing = (CONFIG && CONFIG.sources || []).filter(s =>
    !RESOURCES.some(r => r.kind === s.kind && r.name === s.name));
  if (missing.length){
    box.innerHTML += missing.map(s => `<tr class="${s.enabled ? "" : "off"}">
      <td><input type="checkbox" data-on="${esc(s.kind + "|" + s.name)}" ${s.enabled ? "checked" : ""}></td>
      <td class="name" title="${esc(s.name)}">${esc(s.name)} <span class="badge" style="color:var(--warn);border-color:#57431a">不在列表中</span></td>
      <td><span class="badge ${s.kind}">${s.kind === "collection" ? "组合" : "单条"}</span></td>
      <td class="tiny">手动添加或已改名</td>
      <td><input class="keyinput" data-key="${esc(s.kind + "|" + s.name)}" value="${esc(s.key)}"></td>
      <td class="tiny">导出 /api/export/${esc(s.key)}.yaml</td>
    </tr>`).join("");
  }
  const enabled = (CONFIG && CONFIG.sources || []).filter(s => s.enabled);
  document.getElementById("src-summary").innerHTML =
    `Sub-Store 共 ${RESOURCES.length} 个资源；已选 <b>${enabled.length}</b> 个：` +
    (enabled.map(s => `<span class="badge on">${esc(s.key)}</span>`).join(" ") || "无");
  bindSourceInputs();
}

function bindSourceInputs(){
  document.querySelectorAll("#src-table input[data-on]").forEach(el => {
    el.onchange = () => toggleSource(el.dataset.on, el.checked);
  });
  document.querySelectorAll("#src-table input[data-key]").forEach(el => {
    el.onchange = () => changeKey(el.dataset.key, el.value.trim());
    el.onblur = el.onchange;
  });
}

function splitRef(ref){ const i = ref.indexOf("|"); return [ref.slice(0, i), ref.slice(i + 1)]; }

function nextKey(base){
  const used = new Set((CONFIG.sources || []).map(s => s.key));
  if (!used.has(base)) return base;
  let n = 2;
  while (used.has(base + "-" + n)) n++;
  return base + "-" + n;
}

function slug(text){
  const kept = String(text).split("").filter(c => KEY_BAD.indexOf(c) < 0).join("");
  let out = kept.split(" ").join("-");
  while (out.startsWith(".")) out = out.slice(1);
  return out.slice(0, 48) || "src";
}

async function toggleSource(ref, on){
  const [kind, name] = splitRef(ref);
  const sources = (CONFIG.sources || []).map(s => ({...s}));
  const hit = sources.find(s => s.kind === kind && s.name === name);
  if (hit) hit.enabled = on;
  else sources.push({kind, name, key: nextKey(slug(name)), label: name, enabled: on});
  CONFIG.sources = sources;
  await saveSources(sources);
}

async function changeKey(ref, value){
  const [kind, name] = splitRef(ref);
  const sources = (CONFIG.sources || []).map(s => ({...s}));
  const hit = sources.find(s => s.kind === kind && s.name === name);
  if (!hit) return;
  if (!value){ loadSources(); return; }
  hit.key = value;
  CONFIG.sources = sources;
  await saveSources(sources);
}

async function saveSources(sources){
  const r = await api("/api/config", {method:"POST", body: JSON.stringify({sources})}).then(r=>r.json());
  if (r.error){ alert(r.error); loadSources(); return; }
  CONFIG.sources = r.sources || sources;
  SOURCES_LOADED = true;
  renderSources();
  renderStatusRefresh();
}

async function renderStatusRefresh(){
  const st = await api("/api/status").then(r=>r.json());
  renderStatus(st); renderSettings(st.config);
}

bind("btn-refresh-src", loadSources);
document.getElementById("src-search").oninput = renderSources;
document.getElementById("src-kind").onchange = renderSources;
document.getElementById("src-onlyon").onchange = renderSources;
bind("btn-src-save", async () => {
  await saveSources(CONFIG.sources || []);
  alert("已保存，下一轮生效。想立刻跑点「立即测试」。");
});
/* #btn-alert-test belongs to the settings panel, which renderSettings()
   builds on the first load -- it is bound there, not here. Binding it at
   top level is what used to abort the whole script. */
bind("btn-link", async () => {
  const r = await api("/api/link", {method:"POST", body:"{}"}).then(r=>r.json());
  alert(r.message || r.error || "已同步");
  load();
});
bind("btn-manual", () => {
  document.getElementById("src-manual").classList.toggle("hide");
  document.getElementById("m-err").textContent = "";
});
bind("btn-manual-cancel", () =>
  document.getElementById("src-manual").classList.add("hide"));
bind("btn-manual-add", async () => {
  const name = document.getElementById("m-name").value.trim();
  const kind = document.getElementById("m-kind").value;
  const keyRaw = document.getElementById("m-key").value.trim();
  const label = document.getElementById("m-label").value.trim();
  const err = document.getElementById("m-err");
  if (!name){ err.textContent = "名称不能为空"; return; }
  const key = keyRaw || slug(name);
  if (badKey(keyRaw)){ err.textContent = KEY_HINT; return; }
  const sources = (CONFIG.sources || []).map(s => ({...s}))
    .filter(s => !(s.kind === kind && s.name === name));
  sources.push({kind, name, key: nextKey(key), label: label || name, enabled: true});
  await saveSources(sources);
  document.getElementById("m-name").value = "";
  document.getElementById("m-key").value = "";
  document.getElementById("m-label").value = "";
  document.getElementById("src-manual").classList.add("hide");
  err.textContent = "";
});

function renderStatus(st){
  const s = st.stats || {};
  const cards = [
    ["节点总数", s.total||0, ""],
    ["存活", s.alive||0, "ok"],
    ["观察中", s.pending||0, "warn"],
    ["已死", s.dead||0, "bad"],
    ["本轮存活", (st.last_round&&st.last_round.ok)||0, ""],
    ["上轮耗时", (st.last_round&&st.last_round.duration_s?st.last_round.duration_s+"s":"—"), ""],
  ];
  document.getElementById("cards").innerHTML = cards.map(([k,v,cls])=>
    `<div class="card"><div class="k">${k}</div><div class="v ${cls==="ok"?"s-ok":cls==="bad"?"s-dead":cls==="warn"?"s-pending":""}">${v}</div></div>`).join("");
  document.getElementById("p-round").textContent = st.last_round
    ? `第${st.last_round.id}轮 ${st.last_round.finished_at||"进行中"}` : "尚未运行";
  document.getElementById("p-sched").textContent = st.next_run
    ? `下次 ${st.next_run}（每${st.config.schedule.interval_minutes}分钟）` : "定时已关闭";
  document.getElementById("p-busy").textContent = st.busy ? "⏳ 正在测试" : "";
  document.getElementById("p-busy").className = "pill" + (st.busy ? " warn" : "");
  const sp = document.getElementById("suspect");
  if (st.last_round && st.last_round.suspect){
    sp.classList.remove("hide");
    document.getElementById("suspect-text").textContent = st.last_round.note || "本轮结果可疑，已保留上一轮输出";
  } else sp.classList.add("hide");
  const urls = document.getElementById("urls");
  urls.innerHTML = (st.exports||[]).map(e =>
    `<div class="urlbox"><span class="mono">${esc(e.key)}</span>
      <input readonly value="${esc(e.url)}">
      <span class="pill">${e.count} 节点</span>
      <button onclick="copyUrl(this)">复制</button></div>`).join("") || `<div class="urlbox">还没有输出，先跑一轮</div>`;
}

function copyUrl(btn){
  const input = btn.parentElement.querySelector("input");
  navigator.clipboard.writeText(input.value).then(()=>{btn.textContent="已复制";setTimeout(()=>btn.textContent="复制",1200)});
}

function renderNodes(nd){
  const tb = document.querySelector("#nodes tbody");
  document.getElementById("node-count").textContent = nd.nodes.length + " 个";
  tb.innerHTML = nd.nodes.map(n=>{
    const [label, cls] = STATUS[n.status] || STATUS.unknown;
    const trend = (n.trend||[]).map(v=>v==="ok"?'<i class="s-ok">●</i>':v==="excluded"?'<i class="s-unknown">·</i>':'<i class="s-dead">○</i>').join("");
    return `<tr>
      <td class="mono">${esc(n.source)}</td>
      <td class="name" title="${esc(n.name)}">${esc(n.name)}</td>
      <td class="mono">${esc(n.proto)}</td>
      <td class="mono">${esc(n.country||"—")}</td>
      <td class="mono">${n.ip_total>1?(n.ip_alive||0)+"/"+n.ip_total:"—"}</td>
      <td class="mono">${n.last_delay_ms!=null?n.last_delay_ms+"ms":"—"}</td>
      <td class="mono">${n.consec_fail||0}</td>
      <td class="${cls}">${label}</td>
      <td class="trend">${trend||"—"}</td>
      <td class="detail" title="${esc(n.last_reason)}">${esc(n.last_reason||"—")}</td>
    </tr>`}).join("");
}

function renderLogs(lg){
  document.getElementById("log").innerHTML = (lg.events||[]).map(e=>
    `<div class="${e.level}">${esc(e.ts)}  ${esc(e.message)}</div>`).join("");
}

let CONFIG = null;
function renderSettings(cfg){
  CONFIG = cfg;
  const p = document.getElementById("settings");
  p.innerHTML = `
    <label>测试间隔（分钟）<input id="s-interval" type="number" value="${cfg.schedule.interval_minutes}"></label>
    <label>并发数<input id="s-concurrency" type="number" value="${cfg.test.concurrency}"></label>
    <label>超时（毫秒）<input id="s-timeout" type="number" value="${cfg.test.timeout_ms}"></label>
    <label>重试超时（毫秒）<input id="s-timeout2" type="number" value="${cfg.test.timeout_ms_retry}"></label>
    <label>单节点最大尝试次数<input id="s-attempts" type="number" value="${cfg.test.max_attempts}"></label>
    <label>连续失败几轮判死<input id="s-drop" type="number" value="${cfg.policy.drop_after_consecutive_fails}"></label>
    <label>护栏比例（低于上轮此比例则不发布）<input id="s-floor" type="number" step="0.05" value="${cfg.policy.suspect_floor_ratio}"></label>
    <label>出口验证<select id="s-verify"><option value="1"${cfg.verify.enabled?" selected":""}>开启</option><option value="0"${cfg.verify.enabled?"":" selected"}>关闭</option></select></label>
    <label>排除出口国家（逗号分隔）<input id="s-exclude" value="${(cfg.verify.exclude_countries||[]).join(",")}"></label>
    <label>定时任务<select id="s-sched"><option value="1"${cfg.schedule.enabled?" selected":""}>开启</option><option value="0"${cfg.schedule.enabled?"":" selected"}>关闭</option></select></label>
    <label>告警<select id="s-alert"><option value="1"${cfg.alert.enabled?" selected":""}>开启</option><option value="0"${cfg.alert.enabled?"":" selected"}>关闭</option></select></label>
    <label>存活节点下限（低于则告警，0=关闭）<input id="s-floor-alive" type="number" value="${cfg.alert.alive_floor}"></label>
    <label>同类告警冷却（分钟）<input id="s-cooldown" type="number" value="${cfg.alert.cooldown_minutes}"></label>
    <label>Telegram Bot Token<input id="s-tg-token" value="${esc(cfg.alert.telegram.token||"")}" placeholder="123456:ABC-DEF..."></label>
    <label>Telegram Chat ID<input id="s-tg-chat" value="${esc(cfg.alert.telegram.chat_id||"")}"></label>
    <label style="grid-column:1/-1">Webhook URL（POST JSON）<input id="s-webhook" value="${esc(cfg.alert.webhook.url||"")}" placeholder="https://…"></label>
    <div class="row" style="grid-column:1/-1">
      <label class="inline"><input type="checkbox" id="s-tg-on"${cfg.alert.telegram.enabled?" checked":""}> 启用 Telegram</label>
      <label class="inline"><input type="checkbox" id="s-hook-on"${cfg.alert.webhook.enabled?" checked":""}> 启用 Webhook</label>
      <button id="btn-alert-test">发送测试告警</button>
    </div>
    <label>测活后写入 Sub-Store<select id="s-push"><option value="0"${cfg.publish.push_to_substore?"":" selected"}>关闭</option><option value="1"${cfg.publish.push_to_substore?" selected":""}>开启</option></select></label>
    <label style="grid-column:1/-1">测试目标（每行一个）<textarea id="s-targets" rows="4" style="background:#0d1017;border:1px solid var(--line);color:var(--fg);border-radius:7px;padding:6px 9px;font-family:ui-monospace,monospace">${(cfg.test.targets||[]).join("\\n")}</textarea></label>`;
  // Built just above, so it can only be bound after this markup exists.
  // Binding it at top level (before any render) is what used to throw
  // "Cannot set properties of null" and blank the entire dashboard.
  bind("btn-alert-test", async () => {
    await saveSettings();
    const r = await api("/api/alert-test",{method:"POST",body:"{}"}).then(r=>r.json());
    alert(r.message || r.error || "已发送");
  });
}

bind("btn-run", async () => {
  const btn = document.getElementById("btn-run"); btn.disabled = true; btn.textContent = "测试中…";
  try{
    const r = await api("/api/run",{method:"POST",body:"{}"}).then(r=>r.json());
    if (r.error) alert("未能启动: " + r.error);
  } finally { btn.disabled = false; btn.textContent = "立即测试"; load(); }
});
bind("btn-push", async () => {
  const r = await api("/api/push",{method:"POST",body:"{}"}).then(r=>r.json());
  alert(r.message || "已推送"); load();
});
bind("btn-settings", () =>
  document.getElementById("settings-panel").classList.toggle("hide"));
bind("btn-clear-log", load);
async function saveSettings(){
  const val = id => document.getElementById(id).value;
  const patch = {
    schedule:{interval_minutes:+val("s-interval"), enabled:val("s-sched")==="1"},
    test:{concurrency:+val("s-concurrency"), timeout_ms:+val("s-timeout"),
          timeout_ms_retry:+val("s-timeout2"), max_attempts:+val("s-attempts"),
          targets:val("s-targets").split("\\n").map(s=>s.trim()).filter(Boolean)},
    policy:{drop_after_consecutive_fails:+val("s-drop"), suspect_floor_ratio:+val("s-floor")},
    verify:{enabled:val("s-verify")==="1",
            exclude_countries:val("s-exclude").split(",").map(s=>s.trim()).filter(Boolean)},
    publish:{push_to_substore:val("s-push")==="1"},
    alert:{enabled:val("s-alert")==="1", alive_floor:+val("s-floor-alive"),
           cooldown_minutes:+val("s-cooldown"),
           telegram:{enabled:document.getElementById("s-tg-on").checked,
                     token:val("s-tg-token"), chat_id:val("s-tg-chat")},
           webhook:{enabled:document.getElementById("s-hook-on").checked,
                    url:val("s-webhook")}},
  };
  const r = await api("/api/config",{method:"POST",body:JSON.stringify(patch)}).then(r=>r.json());
  CONFIG = r.config || CONFIG;
  if (r.error) alert(r.error); else load();
  return r;
}
bind("btn-save", async () => {
  const r = await saveSettings();
  if (!r.error) alert("已保存");
});
load();
setInterval(load, 5000);
</script>
</body>
</html>
"""


def render(title, token):
    return PAGE.replace("__TITLE__", title).replace("__TOKEN__", token)
