/* mihomo 测活中心 — 面板前端
 *
 * 前后端分离：本文件是**纯静态资源**，后端只提供 /api/*。
 *   · 同源模式：后端把 index.html 和这几个资源一起发出去，API_BASE 为空。
 *   · CDN 模式：CF Pages 托管本文件，bootstrap 里的 apiBase 指向后端，
 *     走 CORS（后端 cors.origins 白名单）—— 令牌从 URL / localStorage 取，
 *     用 X-Auth-Token 头发送，**不再随页面下发**。
 *
 * 一批离线验证脚本（.local/diag/verify_*.py）用正则解析渲染后的 DOM，
 * 所以下面几处**结构字符串不能改**：
 *   · 卡片必须是 `<div class="card">`
 *   · `<table id="src-table">` 的 id 必须是第一个属性，后面紧跟 <thead>
 *   · 行内复选框必须 `<input type="checkbox" data-x="ref" …>`（type 在前）
 *   · 节点行必须是无属性的 `<tr>`；日志行必须 `<div class="level">`
 *   · 来源行必须固定 10 个 <td>（verify_kind_filter 读 children[6]/[7]）
 *   · thead 里只能有 5 个 <span>（就是那 5 个列名）
 */

/* ---------------- 引导与令牌 ---------------- */

const BOOT = (() => {
  try {
    return JSON.parse(document.getElementById("bootstrap").textContent) || {};
  } catch (err) {
    console.error("bootstrap 解析失败", err);
    return {};
  }
})();

/* 后端地址。同源部署为空串；CDN 部署由构建脚本写死成后端的绝对地址。 */
const API_BASE = String(BOOT.apiBase || "").replace(/\/+$/, "");

const TOKEN_KEY = "mihomo-token";

/* 令牌来源优先级：服务端注入（同源模式）> URL ?token= / #token= > 本机已存。
 * URL 排在本机已存前面，是因为「拿一条新链接打开」的意图就是用链接里那个。 */
function readTokenFromUrl() {
  try {
    const qs = new URLSearchParams(location.search);
    let t = (qs.get("token") || "").trim();
    if (!t && location.hash.length > 1) {
      const h = new URLSearchParams(location.hash.slice(1));
      t = (h.get("token") || "").trim();
    }
    return t;
  } catch (err) {
    return "";
  }
}

function storedToken() {
  try { return (localStorage.getItem(TOKEN_KEY) || "").trim(); } catch (err) { return ""; }
}

/* 把令牌从地址栏抹掉。留在 URL 里就会进浏览历史、Referer 和各种日志 ——
 * 静态前端把令牌交给 localStorage 之后，URL 没有任何理由继续带着它。 */
function scrubUrl() {
  try {
    const u = new URL(location.href);
    if (!u.searchParams.has("token") && !location.hash) return;
    u.searchParams.delete("token");
    u.hash = "";
    const q = u.searchParams.toString();
    history.replaceState(null, "", u.pathname + (q ? "?" + q : ""));
  } catch (err) { /* 非 http(s) 环境（file://）忽略 */ }
}

let TOKEN = (String(BOOT.token || "").trim() || readTokenFromUrl() || storedToken()).trim();
if (readTokenFromUrl() && readTokenFromUrl() === TOKEN) {
  try { localStorage.setItem(TOKEN_KEY, TOKEN); } catch (err) { /* 忽略 */ }
}
if (TOKEN) scrubUrl();

/* 标题来自服务端（同一份 index.html 要给多个部署用），所以在这里落到 DOM 上。 */
if (BOOT.title) {
  document.title = BOOT.title;
  const titleEl = document.getElementById("app-title");
  if (titleEl) titleEl.textContent = BOOT.title;
}

/* ---------------- 主题 ---------------- */

const THEME_KEY = "mihomo-theme";
const THEME_LABEL = { auto: "跟随系统", light: "浅色", dark: "深色" };
let THEME = "auto";
try { THEME = localStorage.getItem(THEME_KEY) || "auto"; } catch (err) { /* 忽略 */ }

function prefersDark() {
  return !!(window.matchMedia
    && window.matchMedia("(prefers-color-scheme: dark)").matches);
}

/* What a mode actually renders as, which is not the same as the mode itself:
   `auto` on a light-preference system renders light. */
function resolvesDark(mode) {
  return mode === "dark" || (mode === "auto" && prefersDark());
}

function applyTheme(mode, persist) {
  THEME = mode;
  document.documentElement.setAttribute("data-theme", resolvesDark(mode) ? "dark" : "light");
  if (persist !== false) {
    try {
      if (mode === "auto") localStorage.removeItem(THEME_KEY);
      else localStorage.setItem(THEME_KEY, mode);
    } catch (err) { /* 忽略 */ }
  }
  const btn = document.getElementById("btn-theme");
  if (btn) {
    btn.textContent = mode === "auto" ? "◐" : (mode === "dark" ? "☾" : "☀");
    btn.title = "主题：" + THEME_LABEL[mode] + "（点击切换）";
  }
}

/* Pick the next mode that actually *looks* different.

   A fixed auto → light → dark cycle has a dead click: on a light-preference
   system `auto` already renders light, so the first click changes the button
   glyph and nothing else -- which reads as "the button is broken". Skipping any
   candidate that resolves to the current appearance makes every click visible,
   which is the only feedback the button has. */
function nextTheme() {
  const order = ["auto", "light", "dark"];
  const now = resolvesDark(THEME);
  const from = order.indexOf(THEME);
  for (let i = 1; i <= order.length; i++) {
    const candidate = order[(from + i) % order.length];
    if (resolvesDark(candidate) !== now) return candidate;
  }
  return THEME === "dark" ? "light" : "dark";
}

if (window.matchMedia) {
  const mq = window.matchMedia("(prefers-color-scheme: dark)");
  const onChange = () => { if (THEME === "auto") applyTheme("auto", false); };
  if (mq.addEventListener) mq.addEventListener("change", onChange);
  else if (mq.addListener) mq.addListener(onChange);
}
applyTheme(THEME, false);

/* ---------------- 基础工具 ---------------- */

const api = (p, opt) => {
  const o = Object.assign({}, opt || {});
  const headers = Object.assign({ "X-Auth-Token": TOKEN }, o.headers || {});
  if (o.body) headers["Content-Type"] = "application/json";
  o.headers = headers;
  // 跨域时带上凭证之外的语义：这是 bearer 令牌，不需要 cookies。
  o.credentials = "omit";
  return fetch(API_BASE + p, o);
};

