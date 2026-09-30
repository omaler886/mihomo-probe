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

## 待办清单
- [ ] R1：切片内 RoundState/RoundId 最小模型
- [ ] R2：完整 NodeState/policy.apply 移植 + 与 Python 对拍测试
- [ ] R7：整轮保护 GuardDecision/PublishDecision

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
