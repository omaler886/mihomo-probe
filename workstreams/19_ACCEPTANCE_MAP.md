# 19 — 验收映射与缺口测试（B6）

> 《指导》§11.1/§11.2/§11.3 与《最终功能清单》§8（F-090..F-094）/§10（验收红线）
> 在本仓库的逐条落地映射。基线：commit `13c2889`（B1/B2核心/B4/B5核心/B2接线 已入库），
> 外加 B6 本批新增测试（见 §6）。**每一条都按测试名在树里 grep 核实过，不采信批次报告。**
>
> 状态词：`covered` = 有真实测试钉住；`covered（stub 级）` = 有测试，但目标远端是
> 进程内 stub/假发布器，非真实 GitHub/Sub-Store；`covered（engine 级）` = 引擎层
> 假内核/假 ledger，非 E2E；`partial` = 部分被钉住，缺口在说明列；`NOT_RUN` =
> 本环境没有可执行证据，禁止伪称通过。
>
> 命令基线：`cargo test --locked --workspace`、`cargo fmt --all -- --check`、
> `cargo clippy --workspace --all-targets -- -D warnings`、
> `python -m unittest discover -s tests`。B6 收尾时的真实数字见 TEST_REPORT.md。

## 1. §11.1 十条单元门禁

| # | 门禁 | 测试/证据 | 状态 | 缺口说明 |
|---|---|---|---|---|
| 1 | 同一 source 连续运行 100 次，只存在一个 artifact | `crates/probe-storage/src/sources/tests.rs::one_source_never_grows_a_second_artifact`（100×ensure+mark 后 `list_artifacts()==1`，UNIQUE(source_id) 兜底）；B6 补充运行/发布交织：`crates/probe-storage/tests/artifact_invariants.rs::runs_and_publishes_interleaved_stay_one_artifact_one_row_per_round` | covered | — |
| 2 | 内容不变时 Gist PATCH 次数为 0 | `crates/probe-publish/tests/publish_stub.rs::no_change_makes_zero_requests`（本地 sha 相同→零请求）、`identical_remote_content_with_a_current_manifest_is_no_change`（远端 sha 相同→零请求）；引擎侧 `crates/probe-engine/src/publish_remote.rs::no_change_records_the_status_and_no_artifact_write`；B6 场景级：`crates/probe-engine/tests/publish_scenarios.rs::rerunning_the_same_input_writes_the_destination_once`（10 轮重跑 1 次写入） | covered（stub 级） | stub 是 axum 假 api.github.com，非真实 Gist；无 revision 断言依赖"零 PATCH"传递成立 |
| 3 | display_name 改名不创建新 artifact | `crates/probe-storage/src/sources/tests.rs::a_rename_updates_attributes_and_keeps_the_identity`；`one_source_never_grows_a_second_artifact` 复核 | covered | — |
| 4 | 相同节点不同显示名只测一次 | 身份不含名字：`crates/probe-source/src/identity.rs::the_name_is_excluded_from_the_identity`；账本按 (source,fingerprint) 折叠成一行（`crates/probe-engine/src/round.rs` NodeFold，注释 round.rs:1109）；发布侧 `crates/probe-engine/src/publish/tests.rs::select_source_exports_collapses_variants_of_one_display_name` | partial | **没有任何测试钉住"只拨号一次"**：同名不同显示名的两个 job 仍会各拨一次（2 dials → 1 ledger row）。§7.2 的字面语义（测一次）在 Rust 重写中未实现，只实现了账本/发布折叠。缺口记录于 §8，属行为偏差而非测试缺口 |
| 5 | 401/403 不重试，429/5xx 有界重试 | `unauthorized_is_never_retried_and_never_leaks_the_token`、`a_missing_gist_is_terminal`、`rate_limit_honors_retry_after_within_the_bound`、`server_errors_are_attempted_exactly_max_retries_times`、`the_backoff_curve_is_exponential_with_jitter`、`error_statuses_map_to_the_contract`（probe-publish/tests/publish_stub.rs + src/gist.rs） | covered（stub 级） | — |
| 6 | 发布失败保留上一成功 SHA256 和正文 | 账本：`a_failed_publish_leaves_the_previous_digest`、`the_digest_advances_only_on_a_successful_publish`；文件层：`crates/probe-engine/src/publish/tests.rs::failed_file_install_keeps_previous_export_and_never_calls_activation`、`failed_activation_keeps_previous_file_and_authoritative_snapshot`；远程腿：`crates/probe-engine/src/publish_remote.rs::a_failed_publish_persists_the_error_code`；B6 场景级：`publish_scenarios.rs::an_unavailable_destination_records_the_failure_and_keeps_the_local_snapshot` | covered | — |
| 7 | 两个并发 run 请求只启动一个任务 | DB 互斥：`crates/probe-storage/src/runs/tests.rs::one_source_holds_exactly_one_open_run`（含部分唯一索引直插拒绝）；HTTP 双击：`crates/probe-api/src/lib.rs::a_round_starts_once_and_closes_with_an_honest_note`；跨进程：`a_round_held_by_another_process_is_refused`；Gist 写锁：`concurrent_publishes_to_one_gist_serialize`（max in-flight==1）；B6 交织：`artifact_invariants.rs` 全链路 claim→close | covered | 并发 run 的 HTTP 级测试走全局 round 锁（`POST /api/v1/rounds`），per-source 的 `POST /api/v1/sources/{id}/runs` 并发重放没有独立 HTTP 测试，靠 DB 测试传递 |
| 8 | Idempotency-Key 重放返回同一 run_id | `crates/probe-storage/src/runs/tests.rs::one_idempotency_key_resolves_to_one_run`（在飞/关闭后同答）、`a_key_cannot_span_two_sources`、`releasing_a_claim_frees_the_slot_and_the_key`；冷却合并 `a_fast_second_trigger_is_coalesced_into_the_last_run` | partial | API 层（`POST /api/v1/sources/{id}/runs` 解析 `Idempotency-Key` 头，`crates/probe-api/src/sources.rs:266`）没有 HTTP 级重放测试；端点把 key 原样交给 claim，claim 语义已钉住 |
| 9 | 整轮护栏命中时不发布 | `crates/probe-engine/src/publish/tests.rs::decisions_distinguish_disabled_empty_guarded_and_diff_limits`（`GuardNotApplied` + current_export 不动 + 文件树不动）、`:1163` 区域（inconclusive 未测量不发布）；护栏判定：`crates/probe-engine/src/round.rs::a_round_below_the_alive_floor_is_marked_suspect`、`a_fetch_failed_source_does_not_mask_the_suspect_guard`、`a_mid_round_controller_failure_preserves_node_state`；B6 场景级：`publish_scenarios.rs::a_guarded_round_withholds_publication_and_keeps_the_previous_snapshot`（两种 guard 形态 + 目的地零调用） | covered（engine 级） | — |
| 10 | 回滚只改变当前 artifact，不创建新 Sub-Store 对象 | 迁移回滚：`crates/probe-source/src/migrate.rs::rollback_restores_the_original_url_exactly_and_twice`、`rollback_renames_quarantined_duplicates_back_and_is_idempotent`、`rollback_from_a_plan_is_a_safe_best_effort`，全程 `assert_no_delete`（结构上无 delete 方法）；发布回滚：`crates/probe-engine/src/publish/tests.rs::publication_and_rollback_preserve_immutable_versions`、`crates/probe-api/src/exports/tests.rs::rollback_is_cas_guarded_and_publish_token_cannot_call_it` | covered | — |

