-- 0015: 轮末聚合摘要落列（workstreams/34 R4，G-3 探测点面板的诚实口径）。
--
-- `run_round_with_ctx` 的轮末聚合消费产出轮级摘要（completed/missing/
-- offline/unresolved probes + strategy/minimum），finish 后由调用方写进
-- 本列；status 端点读最近一轮的摘要给面板。NULL = 兼容模式（无探测点）。
--
-- 追加式：旧实现可忽略；不碰既有列。

ALTER TABLE rounds ADD COLUMN probe_summary TEXT;
