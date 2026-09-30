# 探测引擎

## 目标
完整异步流水线 + 分层检测 + 自适应调度（总控 §7/§8）。

## 当前实现审计（engine.py，2895 行）
- 流水线：collect_entries（enabled 源→entries，fp 唯一定点 _orig_fp）→ chain front 池（三输入：粘贴文本→Sub-Store 物化订阅 / front_pick 名单 / front_source 资源；max_fronts 上限）→ classify_and_expand（双视角 DNS 解析→入口国别批量查→受限 ISP 跳过→每 IP 变体）→ expand_chains（直连/链式双开关、test_plain_nodes 客户端口径、空池 front_dead 语义）→ make_testable（内核校验+裁剪）→ _test_phases（前置先测，活前置才放行链式变体）→ test_one（HTTPS 必过才判活；timeout 升级长超时；TERMINAL_REASONS 不耗重试）→ _verify_chain_payload（204 之外的真拉流；front 失败连坐 chain）→ _verify_egress（lanes 并行出口国别）→ 收敛/发布。
- 并发与取消：ThreadPoolExecutor（测试 concurrency、验证 lanes）；deadline 贯穿（每 attempt 前检查 + 每阶段 checkpoint，超预算 RoundTimeout→释放锁→告警）；无 API 级取消（R9 补 CancellationToken）。
- 重试：max_attempts=3，retry_pause 0.3s；按 FailureKind 决定（TERMINAL_REASONS 即停）。
- 整轮保护：见 07 与 policy.round_is_suspect；补前端池全死告警、控制器失联不计 streak。
- 自适应调度：现状=固定 30 分钟 + 手动 direct/chain 模式；L1/L2/L3 分层与按状态差异化周期为新增设计（R5/R9）。

## 待办清单
- [ ] R5：delay 引擎+失败归类（对拍 test_one）
- [ ] R6：出口验证+ipmap 落地映射
- [ ] R5/R9：L1/L2/L3 与自适应周期（ADR 先行）
- [ ] R9：CancellationToken 取消/优雅停机

## 测试证据
- Python 锚点：RetryTest/HttpsVerdictTest/ChainPayloadVerifyTest/ChainRoundTest/RoundBudgetTest 等（tests/test_logic.py）。

## 风险与回滚
- 引擎为最高风险面，切换默认实现前必须 shadow 双跑（13）。

## 下一步
- R5。
