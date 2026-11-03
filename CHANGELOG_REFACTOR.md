# 重构修改历史（Refactor Changelog）

> 依据：《mihomo-probe 精简重构指导：单一测活产物、Gist 托管、独立 GUI》
> 计划与批次划分见 `workstreams/18_REFACTOR_PLAN.md`。
> 每条记录含：编号、时间、执行者、文件、变更、原因、影响、风险、测试、审查、回滚。

## 2026-11-03 B1 — 止血与「一源一产物」数据模型

- **执行者**：主控 Agent + 调研 Agent ×4（存储/发布链路/API-CLI/SubStore-调度）+ 实现 Agent（B2 未产出，见下）
- **文件**
  - 新增 `migrations/0010_source_artifact.sql`：`sources`（`source_id` 随机 v4 UUID 主键、`key` 唯一自然键、display_name/input_type/input_ref/enabled/created_at/updated_at）与 `artifacts`（`source_id` **UNIQUE**、`gist_file` **UNIQUE**、`last_content_sha256`、`last_success_run_id`、`last_published_at`）；回填既有 `export_snapshots.key` 与 `nodes.source`，幂等（`INSERT OR IGNORE`）。
  - 新增 `migrations/0011_source_runs.sql`：`source_runs` 每源运行槽；部分唯一索引 `ux_source_runs_one_open`（每源最多一个 claimed/active）与 `ux_source_runs_idempotency`（一个 Idempotency-Key 一个槽）。
  - 新增 `crates/probe-storage/src/sources.rs` + `sources/tests.rs`：`sync_source`、`source_id_for_key`、`source_by_id`、`list_sources`、`ensure_artifact`、`artifact_for_source/by_id/by_gist_file`、`list_artifacts`、`mark_artifact_published`、`set_artifact_gist/stable_url`；UUID 形状校验。
  - 新增 `crates/probe-storage/src/runs.rs` + `runs/tests.rs`：`claim_source_run`（幂等键 > 活动槽 > 最小间隔 > 新 claim）、`attach_source_run`、`release_source_run`、`close_source_run`、`reap_orphan_source_runs`、`open_source_run`、`list_source_runs`、`record_source_run`。
  - 新增 `crates/probe-engine/src/sources.rs`：`sync_ledger_sources`、`artifact_for_export_key`、`source_for_export_key`、`default_gist_file`；`probe-engine/src/lib.rs` 导出。
  - `crates/probe-storage/src/migrations.rs`：LATEST_VERSION 9→11；注册 0010/0011；`REQUIRED_TABLES` 增加三张新表；修正 `an_unknown_higher_version_is_refused`（改用 LATEST_VERSION+2，避免与 0010 的名字校验相撞）。
  - `crates/probe-config/src/lib.rs`：`PublishSection.allow_legacy_substore_write`（默认 false）+ `legacy_substore_write_for(stored, env)` 纯函数（`ALLOW_LEGACY_SUBSTORE_WRITE` 可开可关）；`ScheduleSection.min_run_interval_seconds`（默认 900，clamp 0..=3600）；`default_tree` 同步。
  - `crates/probe-source/src/fetch.rs`：`SubAdmin::update_sub`（默认拒绝；`SubStoreClient` 实现为「先 GET 存在性，缺失即报错，绝不 POST 新建」）。
  - `crates/probe-engine/src/publish.rs`：`push_round_exports` 双门禁（`push_to_substore` **且** `allow_legacy_substore_write`）；`push_one` 改 `update_sub`（update-only）。
  - `crates/probe-engine/src/publish.rs`：`ScheduleSection` 新字段接入 AppState/ServeConfig；`sync_ledger_sources` 在 AppState 构造期执行；新增 `sources` 模块与 `/api/v1/sources*`、`/api/v1/runs/{id}`、`/artifacts/{source_id}/subscription`（ETag/If-None-Match/304 + `public, max-age=300, stale-if-error=86400`，并在 `security_headers` 之后重贴 Cache-Control）。
  - `crates/probe-cli/src/main.rs`：round 与 serve 两条路径都先做 `sync_ledger_sources`；ServeConfig 传 `min_run_interval_seconds`。
  - 调度 tick：除 `reap_orphan_rounds` 外再调用 `sources::reap_orphan_claims`，把进程崩溃留下的 per-source claimed/active 槽标为 abandoned（预算 20 分钟），否则该源会永久拒绝新触发。
  - Python 侧：`mihomo_test/config.py`（`allow_legacy_substore_write` 默认 false + 环境变量覆盖）、`mihomo_test/store.py`（新增 `update_only`，缺失即报错不新建）、`mihomo_test/engine.py`（`push_exports` 双门禁 + update-only + 未开启时逐 key 报告原因）。
