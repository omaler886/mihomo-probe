//! Core domain types shared by every probe crate.
//!
//! Per the workspace dependency rule (GLM_5.3_Flash §3) this crate must stay
//! free of IO: no web, no database, no network clients, no clock. Types here
//! are the vocabulary the rest of the workspace speaks; behavior that needs
//! the outside world lives above.

use serde::{Deserialize, Serialize};

/// A round row id, mirroring `rounds.id` in the Python SQLite schema.
pub type RoundId = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundStatus {
    /// The round row has no `finished_at` yet.
    Running,
    /// The round closed with a summary.
    Finished,
}

/// What the API and the CLI report about one round.
///
/// Field names follow the Python `rounds` table so both implementations can
/// read the same rows during the shadow-comparison phase (workstreams/13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundSummary {
    pub round_id: RoundId,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    pub trigger: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default)]
    pub ok: i64,
    #[serde(default)]
    pub total: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("storage error: {0}")]
    Storage(String),
    #[error("kernel error: {0}")]
    Kernel(String),
    #[error("config error: {0}")]
    Config(String),
}

pub type DomainResult<T> = Result<T, DomainError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_summary_roundtrips_through_json() {
        let summary = RoundSummary {
            round_id: 7,
            started_at: "2026-09-30T12:00:00".into(),
            finished_at: Some("2026-09-30T12:03:00".into()),
            trigger: "manual".into(),
            mode: Some("chain".into()),
            note: None,
            ok: 12,
            total: 30,
        };
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("\"round_id\":7"));
        assert!(!json.contains("null"), "None fields must be omitted");
        let back: RoundSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back, summary);
    }

    #[test]
    fn domain_error_is_displayable() {
        assert_eq!(
            DomainError::Kernel("timeout".into()).to_string(),
            "kernel error: timeout"
        );
    }
}
