//! The probe plane's ledger access (G-3/G-4, HK3 §12; `mihomo_test/db.py`
//! 最小访问函数的 Rust 镜像)。
//!
//! `nodes` 仍是聚合后的唯一状态账本：`probe_observations` 只做多探测点原始
//! 观测，不参与死亡计数推进；`probes` 是注册表/心跳；`publish_audit` 是
//! 人工推送与发布动作的审计行。三张表都来自 migration 0013，与 Python
//! `V3_SCHEMA` 逐字同构——同一账本两种实现的行可互换。

use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use crate::{utc_now, DomainError, DomainResult, Storage};

/// One registered probe row (`probes` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProbeRow {
    pub probe_id: String,
    pub name: String,
    pub enabled: bool,
    pub last_seen_at: Option<String>,
    pub version: Option<String>,
    pub state: String,
}

/// One stored observation (`probe_observations` table), probe-then-fingerprint
/// order like the Python `get_observations`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObservationRow {
    /// The rounds row id as text (the Python ledger stores TEXT there).
    pub round_id: String,
    pub probe_id: String,
    pub fingerprint: String,
    /// `None` = the probe answered but gave no verdict.
    pub alive: Option<bool>,
    pub delay_ms: Option<i64>,
    pub reason: Option<String>,
    pub egress_country: Option<String>,
    pub received_at: String,
    pub payload_digest: String,
}

impl Storage {
    /// Register or refresh a probe row. `None` fields mean "leave unchanged"
    /// on an existing row, so re-syncing the config cannot reset a probe's
    /// online state or blank its reported version (Python `upsert_probe`).
    pub fn upsert_probe(
        &self,
        probe_id: &str,
        name: &str,
        enabled: bool,
        last_seen_at: Option<&str>,
        version: Option<&str>,
        state: Option<&str>,
    ) -> DomainResult<()> {
        let mut sets = vec!["name=?".to_string(), "enabled=?".to_string()];
        let mut values: Vec<Box<dyn rusqlite::types::ToSql>> =
            vec![Box::new(name.to_string()), Box::new(enabled)];
        if let Some(seen) = last_seen_at {
            sets.push("last_seen_at=?".to_string());
            values.push(Box::new(seen.to_string()));
        }
        if let Some(version) = version {
            sets.push("version=?".to_string());
            values.push(Box::new(version.to_string()));
        }
        if let Some(state) = state {
            sets.push("state=?".to_string());
            values.push(Box::new(state.to_string()));
        }
        values.push(Box::new(probe_id.to_string()));
        let refs: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(Box::as_ref).collect();
        let updated = self
            .conn
            .execute(
                &format!("UPDATE probes SET {} WHERE probe_id=?", sets.join(", ")),
                refs.as_slice(),
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        if updated > 0 {
            return Ok(());
        }
        self.conn
            .execute(
                "INSERT INTO probes(probe_id, name, enabled, last_seen_at, version, state) VALUES(?,?,?,?,?,COALESCE(?, 'unknown'))",
                params![
                    probe_id,
                    name,
                    enabled,
                    last_seen_at.map(str::to_string).unwrap_or_else(utc_now),
                    version,
                    state
                ],
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Heartbeat: stamp `last_seen_at`, optionally recording version/state.
    /// Returns the rows updated: 0 means the probe is not registered — the
    /// caller treats that as "unknown probe" (audit only, never a node
    /// failure, §4.2).
    pub fn touch_probe_seen(
        &self,
        probe_id: &str,
        version: Option<&str>,
        state: Option<&str>,
    ) -> DomainResult<usize> {
        let mut sets = vec!["last_seen_at=?".to_string()];
        let mut values: Vec<Box<dyn rusqlite::types::ToSql>> =
            vec![Box::new(utc_now().to_string())];
        if let Some(version) = version {
            sets.push("version=?".to_string());
            values.push(Box::new(version.to_string()));
        }
        if let Some(state) = state {
            sets.push("state=?".to_string());
            values.push(Box::new(state.to_string()));
        }
        values.push(Box::new(probe_id.to_string()));
        let refs: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(Box::as_ref).collect();
        self.conn
            .execute(
                &format!("UPDATE probes SET {} WHERE probe_id=?", sets.join(", ")),
                refs.as_slice(),
            )
            .map_err(|e| DomainError::Storage(e.to_string()))
    }

    /// Every registered probe, id order.
    pub fn list_probes(&self) -> DomainResult<Vec<ProbeRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT probe_id, name, enabled, last_seen_at, version, state
                 FROM probes ORDER BY probe_id",
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ProbeRow {
                    probe_id: row.get(0)?,
                    name: row.get(1)?,
                    enabled: row.get::<_, i64>(2)? != 0,
                    last_seen_at: row.get(3)?,
                    version: row.get(4)?,
                    state: row.get(5)?,
                })
            })
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::Storage(e.to_string()))
    }

    /// Idempotently store one probe's verdict for one node this round. The
    /// PK (round_id, probe_id, fingerprint) plus INSERT OR REPLACE means a
    /// re-reported result overwrites instead of duplicating (§2).
    #[allow(clippy::too_many_arguments)]
    pub fn record_observation(
        &self,
        round_id: &str,
        probe_id: &str,
        fingerprint: &str,
        alive: Option<bool>,
        delay_ms: Option<i64>,
        reason: Option<&str>,
        egress_country: Option<&str>,
        payload_digest: &str,
    ) -> DomainResult<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO probe_observations(round_id, probe_id, fingerprint, alive, delay_ms, reason, egress_country, received_at, payload_digest) VALUES(?,?,?,?,?,?,?,?,?)",
                params![
                    round_id,
                    probe_id,
                    fingerprint,
                    alive,
                    delay_ms,
                    reason,
                    egress_country,
                    utc_now(),
                    payload_digest,
                ],
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Every probe's observation for one round, probe then fingerprint order
    /// (the input `aggregation::filter_observations` consumes).
    pub fn get_observations(&self, round_id: &str) -> DomainResult<Vec<ObservationRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT round_id, probe_id, fingerprint, alive, delay_ms, reason, egress_country, received_at, payload_digest FROM probe_observations WHERE round_id=? ORDER BY probe_id, fingerprint",
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let rows = stmt
            .query_map(params![round_id], |row| {
                Ok(ObservationRow {
                    round_id: row.get(0)?,
                    probe_id: row.get(1)?,
                    fingerprint: row.get(2)?,
                    alive: row.get::<_, Option<i64>>(3)?.map(|alive| alive != 0),
                    delay_ms: row.get(4)?,
                    reason: row.get(5)?,
                    egress_country: row.get(6)?,
                    received_at: row.get(7)?,
                    payload_digest: row.get(8)?,
                })
            })
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::Storage(e.to_string()))
    }

    /// Append one publish audit row (G-4; `publish_audit` 表).
    #[allow(clippy::too_many_arguments)]
    pub fn record_publish_audit(
        &self,
        audit_id: &str,
        kind: &str,
        target: &str,
        revision: Option<&str>,
        digest: &str,
        actor: &str,
        status: &str,
        summary_json: Option<&str>,
    ) -> DomainResult<()> {
        self.conn
            .execute(
                "INSERT INTO publish_audit(audit_id, kind, target, revision, digest, actor, status, summary_json, created_at) VALUES(?,?,?,?,?,?,?,?,?)",
                params![
                    audit_id,
                    kind,
                    target,
                    revision,
                    digest,
                    actor,
                    status,
                    summary_json,
                    utc_now()
                ],
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Newest publish audit rows first, bounded.
    pub fn list_publish_audit(&self, limit: usize) -> DomainResult<Vec<serde_json::Value>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT audit_id, kind, target, revision, digest, actor, status, summary_json, created_at FROM publish_audit ORDER BY created_at DESC, audit_id DESC LIMIT ?",
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                let summary: Option<String> = row.get(7)?;
                let summary_value =
                    summary.and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
                Ok(serde_json::json!({
                    "audit_id": row.get::<_, String>(0)?,
                    "kind": row.get::<_, String>(1)?,
                    "target": row.get::<_, String>(2)?,
                    "revision": row.get::<_, Option<String>>(3)?,
                    "digest": row.get::<_, String>(4)?,
                    "actor": row.get::<_, String>(5)?,
                    "status": row.get::<_, String>(6)?,
                    "summary": summary_value,
                    "created_at": row.get::<_, String>(8)?,
                }))
            })
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| DomainError::Storage(e.to_string()))
    }
}

