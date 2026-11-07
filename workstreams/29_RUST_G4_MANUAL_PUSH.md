# 29 — Rust 移植 G-4：人工 Sub-Store 推送三件套

> 关闭 `workstreams/21_RUST_PARITY.md` 的 **G-4**：preview（只读 diff）→
> confirm-token（60s 一次性，绑定 target+digest+节点数，防重放）→ push
> （校验并消费 token → update-only 更新目标对象）→ publish_audit。
> 验收沿用 19 号写法，每条 = 树内指名测试。

## 1. 交付物

| 文件 | 内容 |
|---|---|
| `crates/probe-engine/src/manual_publish.rs` | `manual_publish.py` 的 Rust 镜像：`ConfirmTokens`（进程内注册表：一次性/绑定/同 target 作废旧单/过期消费）、`parse_proxies`（YAML+JSON 双解析，BAD_CONTENT）、`diff_preview`（指纹口径：added/removed/renamed/unchanged + 样例≤20 只带 name/type）、`push_canonical`（update-only，目标缺失 `SUBSTORE_TARGET_MISSING`，绝不新建） |
| `crates/probe-api/src/manual_push.rs` | `POST /api/v1/substore/preview`、`POST /api/v1/substore/confirm-token`、`POST /api/v1/substore/push`（admin 门禁；canonical 产物读本地落盘 `<export_root>/mihomo-alive.yaml`+meta；每个动作（含拒绝）都落 publish_audit） |
| `crates/probe-config/src/lib.rs` | `substore.target_name`（默认 `mihomo-probe-alive`） |
| `crates/probe-storage/src/probes.rs` | `record_publish_audit`/`list_publish_audit`（migration 0013 的 publish_audit 表） |

## 2. 设计裁决

1. **update-only 是唯一写路径**：`SubStoreClient::update_sub` 先查存在性
   （PATCH 缺席对象会被 Sub-Store 的 upsert 语义变成 CREATE——正是本服务
   绝不能做的事），缺失即拒绝；`RecordingAdmin` 测试钉住 `upsert_sub` 永不
   被调用。
2. **token 三重绑定 + 防重放**：consume 校验 target+digest+node_count 与
   签发一致（canonical 已变 → MISMATCH）、一次性（REUSED）、60s TTL
   （EXPIRED，且过期即消费——不留活口）、同 target 重签作废旧单。
3. **审计无死角**：preview/confirm/push 的成功与每一类拒绝都写
   publish_audit（no_artifact/rejected_*/substore_error/pushed/...），
   跨重启可查。
4. **样例无凭据**：diff 样例只含 name/type；§13.7 的连接字段永不进预览与
   日志（测试钉住）。
5. **canonical 产物来源**：本地落盘的 parity 文件（workstreams/27），与
   Python 的 canonical_artifact() 同语义——尚无产物即
   NO_CANONICAL_ARTIFACT（409），先完成一轮测活。

## 3. 验收映射（每条 = 树内指名测试）

| # | 行为 | 测试 | 状态 |
|---|---|---|---|
| 1 | token 一次性 + 绑定 + 同 target 作废 | manual_publish `the_confirm_token_is_one_shot_and_bound` | covered |
| 2 | 过期消费 + 复用拒绝 | `an_expired_token_is_consumed_and_rejected` | covered |
| 3 | BAD_CONTENT 各形 | `content_without_a_proxies_array_is_bad_content` | covered |
| 4 | 改名折叠进 renamed + 样例无凭据 | `the_preview_folds_renames_into_the_renamed_bucket` | covered |
| 5 | update-only：upsert 永不调用 + 目标缺失拒绝 | `the_push_is_update_only_and_never_creates` | covered |
| 6 | 无产物三端点全拒 + 全落审计 | manual_push `without_a_canonical_artifact_every_step_is_refused` | covered |
| 7 | 三步流程 + 防重放可观测 + 审计覆盖 | `the_three_step_flow_issues_consumes_and_audits` | covered |

## 4. 门禁（2026-11-08 实跑）

| 门禁 | 结果 |
|---|---|
| `cargo test --workspace` | 31 目标全 ok、**785 passed / 0 failed**（778 + 7 新增） |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过 |
| `cargo fmt --all -- --check` | 通过 |
| `tools/scan_secrets.py` | clean |
