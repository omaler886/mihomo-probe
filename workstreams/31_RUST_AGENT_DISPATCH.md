# 31 — G-3 收口：dispatch 发送 + Rust agent 模式 + G-7..G-10 平价缺口

> 关闭 `workstreams/21_RUST_PARITY.md` 的 **G-3**（dispatch 发送 + Rust
> agent 模式——此前 28 号留下的两块），并按路线收口 **G-7/G-8/G-9/G-10**
> 四条平价缺口。验收沿用 19 号写法，每条 = 树内指名测试。

## 1. 交付物

| 文件 | 内容 |
|---|---|
| `crates/probe-engine/src/agent.rs` | agent 协议与执行：`JobWire`/`JobNode`（§4.1 线格式，解析校验）、`verify_job`（接收门禁：Bearer 常量时间 → 原始字节签名复核 → JSON 形状，顺序与 Python 一致）、`build_jobs`（每启用探测点一份）、`dispatch_job`（`sign_request` 签名 POST `{endpoint}/jobs`，Bearer+ts+签名三头）、`post_result`（Bearer 回传，不签名）、`JobRunner` trait（dyn 兼容装箱 future）+ `KernelRunner`（prepare→`build_config`→`Controller::reload`→`delay×N`，per-job 配置落 `config_dir`；`egress_country` 恒 None）、`execute_job`（§4.2 payload 组装） |
| `crates/probe-cli/src/main.rs` | `agent` 子命令：axum 面只有 `POST /jobs`（verify→busy CAS→spawn 测量→回传→释放）与 `GET /healthz`；单内核单任务（busy 门禁）；`--id/--token-env/--controller/--kernel-api/--kernel-secret-file/--test-url/--timeout-ms/--concurrency`。轮次接线：`cmd_round` 收集后有启用探测点即签名派发（不可达只记 `dispatch: <id>:err(...)`，绝不让轮次失败）+ `set_round_config_digest` 登记 digest |
| `crates/probe-config/src/lib.rs` | `six_section_digest`（六段 sorted-JSON sha256，`Config::load` 时算好存 `probe_config_digest`——dispatch 登记进 `rounds.probe_config_digest`，结果端点比对 agent 回显） |
| `crates/probe-engine/tests/agent_roundtrip.rs` | 4 条全链路场景（见 §3） |
| `crates/probe-api/tests/parity_gaps.rs` | G-7 + G-10（HTTP 级） |
| `crates/probe-engine/tests/parity_scale_and_cross_round.rs` | G-8 + G-9（引擎级） |

## 2. 设计裁决

1. **协议两侧同函数**：dispatch 签名与 agent 复核都走
   `probe_dispatch::sign_request/verify_signature`——两侧永不吵架（§13.1）。
2. **token 先于 body 字节**：未鉴权调用方一个字节都不读（Python
   `__main__.py` do_POST 的顺序），`verify_job` 只做纯判定，HTTP 层负责
   Bytes extractor 顺序。
3. **busy 门禁**：单内核单任务（第二个并发 Job 会在第一个脚下重载内核），
   `AtomicBool` CAS → 409 BUSY，回传后释放（Python `begin_job/end_job`）。
4. **内核复用 vs 每轮起停**：Python agent 每 Job 起停内核容器；Rust agent
   用常驻内核 + 每 Job `PUT /configs` 重载（busy 门禁保证不并发）——语义
   等价，省每轮容器起停。agent 与内核必须同机（reload 传内核侧路径）。
5. **dispatch 不失败轮次**：探测点不可达/token 未设 = 本轮无远程观测
   （§4.3 观测缺失），逐探测点 `:ok/:err` 汇总进日志。
6. **digest 单一事实源**：`Config::load` 即算六段 digest（与 Python
   `json.dumps(sort_keys=True)` 同字节）；dispatch 写进 round 行，结果端点
   列空 = 未启用 dispatch（宽松分支），列非空 = 强制 DIGEST_MISMATCH 比对。
7. **G-6 保持 accepted 偏差**（19 号 §7 B6-2）：「同名不同显示名只拨一次」
   的字面语义未实现是既有行为决策（省 dials vs 每名字独立测量），不在测试
   批里改行为；21 号表指向 B6-2。
8. **G-11（24h soak）维持 NOT_RUN**：无 soak 环境，结构禁令 + 零写测试替代
   （19 号 §8 口径）。

## 3. 验收映射（每条 = 树内指名测试）

| # | 行为 | 测试 | 状态 |
|---|---|---|---|
| 1 | 接收门禁顺序（401→403→400 四类） | agent `the_job_gate_orders_auth_signature_shape` | covered |
| 2 | build_jobs 只覆盖启用探测点 | `build_jobs_covers_only_enabled_probes` + roundtrip `dispatch_covers_only_enabled_probes` | covered |
| 3 | 签名 Job 全链路：dispatch→agent→execute→post→Controller 落账 | roundtrip `a_signed_job_round_trips_through_the_agent_to_the_controller` | covered |
| 4 | 伪造请求（错 Bearer 401 / 错签名 403）零执行零回传 | roundtrip `a_forged_request_is_rejected_and_nothing_executes` | covered |
| 5 | busy 门禁：并发第二 Job 409，完成后释放 | roundtrip `a_second_job_while_busy_answers_conflict` | covered |
| 6 | dispatch 对死端点软失败（观测缺失） | agent `dispatch_to_a_dead_endpoint_reports_missing_observation` + `posting_a_result_to_a_dead_controller_fails_softly` | covered |
| 7 | G-7：Idempotency-Key HTTP 级重放同 run_id | probe-api/tests/parity_gaps `the_idempotency_key_replay_returns_the_same_run_id_over_http` | covered |
| 8 | G-8：100 节点发布 1 artifact + 重跑零写 | parity_scale `one_hundred_nodes_publish_one_artifact_and_the_rerun_writes_nothing` | covered |
| 9 | G-9：victim 三败跨轮（pending→dead 状态机 × 发布腿串测） | parity_scale `a_dying_node_stays_pending_in_the_ledger_while_the_export_converges` | covered |
| 10 | G-10：双连接（双进程）并发触发恰一起一拒 | probe-api/tests/parity_gaps `two_processes_triggering_concurrently_start_exactly_one_round` | covered |

## 4. 门禁（2026-11-08 实跑）

| 门禁 | 结果 |
|---|---|
| `cargo test --workspace` | 34 个测试目标全 ok、**798 passed / 0 failed**（785 + 13 新增） |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过 |
| `cargo fmt --all -- --check` | 通过 |

## 5. 诚实边界

- `KernelRunner` 的真实内核路径（reload+delay 打真 mihomo）属 NOT_RUN：
  协议行为由 `verify_job`/dispatch 的 loopback stub 与假 runner 钉住，与
  19 号 §8 的「真实内核 NOT_RUN」口径一致。
- dispatch 的 deadline 登记未实现（`verify_result` 的 RESULT_LATE 由
  round-match 门禁兜底）；Python 的 deadline 持久化表是接线时的前置件。
- 轮末聚合消费 `probe_observations`（远程观测进 `policy.apply`）尚未接线：
  派发与回传面已就位，消费侧需要 round 语义决策，独立批次验收。
