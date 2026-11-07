# 27 — Rust canonical 四文件 round 接线（G-2 集成步骤 (a)，workstreams/25 §5）

> 目标：把 G-2 的发布语义层接进 round 流程——收敛后取 alive 节点 →
> `build_canonical` → `publish_canonical`（Gist 或 WebDAV），含护栏判定喂
> `from_round`。同时补齐本地 parity 落盘（`mihomo-alive.yaml` + meta 侧车）。
> 验收沿用 19 号写法，每条 = 树内指名测试。

## 1. 交付物

| 文件 | 内容 |
|---|---|
| `crates/probe-engine/src/canonical_round.rs` | `CanonicalRoundRequest`（与 `PublishRoundRequest` 同一轮状态）、`assemble_round_canonical`（护栏同构判定 + 同一 `select_source_exports` 口径）、`write_local_canonical`（原子替换落盘）、`build_history`（账本最近 100 轮，Python `build_history` 镜像）、`build_readme`（Python `build_readme` 镜像）、`CanonicalPublisher`（provider→目的地解析：Gist/Composite→Gist、Webdav→WebDAV、Local→None）、`publish_round_canonical`（装配→落盘→尽力而为发布） |
| `crates/probe-cli/src/main.rs` | `round` 流程接线：远程腿之后跑 canonical 发布（spawn_blocking + current-thread runtime，与远程腿同型）；masked 控制台输出 |
| `crates/probe-api/src/lib.rs` | `publish_after_round` 接线：同一链路，tracing 记录 |
| `crates/probe-engine/tests/canonical_round_scenarios.rs` | 9 条引擎级场景测试 |

## 2. 设计裁决

1. **同一 selection 口径**：canonical 的入选节点来自 `select_source_exports`
   的**同一次调用形态**（同 publish/sources/collected/verdicts）——canonical
   与逐源产物永不各说各话（场景测试 `selection_feeds_the_canonical_from_the_same_call`）。
2. **护栏完全同构**（§11.2-4）：publish 关闭 / `PreservePreviousState` /
   未测量的 inconclusive → 不装配、零请求、零落盘。
3. **fire-and-forget**：canonical 发布结果只进报告/日志，绝不改本轮节点状态
   （Python `_publish_gist` 契约）；本地落盘失败也只记日志，远端照常尝试。
4. **本地 parity**：`<export_root>/mihomo-alive.yaml` + `mihomo-alive.meta.json`
   原子替换（tmp+rename），provider=local 的部署也能消费 canonical。Rust 产物
   是 pretty JSON 文档（`render_clash_meta` 既有口径），与逐源产物一致。
5. **digest 随行**：status 文件带 `digest`；报告携带 attempted digest，
   「只进不退」判定复用 `canonical_publish::digest_advances`（published 才推进）。
6. **counts 单一事实源**：status 文件的 total/alive/dropped 直接取
   `RoundOutcome::counts`（本轮行），不做二次查询。
7. **重跑幂等**：canonical 字节稳定 → gate 未变 → 零写；`updated_at` 在
   status（随行文件）里，gate 没动就不发（workstreams/25 裁决 1 的镜像）。

## 3. 验收映射（每条 = 树内指名测试，canonical_round_scenarios.rs）

| # | 行为 | 测试 | 状态 |
|---|---|---|---|
| 1 | selection 同口径 | `selection_feeds_the_canonical_from_the_same_call` | covered |
| 2 | 护栏空轮零请求零落盘（§11.2-4 round 级镜像） | `a_guarded_round_builds_no_files_and_sends_no_requests` | covered |
| 3 | 正常轮四文件 + 本地 parity + digest/revision 报告 | `a_normal_round_publishes_four_files_and_materializes_locally` | covered |
| 4 | 同内容重跑零写、digest 不动 | `a_rerun_of_the_same_round_writes_nothing` | covered |
| 5 | 无远程通道仍本地落盘（attempted=false） | `without_a_remote_channel_the_local_materialization_still_happens` | covered |
| 6 | 发布失败报 attempted digest、本地照常、上轮内容站住 | `a_failed_publish_reports_the_attempted_digest_and_keeps_local_files` | covered |
| 7 | history 文件由账本生成（一轮一行 JSON） | `the_history_file_describes_ledger_rounds` | covered |
| 8 | digest 只进不退判定 | `the_digest_only_advances_through_published` | covered |
| 9 | 本地落盘原子替换无 .tmp 残骸 | `local_materialization_replaces_previous_files_atomically` | covered |

## 4. 门禁（2026-11-08 实跑）

| 门禁 | 结果 |
|---|---|
| `cargo test --workspace` | 31 个测试目标全 ok、**757 passed / 0 failed**（748 + 9 新增） |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过（0 警告） |
| `cargo fmt --all -- --check` | 通过 |

## 5. 集成边界（后续批次）

- CLI/API 的 canonical 发布状态目前只进日志/控制台；账本侧 `rounds` 级
  canonical 发布状态列（若要对账 UI 展示）留待后续按需加列。
- G-3（多探测点聚合）落地后，status 文件的 `probes[]` 健康概况与 meta 的
  `best_probe` 才有数据源；本批先置空（与单探测点兼容模式等价）。