impl Storage {
    /// 一轮的结果回传门禁所需的三态：存在 / 已结算 / 最新 id。
    /// 返回 (exists, finished, newest_id)。
    pub fn probe_round_state(&self, round_id: i64) -> DomainResult<(bool, bool, i64)> {
        let exists = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM rounds WHERE id=?",
                params![round_id],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?
            > 0;
        let newest = self
            .conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM rounds", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        let finished = if exists {
            self.conn
                .query_row(
                    "SELECT finished_at IS NOT NULL FROM rounds WHERE id=?",
                    params![round_id],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(|e| DomainError::Storage(e.to_string()))?
                != 0
        } else {
            false
        };
        Ok((exists, finished, newest))
    }

    /// dispatch 侧为该轮登记的配置 digest（列空 = 尚未启用 dispatch）。
    pub fn round_config_digest(&self, round_id: i64) -> DomainResult<Option<String>> {
        let digest = self
            .conn
            .query_row(
                "SELECT probe_config_digest FROM rounds WHERE id=?",
                params![round_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        Ok(digest.filter(|d| !d.is_empty()))
    }

    /// dispatch 侧登记：写 `rounds.probe_config_digest`（幂等覆盖）。
    pub fn set_round_config_digest(&self, round_id: i64, digest: &str) -> DomainResult<()> {
        self.conn
            .execute(
                "UPDATE rounds SET probe_config_digest=? WHERE id=?",
                params![digest, round_id],
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        Ok(())
    }
}

impl Storage {
    /// dispatch 侧登记：本轮该探测点的回传截止（幂等覆盖，PK probe+round）。
    pub fn note_probe_deadline(
        &self,
        probe_id: &str,
        round_id: &str,
        deadline: &str,
    ) -> DomainResult<()> {
        self.conn
            .execute(
                "INSERT INTO probe_deadlines(probe_id, round_id, deadline) VALUES(?,?,?)\
                 ON CONFLICT(probe_id, round_id) DO UPDATE SET deadline=excluded.deadline",
                params![probe_id, round_id, deadline],
            )
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        Ok(())
    }

    /// 该探测点本轮登记的截止时间；None = 未登记（无诚实 deadline 可 enforce）。
    pub fn probe_deadline(&self, probe_id: &str, round_id: &str) -> DomainResult<Option<String>> {
        let deadline = self
            .conn
            .query_row(
                "SELECT deadline FROM probe_deadlines WHERE probe_id=? AND round_id=?",
                params![probe_id, round_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| DomainError::Storage(e.to_string()))?;
        Ok(deadline.filter(|deadline| !deadline.is_empty()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(row: &ObservationRow) -> (String, String, Option<bool>) {
        (row.probe_id.clone(), row.fingerprint.clone(), row.alive)
    }

    /// 注册表 upsert 的「只动传了的字段」语义：重同步不清心跳/状态。
    #[test]
    fn upsert_probe_never_resets_seen_state() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_probe(
                "p1",
                "hk3",
                true,
                Some("2026-11-08T00:00:00"),
                Some("v1"),
                Some("online"),
            )
            .unwrap();
        // 重新同步（不带心跳/状态字段）。
        storage
            .upsert_probe("p1", "hk3-renamed", false, None, None, None)
            .unwrap();
        let probes = storage.list_probes().unwrap();
        assert_eq!(probes.len(), 1);
        assert_eq!(probes[0].name, "hk3-renamed");
        assert!(!probes[0].enabled);
        assert_eq!(
            probes[0].last_seen_at.as_deref(),
            Some("2026-11-08T00:00:00")
        );
        assert_eq!(probes[0].state, "online");
    }

    /// 心跳只对已注册探测点生效：0 = unknown probe（审计口径，非节点失败）。
    #[test]
    fn a_heartbeat_on_an_unknown_probe_touches_nothing() {
        let storage = Storage::open_in_memory().unwrap();
        assert_eq!(storage.touch_probe_seen("ghost", None, None).unwrap(), 0);
        storage
            .upsert_probe("p1", "hk3", true, None, None, None)
            .unwrap();
        assert_eq!(
            storage
                .touch_probe_seen("p1", Some("v2"), Some("online"))
                .unwrap(),
            1
        );
        let probes = storage.list_probes().unwrap();
        assert_eq!(probes[0].version.as_deref(), Some("v2"));
        assert_eq!(probes[0].state, "online");
        assert!(!probes[0].last_seen_at.as_deref().unwrap_or("").is_empty());
    }

    /// 观测的幂等覆盖 + 轮内读取顺序（probe → fingerprint）。
    #[test]
    fn observations_overwrite_and_read_back_ordered() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .record_observation(
                "7",
                "p2",
                "fp-b",
                Some(false),
                None,
                Some("timeout"),
                None,
                "d1",
            )
            .unwrap();
        storage
            .record_observation(
                "7",
                "p1",
                "fp-a",
                Some(true),
                Some(120),
                None,
                Some("JP"),
                "d1",
            )
            .unwrap();
        // 重复上报：覆盖，不新增。
        storage
            .record_observation(
                "7",
                "p2",
                "fp-b",
                Some(false),
                None,
                Some("refused"),
                None,
                "d1",
            )
            .unwrap();
        let rows = storage.get_observations("7").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(obs(&rows[0]), ("p1".into(), "fp-a".into(), Some(true)));
        assert_eq!(obs(&rows[1]), ("p2".into(), "fp-b".into(), Some(false)));
        assert_eq!(rows[1].reason.as_deref(), Some("refused"));
        // no-verdict 行（alive NULL）原样回来。
        storage
            .record_observation("8", "p1", "fp-c", None, None, None, None, "d1")
            .unwrap();
        let rows = storage.get_observations("8").unwrap();
        assert_eq!(rows[0].alive, None);
        assert!(storage.get_observations("9").unwrap().is_empty());
    }

    /// 发布审计：追加 + 最新在前。
    #[test]
    fn publish_audit_appends_and_reads_newest_first() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .record_publish_audit(
                "a1",
                "manual_push",
                "mihomo-probe-alive",
                None,
                "d",
                "gui",
                "pushed",
                None,
            )
            .unwrap();
        storage
            .record_publish_audit(
                "a2",
                "manual_push",
                "mihomo-probe-alive",
                Some("rev1"),
                "d2",
                "gui",
                "denied",
                Some(r#"{"why":"expired"}"#),
            )
            .unwrap();
        let rows = storage.list_publish_audit(10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["audit_id"], "a2");
        assert_eq!(rows[0]["summary"]["why"], "expired");
        assert_eq!(rows[1]["status"], "pushed");
    }
}
