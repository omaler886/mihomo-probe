# 领域状态机

## 目标
把 Python 隐式的五态收敛逻辑固化为纯函数领域模型，新增态先 ADR。

## 输入与依赖
01 审计；policy.py / db.nodes / engine._converge_bucket。

## 当前实现审计
- 状态：unknown/alive/pending/dead/excluded（policy.py 常量）。
- 转换规则（policy.apply）：
  - 成功 → alive，consec_fail=0；从 dead 回 alive 记 "restore"（假杀信号），从 unknown 首活记 "new"。
  - 失败 → streak+1；≥阈值(3) 判 dead（"drop"）；否则 alive/pending 保持 pending，unknown 保持 unknown。
  - excluded：入口在受限 ISP；显式清零 streak（engine._record_excluded_nodes），绕过 policy.apply。
- 护栏：round_is_suspect(alive < max(3, prev*0.5)) → 不发布。
- 约束（现有语义，重写必须保持）：一次成功立即恢复 alive；excluded ≠ dead；数据源缺失→demote_disabled_sources 降 unknown 而非杀。
- fingerprint：16 hex（sha256(连接参数 json,去 name/_)）；链式变体共享 base fp；直连孪生用 variant_fingerprint(fp,"direct")。总控文档要求的 NodeFingerprint/EndpointFingerprint 拆分在此落点。

## 设计决策（草案，R1/R2 固化）
- probe-domain 提供 `State`, `Transition`, `apply(node_state, observation, policy) -> (NodeState, Transition)` 纯函数；时间由调用方注入。
- 新增态 degraded/quarantined/stale 暂不引入；引入前先写 ADR（总控 §4）。
- 失败类别 FailureKind 对齐 engine 现有 14 种 reason 字符串，旧 API/库迁移提供兼容映射。

## 整轮护栏（R7，来源：外部 Go 方案 §九 + 本仓库现状核对）
目的是"测试基础设施故障不得导致全部节点被误杀"。逐条与现状核对如下——
**多数已具备，R7 的任务是把它们收进一个显式的 `GuardDecision` 枚举，而不是新造逻辑**：

| # | 规则 | 现状 |
|---|---|---|
| 1 | 测试目标本身不可达 → 本轮不更新死亡状态 | 部分具备：控制器失联不计 streak（06）；trace 目标故障 → 不发布（07）。R7 统一判定 |
| 2 | Mihomo 内核异常 → 本轮标记 `inconclusive` | **未具备**，见下 |
| 3 | 数据源返回空集合 → 不删除既有节点 | 已具备：`demote_disabled_sources` 降 unknown 而非杀 |
| 4 | 单轮失败率突增超阈值 → 进入保护模式 | 已具备：`round_is_suspect`（ratio 0.5 / absolute 3） |
| 5 | 保护模式下保留上一轮有效发布结果 | 已具备：suspect 轮不发布 |
| 6 | 连续多个有效轮次失败才判死 | 已具备：`drop_after_consecutive_fails = 3` |
| 7 | 恢复成功后清零 / 按策略递减失败计数 | 已具备：成功 → alive + `consec_fail = 0`，记 `restore` |

目标形态（R7）：

```rust
enum GuardDecision { ApplyConvergence, PreservePreviousState, MarkRoundInconclusive }
```

阈值不得写死，须结合真实历史轮次数据确定（现状 0.5/3 是经验值，R7 用真数据复核）。

### 关于 `inconclusive`（**ADR 前置，未定稿**）
外部方案要求"内核异常时本轮标记 inconclusive"。但本文件既定约束是
**新增态引入前必须先写 ADR**（总控 §4），因此此处只登记为候选：
- 候选语义：轮次级（`rounds` 表）标记，**不是**节点级第六态。
  节点态仍为 unknown/alive/pending/dead/excluded 五态不变。
- 优点：把"内核坏了"与"节点真死了"分开，避免前者污染 `consec_fail`。
- 需裁决：是否新增列、Python 读取方（shadow 期共享同一张 `rounds` 表）如何兼容。
- 已登记为 `ADR-0005（待写）`，见 00_MASTER_STATUS。

## 待办清单
- [ ] R1：切片内 RoundState/RoundId 最小模型
- [ ] R2：完整 NodeState/policy.apply 移植 + 与 Python 对拍测试
- [ ] R7：`GuardDecision` 枚举 + 上表 7 条显式化（阈值用真数据复核）
- [ ] R7 前：ADR-0005 `inconclusive` 轮次态裁决

## 修改记录
| 时间 | 文件 | 变更 | 原因 |
|---|---|---|---|
| 2026-09-30 | 本文件 | 审计+设计草案 | R0 |

## 测试证据
- Python 侧行为由 tests/test_logic.py PolicyTest 等锚定；Rust 侧对拍用例在 R2 补。

## 风险与回滚
- 纯函数移植若语义漂移，双跑比对（13）会拦截；默认实现不切换。

## 阻塞项
- 无。

## 下一步
- R2 前冻结 Transition 枚举与字段。