## 2. §11.2 六个集成场景

| 场景 | 期望 | 测试/证据 | 状态 | 缺口说明 |
|---|---|---|---|---|
| 1 输入 100 节点首次发布 | 1 source / 1 artifact / 1 Gist 文件 / Sub-Store 1 个稳定订阅 | 分解钉住：`select_source_exports_publishes_only_alive_original_dictionaries`（选择）、`one_source_never_grows_a_second_artifact`（1 artifact）、`a_changed_content_patches_only_the_target_and_the_manifest`（1 文件）、`create_gist_posts_once_secret_by_default`（一次建 Gist）；B6：`publish_scenarios.rs` 全链路首轮 | partial | 100 节点规模没有专门测试（测试用 1-2 节点）；"Sub-Store 1 个稳定订阅"一腿属消费侧配置，无代码可测 |
| 2 相同输入重跑 10 次 | 10 条 run 历史、0 次额外 Gist PATCH、0 个新增 Sub-Store 对象 | **B6 新增**：`crates/probe-engine/tests/publish_scenarios.rs::rerunning_the_same_input_writes_the_destination_once`（10 轮：10 run 行、1 artifact、1 次写入、9×no_change）；Sub-Store 零新增对象：`tests/test_substore_write_zero.py` 全轮零写 | covered（stub 级） | 目的地是 sha 比对假发布器，非真 Gist HTTP |
| 3 1 个节点死亡 | 前两轮 pending 不发布，第三轮 dead 后更新同一 Gist 文件 | 分解钉住：跨轮收敛状态机 `crates/probe-engine/src/round.rs`（`convergence_and_baseline_errors_still_close_the_round`、`excluded_chains_clear_streaks_without_a_dial`）；同文件更新=`a_changed_content_patches_only_the_target_and_the_manifest` | partial | 没有"pending→dead 三轮"的跨轮集成测试把状态机与发布腿串起来；两段各自 covered |
| 4 整轮网络故障 | 护栏拦截，稳定 URL 继续返回上一成功版本 | **B6 新增**：`publish_scenarios.rs::a_guarded_round_withholds_publication_and_keeps_the_previous_snapshot`（ledger 腿）；**B6 新增**：`crates/probe-api/tests/subscription_endpoint.rs::the_stable_url_tracks_the_last_good_snapshot_only` + `a_current_etag_answers_304_with_no_body`（HTTP 腿，真 router+真 ledger，无内核） | covered（HTTP 级，无真内核） | 缓存头一腿发现缺陷 B6-1（`#[ignore]` 钉住，见 §7） |
| 5 服务重启时 cron 与 GUI 同时触发 | 只存在一个 active run | `one_source_holds_exactly_one_open_run`（DB 互斥）、`a_stale_slot_is_reaped_a_fresh_one_is_not`（重启回收）、`one_idempotency_key_resolves_to_one_run`；调度器 Busy 路径 `crates/probe-engine/src/scheduler.rs`（11 tests）；B6：`artifact_invariants.rs::an_interrupted_run_does_not_fork_the_artifact`（崩溃槽不分裂 artifact，先护后收） | partial | 无真实双进程/双端口并发触发测试；单飞语义由 DB 约束 + 单进程测试传递 |
| 6 GitHub API 不可用 | run 标记 publish_failed，本地旧产物与消费不受影响 | 单元：`a_failed_publish_persists_the_error_code`、`server_errors_are_attempted_exactly_max_retries_times`；**B6 新增**：`publish_scenarios.rs::an_unavailable_destination_records_the_failure_and_keeps_the_local_snapshot`（publish_failed + error_code + 本地快照前进 + digest 停在远端已收内容 + 下一轮重试同内容） | covered（stub 级） | 假发布器代替真 GitHub；5xx 语义由 probe-publish stub 测试传递 |

