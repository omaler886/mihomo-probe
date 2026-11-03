# 18 — 精简重构实施计划（Gist 托管 / 一源一产物 / 独立 GUI）

> 来源：《mihomo-probe 精简重构指导：单一测活产物、Gist 托管、独立 GUI》
> （D:\mihomo-probe_精简重构_Gist托管_独立GUI_修改指导.md，下称《指导》）。
> 本计划是该指导在**本地 Rust 仓库**（D:\ChatGPT\代理测活）的落地映射。
> 指导面向 Python 布局（mihomo_test/…），本仓库已完成 Rust 重构
> （crates/ 10 个 crate，测试 435+），因此所有条目按 Rust 现状重映射，
> 不照搬文件路径。

## 0. 调研结论（2026-11-03）

四个只读调研 Agent（存储/发布链路/API-CLI-GUI/SubStore-调度）已回，关键事实：

| 指导要求 | 本地现状 | 差距 |
|---|---|---|
| 禁止每轮 upsert Sub-Store | `push_to_substore` 默认 false（config 与 Python 双侧）；push 代码路径在 probe-engine/src/publish.rs:1018-1156（`push_round_exports`/`push_one`/`prune_local_subs`），调用点 probe-cli/src/main.rs:475、probe-api/src/lib.rs:998 | 需降级为一次性迁移工具 + 显式 legacy 开关 |
| 一源一产物（source_id/artifact_id 唯一） | 无 sources/artifacts 表；每源身份 = export key（= SourceSpec.key）；export_snapshots(key,sha256,id) + active_exports(key) 已有一源一活跃快照 + 内容寻址复用 | 缺永久 source_id 与 artifact 登记表 |
| Gist 托管（PATCH 固定文件） | 全仓库无 Gist publisher（grep 仅命中无关 registry）；内嵌 Sub-Store 的 `/api/sync/artifacts` 是 bundle 自己的备份功能，与发布无关 | 从零建 publisher |
| 内容 sha 不变不发布 | `publish_export` 已按 (key,sha256) 复用快照（find_identical），但整轮仍会走完渲染/物化/审计，且无显式 `no_change` 判定 | 加显式 NO_CHANGE 短路 |
| single-flight | 全局：`in_flight` AtomicBool CAS + 跨进程 RoundLock（probe-api/src/lib.rs:592-662）+ scheduler Busy；rounds 无 per-source 概念 | 加 Idempotency-Key + min_run_interval |
| 独立 GUI /api/v1 | 路由仅 status/rounds/nodes/events/exports/metrics/config；面板是内嵌只读 PANEL_HTML（lib.rs:1174-1515）；无 CSRF；无 ETag/304；security_headers 后置 layer 会 insert 覆盖自设 Cache-Control | 全新建 sources/runs/artifacts 面 + GUI + 稳定订阅端点 |
| SQLite 存历史 | rounds/results/node_state_history/exit_history/quality_samples 齐全；migration 框架 append-only，LATEST_VERSION=9 | 新增 0010 |

## 1. 批次划分（对应《指导》B1–B7 / P0–P2）

### B1 止血 + 数据模型（P0）
- `migrations/0010_source_artifact.sql`：`sources`（source_id 随机稳定 ID PK、key 自然键 UNIQUE、display_name、input_type、input_ref、enabled、created_at、updated_at）+ `artifacts`（artifact_id PK、source_id UNIQUE FK、profile_id 默认 default、gist_id、gist_file UNIQUE、stable_url、last_content_sha256、last_success_run_id、last_published_at）+ 部分唯一索引 `ux_one_active_run_per_source` 所需的 run 登记（见 B3）。
- 回填：`INSERT OR IGNORE INTO sources(...) SELECT DISTINCT key FROM export_snapshots` 与 nodes 的 DISTINCT source（幂等）。
- `crates/probe-storage/src/sources.rs`：upsert_source/list_sources/source_by_id/artifact_for_source/mark_artifact_published 等 + tests。
- serve 启动时按配置 SourceSpec 同步 sources/artifacts 行（key 不变则 source_id 不变；改名只更新 display_name）。
- `link_substore.py` 降级：只保留 `scan`（dry-run 重复对象报告）/`migrate`（canonical URL 切换）/`rollback`（按报告还原）；删除日常 create/remove 写入语义。
- legacy 开关：`ALLOW_LEGACY_SUBSTORE_WRITE=false`（env）+ `publish.push_to_substore` 保持默认 false，且打开时也只能更新预绑定 canonical 对象。
- 测试：`an_unknown_higher_version_is_refused` 改用 LATEST_VERSION+2；新增「连续 100 次发布只 1 个 artifact」「改名不新建 artifact」。

