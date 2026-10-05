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

### ADR-0005：`inconclusive` 轮次态 — **已裁决（R7 批次，随 ae6b1ec 后批落地）**
- **裁决**：轮次级标记，新增 `rounds.inconclusive INTEGER NOT NULL DEFAULT 0`
  （migration 0003）。**不是**节点级第六态——节点态保持
  unknown/alive/pending/dead/excluded 五态不变。
- **写入条件**：`KernelState::Unreachable` / `NotPrepared`（内核不可达或配置
  生成失败）→ 该轮不测节点、不收敛、不推进任何 streak，`inconclusive=1`。
  这把 runner 原有的隐式行为（跳过节点阶段）显式化为可查询的列。
- **读取方**：`previous_alive_count` 跳过 suspect 与 inconclusive 的轮——
  测了空空的轮不得作为下一轮护栏的存活基线。
- **Python 兼容（shadow 共库）**：Python 读方无此列，INSERT 走 DEFAULT 0，
  行为不变。Python 把 Rust 的 inconclusive 轮读作「ok=0 的完成轮」，
  `_previous_alive_count` 因此回溯——suspect 护栏保守触发，方向安全
  （保护上轮发布），无需改 Python。已在 13 登记为口径差异。

## R7 落地形态（本批）
- `probe-domain::policy`：`Policy` / `NodeSnapshot` / `Observation` / `NodeUpdate` /
  `Transition`（Restore/New/Drop/None）/ `apply()` 纯函数 + `round_is_suspect`。
  无时钟、无 IO（crate 约束）；`last_ok` 戳由调用方注入（storage 的 `utc_now`，
  与 `db.now()` 同格式）。
- `probe-storage`：`converge_nodes`（单事务折叠整轮：get/占位 insert → apply →
  动态列 UPDATE → `node_state_history` 追加 → 转移统计）；`previous_alive_count`
  （跳过 suspect/inconclusive 轮）；`finish_round_full`（全列关闭轮次行）。
- `probe-engine::round`：`GuardDecision` 枚举（上表 7 条中 1/2/4/5/6/7 已显式化，
  3 的 demote_disabled_sources 待 R10 sources 表）；`fold_verdicts` =
  `_score_bucket` 的 any-alive 口径（任一变体活即活、活者最小延迟、category
  跟随通过变体）；suspect 轮收敛照写、note 带「suspect: alive X ...;
  publish preserved」，发布保留的执行在 R8。
- 阈值 0.5/3/3 仍为经验值：`PolicySection` 已全部配置化，真机数据复核
  归入 R5 收尾同一批。

## 待办清单
- [x] R1：切片内 RoundState/RoundId 最小模型
- [x] R2：完整 NodeState/policy.apply 移植（probe-domain 12 测，含与 Python
      `tests/test_logic.py::PolicyTest` 同锚点的用例）
- [x] R7：`GuardDecision` 枚举 + 护栏规则显式化（规则 3 待 R10；阈值复核待真机）
- [x] R7 前：ADR-0005 `inconclusive` 轮次态裁决（见上）

## 修改记录
| 时间 | 文件 | 变更 | 原因 |
|---|---|---|---|
| 2026-09-30 | 本文件 | 审计+设计草案 | R0 |
| 2026-10-05 | 本文件 | ADR-0005 裁决 + R7 落地形态 + 待办勾选 | R7 批次 |

## 测试证据
- Python 侧行为由 tests/test_logic.py PolicyTest 等锚定。
- Rust 侧：probe-domain policy 12 测（含 restore-vs-new 甄别、阈值边界、
  suspect 严格小于）；probe-storage 19 测（三连降级/恢复/history 行数/
  baseline 跳过）；probe-engine 67 测（guard 三态、fold 聚合、converge 交接）。

## 风险与回滚
- 纯函数移植若语义漂移，双跑比对（13）会拦截；默认实现不切换。

## 阻塞项
- 无。

## 下一步
- R8：PublishDecision（suspect 轮保留上轮发布的执行面）。