## 3. §11.3 十条硬验收

| # | 条目 | 测试/证据 | 状态 | 缺口说明 |
|---|---|---|---|---|
| 1 | 一个 source_id 在 DB 中最多一个 artifact | `one_source_never_grows_a_second_artifact`；schema UNIQUE(source_id)（migrations/0010）；B6 交织测试 | covered | — |
| 2 | 一个 source 在 Gist 中最多一个固定 YAML 文件 | `a_changed_content_patches_only_the_target_and_the_manifest`（PATCH 仅触目标文件+manifest.json，永无 content:null）、`no_change_makes_zero_requests`；gist_file 唯一：`two_sources_cannot_share_a_gist_file`；每源建 Gist 一次：`create_gist_posts_once_secret_by_default` | covered（stub 级） | — |
| 3 | 连续 24 小时运行后，Sub-Store 对象数量不增加 | 结构性：引擎 push 双门禁（`crates/probe-config` 两开关默认 false）+ update-only（`crates/probe-source/src/fetch.rs::SubAdmin::update_sub`「缺失即报错，绝不 POST 新建」）；**B6 新增**：`tests/test_substore_write_zero.py` 全轮（默认与 legacy 标志两种形态）记录所有 store 方法调用并断言零写 | partial | 无 24 小时长跑（soak）测试；证据是结构禁令 + 全轮零写测试，不是时间窗观测 |
| 4 | 内容相同不会产生 Gist 新 revision | `no_change_makes_zero_requests` + `identical_remote_content_with_a_current_manifest_is_no_change`（零请求⇒零 revision） | covered（stub 级） | — |
| 5 | 同一 source 不会并发测活 | `one_source_holds_exactly_one_open_run`（部分唯一索引 `ux_source_runs_one_open` 直插被拒）、`a_fast_second_trigger_is_coalesced_into_the_last_run`、B6 崩溃槽测试 | covered | — |
| 6 | GUI 只显示一个稳定订阅 URL | `crates/probe-api/src/gui.rs:778-875`：每源卡片渲染单一稳定 URL（artifact.stable_url 优先，否则 `/artifacts/{id}/subscription`），复制按钮 `navigator.clipboard`/prompt 降级；DTO 白名单/转义由 `gui_tests.rs` 钉住 | partial | 没有专门断言"只渲染一个 URL"的测试；SPA 是静态内嵌结构，被 gui_tests 的结构断言间接覆盖 |
| 7 | Gist/网络故障不会让订阅变空 | 发布失败保留旧文件：`failed_file_install_keeps_previous_export...`、`failed_activation_keeps_previous_file...`；稳定端点只服务 active 快照：B6 `subscription_endpoint.rs::a_source_without_a_snapshot_is_404_not_empty` + `the_stable_url_tracks_the_last_good_snapshot_only`；空体拒绝：`empty_oversized_or_excessive_output_is_rejected` | covered（endpoint 级） | 见缺陷 B6-1：端点的缓存头被 security_headers 覆盖（不影响"不变空"，影响缓存策略） |
| 8 | Token 不出现在日志、API、HTML、SQLite 和备份 | 发布器：`redaction_removes_the_token_everywhere`、`unauthorized_is_never_retried_and_never_leaks_the_token`（含 assert_no_token_leak 全响应扫描）；API：`status_carries_no_secret`、`kernel_status_never_echoes_url_credentials_or_paths`、`the_panel_is_read_only_and_never_injects_markup`、exports read-scope 测试组；工具：`python -I tools/scan_secrets.py` → "secret scan: clean (tracked files)"（2026-11-04 实跑） | partial | SQLite/备份侧靠 schema 不存 token 的结构事实（artifacts 表只有 gist_id/gist_file/stable_url），无专门断言测试；scan_secrets 只扫 tracked 文本 |
| 9 | dry-run、迁移、删除、回滚均有审计记录 | `crates/probe-source/src/migrate.rs`：`a_dry_run_performs_zero_writes`、报告与日志逐条断言（`assert_no_delete`、`write_count`）；apply/rollback 报告 URL 脱敏：`redacted_url_is_redacted_in_debug_and_serde`；API 层（bdee87c 接线）：`POST /api/v1/migrations/substore/*` 全部 401 守卫（`gui_tests.rs::migrations_and_gist_test_are_admin_gated`），每次真实 apply 经 `record_migration_audit` 写 `record_event(level=security)` 审计行（`crates/probe-api/src/migrations.rs:128-133`） | partial | CLI 侧 `migrate apply` 只落报告文件与日志，不写账本审计行（报告文件即证据链）；「删除」动作在结构上不存在（无 delete 方法），审计对象为 retarget/隔离/回滚 |
| 10 | 所有新增测试通过后才允许删除旧写路径 | 流程性条目。旧写路径（`push_round_exports`/`link_substore` 写语义）仍在但已双门禁默认关；测试全绿是删除前置条件 | partial | 旧写路径尚未删除（符合"通过后才允许删"）；删除动作本身是未来批次 |

