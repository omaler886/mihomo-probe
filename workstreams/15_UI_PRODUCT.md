# UI 与产品面

## 目标
第一阶段复用静态页适配新 API；增强面板随后（总控 §13）。

## 当前实现审计
- 形态：web/ 静态壳（index.html + app.js/app.css/theme.js），__BOOTSTRAP__ JSON 注入（title/token/apiBase）；CSP script-src 'self'（bootstrap 为 type=application/json 免疫）；同源与 CDN 双构建（tools/build_web.py，CDN 构建不携带 token）。
- 能力：节点表（状态/趋势 sparkline/分类徽标）、分类统计（双口径 tested/nodes、reasons、top_countries）、轮次历史、日志、设置表单（源/测试/链式/告警/发布）、手动直连/链式按钮、Sub-Store 资源选择器。
- 设置表单回读 /api/status 的 config（R0 起凭据字段为 ***，保存由后端哨兵丢弃保护）。
- 前端测试：test_logic.DashboardScriptTest + test_hardening.CdnBuildGuardTest。

## Rust 适配原则
- API 契约（10）冻结前不动前端框架；适配点仅 bootstrap/apiBase 与字段映射。

## 待办清单
- [ ] R8：/api/v1 字段映射层（保持现有 UI 语义）
- [ ] R8+：进度/保护原因/发布 diff/调度健康展示

## 下一步
- R8。
