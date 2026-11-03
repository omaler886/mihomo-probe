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

## 待办（B2/B4/B5/B7 未完成，已记录）

- **B2 Gist Publisher**：两次派发的实现 Agent 均在执行前失败（无产出）。现状：无 `probe-publish` crate、无 Gist 配置节、无 `gist init/test` 子命令、API 无 `POST /api/v1/publishers/gist/test`。合同与插入点已在 `workstreams/18_REFACTOR_PLAN.md` §B2 写明。
- **B4 独立 GUI**：`/api/v1/sources` 等只读契约已就绪，但未提供 SPA 页面与 CSRF/重新认证。
- **B5 Sub-Store 迁移工具**：Python `push_exports` 已降级为 update-only；`migrate/scan/rollback` 子命令与 `--dry-run/--report/--quarantine-hours` 未实现。
- **B7**：README/ARCHITECTURE 的发布章节尚未改写。