## 4. 功能清单 §10 最终验收红线（10 条）

| 红线 | 测试/证据 | 状态 | 缺口说明 |
|---|---|---|---|
| Sub-Store 写请求 = 0 | **B6 新增**：`tests/test_substore_write_zero.py`（3 tests：默认轮、legacy 标志关、禁用源不读不写；store 的任何未知方法=记录+拒绝，变异探针证明双门禁打开时会抓到 `update_only`）；双门禁单元：`test_the_legacy_flag_alone_does_not_open_the_write_path` 等（test_logic.PushExportsTest）；update-only：`crates/probe-source/src/fetch.rs` 13 tests；`/api/link`（面板联动写入口）同样双门禁（618e659，`mihomo_test/server.py:667`）；`link_substore.py` 已降级为零写入命令打印器（618e659） | covered（测试级） | 非 E2E（store 是进程内记录器；真实 Sub-Store 属生产操作，见 §8「需要用户在场」） |
| 每个 Source 的 active Publication = 1 | `one_source_never_grows_a_second_artifact`、B6 交织、CAS：`publish_export` 的 expected_active 语义（publication.rs 测试组） | covered | — |
| 同源并发 ProbeRun ≤ 1 | runs/tests.rs 全组（部分唯一索引=互斥） | covered | — |
| 连续 100 轮产生的新订阅实体 = 0 | `one_source_never_grows_a_second_artifact`（100×发布→1 artifact）、B6 `rerunning_the_same_input_writes_the_destination_once`（10 轮→1 文件）、`no_change_makes_zero_requests` | partial | 100 这个数字只用在 DB 层测试；端到端 100 轮循环（含远程腿）未跑 |
| Gist 文件按轮次增长 = 0 | `a_changed_content_patches_only_the_target_and_the_manifest`、`no_change_makes_zero_requests`、`create_gist_posts_once_secret_by_default`（POST 一次/永不逐轮） | covered（stub 级） | — |
| 整轮基础设施异常导致的错误死亡推进 = 0 | `a_round_below_the_alive_floor_is_marked_suspect`、`a_fetch_failed_source_does_not_mask_the_suspect_guard`、`a_mid_round_controller_failure_preserves_node_state`、`an_unreachable_kernel_skips_the_node_phase_and_still_closes`、`a_refused_reload_skips_nodes_and_preserves_the_ledger`、`variant_counts_cannot_inflate_the_next_rounds_alive_baseline`（round.rs） | covered（engine 级） | — |
| 未经校验直接覆盖稳定产物 = 0 | CAS：`publication_and_rollback_preserve_immutable_versions`、`rollback_is_cas_guarded...`、`path_containment_and_existing_file_integrity_are_enforced`、`export_root_gate_rejects_traversal...`；Gist 侧：409 重读合并 `a_conflict_regets_and_merges_once`、manifest 损坏拒写 `a_corrupt_manifest_is_refused_not_overwritten` | covered | — |
| 明文凭据泄漏 = 0 | probe-publish 脱敏组（redaction/token-leak）、API `status_carries_no_secret` 等、`test_hardening.py` 脱敏组、`test_substore_bridge.py::test_the_body_carries_no_credentials`、`python -I tools/scan_secrets.py` → clean | covered（测试级） | 同 §3-8 的 SQLite/备份 partial |
| Agent 开发并发 ≤ 3 | 流程性条目，不适用代码测试 | NOT_RUN（N/A） | 流程约束，见批次记录 |
| 无人值守持续运行 ≥ 8 小时 | 无 soak 测试 | NOT_RUN | 需要长跑环境与真实部署，本环境无法出证据 |

