# 25 — Rust 移植 G-2：canonical 四文件发布链路（指导 §8 / §18-2、§11.1-2/6、§11.2-2/4）

> 目标：把 G-1 的 canonical 模型接进发布链路——`build_canonical` 产物 →
> 固定四文件集合 → `probe-publish` 一次 GET + 至多一次 PATCH 的幂等发布。
> 关闭 `workstreams/21_RUST_PARITY.md` 的 **G-2**。验收沿用 19 号写法。

## 1. 交付物

| 文件 | 内容 |
|---|---|
| `crates/probe-publish/src/files.rs` | `FilesPublisher` trait + `FilesOutcome`（NoChange / Published{changed, revision}）——固定文件集发布的最小接缝 |
| `crates/probe-publish/src/gist.rs` | `impl FilesPublisher for GistPublisher`：一次 GET → 逐文件 sha 对比 → gate 未变零 PATCH、变了单次 PATCH 只携带变化文件（永不含 content:null）；与逐 artifact 发布共用 per-gist 写锁（F-033）、重试/退避/终态/脱敏机制 |
| `crates/probe-engine/src/canonical_publish.rs` | 固定四文件名常量、`CanonicalFiles::from_round`（None = 空轮/护栏 → 不构造不请求）、`publish_canonical`（结果复用账本口径 published/no_change/publish_failed，始终携带 attempted digest）、`digest_advances`（调用方"只进不退"判定） |
| `crates/probe-engine/tests/canonical_publish_scenarios.rs` | 6 条引擎级场景测试（假目的地实现同一契约，与 publish_scenarios 的 ShaGist 分工方式一致） |

## 2. 验收映射（每条 = 树内指名测试）

| # | 行为 | 测试（canonical_publish_scenarios.rs） | 状态 |
|---|---|---|---|
| 1 | 同输入 10 轮重跑 = 1 次 PATCH + 9×no_change（§11.2-2 引擎级镜像） | `ten_reruns_of_the_same_round_write_the_destination_once` | covered |
| 2 | 随行文件（status）只在 canonical 变化的同一 PATCH 里同行；仅 status 变化 → 零 PATCH | `the_status_file_rides_along_only_when_the_canonical_changes` | covered |
| 3 | 护栏空轮不构造文件集、零请求，上一稳定四文件站住（§11.2-4） | `a_guarded_round_builds_no_files_and_sends_no_requests` | covered |
| 4 | 发布失败（5xx 可重试）：publish_failed + attempted digest，上一成功内容原地站住；下一轮重试同内容成功才推进 digest（§11.1-6「digest 只进不退」） | `a_failed_publish_keeps_the_previous_content_and_reports_the_attempted_digest` | covered |
| 5 | 401/403 终态映射为非可重试（gist_auth） | `terminal_auth_failures_are_reported_non_retryable` | covered |
| 6 | 首建一次 PATCH 建齐四文件、永不含 content:null（删除语义） | `a_first_publish_seeds_all_four_files_in_one_patch` | covered |

## 3. 裁决（指导 §17.8）

1. **"内容未变零 PATCH"钉在 canonical 文件字节上**：改名会改变 canonical 内容
   （订阅方本应看到新名），那是一次真实发布——digest（连接字段身份）不移动
   与内容变化是两回事，两者由不同测试分别钉住（G-1 的 digest 用例 vs 本批的
   ride-along 用例）。
2. **revision 语义**：`FilesOutcome::Published.revision` = gate 文件 sha256
   首 12 hex（与 manifest revision 同约定，可本地复算）；真实 GitHub revision
   以 history_url 为准，未在此层伪造。
3. ** Gates 落在 probe-publish 层**：四文件布局是发布语义（§8），不是引擎私有；
   引擎只做装配（文件名常量 + payload 序列化 + 账本口径映射）。真 GitHub 属
   NOT_RUN（沿用 §11 的登记口径），协议行为由假目的地钉住。

## 4. 门禁（2026-11-06 实跑）

| 门禁 | 结果 |
|---|---|
| `cargo test --locked --workspace` | 30 个测试目标全 ok、**730 passed / 0 failed**（724 基线 + 6 新增） |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 通过 |
| `cargo fmt --all -- --check` | 通过 |

## 5. 集成边界（后续批次）

本批交付**发布语义层**：canonical 产物 → 四文件 → 幂等远端发布。尚差两步才
是完整链路：(a) round 流程内的调用接线（round.rs 收敛后取 alive 节点 →
`build_canonical` → `publish_canonical`，含 suspect 护栏判定喂 from_round）——
与 G-3（多探测点聚合）同批，因为 alive 集合的输入正是聚合结论；(b) CLI/API
面。Python 侧行为零改动。
