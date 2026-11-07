# 33 — API 轮 dispatch 接线（与 CLI 同构）

> 关闭 workstreams/32 §5 的「API 侧 dispatch 发送」边界：`spawn_round` 内
> 复用 `probe_engine::agent::dispatch_job`，与 CLI 完全同构——建 Job →
> 签名 POST /jobs → `set_round_config_digest` → `note_probe_deadline` →
> `plan.dispatched`。至此两条生产轮次入口（CLI/API）都派发，与 Python
> `_dispatch_probe_jobs` 的两条调用点对齐。

## 1. 交付物

| 文件 | 内容 |
|---|---|
| `crates/probe-api/src/lib.rs` | `spawn_round` 在 preflight 后、plan 构造前插入 dispatch 块：digest 登记（rounds.probe_config_digest）→ deadline = 派发时刻 + interval_minutes → 节点集来自 collected.jobs×originals → `build_jobs`/`dispatch_job`（Bearer + ts + 签名三头）→ `note_probe_deadline` → 派发被接受的探测点 id 进 `plan.dispatched`（轮内等待与聚合消费自动生效）。逐探测点 tracing（accepted/failed），探测点故障绝不失败轮次（§4.3 观测缺失） |
| `crates/probe-api/tests/parity_gaps.rs` | `the_api_round_dispatches_signed_jobs_and_registers_the_bookkeeping`：真 router + 假 agent（verify_job 复核签名）——POST /api/v1/rounds 后假 agent 收到签名 Job（probe_id/round_id 正确）、rounds 行携带 digest、probe_deadlines 行登记 |

## 2. 设计裁决

1. **同一函数两侧复用**：`dispatch_job`/`build_jobs` 是 probe-engine 的公共
   面——CLI 与 API 只差调用上下文（同步循环 vs spawn 任务），签名算法、
   线格式、簿记完全一致（§13.1 的「两侧永不吵架」扩展到「两入口永不吵架」）。
2. **派发先于 plan 构造**（31 号批 bug 的教训在 API 侧直接规避）：Job 节点
   来自 `collected.jobs`，而 plan 会 `mem::take` 它。
3. **无 sources 的轮**：收集为空 → 0 节点 Job 仍派发（协议有效，agent 端
   202）——簿记一致性优先于微优化，agent 侧执行空 Job 无副作用。

## 3. 验收映射

| # | 行为 | 测试 | 状态 |
|---|---|---|---|
| 1 | API 轮派发签名 Job 且 agent 复核通过 | parity_gaps `the_api_round_dispatches_signed_jobs_and_registers_the_bookkeeping` | covered |
| 2 | digest/deadline 簿记落库 | 同上（round_config_digest + probe_deadline 断言） | covered |
| 3 | 轮次在派发后的内核失败不影响簿记 | 同上（无内核环境，轮次异步失败，断言在簿记侧） | covered |

## 4. 门禁（2026-11-08 实跑）

| 门禁 | 结果 |
|---|---|
| `cargo test --workspace` | 34 目标全 ok、**809 passed / 0 failed**（808 + 1 新增） |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过 |
| `cargo fmt --all -- --check` | 通过 |
| `tools/scan_secrets.py` | clean |