### B2 Gist Publisher（P0/P1）
- 新 crate `crates/probe-publish`（依赖 reqwest/probe-storage/probe-config；位于 engine 之下）：
  - `Publisher` trait：`publish(artifact, content) -> NoChange|Published|Failed` + `healthcheck()`。
  - `GistPublisher`：token 文件（0600 校验）→ `GET /gists/{id}` 读现文件 sha 比对 → `PATCH /gists/{id}` 固定文件；429/5xx 指数退避（有界），401/403 不重试并告警；超时/重试可配。
  - `LocalPublisher`（现有 export root + DB 即本地态）+ `CompositePublisher`（local 成功后 gist，gist 失败保留本地旧产物、run 记 publish_failed）。
- 配置：`publish.provider`（local|gist|composite）、`publish.gist.{gist_id,token_file,public,timeout_seconds,max_retries}`；`.env.example` 增加 `GITHUB_TOKEN_FILE`。
- 引擎钩子：`publish_round` 成功后逐源过 publisher；`content_sha256 == artifacts.last_content_sha256` → NO_CHANGE（不 PATCH）。
- CLI：`probe-cli gist init|test`（init 创建一次 Gist 并落 config；init 幂等）。
- API：`POST /api/v1/publishers/gist/test`。

### B3 Single-flight + API v1（P1）
- `POST /api/v1/sources/{source_id}/runs`：`Idempotency-Key` 头（24h 同 key 同 run_id，内存 Map+TTL）；触发走现有 spawn_round。
- `GET /api/v1/sources`、`GET /api/v1/sources/{source_id}`、`GET /api/v1/sources/{source_id}/runs?limit=20`、`GET /api/v1/runs/{run_id}`（round_detail 别名）。
- run 登记表 `source_runs`（0010 或 0011）：每轮每源一条（round_id, source_id, status, content_sha256, publish_status, error_code）；部分唯一索引保证每源一个活跃 run。
- `min_run_interval_seconds`（默认 900）+ coalesce：手动/API 触发间隔不足返回已有 run。
- `GET /artifacts/{source_id}/subscription`：稳定订阅端点；ETag=`"<sha256>"`、If-None-Match→304、Cache-Control public/max-age=300/stale-if-error=86400（security_headers 中间件放行该路径，不强制 no-store）；凭据 publish token 或 admin token；始终返回上一成功快照。

### B4 独立 GUI（P1）
- 新增 `crates/probe-api/src/gui.rs`：内嵌 SPA（`/gui`），只调 `/api/v1/*` 与 `/artifacts/*`；四页面=总览（每源一卡：名称/source_id 短码/上次测活/五态计数/产物 sha 短码/发布状态/稳定 URL 一键复制/立即测活）、Source 详情（状态分布/近 20 run/失败聚合/节点指纹表/产物 diff/回滚）、发布设置（gist configured 布尔/Gist ID/文件名映射/最近发布/测试连接/初始化 Gist）、清理中心（Sub-Store 重复扫描 dry-run + canonical + 删除/回滚）。
- 安全：写操作（立即测活/回滚/迁移 apply）要求重新输入 admin token + 双提交 CSRF token（`GET /api/v1/session` 取 token，写请求带 `X-CSRF-Token`）+ security_audit 审计行。
- 旧面板 `/` 保持只读，不动。

