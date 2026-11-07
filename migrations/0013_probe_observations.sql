-- 0013: 多探测点观察面（G-3，HK3 schema v3 §12，SQL 与 mihomo_test/db.py
-- 的 V3_SCHEMA 逐字同构——同一账本两种实现的行必须可互换）。
--
-- 追加式：旧版可以忽略这三张表，回滚不需要 down migration；既有
-- nodes/rounds/results 一律不重建、不清空（§7.8：升级不得清空 nodes、
-- consec_fail、最后成功时间或历史轮次）。
--
-- * probes —— 探测点注册表（心跳/状态/版本）；
-- * probe_observations —— 每个探测点对每个节点每轮的原始观测，
--   PK (round_id, probe_id, fingerprint) + INSERT OR REPLACE = 重复上报
--   幂等覆盖（§2）；节点仍是聚合后的唯一状态账本，此表不参与死亡计数；
-- * publish_audit —— 人工推送三件套（G-4）与发布动作的审计行。

CREATE TABLE IF NOT EXISTS probes (
  probe_id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  last_seen_at TEXT,
  version TEXT,
  state TEXT NOT NULL DEFAULT 'unknown'
);

CREATE TABLE IF NOT EXISTS probe_observations (
  round_id TEXT NOT NULL,
  probe_id TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  alive INTEGER,
  delay_ms INTEGER,
  reason TEXT,
  egress_country TEXT,
  received_at TEXT NOT NULL,
  payload_digest TEXT NOT NULL,
  PRIMARY KEY (round_id, probe_id, fingerprint)
);

CREATE TABLE IF NOT EXISTS publish_audit (
  audit_id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  target TEXT NOT NULL,
  revision TEXT,
  digest TEXT NOT NULL,
  actor TEXT NOT NULL,
  status TEXT NOT NULL,
  summary_json TEXT,
  created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_probe_observations_round
    ON probe_observations(round_id, probe_id, fingerprint);
CREATE INDEX IF NOT EXISTS idx_publish_audit_created
    ON publish_audit(created_at);

-- dispatch 侧签发 Job 时写入的六段配置 digest（probe_dispatch::config_digest）；
-- 结果端点在列非空时强制比对（DIGEST_MISMATCH），空 = 尚未启用 dispatch 的
-- 部署（Python expected_digest=None 的重算口径由 dispatch 落列后启用）。
ALTER TABLE rounds ADD COLUMN probe_config_digest TEXT;
