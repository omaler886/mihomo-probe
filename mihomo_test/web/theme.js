/* 主题引导：必须在 <head> 里**同步**执行，否则深色模式会先闪一下浅色。
 * 单独一个文件而不是内联 <script>，是因为后端的 CSP 已经收紧到
 * `script-src 'self'`（没有 'unsafe-inline'）—— 内联脚本会被浏览器直接拦掉。 */
(function () {
  var saved = null;
  try { saved = localStorage.getItem("mihomo-theme"); } catch (e) { /* 隐私模式 */ }
  var mode = saved || "auto";
  var dark = mode === "dark" ||
    (mode === "auto" && window.matchMedia &&
     window.matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.setAttribute("data-theme", dark ? "dark" : "light");
})();
