-- 0014: dispatch 的 deadline 登记表（workstreams/32，G-3 §4.2/§14 集成场景）。
--
-- dispatch 侧派发 Job 时登记「本轮该探测点的回传截止」；结果端点据此对
-- 超时回传给 RESULT_LATE（audit-only）。Python 把同一张表持久化在
-- deadline.json（probe_dispatch._deadlines_path）；SQLite 化让它与账本
-- 同生命周期：Controller 重启不丢截止时间，丢失的回传仍能在下一轮前被
-- 诚实拒绝。
--
-- 追加式：旧实现可忽略这张表；不碰 nodes/rounds/results。

CREATE TABLE IF NOT EXISTS probe_deadlines (
  probe_id TEXT NOT NULL,
  round_id TEXT NOT NULL,
  deadline TEXT NOT NULL,
  PRIMARY KEY (probe_id, round_id)
);
