# 32 — G-3 语义闭环：轮末聚合消费 + deadline 登记表

> 关闭 workstreams/31 §5 的两条诚实边界，完成 G-3 多探测点语义的**代码
> 闭环**：远程 `probe_observations` 经 `aggregation::aggregate_round` 喂进
> round 的收敛（policy.apply 口径），dispatch 的 deadline 登记使
> `verify_result` 的 RESULT_LATE 真正生效。验收沿用 19 号写法。

## 1. 交付物

| 文件 | 内容 |
|---|---|
| `migrations/0014_probe_deadlines.sql` | `probe_deadlines(probe_id, round_id, deadline, PK(probe_id, round_id))`——dispatch 登记的回传截止（Python deadline.json 的 SQLite 化：与账本同生命周期，重启不丢） |
| `crates/probe-storage/src/probes.rs` | `note_probe_deadline`（幂等覆盖）/ `probe_deadline`（None = 未登记，诚实无 enforce） |
| `crates/probe-engine/src/probe_aggregate.rs` | `consume(AggregateConsumeRequest)`：①本地观测落表（`probe_id="local"`，与远程同一条聚合路径——灰度第 1 步 no-op）；②等待循环（轮询观测表，直到已回传 ⊇ waitable 或截止+late 到点；waitable 剔除 dark——disabled 或心跳 > 2×interval；§4.3 观测缺失语义）；③`aggregate_round` 折叠并按三条语义改写 folded：alive（ok=true，最快探测点 delay，国别跟随）/ fail（代表原因 = probe_id 序第一条）/ **inconclusive（`is_inconclusive=true`——persist 的 converge 过滤器跳过，账本状态与 consec_fail 原样保留，§7.7）**；没听过的指纹保留本地 verdict；④轮级摘要 |
| `crates/probe-engine/src/round.rs` | fold 与 persist 之间插入 `consume`；`Ledger` trait 增 `record_observation`/`get_observations`/`list_probes`（默认空实现——假账本与兼容模式零成本）；`Mutex<Storage>` 实现；`RoundPlan` 增 probes/aggregation/dispatched/probe_deadline/probe_config_digest；`RoundOutcome.aggregate_summary` |
| `crates/probe-engine/src/probe_dispatch.rs` | `verify_result` 增 `finished_at_epoch`：迟到两种——接收时刻越窗与 payload 自报 finished_at 越窗，同判 RESULT_LATE |
| `crates/probe-config/src/lib.rs` | `AggregationSection.interval_minutes`（默认 30，clamp 1..1440——Python `probe_policy.interval_minutes` 的口径） |
| `crates/probe-cli/src/main.rs` | dispatch 重构：**先派发后建 plan**（修复 31 号批的真实 bug——plan 先 `take` 走 jobs，派发读到的节点列表恒空）；deadline = 派发时刻 + interval 登记进 probe_deadlines；派发被接受的探测点 id 进 `plan.dispatched`；canonical 请求携带聚合摘要 |
| `crates/probe-engine/src/canonical_round.rs` | status 文件增 `probes` 键（completed/missing/offline/unresolved/strategy/minimum——探测点面板的诚实口径） |

## 2. 设计裁决

1. **插入点 = fold 之后、persist 之前**：`fold_verdicts` 的 FoldedNode 是
   每指纹一条的收敛前形态，`persist_measurements` 对 `is_inconclusive` 节点
   跳过 converge——聚合只需改写 folded 即可实现「inconclusive 不得推进
   consec_fail」（§7.7），无需动状态机。
2. **本地观测落表**（Python `_record_local_observations` 同语义）：本地是
   聚合的一名普通探测点；probe_id="local" 保证单探测点兼容模式与多探测点
   走同一条 `aggregate_round`。兼容模式（无 probes 无 dispatched）整体跳过。
3. **等待窗口**：deadline + late − now，被取消（ctx.is_cancelled）短路——
   已测得的 verdict 不能因等待被 watchdog 整轮作废。dark 探测点（disabled /
   心跳超 2×interval）不等：为离线盒子等满窗口会拖死每一轮。
4. **digest 门禁**：等待循环与聚合共用 `filter_observations`（known =
   dispatched ∪ registry ∪ local，digest_gate="engine"）——digest 不符的
   回传只进审计，不进聚合。
5. **31 号批 bug 修复**：dispatch 原本在 plan `take` 走 `collected.jobs`
   之后读它——派发的节点列表恒空。现在派发先于 plan 构造，真实节点集
   随 Job 出站；roundtrip 测试（agent 侧）未能捕获此 bug，记录在案。

## 3. 验收映射（每条 = 树内指名测试）

| # | 行为 | 测试 | 状态 |
|---|---|---|---|
| 1 | any_alive：本地活 + 远程败 = 活 | probe_aggregate `any_alive_keeps_a_locally_alive_node_alive_against_one_remote_fail` | covered |
| 2 | 本地败 + 远程活 = 活（delay/国别取最快探测点） | `a_remote_pass_revives_a_locally_failed_node` | covered |
| 3 | 全败且足数 → fail，代表原因跨轮稳定 | `all_failed_with_enough_probes_fails_with_the_stable_reason` | covered |
| 4 | 完成数不足 → inconclusive，converge 跳过（§7.7） | `insufficient_probes_marks_the_node_inconclusive_and_skips_convergence` | covered |
| 5 | 没听过的指纹保留本地 verdict | `a_fingerprint_no_probe_answered_for_keeps_the_local_verdict` | covered |
| 6 | 摘要诚实口径（unresolved/offline） | `the_summary_names_missing_offline_and_unresolved_probes` | covered |
| 7 | 兼容模式整体跳过（no-op） | `the_compat_mode_skips_the_whole_consume` | covered |
| 8 | digest 不符的远程观测被拒不进聚合 | `a_digest_mismatched_remote_observation_is_rejected` | covered |
| 9 | RESULT_LATE 双路径（接收越窗 / finished_at 越窗）+ 无登记不比 | probe_dispatch `result_late_fires_on_both_arrival_and_finished_at` | covered |
| 10 | status 文件携带 probes 摘要 | canonical_round_scenarios `the_status_file_carries_the_probe_aggregate_summary` | covered |
| 11 | deadline 登记幂等 + 读取 | probe-storage probes 测试（既有 4 条 + 0014 迁移版本钉） | covered |

## 4. 门禁（2026-11-08 实跑）

| 门禁 | 结果 |
|---|---|
| `cargo test --workspace` | 34 目标全 ok、**808 passed / 0 failed**（798 + 10 新增） |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过 |
| `cargo fmt --all -- --check` | 通过 |
| `tools/scan_secrets.py` | clean |

## 5. 诚实边界

- 等待循环的真实超时路径（窗口到点）与多秒轮询未做长跑验证——窗口逻辑由
  单测的立即满足/立即到点分支钉住；真实跨进程时序属 NOT_RUN（§8 口径）。
- API 轮 dispatch 发送已接线（workstreams/33，与 CLI 同构）。
- 21 号「生产双跑 ≥2 周无回归」的完全替代宣告仍依赖部署环境（会话外）。
