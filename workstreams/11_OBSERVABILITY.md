# 可观测性

## 目标
结构化 tracing + Prometheus 指标，无凭据泄漏（总控 §14）。

## 当前实现审计
- 日志：db.events 表（level+message，滚动 2000），面板 /api/logs；round.state.json 文件（phase/round_id/pid/ts/mode，运维 cat 用）；ipmap 走 stdout。
- 日志卫生：db.log 调用点只含节点名/计数/失败原因；异常文本有界截断（str(exc)[:120~200]）；无订阅正文/token 入日志（SECURITY_REVIEW §C 复核）。
- 指标：无 Prometheus 端点——R9 新建 probe_observability。
- 追踪字段（目标）：round_id/source_id/node_fingerprint/stage/failure_kind/duration_ms；禁止节点名/完整 IP/高基数字段入 label。

## 待办清单
- [x] R1：tracing-subscriber 初始化（切片）
- [ ] R9：指标族 probe_round_total 等 11 项 + /metrics
- [ ] R9：round 生命周期 span

## 下一步
- R9。