const esc = s => String(s == null ? "" : s).replace(/[&<>"]/g,
  c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

/* Bind a handler by id, tolerating a missing element. A top-level
   `getElementById(x).onclick = ...` on an element that only exists after a
   later render throws a TypeError and kills the entire script -- which is
   exactly how this dashboard ended up permanently blank. Warn instead. */
function bind(id, fn) {
  const el = document.getElementById(id);
  if (!el) { console.warn("缺少元素 #" + id + "，跳过绑定"); return null; }
  el.onclick = fn;
  return el;
}

/* Same contract as `bind`, for any other event.
 *
 * A top-level `getElementById(x).onchange = …` on an element that is missing
 * throws a TypeError and terminates the whole script -- which is precisely how
 * this dashboard once ended up permanently blank (static markup painted, every
 * panel empty, and nothing in the UI to say why). `bind` exists for that
 * reason; this is the same guard for the events `onclick` does not cover. */
function bindEvent(id, event, fn) {
  const el = document.getElementById(id);
  if (!el) { console.warn("缺少元素 #" + id + "，跳过绑定 " + event); return null; }
  el.addEventListener(event, fn);
  return el;
}

/* Resolve a server-relative URL against the backend.
 *
 * `engine.export_url` returns a path (`/api/export/<key>.yaml?token=…`) because
 * the backend has no idea which origin it is being reached from. On a
 * same-origin deployment that is exactly right. On the CDN deployment the page
 * lives on a *different* origin, so an unprefixed path resolves against the
 * static host: the 复制 button would hand out a link to `cobalt-anchor.pages.dev`
 * that 404s. Anything already absolute is left alone, so this stays correct if
 * the server ever starts returning a full URL. */
function absUrl(path) {
  const text = String(path == null ? "" : path);
  return text.startsWith("/") ? API_BASE + text : text;
}

const KEY_HINT = "key 含非法字符，只能用字母数字、-、_、. 以及中文";
const BACKSLASH = String.fromCharCode(92);
const KEY_BAD = ["/", ":", "*", "?", '"', "<", ">", "|", BACKSLASH];
function badKey(text) { return KEY_BAD.some(c => String(text).indexOf(c) >= 0); }

const STATUS = {
  alive: ["存活", "s-ok"], pending: ["观察中", "s-pending"],
  dead: ["已死", "s-dead"], unknown: ["未知", "s-unknown"],
  excluded: ["不可测", "s-unknown"],
};

/* Only repaint a panel when its data actually changed. Replacing these large
   innerHTML blocks every poll made the page relayout constantly, which is
   wasteful on a small VPS and left the layout never stable enough to click. */
const SIGNATURE = {};
function changed(key, value) {
  const sig = JSON.stringify(value);
  if (SIGNATURE[key] === sig) return false;
  SIGNATURE[key] = sig;
  return true;
}

/* ---------------- 结果提示 ---------------- */
/* Replaces the native `alert()` for every action that reports an outcome.
   A dialog holding "a: …；b: …；c: …" scrolled the line that mattered off the
   bottom on a phone, could not be re-read after dismissal and could not be
   copied. Here the summary sits on top, one outcome per row below it, and
   failures sort first. */
let NOTICE_TIMER = null;

/* `action` 可选：`{label, run, primary}`。给了它就挂一个按钮，并且**不自动消失**
   —— 需要用户拍板的东西不能自己溜走。 */
function showNotice(title, message, detail, level, action) {
  const box = document.getElementById("notice");
  if (!box) return;
  document.getElementById("notice-title").textContent = title;
  const msg = document.getElementById("notice-msg");
  msg.textContent = message || "";
  msg.className = "notice-msg " + (level || "");
  // Tolerate both shapes: the push endpoint sends records, the link endpoint
  // still sends plain sentences.
  const items = (detail || []).map(d => typeof d === "string" ? { text: d } : d);
  items.sort((a, b) => (b.level === "error") - (a.level === "error"));
  document.getElementById("notice-list").innerHTML = items.map(d =>
    `<div class="notice-line ${esc(d.level || "")}">${esc(d.text)}</div>`).join("");
  const act = document.getElementById("notice-action");
  if (act) {
    if (action) {
      act.textContent = action.label || "确认";
      act.className = action.primary === false ? "" : "primary";
      act.onclick = () => { hideNotice(); action.run(); };
    } else {
      act.className = "hide";
      act.onclick = null;
    }
  }
  box.className = "panel" + (level ? " lv-" + level : "");
  if (NOTICE_TIMER) { clearTimeout(NOTICE_TIMER); NOTICE_TIMER = null; }
  // A clean run clears itself; anything that failed stays until dismissed,
  // because that is the one the operator has to act on. A notice carrying a
  // decision waits for that decision instead.
  if (!action && !items.some(d => d.level === "error")) {
    NOTICE_TIMER = setTimeout(hideNotice, 25000);
  }
}

function hideNotice() {
  if (NOTICE_TIMER) { clearTimeout(NOTICE_TIMER); NOTICE_TIMER = null; }
  const box = document.getElementById("notice");
  if (box) box.classList.add("hide");
}

/* Reports an API result that carries `{message, detail}`; returns true when it
   was an error, so callers can stop before they claim success. */
function noticeResult(title, r) {
  if (r && r.error) { showNotice(title + "失败", r.error, [], "error"); return true; }
  const items = (r && r.detail) || [];
  const bad = items.some(d => d && d.ok === false) ||
    items.some(d => typeof d === "string" && d.indexOf("失败") >= 0);
  showNotice(title, (r && r.message) || "完成", items, bad ? "warn" : "ok");
  return false;
}

bind("btn-notice-close", hideNotice);

/* ---------------- 数据加载 ---------------- */

async function load() {
  let rs, rn, rl, rc;
  try {
    [rs, rn, rl, rc] = await Promise.all([
      api("/api/status"),
      api("/api/nodes"),
      api("/api/logs"),
      api("/api/stats" + (CAT_ROUND ? "?round_id=" + CAT_ROUND : "")),
    ]);
  } catch (err) {
    // A single failed request must not blank the whole dashboard silently.
    console.error("load failed", err);
    showLoadError("加载失败：" + (err && err.message || err));
    return;
  }
  // 401 要单独处理，不能和别的错误混在一起：令牌轮换过、或者本机存了个错的，
  // 继续每 5 秒轮询只会反复拿 401，而页面看起来只是「空的」—— 用户没有任何
  // 提示说该去哪儿改。回到令牌闸门，把恢复路径摆在他面前。
  if (rs.status === 401 || rn.status === 401 || rl.status === 401 || rc.status === 401) {
    tokenRejected();
    return;
  }
  let st, nd, lg, cs;
  try {
    [st, nd, lg, cs] = await Promise.all([rs.json(), rn.json(), rl.json(), rc.json()]);
  } catch (err) {
    console.error("load parse failed", err);
    showLoadError("响应解析失败：" + (err && err.message || err));
    return;
  }
  if (st.error && !st.stats) { showLoadError("接口返回错误：" + st.error); return; }
  // `busy_mode` is part of the signature on purpose: two consecutive manual
  // rounds differ only in that field, so leaving it out kept `st.busy` true
  // across the switch and the header went on showing the previous round's
  // mode. `changed()` only re-renders when the listed values differ.
  if (changed("status", [st.stats, st.last_round, st.busy, st.busy_mode,
                         st.next_run, st.exports]))
    renderStatus(st);
  if (changed("nodes", nd.nodes)) renderNodes(nd);
  if (changed("logs", lg.events)) renderLogs(lg);
  if (cs && !cs.error && changed("cats", cs)) renderCategories(cs);
  if (st.config && changed("config", st.config)) { renderSettings(st.config); renderSources(); }
  if (!SOURCES_LOADED) loadSources();
}

function showLoadError(msg) {
  const box = document.getElementById("log");
  if (box) box.innerHTML = `<div class="error">${esc(msg)}</div>`;
}

/* ---------------- 数据源面板 ---------------- */

let RESOURCES = [], SOURCES_LOADED = false;

async function loadSources() {
  const box = document.querySelector("#src-table tbody");
  box.innerHTML = `<tr><td colspan="10" class="tiny">读取中…</td></tr>`;
  const r = await api("/api/substore-resources").then(r => r.json());
  if (r.error) {
    box.innerHTML = `<tr><td colspan="10" class="detail s-dead">${esc(r.error)}</td></tr>`;
    return;
  }
  RESOURCES = r.available || [];
  SOURCES_LOADED = true;
  renderSources();
}

function configuredMap() {
  // Keyed by kind|name because that is what Sub-Store's resource list gives us.
  //
  // Two configured sources can share a kind and name -- 链式聚合 is a collection
  // named `air`, and so is the plain `air` entry it was derived from. Last-one-
  // wins would let a disabled twin shadow the live one, so the enabled entry
  // always takes the slot; ties go to the first seen. Anything less makes a row
  // render another source's key and export URL, which is how the 导出 column
  // first showed a wrong value.
  const map = {};
  (CONFIG && CONFIG.sources || []).forEach(s => {
    const k = s.kind + "|" + s.name;
    const cur = map[k];
    if (!cur || (!cur.enabled && s.enabled)) map[k] = s;
  });
  return map;
}

/* 类型筛选。`collection` / `sub` 比的是 kind；`remote` / `local` 比的是**单条订阅的
   来源** —— Sub-Store 的 `source` 字段：从远程 URL 拉的是 `remote`，内容内嵌在
   Sub-Store 里的是 `local`（`store.list_resources` 把它取成 `source_type`）。
   组合订阅没有这个字段，所以在后两个筛选项下不出现 —— 这是对的，它们不是订阅，
   是订阅的组合。 */
function matchKindFilter(r, want) {
  if (want === "remote" || want === "local") {
    // 拿不到 `source_type` 的行（组合订阅、以及已配置但不在 Sub-Store 列表里的来源）
    // 判不出来源类型，这两个筛选项下都不显示 —— 猜一个只会给出错的答案。
    if (r.source_type == null) return false;
    return r.kind === "sub" && r.source_type === want;
  }
  return r.kind === want;
}

function renderSources() {
  SIGNATURE.sources = null;
  const cfgMap = configuredMap();
  const q = document.getElementById("src-search").value.trim().toLowerCase();
  const kindFilter = document.getElementById("src-kind").value;
  const onlyOn = document.getElementById("src-onlyon").checked;
  const box = document.querySelector("#src-table tbody");
  // 本系统自己的输出集合（`collection/<publish.prefix>`，线上是 `probe`）不能当输入
  // 来源 —— 服务端 `reject_self_reference` 会拒掉整批保存。见 `selfRef` 那处的注释。
  const prefix = String((CONFIG && CONFIG.publish || {}).prefix || "").trim();
  /* 一套筛选条件，资源行和下面「已配置但不在 Sub-Store 列表里」的行共用 ——
     否则筛选只作用于半张表。 */
  const pass = (r, sel) => {
    if (q && !r.name.toLowerCase().includes(q)) return false;
    if (kindFilter && !matchKindFilter(r, kindFilter)) return false;
    if (onlyOn && !(sel && sel.enabled)) return false;
    return true;
  };
  const rows = RESOURCES.filter(r => pass(r, cfgMap[r.kind + "|" + r.name]));
  // Sources that the main loop below does not render a row for.
  //
  // This cannot be a plain "is the kind|name in Sub-Store's list" test. Two
  // configured sources may share both -- 链式聚合 and air are both collections
  // named `air` -- while Sub-Store lists that resource once. The main loop emits
  // one row, `cfgMap` hands it to the enabled twin, and the other would fall
  // through both branches and become invisible: unmanageable from the panel,
  // and the reason `air` looked like dead weight rather than a deliberate entry.
  //
  // So compare against what will actually be consumed. `cfgMap[k]` is precisely
  // the source a rendered row shows for key k, so anything not identical to it
  // has no row of its own.
  //
  // ⚠️ 必须按**全部**资源算，不能按筛选后的 `rows`：`rendered` 一旦随筛选收缩，
  // 被筛掉的那些 kind 的已配置来源就全掉进 missing 分支，以「已配置，键 XXX」的行
  // 冒出来 —— 筛「组合订阅」时会冒出全部单条来源，正是这个原因。
  const rendered = new Set(
    RESOURCES.map(r => cfgMap[r.kind + "|" + r.name])
      .filter(Boolean)
      .map(s => s.key));
  const missing = (CONFIG && CONFIG.sources || [])
    .filter(s => !rendered.has(s.key))
    .filter(s => pass(s, s));
  /* 两个行生成器都只往 `items` 里攒，最后统一排序渲染 —— 启用的行要排到最前面，
     主列表和 missing 行必须进**同一个**排序：只排主列表的话，手动添加的启用来源
     仍会沉在全部禁用行后面。排序键：启用优先；同状态下主列表行在 missing 行前；
     `i` 兜底，组内维持 Sub-Store 的原顺序。 */
  const items = [];
  rows.forEach(r => {
    const sel = cfgMap[r.kind + "|" + r.name];
    const on = !!(sel && sel.enabled);
    // A muted source is still tested and still listed -- it just owns no
    // export file. That is what the 导出 checkbox controls; absent = export.
    const exp = !sel || sel.export !== false;
    const key = sel ? sel.key : "";
    const relay = !!(sel && sel.relay);
    // Absent reads as on, matching the backend default -- only an explicit
    // false turns a measurement off, so a config written before these
    // switches existed keeps measuring exactly as it did.
    const direct = !sel || sel.direct !== false;
    const chain = !sel || sel.chain !== false;
    const ref = sourceRef(sel, r.kind, r.name);
    /* 本系统自己的输出集合不能启用：`probe` 订阅就是由这些来源的导出拼出来的，
       拿它当输入就是自我循环，服务端 `reject_self_reference` 会拒掉整批保存。
       `bulkSet` 过去只是**静默跳过**它，于是「点列头全选」永远差这一行、列头也
       就永远停在横杠上（实测 47/48），用户读到的就是「点了没变全选」。
       渲染成不可勾选，列头的三态才对得上它真正能控制的那批行。 */
    const selfRef = !!prefix && r.kind === "collection" && r.name === prefix;
    const lock = selfRef ? "disabled" : "";
    const selfTitle = selfRef
      ? `本系统自己的输出集合（publish.prefix=${prefix}），不能作为来源，否则会自我循环`
      : "";
    const meta = r.kind === "collection"
      ? `成员 ${r.members}`
      : `来源 ${esc(r.source_type || "local")}`;
    const html = `<tr class="${on ? "" : "off"}">
        <td><input type="checkbox" data-on="${esc(ref)}" ${on ? "checked" : ""} ${lock} title="${esc(selfTitle)}"></td>
        <td><input type="checkbox" data-export="${esc(ref)}" ${exp ? "checked" : ""} ${(on && !selfRef) ? "" : "disabled"} title="取消勾选后仍会测活，但不产出订阅文件"></td>
        <td><input type="checkbox" data-relay="${esc(ref)}" ${relay ? "checked" : ""} ${(sel && !selfRef) ? "" : "disabled"} title="把这些节点计入「中转节点」分类统计；只影响统计口径，不影响测活与导出"></td>
        <td><input type="checkbox" data-direct="${esc(ref)}" ${direct ? "checked" : ""} ${(sel && !selfRef) ? "" : "disabled"} title="按直连测：剥掉 dialer-proxy，把节点当作它自己的服务器"></td>
        <td><input type="checkbox" data-chain="${esc(ref)}" ${chain ? "checked" : ""} ${(sel && !selfRef) ? "" : "disabled"} title="按链式测：带 dialer-proxy 的节点经前置池测，每个前置一条变体"></td>
        <td class="name" title="${esc(r.name)}">${esc(r.name)}${selfRef
          ? ` <span class="badge" style="color:var(--warn);border-color:var(--warn-line)">本系统输出</span>`
          : ""}</td>
        <td><span class="badge ${r.kind}">${r.kind === "collection" ? "组合" : "单条"}</span></td>
        <td class="tiny">${meta}</td>
        <td>${sel ? `<input class="keyinput" data-key="${esc(ref)}" value="${esc(key)}">` : '<span class="tiny">—</span>'}</td>
        <td class="tiny">${on ? (exp ? `<span class="mono">${esc(absUrl("/api/export/" + key + ".yaml"))}</span>` : '<span style="color:var(--warn)">仅测活，不导出</span>') : ""}</td>
      </tr>`;
    items.push({ on, grp: 0, i: items.length, html });
  });
  missing.forEach(s => {
    // A twin losing the slot to its enabled sibling is a different situation
    // from a hand-added name, and saying "不在列表中" for it would be a lie --
    // it is in the list, it just shares the resource with a longer-lived key.
    const shadowed = RESOURCES.some(r => r.kind === s.kind && r.name === s.name);
    const note = shadowed ? "与同名的启用来源共用资源" : "手动添加或已改名";
    // 同一个「本系统输出」判断，见主循环里的注释。
    const selfRef = !!prefix && s.kind === "collection" && s.name === prefix;
    const lock = selfRef ? "disabled" : "";
    const selfTitle = selfRef
      ? `本系统自己的输出集合（publish.prefix=${prefix}），不能作为来源，否则会自我循环`
      : "";
    const html = `<tr class="${s.enabled ? "" : "off"}">
        <td><input type="checkbox" data-on="${esc(s.key)}" ${s.enabled ? "checked" : ""} ${lock} title="${esc(selfTitle)}"></td>
        <td><input type="checkbox" data-export="${esc(s.key)}" ${s.export === false ? "" : "checked"} ${(s.enabled && !selfRef) ? "" : "disabled"} title="取消勾选后仍会测活，但不产出订阅文件"></td>
        <td><input type="checkbox" data-relay="${esc(s.key)}" ${s.relay ? "checked" : ""} ${selfRef ? "disabled" : ""} title="把这些节点计入「中转节点」分类统计；只影响统计口径，不影响测活与导出"></td>
        <td><input type="checkbox" data-direct="${esc(s.key)}" ${s.direct === false ? "" : "checked"} ${selfRef ? "disabled" : ""} title="按直连测：剥掉 dialer-proxy，把节点当作它自己的服务器"></td>
        <td><input type="checkbox" data-chain="${esc(s.key)}" ${s.chain === false ? "" : "checked"} ${selfRef ? "disabled" : ""} title="按链式测：带 dialer-proxy 的节点经前置池测，每个前置一条变体"></td>
        <td class="name" title="${esc(s.name)}">${esc(s.name)} <span class="badge" style="color:var(--warn);border-color:var(--warn-line)">${selfRef ? "本系统输出" : note}</span></td>
        <td><span class="badge ${s.kind}">${s.kind === "collection" ? "组合" : "单条"}</span></td>
        <td class="tiny">已配置，键 ${esc(s.key)}</td>
        <td><input class="keyinput" data-key="${esc(s.key)}" value="${esc(s.key)}"></td>
        <td class="tiny">${s.enabled ? (s.export === false ? '<span style="color:var(--warn)">仅测活，不导出</span>' : `<span class="mono">${esc(absUrl("/api/export/" + s.key + ".yaml"))}</span>`) : ""}</td>
      </tr>`;
    items.push({ on: !!s.enabled, grp: 1, i: items.length, html });
  });
  if (!items.length) {
    box.innerHTML = `<tr><td colspan="10" class="tiny">没有匹配的订阅。用「＋ 手动添加」直接填名称。</td></tr>`;
  } else {
    items.sort((a, b) => (b.on - a.on) || (a.grp - b.grp) || (a.i - b.i));
    box.innerHTML = items.map(x => x.html).join("");
  }
  const enabled = (CONFIG && CONFIG.sources || []).filter(s => s.enabled);
  const muted = enabled.filter(s => s.export === false);
  document.getElementById("src-summary").innerHTML =
    `Sub-Store 共 ${RESOURCES.length} 个资源；已选 <b>${enabled.length}</b> 个：` +
    (enabled.map(s => `<span class="badge on">${esc(s.key)}</span>`).join(" ") || "无") +
    (muted.length ? `　仅测活不导出：` +
      (muted.map(s => `<span class="badge" style="color:var(--warn)">${esc(s.key)}</span>`).join(" ")) : "");
  bindSourceInputs();
}

function bindSourceInputs() {
  document.querySelectorAll("#src-table input[data-on]").forEach(el => {
    el.onchange = () => toggleSource(el.dataset.on, el.checked);
  });
  document.querySelectorAll("#src-table input[data-export]").forEach(el => {
    el.onchange = () => toggleExport(el.dataset.export, el.checked);
  });
  document.querySelectorAll("#src-table input[data-relay]").forEach(el => {
    el.onchange = () => toggleRelay(el.dataset.relay, el.checked);
  });
  document.querySelectorAll("#src-table input[data-direct]").forEach(el => {
    el.onchange = () => toggleMeasure(el.dataset.direct, "direct", el.checked);
  });
  document.querySelectorAll("#src-table input[data-chain]").forEach(el => {
    el.onchange = () => toggleMeasure(el.dataset.chain, "chain", el.checked);
  });
  document.querySelectorAll("#src-table input[data-key]").forEach(el => {
    el.onchange = () => changeKey(el.dataset.key, el.value.trim());
    el.onblur = el.onchange;
  });
  // 列头的批量开关。绑在这里而不是初始化时：表头是静态 HTML，绑一次就够，
  // 但跟着行一起绑，能让「渲染完立刻同步列头状态」只有一个入口。
  document.querySelectorAll("#src-table thead input[data-all]").forEach(el => {
    el.onchange = () => bulkSet(el.dataset.all, el.checked);
  });
  syncBulkBoxes();
}

/* 列头批量开关：字段名 → 行内复选框的 data 属性名（「启用」列的行内属性叫 on）。 */
const BULK_COLUMNS = {
  enabled: { attr: "on", label: "启用" },
  export: { attr: "export", label: "导出" },
  relay: { attr: "relay", label: "中转" },
  direct: { attr: "direct", label: "直连" },
  chain: { attr: "chain", label: "链式" },
};

/* 本列当前可操作的行。未配置来源的后四列是 disabled（还没有 key 可写），跳过；
   「启用」列对所有列出的资源都可用。返回的 ref 与行内复选框完全一致。 */
function bulkRefs(attr) {
  return Array.from(
    document.querySelectorAll(`#src-table tbody input[data-${attr}]:not(:disabled)`),
    el => el.dataset[attr]);
}

/* 把一列整体设为 on / off。

   三态循环交给浏览器自己走，不要在这里插确认框：列头在「部分选中」时
   `checked=false / indeterminate=true`，点一下就变成 `checked=true` → 全选；
   全选时再点变成 `checked=false` → 全不选。曾经的确认框恰恰毁掉了这个循环 ——
   它渲染在页面顶部的 `#notice` 里，而这张表在页面下方，用户根本看不到它，
   只看到 `syncBulkBoxes()` 立刻把列头回弹成横杠，于是「点了没反应」。
   重操作的安全性改由结果提示里的「撤销」按钮兜底，它同样只发一次 POST。

   逐行调 toggle* 会发 N 次 POST（每次还各自重渲染一遍），批量必须自己合并成一次，
   否则改 40 行就是 40 个请求，中途失败还会留下半截状态。

   「启用」列还要能**凭空建来源**。Sub-Store 列出的资源大多在 config 里还没有条目
   （线上 51 个资源只配过 11 个），行内复选框有 `toggleSource` 的 push 分支兜着，
   批量这边漏了它，点列头就只会动到那几个已配置的 —— 看起来完全像「点了没用」。 */
async function bulkSet(field, on) {
  const col = BULK_COLUMNS[field];
  const refs = bulkRefs(col.attr);
  // 撤销用的快照，必须在乐观更新之前取 —— 调用方写进 `CONFIG.sources` 的是新值，
  // 之后再读回来拿到的就是新值了。
  const prev = (CONFIG.sources || []).map(s => ({ ...s }));
  const sources = (CONFIG.sources || []).map(s => ({ ...s }));
  const prefix = String((CONFIG.publish || {}).prefix || "").trim();
  const skipped = [];
  let changed = 0;
  for (const ref of refs) {
    const hit = findSource(sources, ref);
    if (!hit) {
      // 只有「启用」能新建；其余四列对未配置的来源本来就是 disabled。
      if (field !== "enabled" || !on) continue;
      const [kind, name] = splitRef(ref);
      // 本系统自己的输出集合不能当输入（服务端 reject_self_reference 会拒）。
      // 批量里必须先摘掉：一个成员就能让整批保存失败、所有改动一起回滚。
      if (prefix && kind === "collection" && name === prefix) {
        skipped.push(name);
        continue;
      }
      sources.push({
        kind, name, key: nextKey(slug(name), sources), label: name,
        enabled: true, export: true, relay: false,
        direct: true, chain: true,
      });
      changed++;
      continue;
    }
    if (hit[field] === on) continue;
    hit[field] = on;
    changed++;
  }
  if (!changed) {
    syncBulkBoxes();
    showNotice(`「${col.label}」无需改动`,
      on ? "当前列表里的来源都已经勾选。" : "当前列表里的来源都已经取消。",
      skipped.map(n => `已跳过 ${n}：那是本系统自己的输出集合。`), "ok");
    return;
  }
  CONFIG.sources = sources;
  const r = await saveSources(sources);
  if (r && r.error) return;
  /* 「启用」列全选是唯一的重操作：它把当前列表里的资源真的纳入下一轮测活，每个还会
     产出一个导出文件。确认框被拿掉了（见上），安全性改由这个撤销按钮兜底 —— 它把
     批量前那整份 sources 原样写回，同样只发一次 POST。其余四列改错了逐行点回来也就
     几下，不值得为此让提示永不消失。 */
  const action = field === "enabled"
    ? {
      label: "撤销", primary: false, run: async () => {
        const back = await saveSources(prev);
        if (back && back.error) return;
        showNotice(`已撤销「${col.label}」的批量改动`,
          `恢复到操作前的 ${prev.length} 个来源。`, [], "ok");
      },
    }
    : null;
  showNotice(`「${col.label}」已${on ? "全选" : "全不选"}`,
    `改了 ${changed} 个来源（本列共 ${refs.length} 个）。`,
    skipped.map(n => `已跳过 ${n}：那是本系统自己的输出集合，纳进来会自我循环。`),
    "ok", action);
}

/* 渲染后同步列头：全选 / 全不选 / 部分（indeterminate）。 */
function syncBulkBoxes() {
  Object.entries(BULK_COLUMNS).forEach(([field, col]) => {
    const head = document.querySelector(`#src-table thead input[data-all="${field}"]`);
    if (!head) return;
    const boxes = Array.from(document.querySelectorAll(
      `#src-table tbody input[data-${col.attr}]:not(:disabled)`));
    const on = boxes.filter(b => b.checked).length;
    head.checked = boxes.length > 0 && on === boxes.length;
    head.indeterminate = on > 0 && on < boxes.length;
    head.disabled = boxes.length === 0;
  });
}

/* ref 要么是「已配置来源的 key」（唯一，不含 `|`），要么是「未配置资源的 kind|name」。
   前者拆不出有意义的两半，直接原样返回，交给调用方的 `!hit` 分支判断。 */
function splitRef(ref) {
  const i = ref.indexOf("|");
  return i < 0 ? [ref, ref] : [ref.slice(0, i), ref.slice(i + 1)];
}

/* A source's identity inside the table, and the value every checkbox carries.
 *
 * `key` whenever the source is configured: it is the only field
 * `config.normalize_sources` guarantees unique, and it is what a mutation has
 * to target. `kind|name` only for a resource Sub-Store lists that the config
 * does not have yet -- there is no key to point at, and enabling it is what
 * mints one.
 *
 * Carrying `kind|name` for configured sources too was a real defect, not a
 * stylistic one: 链式聚合 and air are both collections named `air`, so
 * `findSource` below matched whichever came first in the config array -- the
 * disabled twin -- and every checkbox on the 链式聚合 row edited the wrong
 * source. `configuredMap` already resolves that collision for rendering; this
 * resolves it for writing. */
function sourceRef(sel, kind, name) {
  return sel ? sel.key : kind + "|" + name;
}

/* Resolve a checkbox's ref back to the source it belongs to.
 *
 * Key first (unique, so a hit is unambiguous), then the kind|name fallback for
 * a resource that has no config entry yet. A ref that resolves to nothing is a
 * genuine miss, and the caller decides what that means. */
function findSource(sources, ref) {
  const byKey = sources.find(s => s.key === ref);
  if (byKey) return byKey;
  const [kind, name] = splitRef(ref);
  return sources.find(s => s.kind === kind && s.name === name) || null;
}

/* `list` 默认取当前配置；批量操作要传自己那份正在改的副本，否则同一批里新加的几个
   来源会算出同一个 key。 */
function nextKey(base, list) {
  const used = new Set((list || CONFIG.sources || []).map(s => s.key));
  if (!used.has(base)) return base;
  let n = 2;
  while (used.has(base + "-" + n)) n++;
  return base + "-" + n;
}

function slug(text) {
  const kept = String(text).split("").filter(c => KEY_BAD.indexOf(c) < 0).join("");
  let out = kept.split(" ").join("-");
  while (out.startsWith(".")) out = out.slice(1);
  return out.slice(0, 48) || "src";
}

async function toggleSource(ref, on) {
  const sources = (CONFIG.sources || []).map(s => ({ ...s }));
  const hit = findSource(sources, ref);
  if (hit) hit.enabled = on;
  else {
    // A resource with no config entry: this is the one path that still needs
    // kind|name, because the new source's key has to be derived from the name.
    const [kind, name] = splitRef(ref);
    sources.push({
      kind, name, key: nextKey(slug(name)), label: name, enabled: on,
      export: true, relay: false, direct: true, chain: true,
    });
  }
  CONFIG.sources = sources;
  await saveSources(sources);
}

async function toggleExport(ref, on) {
  const sources = (CONFIG.sources || []).map(s => ({ ...s }));
  const hit = findSource(sources, ref);
  if (!hit) return;
  hit.export = on;
  CONFIG.sources = sources;
  await saveSources(sources);
}

/* Marks a source's nodes as 中转节点 for the 分类统计 panel.
   Purely a reporting flag: the round tests these nodes exactly as it did
   before, and the export file is byte-for-byte unchanged. That is why it does
   not go through the same "下一轮生效" path the other toggles warn about --
   the next poll recomputes the panel from the ledger, no round needed. */
async function toggleRelay(ref, on) {
  const sources = (CONFIG.sources || []).map(s => ({ ...s }));
  const hit = findSource(sources, ref);
  if (!hit) return;
  hit.relay = on;
  CONFIG.sources = sources;
  await saveSources(sources);
  load();
}

/* 直连 / 链式：两个互相独立的测量开关，作用范围只有带 dialer-proxy 的节点
   —— 普通节点没有 dialer 可留可剥，两种方式测出来是同一件事，所以只测一次，
   两个都开也不会让整轮翻倍。

   两个都关时后端 `_measure_flags` 回落到直连，前端这里说清楚为什么不是
   「什么都不测」：本轮没测到的节点会被 `_prune_removed_nodes` 从账本删掉、
   `_publish_sources` 把导出截断，所以「两个都关」只能是「换个方式测」，
   要真停测得取消「启用」。 */
async function toggleMeasure(ref, field, on) {
  const sources = (CONFIG.sources || []).map(s => ({ ...s }));
  const hit = findSource(sources, ref);
  if (!hit) return;
  hit[field] = on;
  CONFIG.sources = sources;
  const r = await saveSources(sources);
  // A rejected save already raised its own notice; this one would overwrite it
  // and describe a state the server never accepted.
  if (r && r.error) return;
  const other = field === "direct" ? "chain" : "direct";
  if (hit[field] === false && hit[other] === false) {
    showNotice("两个测量开关都关了",
      `${esc(hit.key)} 本轮仍按「直连」测一遍。`,
      ["两个都关不会让来源停测 —— 本轮没测到的节点会被账本清理、导出被截断。要真正停测请取消「启用」。"],
      "warn");
  }
}

async function changeKey(ref, value) {
  const sources = (CONFIG.sources || []).map(s => ({ ...s }));
  const hit = findSource(sources, ref);
  if (!hit) return;
  if (!value) { loadSources(); return; }
  hit.key = value;
  CONFIG.sources = sources;
  await saveSources(sources);
}

async function saveSources(sources) {
  /* The rollback asks the *server*, not a local snapshot.
   *
   * Every caller writes its optimistic array into `CONFIG.sources` before
   * calling (that is the "optimistic toggle"), so a snapshot taken here *is*
   * the unsaved state -- assigning it back would change nothing and the table
   * would go on showing a save that never happened. The next poll cannot
   * rescue it either: `changed("config", ...)` compares signatures, and a
   * rejected save leaves the server's config byte-identical, so
   * `renderSettings` is never called. `GET /api/status` is the only place that
   * still knows the pre-save state, so read it back from there. */
  let r;
  try {
    r = await api("/api/config", { method: "POST", body: JSON.stringify({ sources }) }).then(r => r.json());
  } catch (exc) {
    // fetch() rejects outright when the server is unreachable, so there is no
    // JSON `error` field to test -- without this the promise just died and the
    // caller's `if (r.error)` was never reached.
    r = { error: `请求失败: ${(exc && exc.message) || exc}` };
  }
  if (r.error) {
    showNotice("保存数据源失败", r.error, [], "error");
    try {
      const st = await api("/api/status").then(res => res.json());
      if (st && st.config) CONFIG = st.config;
    } catch (exc) {
      // Server unreachable: the notice above is the only signal we can give,
      // and leaving the table alone beats pretending the save landed.
    }
    renderSources();                       // paint the rollback right away
    loadSources().catch(() => { });         // best effort; server may be down
    return r;
  }
  CONFIG.sources = r.sources || sources;
  SOURCES_LOADED = true;
  renderSources();
  renderStatusRefresh();
  // The server re-runs `link_substore` on every source change; its outcome was
  // previously thrown away, so a source that failed to link looked fine.
  return r;
}

async function renderStatusRefresh() {
  const st = await api("/api/status").then(r => r.json());
  renderStatus(st); renderSettings(st.config);
}

bind("btn-refresh-src", loadSources);
bindEvent("src-search", "input", renderSources);
bindEvent("src-kind", "change", renderSources);
bindEvent("src-onlyon", "change", renderSources);
/* Filters the node table client-side -- the whole list is already in hand, so
   there is nothing to ask the server for. `NODES_CACHE` is what makes that
   possible: `renderNodes` reads it, so a filter change can repaint without a
   round trip or a `changed()` signature miss. */
bindEvent("node-cat", "change", repaintNodes);
bind("btn-cat-refresh", () => { CAT_ROUND = ""; load(); });
bind("btn-src-save", async () => {
  const r = await saveSources(CONFIG.sources || []);
  if (r && r.error) return;            // saveSources already reported it
  showNotice("数据源已保存", "已保存，下一轮生效。想立刻跑点「立即测试」。",
    r ? r.link : [], "ok");
});
/* #btn-alert-test belongs to the settings panel, which renderSettings()
   builds on the first load -- it is bound there, not here. Binding it at
   top level is what used to abort the whole script. */
bind("btn-link", async () => {
  const r = await api("/api/link", { method: "POST", body: "{}" }).then(r => r.json());
  noticeResult("同步 Sub-Store 联动", r);
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
  if (!name) { err.textContent = "名称不能为空"; return; }
  const key = keyRaw || slug(name);
  if (badKey(keyRaw)) { err.textContent = KEY_HINT; return; }
  const sources = (CONFIG.sources || []).map(s => ({ ...s }))
    .filter(s => !(s.kind === kind && s.name === name));
  sources.push({
    kind, name, key: nextKey(key), label: label || name, enabled: true,
    export: true, relay: false, direct: true, chain: true,
  });
  const r = await saveSources(sources);
  if (r && r.error) return;   // 失败时保留刚填的内容，别把表单清掉
  document.getElementById("m-name").value = "";
  document.getElementById("m-key").value = "";
  document.getElementById("m-label").value = "";
  document.getElementById("src-manual").classList.add("hide");
  err.textContent = "";
});

/* ---------------- 概览与订阅输出 ---------------- */

function renderStatus(st) {
  const s = st.stats || {};
  const pct = (part, whole) => whole ? Math.round(part / whole * 100) + "%" : "—";
  const cards = [
    ["节点总数", s.total || 0, "", "账本按指纹去重"],
    ["存活", s.alive || 0, "ok", "占比 " + pct(s.alive || 0, s.total || 0)],
    ["观察中", s.pending || 0, "warn", "待连续确认"],
    ["已死", s.dead || 0, "bad", "占比 " + pct(s.dead || 0, s.total || 0)],
    ["本轮存活", (st.last_round && st.last_round.ok) || 0, "", "上轮拨号通过数"],
    ["上轮耗时", (st.last_round && st.last_round.duration_s ? st.last_round.duration_s + "s" : "—"), "", "整轮墙钟时间"],
  ];
  document.getElementById("cards").innerHTML = cards.map(([k, v, cls, sub]) =>
    `<div class="card"><div class="k">${k}</div><div class="v ${cls === "ok" ? "s-ok" : cls === "bad" ? "s-dead" : cls === "warn" ? "s-pending" : ""}">${v}</div><div class="sub">${esc(sub)}</div></div>`).join("");
  document.getElementById("p-round").textContent = st.last_round
    ? `第${st.last_round.id}轮 ${st.last_round.finished_at || "进行中"}` : "尚未运行";
  document.getElementById("p-sched").textContent = st.next_run
    ? `下次 ${st.next_run}（每${st.config.schedule.interval_minutes}分钟）` : "定时已关闭";
  // Name the running mode rather than a bare "正在测试": a 直连测活 round treats
  // chained nodes as standalone servers and can therefore drop them from the
  // export, so an operator watching the node table needs to know which kind of
  // round produced it.
  //
  // `busy_mode === null` is a scheduler round (a full chain-aware round), so it
  // is labelled 自动调度. A *non-null* value the map does not know about is NOT
  // a scheduler round -- it is a mode this build has not been taught, and
  // labelling it 自动调度 would be a confident lie about provenance. Fall back
  // to the neutral 测试 instead. (`hasOwnProperty` rather than `MODE_LABEL[x]`
  // so a mode literally named "constructor" cannot leak a function into the
  // label.)
  const MODE_LABEL = { direct: "直连测活", chain: "链式测活" };
  const known = st.busy_mode != null
    && Object.prototype.hasOwnProperty.call(MODE_LABEL, st.busy_mode);
  const label = st.busy_mode == null ? "自动调度" : (known ? MODE_LABEL[st.busy_mode] : "测试");
  document.getElementById("p-busy").textContent = st.busy ? `正在${label}` : "";
  document.getElementById("p-busy").className = "pill" + (st.busy ? " warn" : "");
  const prog = document.getElementById("progress");
  if (prog) prog.classList.toggle("hide", !st.busy);
  // A round is process-wide: while one runs, neither manual launch button may
  // start another (the lock would refuse it anyway, but disabling says so).
  const rb = document.getElementById("btn-run-direct"),
    rc = document.getElementById("btn-run-chain");
  if (rb) rb.disabled = st.busy;
  if (rc) rc.disabled = st.busy;
  const sp = document.getElementById("suspect");
  if (st.last_round && st.last_round.suspect) {
    sp.classList.remove("hide");
    document.getElementById("suspect-text").textContent = st.last_round.note || "本轮结果可疑，已保留上一轮输出";
  } else sp.classList.add("hide");
  const urls = document.getElementById("urls");
  urls.innerHTML = (st.exports || []).map(e =>
    `<div class="urlbox"><span class="mono">${esc(e.key)}</span>
      <input readonly value="${esc(absUrl(e.url))}">
      <span class="pill">${e.count} 节点</span>
      <button data-copy="1">复制</button></div>`).join("") || `<div class="urlbox">还没有输出，先跑一轮</div>`;
}

/* 复制按钮走事件委托：生成的行里不能写 `onclick="…"` —— CSP 已经是
   `script-src 'self'`，内联事件处理器会被浏览器拦掉（点了没反应）。 */
bindEvent("urls", "click", ev => {
  const btn = ev.target.closest("button[data-copy]");
  if (btn) copyUrl(btn);
});

function copyUrl(btn) {
  const input = btn.parentElement.querySelector("input");
  const done = () => { btn.textContent = "已复制"; setTimeout(() => btn.textContent = "复制", 1200); };
  if (navigator.clipboard && navigator.clipboard.writeText) {
    navigator.clipboard.writeText(input.value).then(done).catch(() => { input.select(); done(); });
  } else {
    input.select();
    try { document.execCommand("copy"); done(); } catch (err) { /* 忽略 */ }
  }
}

/* The last payload `/api/nodes` returned. `renderNodes` stashes it so the
   分类 filter can repaint the table without a refetch -- and so the filter
   survives the next poll, which would otherwise replace the rows with the
   unfiltered set. */
let NODES_CACHE = null;

function repaintNodes() {
  if (NODES_CACHE) renderNodes(NODES_CACHE);
}

function renderNodes(nd) {
  NODES_CACHE = nd;
  const tb = document.querySelector("#nodes tbody");
  const want = (document.getElementById("node-cat") || {}).value || "";
  const rows = (nd.nodes || []).filter(n => !want || (n.category || "direct") === want);
  document.getElementById("node-count").textContent =
    want ? `${rows.length}/${nd.nodes.length} 个` : nd.nodes.length + " 个";
  tb.innerHTML = rows.map(n => {
    const [label, cls] = STATUS[n.status] || STATUS.unknown;
    const trend = (n.trend || []).map(v => v === "ok" ? '<i class="s-ok">●</i>' : v === "excluded" ? '<i class="s-unknown">·</i>' : '<i class="s-dead">○</i>').join("");
    const cat = n.category || "direct";
    return `<tr>
      <td class="mono">${esc(n.source)}</td>
      <td class="name" title="${esc(n.name)}">${esc(n.name)}</td>
      <td><span class="badge cat-${esc(cat)}">${esc(CAT_SHORT[cat] || cat)}</span></td>
      <td class="mono">${esc(n.proto)}</td>
      <td class="mono">${esc(n.country || "—")}</td>
      <td class="mono num">${n.ip_total > 1 ? (n.ip_alive || 0) + "/" + n.ip_total : "—"}</td>
      <td class="mono num">${n.last_delay_ms != null ? n.last_delay_ms + "ms" : "—"}</td>
      <td class="mono num">${n.consec_fail || 0}</td>
      <td class="${cls}">${label}</td>
      <td class="trend">${trend || "—"}</td>
      <td class="detail" title="${esc(n.last_reason)}">${esc(n.last_reason || "—")}</td>
    </tr>`;
  }).join("") || `<tr><td colspan="11" class="tiny">${want ? "该分类下暂无节点。" : "还没有节点，先跑一轮。"}</td></tr>`;
}

/* ---------------- 分类统计 ---------------- */
/* Which round the panel is showing; "" means "newest round with results", which
   is what the endpoint defaults to. Kept in a module variable rather than read
   from the DOM so the poll and a manual refresh cannot disagree. */
let CAT_ROUND = "";

const CAT_META = {
  direct: ["直连节点", "var(--ok)", "var(--ok)"],
  relay: ["中转节点", "var(--warn)", "var(--warn)"],
  chain: ["链式代理", "var(--info)", "var(--info)"],
};
const CAT_SHORT = { direct: "直连", relay: "中转", chain: "链式" };

function fmtDelay(d) {
  if (!d || d.median == null) return "—";
  const parts = [`中位 ${d.median}ms`];
  if (d.avg != null) parts.push(`均 ${Math.round(d.avg)}ms`);
  if (d.min != null && d.max != null) parts.push(`${d.min}–${d.max}`);
  return parts.join(" · ");
}

function renderCategories(cs) {
  const cats = cs.categories || {};
  const box = document.getElementById("cat-cards");
  if (!box) return;
  box.innerHTML = Object.keys(CAT_META).map(key => {
    const b = cats[key] || {};
    const n = b.nodes || {}, t = b.tested || {}, d = b.delay_ms || {};
    const [title, color, dot] = CAT_META[key];
    // The two units are both shown because they answer different questions and
    // differ by ~1.45x on this deployment: `nodes` is how many such nodes exist
    // (one ledger row each), `tested` is how much work they cost this round (a
    // chained node becomes one variant per front, a domain node one per
    // address). Showing only one of them is how "348 nodes" and "504 tests"
    // looked like a bug.
    const rate = t.total ? Math.round((t.ok / t.total) * 100) : null;
    const total = n.total || 0;
    const share = v => total ? (v / total * 100).toFixed(3) + "%" : "0%";
    const reasons = (b.reasons || []).map(r =>
      `<span class="tag" title="失败原因">${esc(r.reason)} ${r.count}</span>`).join("");
    const countries = (b.top_countries || []).map(c =>
      `<span class="tag" title="存活出口国家">${esc(c.country)} ${c.count}</span>`).join("");
    return `<div class="catcard">
      <h3><span class="dot" style="background:${dot};color:${dot}"></span>${title}</h3>
      <div class="big" style="color:${color}">${total}<span style="font-size:12px;color:var(--dim);font-weight:400"> 个节点</span></div>
      <div class="sub">本轮测活 ${t.total || 0} 次${rate == null ? "" : "，通过率 " + rate + "%"}</div>
      <div class="catbar" title="存活 / 观察中 / 已死 / 未测">
        <i class="b-alive" style="width:${share(n.alive || 0)}"></i>
        <i class="b-pending" style="width:${share(n.pending || 0)}"></i>
        <i class="b-dead" style="width:${share(n.dead || 0)}"></i>
        <i class="b-unknown" style="width:${share(n.unknown || 0)}"></i>
      </div>
      <div>
        <div class="kv"><span>存活</span><span class="s-ok">${n.alive || 0}</span></div>
        <div class="kv"><span>观察中</span><span class="s-pending">${n.pending || 0}</span></div>
        <div class="kv"><span>已死</span><span class="s-dead">${n.dead || 0}</span></div>
        <div class="kv"><span>未测</span><span class="s-unknown">${n.unknown || 0}</span></div>
        <div class="kv"><span>本轮通过 / 失败</span><span>${t.ok || 0} / ${t.fail || 0}${t.skipped ? " (跳过 " + t.skipped + ")" : ""}</span></div>
        <div class="kv"><span>延迟</span><span>${fmtDelay(d)}</span></div>
      </div>
      ${reasons ? `<div class="tags-title">失败原因</div><div class="tags">${reasons}</div>` : ""}
      ${countries ? `<div class="tags-title">存活出口</div><div class="tags">${countries}</div>` : ""}
    </div>`;
  }).join("");
  const label = document.getElementById("cat-round");
  if (label) label.textContent = cs.round_id == null ? "暂无轮次数据" : "第 " + cs.round_id + " 轮";
  const note = document.getElementById("cat-note");
  if (note) note.textContent = cs.round_id == null
    ? "还没有任何测活结果。"
    : "「个节点」按账本去重，指这类节点有多少个；「本轮测活」按实际拨号次数算，一个链式节点会按前置数、一个域名节点会按解析出的地址数各算一次，所以两者通常不相等。";
}

function renderLogs(lg) {
  document.getElementById("log").innerHTML = (lg.events || []).map(e =>
    `<div class="${e.level}">${esc(e.ts)}  ${esc(e.message)}</div>`).join("");
}

/* ---------------- 设置 ---------------- */

let CONFIG = null;
/* Front-pool picker state. `FRONT_PICK` mirrors `chain.front_pick` but is only
   written back by 保存设置: ticking a box is not a save, so a rejected save
   cannot leave the panel showing a selection the server never took. It is a
   Set so that names not present in the currently loaded resource survive a
   re-render instead of being silently dropped. */
let FRONT_PICK = new Set();
let FRONT_NODES = [];

function renderFrontPickInfo() {
  const el = document.getElementById("front-pick-info");
  if (el) el.textContent = FRONT_PICK.size
    ? `已选 ${FRONT_PICK.size} 个节点（只从该来源取这些）`
    : "未选：使用来源里的全部节点";
}

function renderFrontPicker() {
  const box = document.getElementById("front-picker");
  if (!box) return;
  if (!FRONT_NODES.length) {
    box.innerHTML = '<span style="color:var(--dim);font-size:12px">这个来源没有节点</span>';
    return;
  }
  box.innerHTML = FRONT_NODES.map((n, i) =>
    `<label style="flex-direction:row;align-items:center;gap:8px;color:var(--fg);font-size:12px;padding:2px 0"><input type="checkbox" data-fp="${i}"${FRONT_PICK.has(n.name) ? " checked" : ""}><span>${esc(n.name)}</span><span style="color:var(--dim)">${esc(n.type)} ${esc(n.server)}</span></label>`
  ).join("");
  box.querySelectorAll("input[data-fp]").forEach(el => {
    el.onchange = () => {
      const node = FRONT_NODES[+el.dataset.fp];
      if (!node) return;
      if (el.checked) FRONT_PICK.add(node.name); else FRONT_PICK.delete(node.name);
      renderFrontPickInfo();
    };
  });
}

async function loadFrontNodes() {
  const raw = ((document.getElementById("s-chain-front") || {}).value || "").trim();
  const parts = raw.split("/");
  const hasKind = parts.length > 1;
  const kind = (hasKind ? parts[0] : "sub").trim();
  const name = (hasKind ? parts.slice(1).join("/") : raw).trim();
  if (!name) return { error: "先在上面填前置来源（kind/名称）" };
  const qs = `kind=${encodeURIComponent(kind === "collection" ? "collection" : "sub")}&name=${encodeURIComponent(name)}`;
  return await api("/api/substore-nodes?" + qs).then(r => r.json());
}

function renderSettings(cfg) {
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
    <label>出口验证<select id="s-verify"><option value="1"${cfg.verify.enabled ? " selected" : ""}>开启</option><option value="0"${cfg.verify.enabled ? "" : " selected"}>关闭</option></select></label>
    <label>排除出口国家（逗号分隔）<input id="s-exclude" value="${(cfg.verify.exclude_countries || []).join(",")}"></label>
    <label>链式代理测活<select id="s-chain"><option value="1"${(cfg.chain || {}).enabled ? " selected" : ""}>开启</option><option value="0"${(cfg.chain || {}).enabled ? "" : " selected"}>关闭</option></select></label>
    <label>前置来源（kind/名称，如 sub/cm-xhttp）<input id="s-chain-front" value="${esc((((cfg.chain || {}).front_source) || {}).kind || "sub")}/${esc((((cfg.chain || {}).front_source) || {}).name || "")}" placeholder="sub/cm-xhttp"></label>
    <label>前置池上限<input id="s-chain-max" type="number" value="${((cfg.chain || {}).max_fronts) || 8}"></label>
    <div style="grid-column:1/-1">
      <div class="row">
        <button id="btn-front-pick">从来源里挑前置节点</button>
        <button id="btn-front-pick-clear">清空名单（用全部）</button>
        <span id="front-pick-info" style="color:var(--dim);font-size:12px"></span>
      </div>
      <div id="front-picker" class="hide" style="margin-top:8px;border:1px solid var(--line);border-radius:8px;max-height:240px;overflow:auto;padding:8px 10px"></div>
    </div>
    <label style="grid-column:1/-1">手动前置：每行一条分享链接（vless:// / ss:// / trojan:// / vmess:// …），也可以直接粘贴 base64 订阅内容<textarea id="s-chain-text" rows="4" placeholder="vless://…&#10;ss://…" style="background:var(--panel-2);border:1px solid var(--line-strong);color:var(--fg);border-radius:7px;padding:6px 10px;font-family:var(--mono);font-size:12px">${esc(((cfg.chain || {}).front_text) || "")}</textarea></label>
    <label>定时任务<select id="s-sched"><option value="1"${cfg.schedule.enabled ? " selected" : ""}>开启</option><option value="0"${cfg.schedule.enabled ? "" : " selected"}>关闭</option></select></label>
    <label>告警<select id="s-alert"><option value="1"${cfg.alert.enabled ? " selected" : ""}>开启</option><option value="0"${cfg.alert.enabled ? "" : " selected"}>关闭</option></select></label>
    <label>存活节点下限（低于则告警，0=关闭）<input id="s-floor-alive" type="number" value="${cfg.alert.alive_floor}"></label>
    <label>同类告警冷却（分钟）<input id="s-cooldown" type="number" value="${cfg.alert.cooldown_minutes}"></label>
    <label>Telegram Bot Token<input id="s-tg-token" value="${esc(cfg.alert.telegram.token || "")}" placeholder="123456:ABC-DEF..."></label>
    <label>Telegram Chat ID<input id="s-tg-chat" value="${esc(cfg.alert.telegram.chat_id || "")}"></label>
    <label style="grid-column:1/-1">Webhook URL（POST JSON）<input id="s-webhook" value="${esc(cfg.alert.webhook.url || "")}" placeholder="https://…"></label>
    <div class="row" style="grid-column:1/-1">
      <label class="inline"><input type="checkbox" id="s-tg-on"${cfg.alert.telegram.enabled ? " checked" : ""}> 启用 Telegram</label>
      <label class="inline"><input type="checkbox" id="s-hook-on"${cfg.alert.webhook.enabled ? " checked" : ""}> 启用 Webhook</label>
      <button id="btn-alert-test">发送测试告警</button>
    </div>
    <label>测活后写入 Sub-Store<select id="s-push"><option value="0"${cfg.publish.push_to_substore ? "" : " selected"}>关闭</option><option value="1"${cfg.publish.push_to_substore ? " selected" : ""}>开启</option></select></label>
    <label style="grid-column:1/-1">允许跨域的前端来源（逗号分隔，CDN 部署用；留空 = 只允许同源）<input id="s-cors" value="${esc(((cfg.server || {}).cors_origins || []).join(","))}" placeholder="https://panel.example.com"></label>
    <label style="grid-column:1/-1">测试目标（每行一个）<textarea id="s-targets" rows="4" style="background:var(--panel-2);border:1px solid var(--line-strong);color:var(--fg);border-radius:7px;padding:6px 10px;font-family:var(--mono)">${(cfg.test.targets || []).join("\n")}</textarea></label>`;
  // Built just above, so it can only be bound after this markup exists.
  // Binding it at top level (before any render) is what used to throw
  // "Cannot set properties of null" and blank the entire dashboard.
  bind("btn-alert-test", async () => {
    await saveSettings();
    const r = await api("/api/alert-test", { method: "POST", body: "{}" }).then(r => r.json());
    noticeResult("测试告警", r);
  });
  // Built just above, so these are bound here for the same reason.
  FRONT_PICK = new Set(((cfg.chain || {}).front_pick) || []);
  FRONT_NODES = [];
  renderFrontPickInfo();
  bind("btn-front-pick", async () => {
    const box = document.getElementById("front-picker");
    if (!box) return;
    if (!box.classList.contains("hide")) { box.classList.add("hide"); return; }
    box.classList.remove("hide");
    box.innerHTML = '<span style="color:var(--dim);font-size:12px">读取中…</span>';
    const r = await loadFrontNodes();
    if (r.error) {
      box.innerHTML = `<span style="color:var(--bad);font-size:12px">${esc(r.error)}</span>`;
      return;
    }
    FRONT_NODES = r.nodes || [];
    renderFrontPicker();
  });
  bind("btn-front-pick-clear", () => {
    FRONT_PICK = new Set();
    renderFrontPickInfo();
    renderFrontPicker();
  });
}

/* Two manual launch buttons. `mode` tells the backend how to treat chaining:
   `direct` (直连测活) skips the front pool and ignores the per-source switches,
   so every node is measured as its own server; `chain` (链式测活) honours the
   per-source switches and refuses outright when chaining is unconfigured --
   the server answers 400 rather than starting the direct round it would
   otherwise silently become. Both still test every source, so the ledger and
   the exports stay whole. The scheduler runs its own complete round on its
   interval, independent of these buttons. */
async function runMode(btnId, label, mode) {
  const btn = document.getElementById(btnId);
  if (!btn || btn.disabled) return;
  btn.disabled = true; btn.textContent = label + "中…";
  try {
    const r = await api("/api/run", {
      method: "POST",
      body: JSON.stringify({ trigger: "manual", mode: mode }),
    }).then(r => r.json());
    if (r.error) showNotice("未能启动", r.error, [], "error");
    else showNotice(label + "已启动", "本轮在后台运行，结果随刷新出现", [], "ok");
  } catch (err) { showNotice("未能启动", String(err && err.message || err), [], "error"); }
  finally { btn.disabled = false; btn.textContent = label; load(); }
}

bind("btn-run-direct", () => runMode("btn-run-direct", "直连测活", "direct"));
bind("btn-run-chain", () => runMode("btn-run-chain", "链式测活", "chain"));
bind("btn-push", async () => {
  const btn = document.getElementById("btn-push");
  btn.disabled = true;
  try {
    const r = await api("/api/push", { method: "POST", body: "{}" }).then(r => r.json());
    noticeResult("推送 Sub-Store", r);
  } finally { btn.disabled = false; load(); }
});
bind("btn-settings", () =>
  document.getElementById("settings-panel").classList.toggle("hide"));
bind("btn-clear-log", load);

bind("btn-theme", () => applyTheme(nextTheme()));

async function saveSettings() {
  const val = id => document.getElementById(id).value;
  const patch = {
    schedule: { interval_minutes: +val("s-interval"), enabled: val("s-sched") === "1" },
    test: {
      concurrency: +val("s-concurrency"), timeout_ms: +val("s-timeout"),
      timeout_ms_retry: +val("s-timeout2"), max_attempts: +val("s-attempts"),
      targets: val("s-targets").split("\n").map(s => s.trim()).filter(Boolean),
    },
    policy: { drop_after_consecutive_fails: +val("s-drop"), suspect_floor_ratio: +val("s-floor") },
    verify: {
      enabled: val("s-verify") === "1",
      exclude_countries: val("s-exclude").split(",").map(s => s.trim()).filter(Boolean),
    },
    // 前置来源写成一个 "kind/名称" 输入框：名称可能含中文和空格，用两个下拉/输入框
    // 反而更容易填错。kind 只认 sub / collection，认不出来就退回 sub。
    // 三种录入方式合成一个池子：来源 + 名单（挑了哪些节点）+ 手动粘贴。名单空数组
    // 表示"用全部"，所以这里必须真的发出去 —— 省略它会让 deep merge 保留旧名单，
    // 用户点了「清空名单」却不生效。
    chain: (() => {
      const raw = val("s-chain-front").split("/");
      const kind = (raw.length > 1 ? raw[0] : "").trim();
      const name = (raw.length > 1 ? raw.slice(1).join("/") : raw.join("")).trim();
      return {
        enabled: val("s-chain") === "1", max_fronts: +val("s-chain-max"),
        front_source: { kind: (kind === "collection" ? "collection" : "sub"), name },
        front_pick: [...FRONT_PICK], front_text: val("s-chain-text"),
      };
    })(),
    publish: { push_to_substore: val("s-push") === "1" },
    // 跨域白名单。CDN 上那份静态前端的来源必须在这里，否则浏览器会拦掉响应
    // ——而且是被 CORS 拦在 JS 之前，看起来像「接口挂了」。
    server: {
      cors_origins: val("s-cors").split(",").map(s => s.trim().replace(/\/+$/, "")).filter(Boolean),
    },
    alert: {
      enabled: val("s-alert") === "1", alive_floor: +val("s-floor-alive"),
      cooldown_minutes: +val("s-cooldown"),
      telegram: {
        enabled: document.getElementById("s-tg-on").checked,
        token: val("s-tg-token"), chat_id: val("s-tg-chat"),
      },
      webhook: { enabled: document.getElementById("s-hook-on").checked, url: val("s-webhook") },
    },
  };
  const r = await api("/api/config", { method: "POST", body: JSON.stringify(patch) }).then(r => r.json());
  CONFIG = r.config || CONFIG;
  if (r.error) showNotice("保存设置失败", r.error, [], "error"); else load();
  return r;
}

bind("btn-save", async () => {
  const r = await saveSettings();
  if (!r.error) showNotice("设置已保存", "已应用，下一轮生效。", [], "ok");
});

/* ---------------- 令牌闸门与启动 ---------------- */

function showGate(message) {
  const gate = document.getElementById("gate");
  if (gate) gate.classList.remove("hide");
  const err = document.getElementById("gate-err");
  if (err) err.textContent = message || "";
  const input = document.getElementById("gate-token");
  if (input) input.focus();
}

bind("gate-enter", () => {
  const input = document.getElementById("gate-token");
  const value = (input && input.value || "").trim();
  if (!value) { showGate("令牌不能为空"); return; }
  TOKEN = value;
  try { localStorage.setItem(TOKEN_KEY, value); } catch (err) { /* 忽略 */ }
  start();
});
bind("gate-clear", () => {
  try { localStorage.removeItem(TOKEN_KEY); } catch (err) { /* 忽略 */ }
  const input = document.getElementById("gate-token");
  if (input) input.value = "";
  showGate("已清除本机保存的令牌。");
});
const gateInput = document.getElementById("gate-token");
if (gateInput) {
  gateInput.addEventListener("keydown", ev => {
    if (ev.key === "Enter") { ev.preventDefault(); document.getElementById("gate-enter").click(); }
  });
}

let STARTED = false;
let POLL = null;
let REJECTED = false;

function start() {
  if (STARTED) return;
  STARTED = true;
  REJECTED = false;
  const gate = document.getElementById("gate");
  if (gate) gate.classList.add("hide");
  load();
  POLL = setInterval(load, 5000);
}

/* 令牌不被接受：停掉轮询、丢掉本机存的那份、回到闸门让用户重填。

   只做一次。不做这个 guard 的话，5 秒一次的错误会反复清空用户正在输入的表单。
   轮询也必须停 —— 一个已失效的令牌每 5 秒打一次 401 没有任何意义。 */
function tokenRejected() {
  if (REJECTED) return;
  REJECTED = true;
  STARTED = false;
  if (POLL) { clearInterval(POLL); POLL = null; }
  try { localStorage.removeItem(TOKEN_KEY); } catch (err) { /* 忽略 */ }
  TOKEN = "";
  const input = document.getElementById("gate-token");
  if (input) input.value = "";
  showGate("令牌被拒绝（401）：可能已经换过，或者本机存的这份不对。重新粘贴一次即可。");
}

if (TOKEN) start();
else showGate();