### B5 Sub-Store 迁移工具（P1）
- `POST /api/v1/migrations/substore/scan`（只读）+ `apply`（24h 隔离=标记 deprecated，二次确认才删除）+ `rollback`（按报告）。
- 与 B1 的 link_substore.py 共用扫描算法（Rust 实现，Python 脚本作为 SSH 侧入口）。

### B6 测试与验收（P2 门禁）
- 《指导》§11.1 十条单测、§11.2 六场景集成、§11.3 十条硬验收逐条落地。
- `cargo test --workspace`、`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings` 全绿；Python 单测不回归（基线 596）。

### B7 文档（贯穿）
- `CHANGELOG_REFACTOR.md` 按批记录（编号/时间/执行Agent/文件/变更/原因/影响/风险/测试/审查/回滚）。
- README「与 Sub-Store 联动」「发布」章节改写；ARCHITECTURE.md 补 artifact 层。

## 2. 依赖与顺序

```
B1（migration 0010 + sources/artifacts + link_substore 降级）
 └─> B2（Gist publisher，依赖 artifacts 表）
      └─> B3（API v1 + 稳定端点，依赖 artifacts + publisher）
           └─> B4（GUI，只依赖 B3 的 API）
B5 依赖 B1 的扫描；B6 全量；B7 贯穿。
```

## 3. 并行分工（Subagent）

| Agent | 批次 | 输入 | 输出 | 验收 |
|---|---|---|---|---|
| 实现A | B1 | 本计划+调研报告 | migration 0010、probe-storage/sources.rs、serve 同步、link_substore.py、测试 | cargo test 新增用例过；db migrate 幂等 |
| 实现B | B2 | B1 产物 | probe-publish crate、engine 钩子、CLI gist 子命令、API test、配置 | 401/429/503 故障注入测试；NO_CHANGE 0 PATCH |
| 实现C | B3+B5 | B1/B2 产物 | API v1 路由、稳定订阅端点、source_runs、Idempotency-Key、迁移 scan/apply | 集成测试（fake GitHub+fake Sub-Store） |
| 实现D | B4 | B3 产物 | gui.rs + CSRF + 审计 | 页面契约测试（路由/头/DTO 白名单） |
| 测试Agent | B6 | 各批产物 | §11 全部门禁测试 + 回归 | cargo/fmt/clippy/Python 全绿 |
| 审查Agent | 全程 | 每批 diff | 对抗审查报告 | 高危=0 才进下一批 |

## 4. 风险与对策

| 风险 | 对策 |
|---|---|
| 改发布链路影响线上轮次 | 全部默认关闭/只增不改：provider 默认 local；push 已默认 false；旧路由不动 |
| migration 回填与 Python 共写同一库 | 0010 纯 additive + INSERT OR IGNORE；Python 侧不读新表，字节兼容 |
| Gist token 泄露 | 只从 0600 文件读；不落 config/SQLite/日志/API；日志脱敏 Authorization |
| security_headers 覆盖 Cache-Control | 稳定订阅端点路径豁免，单独测试钉住 |
| `&'static str` trigger 限制 | spawn_round 改 String 或包一层，保持现有调用点兼容 |
| 大规模改动一次做完难验收 | 批量化，每批独立测试+审查+CHANGELOG 记录后才进下一批 |

## 5. 验收标准（汇总）

1. `cargo test --workspace` 全绿（新增用例覆盖 §11.1/11.2）。
2. `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings` 退出 0。
3. Python `python -m unittest discover -s tests` 不回归。
4. §11.3 十条硬验收逐条有测试或可复现命令证明。
5. CHANGELOG_REFACTOR.md 每批条目齐全（含回滚方法）。
6. Token 不出现在日志/API/HTML/SQLite/备份（scan_secrets 通过）。