## 5. 功能清单 §8：F-090..F-094

| 条目 | 测试/证据 | 状态 | 缺口说明 |
|---|---|---|---|
| F-090 100 轮稳定性 | §4 同名行（100 次 DB 发布 / 10 轮场景 / 零 PATCH） | partial | 同 §4 |
| F-091 并发测试 | Gist 并发序列化 `concurrent_publishes_to_one_gist_serialize`；单飞组；HTTP 双击 `a_round_starts_once...`；跨进程锁 `a_round_held_by_another_process_is_refused`；B6 崩溃槽 | partial | "PATCH 前后崩溃、数据库提交失败重放、陈旧锁回收"只覆盖到陈旧槽回收（`a_stale_slot_is_reaped...` + B6），崩溃重放无测试 |
| F-092 故障注入 | GitHub 401/403/404/429/5xx/409/422：publish_stub.rs 全组 + `error_statuses_map_to_the_contract`；mihomo 失联：`a_mid_round_controller_failure_preserves_node_state`、`an_unreachable_kernel_skips...`；DNS：`uncertain_dns_preserves_the_node_and_isolates_its_source`、resolver/wire 测试组 | partial | SQLite locked、磁盘满、临时目录只读、Sub-Store 返回非法 YAML（Rust 侧有 `invalid_yaml_is_an_error_not_a_panic`，轮内注入无）、Gist 被人工编辑（manifest drift 有：`a_live_content_with_a_drifted_manifest_repairs_the_manifest_only`）、进程中途终止（B6 崩溃槽测试覆盖账本侧）——其余 NOT_RUN |
| F-093 安全测试 | token 泄漏/脱敏组（§3-8 行）；CSRF：gui_tests 四种拒绝形态；XSS：DTO 白名单 + display_name 转义；路径遍历：`export_root_gate_rejects_traversal_absolute_names_and_shared_directories`、`symlinks_junction_equivalents_and_shared_files_are_rejected`、`hostile_file_names_are_refused`；SSRF/命令注入：无专门测试 | partial | SSRF、命令注入、恶意 Gist 文件名（`validate_gist_id`+`hostile_file_names_are_refused` 有部分）未成体系 |
| F-094 部署验证 | 2026-11-04 实跑：`python3 -m unittest discover -s tests` → 605 tests OK (skipped=2)；`python3 -m compileall -q mihomo_test tests` → exit 0；`node --check substore_bridge/probe_filter.script.js` → exit 0；`cargo test --locked --workspace` / fmt / clippy → 见 TEST_REPORT.md（B6 收尾数字） | partial | `docker compose config` → **NOT_RUN**（本机无 docker 命令，exit 127）；按 F-094 规则如实标记 |

