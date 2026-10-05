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

## RoundRunner：闸门接入流水线（R5）— **已实现**
此前轮次逻辑**有两份**（`probe-cli::cmd_round`、`probe-api::start_round`），
都是同一段占位代码且都没走闸门 —— `RoundCtx` 没有调用方。现在收成一条路径：
`crates/probe-engine/src/round.rs::run_round`。

| 环节 | 谁负责 |
|---|---|
| 开轮次行 | **调用方**（`Ledger::start_round`）—— API 要在响应里返回 `round_id`，等不了 spawned task |
| 内核准备 | `KernelPrep`：`ControllerPrep`（version + reload）或 `NotPrepared`（配置生成失败） |
| 节点测试 | `NodeTester` 经 `RoundCtx` 逐层取槽；`KernelDelayTester` 是真实实现 |
| 写结果 | `Ledger::record_verdicts`，**单事务批量**（外部方案 §2.5 的要求） |
| 关轮次行 | `run_round`，**每条路径都关**（唯一的例外：`finish_round` 自身失败，
  此时行确实留在打开态——`run_round` 把该错误原样返回，日志明确报出，
  依赖 `open_round_ids()` / 孤儿回收） |

**取消的两条不同规则**（容易混）：
- 取消后**不写结果** —— 掉队者的判定属于一个已经不存在的轮次，写下去会推进失败计数；
- 取消后**仍要关轮次行** —— "别写"和"别把账本留成孤儿"是两件事。

内核不可达 / 配置生成失败 → **跳过节点阶段**（与 03 的整轮护栏同一条原则：
基础设施故障不得记成节点故障），轮次行照常关闭。

测试目标的选择与 Python 对齐：取 `test.targets` 稳定分区后的**首个**目标
（有 https 就是 https），"是否要求 https"由它决定 ——
`https_required = 首个目标是 https`。**全 http 目标列表不算"无法测试"**，
Python 在该情况下会把 http 通过当作存活；Rust 若把它当 blocked 就会在每轮
双跑里制造一处无意义的差异（这条曾被独立审查指出，已改）。

## 前置池 + 链式展开 + 两遍测试（R6）— **已实现**

Python 锚点：`engine.collect_fronts` / `expand_chains` / `_measure_flags` /
`_test_phases` / `manual_fronts` / `chain_block`；`config.normalize_chain`。
落地分布：`probe-config::ChainSection`（config 段 + `is_configured`）、
`probe-source::SubAdmin`（manual 前置的 upsert/delete）+ `Role`（RawEntry/
PreparedNode/Job 三层透传）、`probe-engine::collect`（`collect_fronts` +
`expand_chains`）、`probe-engine::round::run_phases`（两遍）。

| 语义 | 移植要点 |
|---|---|
| `chain_block` 三态 | `ChainSection::is_configured()`：`enabled is True` 且
  （front_source_name 或 front_text）非空。半配置=未配置（否则一次手改配置会
  让全链式 `front_dead`，读起来像网络故障） |
| 三输入池 | manual（front_text → Sub-Store 物化订阅 `{prefix}-front-manual`，digest
  缓存防重复写，失败不缓存下次重试，清空粘贴→删除订阅且只删一次）+ front_pick
  名单（**窄化**资源、保持资源自身顺序，与 Python `retain` 一致）+ front_source
  整源；manual 在前 |
| 内核名预留 | `__FRONT{i}__` 位置派生（Python `FRONT_NAME_PREFIX`），链式变体的
  dialer 永远解析到前置而不是同名的用户节点 |
| 前置身份 | front job 的 fp = 原 proxy 的 fingerprint（前置自己也是别处的节点）；
  前置 job 的 server_ip = 前置自己的 server（第一遍闸门） |
| 变体展开 | 带上游 dialer 的节点 × 每前置一条，**fp 不变**（同一账本行：任一前置
  拉通即活）；source 的 `direct`/`chain` 双开关（`_measure_flags`：both-off 兜底
  直连、无 flags=历史 chain-only）；relay 节点永不展开；front 自身永不展开 |
| 直连孪生 | 双开关同开时才有，fp = `variant_fingerprint(原fp, "direct")` —— 否则
  两个测量互相覆盖；chain 关时单变体不改键（否则孤儿化历史行） |
| `test_plain_nodes` | 无上游 dialer 的普通节点也过链、**不发直连孪生**（账本判活口径
  =客户端口径）；chain 开关关的源保持直连 |
| 空池三态 | 直连轮（flags=None）不展开；半配置/链关：fall-through 直连（记 warn）；
  链式轮真空池：变体带 `front=None` 进 config（keep_dialer 空→剥 dialer），runner
  判 `front_dead` 不拨号 |
| 两遍测试 | `run_phases`：第一遍 role≠Chain（含前置），活前置集合（role=Front 且 alive）
  才放行第二遍；未试节点按 (source, fp) 去重判 `front_dead`（attempts=0、
  detail「前置全部不通，链式未测」、category=chain、照写账本推进 streak） |
| 轮级口径 | `RoundOutcome.live_fronts` / `front_dead`；chain job 的 `server_ip` = 前置
  地址（闸门 per_server_ip 限的是本机拨出去的那一跳） |
| API/CLI | CLI `round --mode direct|chain`（None=调度语义）；API 轮次固定调度语义
  （`mode=None`）；`ManualFrontCache` 在 AppState 常驻、CLI 每进程一份 |

已知口径差异（记录，不改）：Python `finish_round` 的 total 按 (source, fp)
去重（同节点多前置变体只计 1），Rust `counts.total` 按 verdict 数（=变体数）。
`results` 行两边都按变体写；轮行 total 在双跑对账时按此口径折算。

冒烟（smoke-root，fake Sub-Store）：直连轮 2 节点诚实关行；链式轮 front pool
1 条 → jobs = 2 直连 + 链式变体 + 直连孪生 + front = 5，`dialer-proxy:
__FRONT0__` 与前置代理都进了内核配置；`--mode direct` 池被跳过、dialer 全剥。

## 待办清单
- [x] R5：分层并发 Limits（global/per_source/per_server_ip/diagnose）— 见上
- [x] R5：`RoundCtx` 接进轮次流水线（`run_round` + 单事务结果写入 + 取消语义）
- [x] R5：**节点采集**（`probe-source` crate：fetch→flatten→prepare→jobs，
      1d070f2）
- [x] R5：`test_one` 的 HTTPS 优先重排 + `max_attempts=3` + 超时升级 + TERMINAL_REASONS
      （1d070f2，`measure.rs::TestOne`）
- [ ] R5：用真机轮次数据复核四层阈值（当前 `from_concurrency(20)` 的比例值是初值，**不是调优结果**）
- [x] R6：**前置池 + 链式展开 + 两遍测试**（本批，见上节）
- [ ] R6：出口验证+ipmap 落地映射（07 主题）
- [ ] R7：整轮护栏 `GuardDecision`（内核不可达已在 runner 内处理，其余待补）
- [ ] R5/R9：L1/L2/L3 与自适应周期（ADR 先行）
- [ ] R9：对外取消入口 `POST /api/v1/rounds/{id}/cancel`（闸门与 runner 侧已具备）

## 测试证据
- Python 锚点：RetryTest/HttpsVerdictTest/ChainPayloadVerifyTest/ChainRoundTest/RoundBudgetTest 等（tests/test_logic.py）。

## 风险与回滚
- 引擎为最高风险面，切换默认实现前必须 shadow 双跑（13）。

## 下一步
- R5。
