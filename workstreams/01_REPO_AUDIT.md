# 仓库审计

## 目标
完整读取仓库，建立模块/调用链/并发模型/外部服务/配置/API/测试的真实清单，作为 Rust 重构映射基线。

## 当前实现审计（2026-09-30，HEAD=03cb6ea）
### 模块与入口
- 包 `mihomo_test/`（唯一运行入口 `python3 -m mihomo_test`，`__main__.py`）：
  - `config.py`（676 行）：config.json 读写、DEFAULTS、normalize_sources/chain、validate_patch（DEAD_KEYS/IMMUTABLE_PATHS/NUMERIC_BOUNDS）、token 保障（_ensure_tokens，MIN_TOKEN_LEN=16）、core.secret/tunnel.token 文件。
  - `core.py`（680 行）：节点 prepare/fingerprint（16hex sha256）、build_config（kernel YAML，lanes 机制）、`mihomo -t` 校验（docker run metacubex/mihomo）、Core 客户端（version/reload/wait_ready/delay/select/egress/fetch）。
  - `engine.py`（2895 行）：整轮流水线 collect→classify_and_expand（双视角 DNS+每 IP 变体）→expand_chains（前置×链式）→make_testable→_test_phases（前置先行）→_verify_chain_payload（真实拉流）→_verify_egress（车道出口）→_converge_bucket（policy 收敛）→guard→原子发布（tmp+replace）→push/link Sub-Store。
  - `server.py`（613 行）：ThreadingHTTPServer + 调度线程；鉴权（query/X-Auth-Token/Bearer 三通道、admin vs publish 域）；/api/status、/api/nodes、/api/stats、/api/rounds、/api/probe/nodes、/api/export/<key>.yaml、/api/run、/api/push、/api/config、/api/reload、/api/link、/api/alert-test、/api/substore-*。
  - `db.py`（731 行）：sqlite3 单连接 + RLock，WAL/busy_timeout=30s/synchronous=NORMAL；表 nodes/rounds/results/events/ip_geo/domain_views；迁移为代码内 PRAGMA 检查 + ALTER（非 SQL migration 文件）。
  - `store.py`（198 行）：Sub-Store 客户端（404/500-SUBSCRIPTION_NOT_FOUND 归一化、重试、Cloudflare UA 拦截识别）。
  - `policy.py`（91 行）：连续失败收敛（drop_after_consecutive_fails=3）、round_is_suspect 护栏（ratio 0.5 / absolute 3）。
  - `doh.py`（140 行）：手写 DoH+EDNS Client Subnet（A/AAAA、ECS、idna 标签、压缩指针跳读）。
  - `ipmap.py`（525 行）：IP 回显实测（独立容器 mihomo-ipmap/端口 19191/19300+），产出映射 + 写回 Sub-Store。
  - `notifier.py`（186 行）：Telegram/webhook 告警，per-key 冷却（投递成功才记账）。
  - `substore_bridge.py`（93 行）：/api/probe/nodes 九字段白名单投影。
  - `ui.py`（102 行）：web/ 静态壳 + __BOOTSTRAP__ JSON（含 admin token，同源部署）；CSP script-src 'self'。
- 并发模型：轮次串行（线程 Lock + fcntl 文件锁双保险）；测试并发 ThreadPoolExecutor（concurrency=20）；出口验证/拉流校验按 lanes 分桶。
- 外部服务：Sub-Store 后端（拉源/写回/联动）、mihomo Controller（127.0.0.1:19190）、ip-api.com batch（入口国别）、DoH（doh.pub/cloudflare-dns）、Cloudflare trace、ipify/icanhazip（ipmap）、Telegram/webhook、Cloudflare Tunnel + API（setup_tunnel.py）、docker daemon（挂载 socket）。
- SQLite schema 与迁移：见 08。
- 配置键/默认/环境变量：FEATURE_INVENTORY.md §配置 全量清单已核对与代码一致；差异点：`core.mixed_port` 曾不在 DEFAULTS（R0 已修，见 12）。
- API 鉴权：admin token 全域；publish token 仅 /api/export/* 与 /api/probe/nodes；空 token 拒绝（不再"无 token=放行"）；hmac.compare_digest UTF-8。
- 状态机与失败分类：policy 五态 unknown/alive/pending/dead/excluded；失败类别 timeout/kernel_error/bad_request/bad_response/bad_delay/unreachable/controller_error/http_<status>/verify_failed/entry_cn/front_dead/payload_fail/kernel_rejected/exit_<CC>。
- 导出与 Sub-Store 接入：双路径（拉模式 remote sub + 推模式 -local sub）+ probe_filter.script.js 账本消费；导出原子写 + meta.json；零节点不联动。
- Docker 权限/网络：三容器全 host 网络；应用容器挂 /var/run/docker.sock（S-11，P2）；mihomo 容器 cap NET_ADMIN/NET_RAW、no-new-privileges、pids/mem 限制；应用容器 root 运行、无 no-new-privileges（记录为待加固）。
- 测试分类与数量：tests/ 7 文件 554 用例（test_logic 2910 行单元、test_hardening 鉴权/HTTP 面、test_live 内核集成（MIHOMO_TEST_LIVE 门控）、test_substore_bridge 契约、test_alerts_lanes、test_ipmap、_isolation 临时根夹具）。
- TODO/FIXME/异常吞噬：grep 无 TODO/FIXME；异常吞噬集中在"告警/清理/状态文件"等 best-effort 路径且有日志（engine.py 显式 noqa 注释）；`_write_state` 失败会 db.log(warn)。
- Python→Rust 映射：见 MIGRATION_MAPPING.md（F-01~F-28 已核对有效；F-06 混合端口陷阱 R0 已解除）。

## 输入与依赖
无外部依赖；本文件为 02~16 的输入。

## 待办清单
- [x] 完整读取 mihomo_test/*.py、tests/*.py（结构）、tools/*.py（清单）、部署脚本
- [x] 与四份迁移文档（FEATURE_INVENTORY/MIGRATION_MAPPING/ARCHITECTURE/REVIEW）交叉核对
- [ ] tools/*.py 逐文件审计（当前仅清点：22 个运维脚本，均不在请求路径，基线前已占位符化）

## 修改记录
| 时间 | 文件 | 变更 | 原因 |
|---|---|---|---|
| 2026-09-30 | 本文件 | 首版审计 | R0 |

## 执行命令与输出摘要
- `git rev-parse HEAD` → 03cb6eaf344a822093ec7bab4181336909fd4a79
- `python -m py_compile mihomo_test/*.py` → 无输出（干净）
- `python -m unittest discover -s tests -v` → Ran 554 / OK (skipped=2) / 38.797s
- `wc -l mihomo_test/*.py tests/*.py` → 主包 7345 行 + 测试 8969 行

## 测试证据
见 13_TEST_COMPATIBILITY.md 基线节。

## 风险与回滚
- 本文件为纯文档，无回滚需求。

## 阻塞项
- 本机无 Docker：容器内测试与 `mihomo -t` 实测须在 vps 复核。

## 下一步
- R1 起为每个 Rust crate 建立与 Python 行为的逐项对照（先 fingerprint/reason 分类）。