## 6. B6 本批新增测试（缺口补齐）

| 文件 | 测试 | 钉住什么 |
|---|---|---|
| `crates/probe-engine/tests/publish_scenarios.rs`（新增，3 tests） | `rerunning_the_same_input_writes_the_destination_once` | 场景2：10 轮同输入 → 10 run 行、1 artifact、1 次目的地写入、9×no_change、digest 只前进一次 |
| | `a_guarded_round_withholds_publication_and_keeps_the_previous_snapshot` | 场景4 ledger 腿：两种 guard 形态都不发布、快照/文件/digest 原地、目的地零调用 |
| | `an_unavailable_destination_records_the_failure_and_keeps_the_local_snapshot` | 场景6：publish_failed+error_code、本地快照前进、digest 停在远端已收内容、下一轮按 digest 重试 |
| `crates/probe-storage/tests/artifact_invariants.rs`（新增，2 tests） | `runs_and_publishes_interleaved_stay_one_artifact_one_row_per_round` | §11.3-1/§11.1-7 交织：发布与 run claim/close 交错 10 轮仍 1 artifact、每轮 1 行、digest 只在真发布轮前进 |
| | `an_interrupted_run_does_not_fork_the_artifact` | 崩溃槽：开着 round 时 reaper 不动它（护栏），round 关闭后收尸，artifact 不分裂 |
| `crates/probe-api/tests/subscription_endpoint.rs`（新增，6 tests，1 ignored） | `a_subscription_url_needs_a_read_credential` | 稳定 URL 凭据边界（无 token 401 / 错 token 401 / publish+admin 200） |
| | `the_stable_url_tracks_the_last_good_snapshot_only` | 场景4 HTTP 腿：逐字节服务 active 快照，新轮提交后跟随 |
| | `a_current_etag_answers_304_with_no_body` | ETag/304 语义（含弱校验拒绝、空体） |
| | `the_cache_policy_survives_the_security_headers` | **ignored（缺陷 B6-1）**：断言 `public, max-age=300, stale-if-error=86400` |
| | `an_unknown_or_malformed_source_id_is_a_plain_404` | 未知/恶意 id 一律 404 不泄结构 |
| | `a_source_without_a_snapshot_is_404_not_empty` | 无快照 404，绝不出空体 |
| `tests/test_substore_write_zero.py`（新增，3 tests） | `test_a_default_round_records_zero_substore_writes` | F-001：整轮 `_run_round` 零 store 变更调用 + 本地导出仍在（防"零写=零动作"假阳性） |
| | `test_the_legacy_push_flag_alone_still_records_zero_writes` | 双门禁：push 标志开、legacy 关 → 零写 |
| | `test_a_retyped_source_never_reaches_the_store` | 禁用源不读不写 |

