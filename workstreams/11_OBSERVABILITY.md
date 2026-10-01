# 可观测性

## 目标
结构化 tracing + Prometheus 指标，无凭据泄漏（总控 §14）。

## 当前实现审计
- 日志：db.events 表（level+message，滚动 2000），面板 /api/logs；round.state.json 文件（phase/round_id/pid/ts/mode，运维 cat 用）；ipmap 走 stdout。
- 日志卫生：db.log 调用点只含节点名/计数/失败原因；异常文本有界截断（str(exc)[:120~200]）；无订阅正文/token 入日志（SECURITY_REVIEW §C 复核）。
- 指标：无 Prometheus 端点——R9 新建 probe_observability。
- 追踪字段（目标）：round_id/source_id/node_fingerprint/stage/failure_kind/duration_ms；禁止节点名/完整 IP/高基数字段入 label。

## 指标清单（R9，展开原"11 项"为显式定义）
来源：外部 Go 方案 §十，按本仓库命名核对。`/metrics` 暴露，Prometheus 文本格式。

```
# 轮次
probe_round_total
probe_round_duration_seconds
probe_round_inconclusive_total          # 依赖 ADR-0005；未定稿前不实现
# 节点态（Gauge，按当前账本）
probe_nodes_total / probe_nodes_alive / probe_nodes_dead / probe_nodes_unknown
# 测量
probe_measurement_total
probe_measurement_duration_seconds
probe_measurement_failure_total{class}  # class = 现有 14 种 FailureKind
# 数据源
probe_source_fetch_total{source,status}
probe_source_fetch_duration_seconds{source}
# 内核
probe_kernel_reload_total{status}
probe_kernel_reload_duration_seconds
# 发布
probe_publish_total{status}
probe_publish_nodes
# 存储
probe_db_transaction_duration_seconds
# 并发（对应 06 的分层 Limits）
probe_worker_queue_depth{lane}
probe_worker_active{lane}
# 护栏（本仓库补充，服务 03 的 GuardDecision）
probe_guard_decision_total{decision}
```

Label 约束（沿用本文件既有规则，不得放宽）：
- **禁止**节点名、完整 IP、fingerprint、round_id 入 label——高基数会打爆 Prometheus。
- `{source}` 与 `{class}` 允许，因为取值域有界（数据源数量、14 种 FailureKind）。
- `round_id` 只进结构化日志与 span，不进指标 label。

## 待办清单
- [x] R1：tracing-subscriber 初始化（切片）
- [ ] R9：上列指标族 + /metrics 端点（`probe_round_inconclusive_total` 待 ADR-0005）
- [ ] R9：round 生命周期 span

## 下一步
- R9。