- **原因**：《指导》§1.1/§2.1/§4.3/§7。此前「源」只是 TEXT 列（配置 key），改名即分叉；push 模式虽默认关，但打开后可新建对象；无 per-source 单飞与幂等。
- **影响**：DB 只增表（additive），不重写任何 Python 列；`push_to_substore=true` 的部署现在还需要 `allow_legacy_substore_write` 才生效，且只能更新已有 canonical 对象。
- **风险**：低。两个开关默认关；Gist 未接（B2 见下）。
- **测试**：`cargo test --workspace` **579 passed / 0 failed**（含 probe-storage 95）；`cargo clippy --workspace --all-targets -- -D warnings` **exit 0**；`cargo fmt --all -- --check` 通过；`python -m unittest discover -s tests` **598 passed, OK (skipped=2)**。
- **审查**：新增用例覆盖「100 次发布只 1 个 artifact」「改名不新建 artifact」「gist_file 唯一冲突」「幂等键复用/跨源冲突/释放后可重试」「最小间隔 coalesce」「崩溃槽回收」「双门禁不放行」。
- **回滚**：`git revert` 本提交即可（迁移为 additive，旧二进制读新表会忽略；如需彻底回退 schema，删除 `sources`/`artifacts`/`source_runs` 三表并 `DELETE FROM schema_migrations WHERE version IN (10,11)`）。

## 2026-11-04 B2 核心 — probe-publish crate（Gist 固定文件发布器）

- **执行者**：实现 Agent A1 + 主控集成（commit `67317b0`）
- **文件**：新增 `crates/probe-publish/`（lib/types/gist/local/composite + `tests/publish_stub.rs`）；`crates/probe-config/src/lib.rs`（`publish.provider` local|gist|composite 默认 local + `publish.gist.*` 钳制 + default_tree 全键）；根 `Cargo.toml`/`Cargo.lock`；`.env.example` 增 `GITHUB_TOKEN_FILE=`（假值）。
- **变更**：`Publisher` trait（RPITIT+Send）；`GistPublisher`——token 仅从 token_file 读、构造期快速失败、Unix 0600 校验；sha 相同零请求；per-gist 进程内写锁+锁内重读合并；PATCH 仅触目标文件与 `manifest.json`、永不 `content:null`；401/403/404 终态、429 尊重 Retry-After 有界、5xx/网络退避 1s/2s/4s 可注入、409/422 重读合并一次；错误文本 token 脱敏。`LocalPublisher` 原子写；`CompositePublisher` 本地失败不碰 Gist、Gist 失败随行不失败整体。`POST /gists`（create_gist）留待 A2 接线批。
- **原因**：指导 §3（首建一次、后续 PATCH 固定文件；sha 不变零 revision；同 Gist 写锁）。
- **影响**：provider 默认 local，引擎尚未接线——现有发布路径零行为变化。
- **测试**：`cargo test -p probe-publish -p probe-config` 68/68（stub api.github.com：零 PATCH 幂等、仅触目标文件、manifest 合并保外部条目、429/401/404/500/409 语义、同 Gist 并发 max in-flight==1）；全量门禁 cargo 617 / Python 598 / scan_secrets 干净。
- **审查**：主控抽检——content:null 禁令有注释与用例钉住（含远端截断 null 边界）；Authorization 用常量构造 + 脱敏单测；成员注册确认。
- **回滚**：`git revert 67317b0`（纯新增 crate + config 新键带 serde 默认值）。

## 2026-11-04 B4 — 独立 GUI SPA(/gui) + CSRF 会话与审计

