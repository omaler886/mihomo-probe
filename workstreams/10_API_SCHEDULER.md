# API、调度与 UI 适配

## 目标
API 清单冻结 → UI 适配（总控 §13）。

## 当前实现审计（server.py）
- 现有 API：GET /healthz(/api/health)、/、/ui、静态资产、/api/status、/api/stats、/api/nodes、/api/probe/nodes、/api/logs、/api/rounds、/api/substore-resources、/api/substore-nodes、/api/export/<key>.yaml；POST /api/run（mode 白名单 direct|chain）、/api/push、/api/alert-test、/api/link、/api/config、/api/reload。
- 鉴权：三通道（query ?token= / X-Auth-Token / Bearer）；admin 全域，publish token 仅 export 前缀 + /api/probe/nodes；空 token 拒绝；hmac.compare_digest。R0 修复：query token 标记兼容模式（admin 域命中时一次性 warn 日志 + 401 hint 改推头部）。
- 泄密：redacted_config 原掩 auth.token/publish.token；R0 扩展掩 alert.telegram.token、alert.webhook.url、substore.backend（保 scheme+host 掩路径），并让 validate_patch 丢弃哨兵 "***" 防表单回写覆盖。
- 防重复轮：BUSY Lock + 引擎级锁 + 文件锁三重；409。
- CORS：显式白名单精确匹配；OPTIONS 无鉴权（浏览器 preflight 语义）；安全头全套（CSP script-src 'self'）。
- 调度：scheduler_loop 20s 轮询、interval_minutes 夹取、孤儿轮回收（reap_orphan_rounds）。
- 错误响应：统一 JSON，但 500 直接回 `type: message`（内部细节外泄面，记录待改稳定错误码）。

## Rust API 契约（v1，冻结基线）
GET /healthz, /readyz, /metrics, /api/v1/status, /api/v1/nodes, /api/v1/nodes/{fingerprint}, /api/v1/rounds, /api/v1/rounds/{id}, POST /api/v1/rounds, POST /api/v1/rounds/{id}/cancel, GET /api/v1/exports, POST /api/v1/exports/{key}/rollback。
- 管理令牌与只读发布令牌分离；错误响应 {error:{code,message}}；状态接口零秘密；默认 loopback。

## 待办清单
- [x] R0：P0 泄密/兼容标记修复（本文件"泄密/鉴权"节）
- [x] R1：axum /healthz /readyz /api/v1/status POST /api/v1/rounds（切片；鉴权 Bearer/X-Auth-Token，无 query 通道；401/409/202；单测 5 例 + 冒烟）
- [x] R5：`POST /api/v1/rounds` 改为驱动 `probe-engine::run_round`
      （轮次行由 handler 同步开启以便在响应里返回 `round_id`，由 runner 关闭；
      响应体形状不变，仍是 `{"round_id": <id>, "started": true}`）
- [x] R9：`POST /api/v1/rounds/{id}/cancel`（2026-10-20：`AppState` 注册在飞 `RoundCtx`，
      在飞且匹配 200，无在飞 409 `no_round`，不匹配/非法 404；顺带修了开行失败不释放 in-flight 标志）
- [x] R9：**进程内调度器**（Python `scheduler_loop` 对齐）——`probe-engine::scheduler::{decide, reap_orphan_rounds}`
      + `probe-api::spawn_scheduler`（20s tick，serve 启动时拉起）；`GET /api/v1/status` 暴露
      `schedule:{enabled,interval_minutes,next_run}`；孤儿轮按 watchdog 预算回收（note 与 duration_s=0 对齐 Python）
- [x] R8：v1 其余端点 —— `GET /api/v1/nodes`、`/nodes/{fp}`、`/rounds`、`/rounds/{id}` 全部落地
      （keyset 分页、字段白名单、publish token 一律 401）；导出面见 workstreams/09
- [x] G16 `/metrics`（2026-10-20）：Prometheus 文本，全部数字在抓取时从账本现算（没有第二份计数器可漂移）——
      轮次 finished/suspect/inconclusive/with_duration、节点按状态、活跃导出数、调度开关与间隔、上一轮 total/ok/failed/duration；
      标签只有 `status`（≤5 值），key/指纹/主机名/凭据绝不进标签；admin 鉴权 + `no-store`；标签转义有专测
- [ ] 剩余：OpenAPI schema、UI 适配
- [ ] R8+：UI 适配（先复用静态页）

## 测试证据
- Python 锚点：test_hardening.HttpSurfaceTest/AuthLogicTest；Rust：crates/probe-api 测试（切片）。

## 下一步
- v1 契约以本文件为基线补 OpenAPI。