设计原则：不重复既有覆盖——§11.1-1/3/6、§11.3-1/5 的纯账本断言已有 B1 测试钉住（见各表"测试/证据"列），B6 只补**组合/交织/HTTP 面**。所有新测试均有行为断言（变异探针验证过 python 侧：双门禁打开时记录器立刻抓到 `update_only` 调用）。

## 7. 缺陷登记（B6 发现）

| 编号 | 发现 | 证据 | 严重度 | 状态 |
|---|---|---|---|---|
| B6-1 | `GET /artifacts/{source_id}/subscription` 的 `Cache-Control` 被 `security_headers` 覆盖为 `no-store`：`build_router`（probe-api/src/lib.rs:1290-1296，**在 `618e659` 复核仍在**）把 `subscription_cache_headers` 放在全局 `security_headers` **内侧**，外层后写 no-store。计划（18_REFACTOR_PLAN.md §4）明确把"稳定订阅端点路径豁免，单独测试钉住"列为对策，但豁免从未接线，且此前无人写这个测试。影响：客户端无法缓存（语义正确性不受影响，ETag/304 与正文都正确），每 5 分钟轮询变成每请求全量拉取。修复属 probe-api 所有者（交换 layer 顺序或在 `security_headers` 内对 `/artifacts/` 前缀放行 Cache-Control），修复后去掉 `subscription_endpoint.rs::the_cache_policy_survives_the_security_headers` 的 `#[ignore]` | `crates/probe-api/tests/subscription_endpoint.rs`（ignored 测试含复现说明） | low（性能/缓存策略，非安全非正确性） | **open**（B6 不得改 src；已留 ignored 测试钉住期望行为） |
| B6-2 | §7.2 字面语义"相同节点不同显示名只测一次"未实现：不同显示名同指纹的 job 各拨一次，仅在账本折叠为一行（round.rs NodeFold）与发布折叠（originals first-wins / display 归并）。是 Rust 重写的既有行为决策，非回归；如需对齐 §7.2 字面语义需在计划层决策（省 dials vs 保持每名字独立测量） | 本文件 §1-4 行；`crates/probe-engine/src/collect.rs`（jobs 不按指纹去重） | info | accepted（记录偏差，不在 B6 内改行为） |

## 8. NOT_RUN 汇总（原因）

| 项 | 原因 |
|---|---|
| §4 红线「Agent 开发并发 ≤ 3」 | 流程约束，非代码可测 |
| §4 红线「无人值守持续运行 ≥ 8 小时」、F-092 长跑部分 | 无 soak 环境；本机开发箱非部署形态 |
| F-094 `docker compose config` | 本机无 docker（exit 127） |
| §11.2 场景 1/3 的全链路版本、场景 5 的真实双进程 | 需要真实 mihomo 内核与真实并发触发；按任务约束用 ledger/GuardDecision 缝覆盖可达部分，其余如实标记 |
| CLI 面迁移审计行 | CLI `migrate apply` 落报告文件+日志，不写账本审计行（API 面每次 apply 写 `record_event(security)`，bdee87c） |
| 真实 GitHub / 真实 Sub-Store 的全部 E2E | 生产资源；属于 MIGRATION_GIST_ONLY.md 的「需要用户在场」步骤 |
| SQLite locked / 磁盘满 / 只读目录故障注入 | 无故障注入框架；现有错误路径测试（storage error 映射）覆盖部分分支 |
