# 探测引擎

## 目标
完整异步流水线 + 分层检测 + 自适应调度（总控 §7/§8）。

## 当前实现审计（engine.py，2895 行）
- 流水线：collect_entries（enabled 源→entries，fp 唯一定点 _orig_fp）→ chain front 池（三输入：粘贴文本→Sub-Store 物化订阅 / front_pick 名单 / front_source 资源；max_fronts 上限）→ classify_and_expand（双视角 DNS 解析→入口国别批量查→受限 ISP 跳过→每 IP 变体）→ expand_chains（直连/链式双开关、test_plain_nodes 客户端口径、空池 front_dead 语义）→ make_testable（内核校验+裁剪）→ _test_phases（前置先测，活前置才放行链式变体）→ test_one（HTTPS 必过才判活；timeout 升级长超时；TERMINAL_REASONS 不耗重试）→ _verify_chain_payload（204 之外的真拉流；front 失败连坐 chain）→ _verify_egress（lanes 并行出口国别）→ 收敛/发布。
- 并发与取消：ThreadPoolExecutor（测试 concurrency、验证 lanes）；deadline 贯穿（每 attempt 前检查 + 每阶段 checkpoint，超预算 RoundTimeout→释放锁→告警）；无 API 级取消（R9 补 CancellationToken）。
- 重试：max_attempts=3，retry_pause 0.3s；按 FailureKind 决定（TERMINAL_REASONS 即停）。
- 整轮保护：见 07 与 policy.round_is_suspect；补前端池全死告警、控制器失联不计 streak。
- 自适应调度：现状=固定 30 分钟 + 手动 direct/chain 模式；L1/L2/L3 分层与按状态差异化周期为新增设计（R5/R9）。

## 分层并发 Limits（R5）— **已实现**（`crates/probe-engine`）
现状（Python）只有单一 `concurrency=20` 的 ThreadPoolExecutor，无分层，无法阻止
"某一台服务器被同一批节点打满"或"诊断车道拖垮快速车道"。Rust 侧已改为显式分层：

```
全局测试并发（global）
├── 每数据源并发（per_source）
└── 每服务器 IP 并发（per_server_ip）   ← 防同一落地被打满

失败诊断并发（diagnose）—— 与 global 平级，不在其下
```

`diagnose` **刻意不挂在 global 之下**：它是"已经知道慢"的重测车道，
挂在 global 下就会排到它所绕开的拥塞后面，失去隔离意义。
代价是**单轮在飞峰值 = `global + diagnose`**，配置时两层要一起算。

落点：`crates/probe-engine/src/limits.rs`（crate 在 02 的计划里已列，本批建立）。

| 类型 | 职责 |
|---|---|
| `Limits` | 四层上限；`effective()` 把每层夹取到 `1..=1024`（**0 会死锁**，必须降级为 1） |
| `Job` | `source_id` / `node_id` / `variant` / `server_ip` |
| `Gate` | 四个信号量；按 key 惰性建 `Semaphore`，超 1024 个空闲条目自动剪枝 |
| `Permit` | RAII，`Drop` 逆序归还；**没有** `release()` 方法（防重复释放/忘记释放） |
| `RoundCtx` | `Arc<Gate>` + `CancellationToken`；`acquire` / `acquire_diagnose` / `check` |

两条必须遵守的不变量：

1. **固定获取顺序**：global → source → server_ip，任务不会回头要已持有的资源 →
   不可能成环，因此不会死锁。破坏它的唯一方式是**同一个任务对同一个 key 取两次**：
   信号量不可重入。链式节点会碰两个地址（前置 + 落地），所以 `Job::server_ip`
   必须传**内核从本机拨出去的那个地址（前置）**，不能两个都传。
2. **写账本前必须调 `RoundCtx::check()`**：持有 permit ≠ 轮次还有效。
   取消后继续写，会让一个从未完成的测试去推进节点的失败计数
   （Python 侧"控制器失联推进全节点 streak"就是这类事故）。

取消语义：`acquire` 用 `select! { biased; cancel.cancelled() ... }`，
取消优先于获取 —— 被取消的轮次不允许排队的任务再溜进来。

## 待办清单
- [x] R5：分层并发 Limits（global/per_source/per_server_ip/diagnose）— 见上
- [ ] R5：把 `RoundCtx` 接进实际轮次流水线（现在只有闸门本身，无调用方）
- [ ] R5：用真机轮次数据复核四层阈值（当前 `from_concurrency(20)` 的比例值是初值，**不是调优结果**）
- [ ] R5：delay 引擎+失败归类（对拍 test_one）
- [ ] R6：出口验证+ipmap 落地映射
- [ ] R5/R9：L1/L2/L3 与自适应周期（ADR 先行）
- [ ] R9：CancellationToken 取消/优雅停机（闸门侧已具备，待接调度）

## 测试证据
- Python 锚点：RetryTest/HttpsVerdictTest/ChainPayloadVerifyTest/ChainRoundTest/RoundBudgetTest 等（tests/test_logic.py）。

## 风险与回滚
- 引擎为最高风险面，切换默认实现前必须 shadow 双跑（13）。

## 下一步
- R5。