- **执行者**：实现 Agent B4 + 主控集成（commit `b50a306`）
- **文件**：新增 `crates/probe-api/src/gui.rs`（1284 行）+ `gui_tests.rs`（447 行）；`crates/probe-api/src/lib.rs`（`pub mod gui;` + 未守卫组 `.merge(gui::gui_routes())`）。
- **变更**：`GET /gui` 内嵌 SPA 四页（总览/Source 详情/发布设置/清理中心；textContent 渲染零 innerHTML、亮暗主题、≥360px、aria-live）；`GET /api/v1/session`（复用 admin token 校验→CSRF token+HttpOnly SameSite=Strict cookie 双提交；内存会话表 TTL 8h 上限 256）；`POST /api/v1/gui/runs`（admin 401→CSRF 403→复用 spawn_round，全程 `record_event(security)` 审计）；未来契约端点 404 降级「未启用」。
- **集成修正**：gui_tests.spawn 原二次挂载与正式挂载重叠→workspace 全量门禁暴露 axum 重叠路由 panic；改用真实 `build_router`（单包绿≠全量绿的实例，已记录）。
- **偏差**：审计暂走 `record_event(level=security)`（probe-storage 的 `activate_and_audit` 为私有）；CSRF token 为 splitmix64（无 rand 依赖）。均为单点换接/可接受，记录于代码注释。
- **测试**：`cargo test -p probe-api` 54/54（8 新增：安全头/401/CSRF 四种拒绝形态/审计行/DTO 白名单/XSS display_name 转义/未来契约降级）。
- **回滚**：`git revert b50a306`（纯新增路由，旧 / 面板未动）。

## 2026-11-04 B5 核心 — Sub-Store 迁移 scan/apply/rollback

- **执行者**：实现 Agent C + 主控复核（commit `4f27413`）
- **文件**：新增 `crates/probe-source/src/migrate.rs`（含测试）；`probe-source/src/lib.rs` 模块行+再导出。
- **变更**：`MigrationStore` trait **无 delete 方法**（结构级禁删）+ stub 测试断言请求日志零 DELETE；scan 按 §4.2 优先级归并（saved_source_id > 同 URL > 节点集摘要(取回上限 50) > 名称相似度仅报告），keep/duplicate/uncertain 分类；apply 仅 update-only retarget + quarantine 改名 `deprecated-*`（`rename_supported` 默认 false，真实后端保守禁用）；rollback/rollback_plan 幂等还原；报告 URL 查询串脱敏（原文仅内存保留供精确回滚）。
- **测试**：`cargo test -p probe-source` 78/78（23 新增）；clippy/fmt 干净。
- **审查**：主控抽检——delete 仅存在于自我声明注释；lib.rs diff 精确为模块行；独立复跑 78/78。
- **NOT_RUN**：collections 面（crate 内无 collection list/update 路径，plan 内如实记录）。
- **回滚**：`git revert 4f27413`（纯新增模块，无调用方）。

## 2026-11-04 B2 接线 — engine 远程发布腿 + CLI gist + API 端点

- **执行者**：实现 Agent A2 + 主控门禁（commit `29cf9cd`）
- **文件**：新增 `probe-engine/src/publish_remote.rs`、`probe-api/src/publishers.rs`、`probe-cli/tests/gist_commands.rs`、`migrations/0012_source_run_publish.sql`；改 probe-publish（create_gist）、probe-storage（0012 + record_source_publish_status 冻结词表）、probe-cli（round 远程腿 + gist test/init）、probe-api（挂载 + run_dto + gui_tests 钉住更新）。
- **变更**：`publish_round_remote`——provider=local 零行为变化；Published→mark_artifact_published+published；NoChange→no_change 且**不写 artifact**（§3.4(5)：sha 只在真发布后推进）；Failed→publish_failed+error_code；gist_failed 随行不失败整体；构造失败→gist_config_error 轮次不受影响；stable_url 不写（§3.2 禁 revision URL）。`create_gist` 单次 POST、错误强制不可重试。
- **测试**：workspace **cargo 664/0**（+45）；Python 598 不回退；fmt/clippy/scan_secrets/diff --check 干净。
- **风险**：远程腿 spawn_blocking 持存储互斥跨 gist HTTP（沿 legacy push 先例，single-flight 限界）；create 丢失响应可孤儿 gist（注释+运维动作）。
- **回滚**：`git revert 29cf9cd`（0012 additive，旧二进制忽略新列）。

## 待办（B5 接线 / B6 / B7 未完成）

- **B5 接线**：API `POST /api/v1/migrations/substore/{scan,apply,rollback}`（apply 默认 dry-run=true）、CLI `migrate` 子命令、`link_substore.py` 降级。
- **B6**：指导 §11.1/11.2/11.3 + 功能清单 §10 红线逐条测试映射；缺口补测（仅新文件）。
- **B7**：README/ARCHITECTURE 发布章节；`ARCHITECTURE_GIST_ONLY.md`、`MIGRATION_GIST_ONLY.md`；`/api/v1/publishers/gist/init` API 面（CLI 已有）。
