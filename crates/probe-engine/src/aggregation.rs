//! 多探测点观察聚合（G-3，HK3 §4.3 / workstreams/28；`mihomo_test/aggregation.py`
//! 的 Rust 镜像，纯函数：不读数据库、不触网、不读时钟）。
//!
//! Controller 每轮从若干探测点收集同一批节点的观测，按 fingerprint 聚合成
//! **一条**结论。三条不可协商的语义（§4.3）：
//!
//! * 任一已完成探测点 alive → 该节点本轮 alive（`any_alive`）。
//! * 所有已完成探测点均失败、且完成数达到 `minimum_completed_probes` → 失败。
//! * 完成数不足 `minimum_completed_probes` → `inconclusive`，不得推进
//!   consec_fail。
//!
//! 探测点自身的问题——离线、未知 probe_id、digest 不符、超时未回传——表现为
//! 「观测缺失」，因此天然不计入完成数，更不计入节点失败。这条是防止
//! 「探测点越多，节点死得越快」的关键门禁（§4.3 末段）。
//!
//! 本地测量经 [`local_observation`] 包装成 `probe_id="local"` 的同形状观测，
//! 本地单探测点兼容模式与远程多探测点走同一条聚合路径。

use std::collections::{BTreeMap, BTreeSet};

use probe_config::AggregationSection;
use serde_json::Value;

/// Controller 自身测量的保留探测点 id（不算注册表成员，但计入完成数）。
pub const LOCAL_PROBE_ID: &str = "local";

/// 当前唯一聚合策略（§4.3）。
pub const STRATEGY_ANY_ALIVE: &str = "any_alive";

pub const VERDICT_ALIVE: &str = "alive";
pub const VERDICT_FAIL: &str = "fail";
pub const VERDICT_INCONCLUSIVE: &str = "inconclusive";

/// 记在 inconclusive 节点的 `reason` 上。与节点自身的失败原因刻意不同：
/// 本轮对该节点一无所知。
pub const INSUFFICIENT_PROBES: &str = "insufficient_probes";

/// 探测点自身的不可用状态（摘要口径，不影响完成数——离线探测点本来就没有
/// 观测行）。
fn offline(state: &str) -> bool {
    matches!(state, "offline" | "disabled" | "down" | "error")
}

/// 一条规整后的观测行（与 `db.get_observations()` 行 / 远程 ProbeResult 同形）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Observation {
    pub round_id: Option<i64>,
    pub probe_id: String,
    pub fingerprint: String,
    /// `None` = 探测点回答了但没给结论：它自己的不确定，永不计入完成数。
    pub alive: Option<bool>,
    pub delay_ms: Option<i64>,
    pub reason: Option<String>,
    pub egress_country: Option<String>,
    pub payload_digest: Option<String>,
}

/// 把一条存储/回传观测规整成 [`Observation`]；字段容忍字符串布尔与数字。
pub fn normalize_observation(row: &Value, round_id: Option<i64>) -> Observation {
    let get = |key: &str| row.get(key);
    let clean = |key: &str| -> Option<String> {
        get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let as_bool = |key: &str| -> Option<bool> {
        match get(key) {
            None | Some(Value::Null) => None,
            Some(Value::Bool(value)) => Some(*value),
            Some(Value::Number(value)) => Some(value.as_f64().unwrap_or(0.0) != 0.0),
            Some(Value::String(text)) => Some(matches!(
                text.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "y" | "alive" | "ok"
            )),
            Some(_) => None,
        }
    };
    let as_int = |key: &str| -> Option<i64> {
        get(key).and_then(|value| value.as_i64().or_else(|| value.as_f64().map(|f| f as i64)))
    };
    let row_round = get("round_id").and_then(Value::as_i64).or(round_id);
    Observation {
        round_id: row_round,
        probe_id: clean("probe_id").unwrap_or_default(),
        fingerprint: clean("fingerprint").unwrap_or_default(),
        alive: as_bool("alive"),
        delay_ms: as_int("delay_ms"),
        reason: clean("reason"),
        egress_country: clean("egress_country"),
        payload_digest: clean("payload_digest"),
    }
}

/// 把 Controller 自己的测量包装成一条观测（`probe_id="local"`），使本地单
/// 探测点与远程多探测点共用同一聚合路径。
#[allow(clippy::too_many_arguments)]
pub fn local_observation(
    round_id: i64,
    fingerprint: &str,
    alive: Option<bool>,
    delay_ms: Option<i64>,
    reason: Option<&str>,
    egress_country: Option<&str>,
    payload_digest: &str,
) -> Observation {
    Observation {
        round_id: Some(round_id),
        probe_id: LOCAL_PROBE_ID.to_string(),
        fingerprint: fingerprint.to_string(),
        alive,
        delay_ms,
        reason: reason.map(str::to_string),
        egress_country: egress_country.map(str::to_string),
        payload_digest: (!payload_digest.is_empty()).then(|| payload_digest.to_string()),
    }
}

/// 一条被拒观测及其原因（稳定拼写，进审计事件）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub observation: Observation,
    pub why: &'static str,
}

/// [`filter_observations`] 的三槽结果。
#[derive(Debug, Clone, Default)]
pub struct FilterOutcome {
    pub accepted: Vec<Observation>,
    pub rejected: Vec<Rejected>,
    /// 摘要层无法复核但行本身有效的观测（digest 门禁属于 payload 所在层）。
    pub deferred: Vec<Rejected>,
}

/// 丢弃绝不能进账本的观测（§4.2）。被拒行保留解析形态 + `why`，调用方以
/// 事件审计，绝不静默丢弃。没有 `payload_digest` 的行不因 digest 被拒——
/// digest 检查抓的是「探测点在回答一个它没见过的配置」，空 digest 是调用方
/// 本来就没有可比对象。
pub fn filter_observations(
    observations: &[Value],
    round_id: Option<i64>,
    known_probes: Option<&BTreeSet<String>>,
    config_digest: Option<&str>,
    digest_gate: Option<&str>,
) -> FilterOutcome {
    let mut outcome = FilterOutcome::default();
    for raw in observations {
        let row = normalize_observation(raw, round_id);
        let mut why = None;
        let mut defer = false;
        if round_id.is_some_and(|round| row.round_id.is_some_and(|row_round| row_round != round)) {
            why = Some("stale_round");
        } else if row.probe_id.is_empty() || row.fingerprint.is_empty() {
            why = Some("malformed");
        } else if let Some(known) = known_probes {
            if !known.contains(&row.probe_id) {
                why = Some("unknown_probe");
            }
        }
        if why.is_none() {
            if let (Some(config), Some(payload)) = (config_digest, row.payload_digest.as_deref()) {
                if !config.is_empty() && payload != config {
                    if digest_gate.is_some() {
                        defer = true;
                    } else {
                        why = Some("digest_mismatch");
                    }
                }
            }
        }
        match why {
            None => {
                if defer {
                    outcome.deferred.push(Rejected {
                        observation: row,
                        why: "digest_deferred",
                    });
                } else {
                    outcome.accepted.push(row);
                }
            }
            Some(why) => outcome.rejected.push(Rejected {
                observation: row,
                why,
            }),
        }
    }
    outcome
}

/// 一个 fingerprint 的聚合结论。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct NodeAggregate {
    pub verdict: &'static str,
    pub alive: bool,
    pub delay_ms: Option<i64>,
    pub egress_country: Option<String>,
    pub reason: Option<String>,
    pub best_probe: Option<String>,
    pub completed_probes: usize,
    pub minimum_completed_probes: usize,
    /// 每个已回答探测点的结论（alive/fail）。
    pub probes: BTreeMap<String, &'static str>,
}

/// 轮级摘要（探测点面板的诚实口径）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RoundSummary {
    pub strategy: String,
    pub minimum_completed_probes: usize,
    pub completed_probes: Vec<String>,
    pub missing_probes: Vec<String>,
    pub offline_probes: Vec<String>,
    pub unresolved_probes: Vec<String>,
    pub alive: Vec<String>,
    pub failed: Vec<String>,
    pub inconclusive: Vec<String>,
    pub nodes: usize,
}

/// 一轮的聚合结果：`{fingerprint: 结论}` + 轮级摘要。
#[derive(Debug, Clone)]
pub struct RoundAggregate {
    pub nodes: BTreeMap<String, NodeAggregate>,
    pub summary: RoundSummary,
}

impl RoundAggregate {
    /// 某指纹的结论；`None` = 本轮对它一无所知（调用方保留自己的路径，
    /// 绝不发明结论）。
    pub fn observation_for(&self, fingerprint: &str) -> Option<&NodeAggregate> {
        self.nodes.get(fingerprint)
    }
}

/// 把一轮的全部观测（本地 + 远程）折叠成每个指纹一条结论。
pub fn aggregate_round(
    observations: &[Value],
    aggregation: &AggregationSection,
    probe_states: &BTreeMap<String, String>,
) -> RoundAggregate {
    let minimum = aggregation.minimum_completed_probes.max(0) as usize;
    let strategy = if aggregation.strategy.trim().is_empty() {
        STRATEGY_ANY_ALIVE.to_string()
    } else {
        aggregation.strategy.trim().to_ascii_lowercase()
    };

    let mut by_fp: BTreeMap<String, Vec<Observation>> = BTreeMap::new();
    let mut unresolved: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for raw in observations {
        let row = normalize_observation(raw, None);
        if row.fingerprint.is_empty() {
            continue;
        }
        if row.alive.is_none() {
            // 探测点回答了但没给结论：它自己的不确定，永不计入完成数（§7.7）。
            unresolved
                .entry(row.fingerprint.clone())
                .or_default()
                .push(row.probe_id);
            continue;
        }
        by_fp.entry(row.fingerprint.clone()).or_default().push(row);
    }

    let mut nodes: BTreeMap<String, NodeAggregate> = BTreeMap::new();
    let mut fingerprints: BTreeSet<String> = by_fp.keys().cloned().collect();
    fingerprints.extend(unresolved.keys().cloned());
    for fingerprint in fingerprints {
        let Some(rows) = by_fp.get(&fingerprint) else {
            // 只有 no-verdict 行：听见了探测点但一无所知 → inconclusive，
            // 绝不是「缺席」——缺席会让调用方发明自己的结论。
            nodes.insert(
                fingerprint,
                NodeAggregate {
                    verdict: VERDICT_INCONCLUSIVE,
                    alive: false,
                    delay_ms: None,
                    egress_country: None,
                    reason: Some(INSUFFICIENT_PROBES.to_string()),
                    best_probe: None,
                    completed_probes: 0,
                    minimum_completed_probes: minimum,
                    probes: BTreeMap::new(),
                },
            );
            continue;
        };
        // 确定性顺序：失败节点的代表原因绝不依赖探测点回答的先后。
        let mut rows = rows.clone();
        rows.sort_by(|left, right| {
            (&left.probe_id, left.delay_ms.unwrap_or(0))
                .cmp(&(&right.probe_id, right.delay_ms.unwrap_or(0)))
        });
        let completed = rows.len();
        let alive_rows: Vec<&Observation> =
            rows.iter().filter(|row| row.alive == Some(true)).collect();
        let node = if !alive_rows.is_empty() {
            // any_alive 与 §4.3 的规则顺序：一个探测点说活就是活，先于
            // minimum_completed_probes——最小完成数拦的是「凭太少的证据
            // 判死」，不是「凭一条证据判活」。
            let best = alive_rows
                .into_iter()
                .min_by_key(|row| (row.delay_ms.unwrap_or(i64::MAX), row.probe_id.clone()))
                .expect("non-empty alive rows");
            NodeAggregate {
                verdict: VERDICT_ALIVE,
                alive: true,
                delay_ms: best.delay_ms,
                egress_country: best.egress_country.clone(),
                reason: None,
                best_probe: Some(best.probe_id.clone()),
                completed_probes: completed,
                minimum_completed_probes: minimum,
                probes: rows
                    .iter()
                    .map(|row| {
                        (
                            row.probe_id.clone(),
                            if row.alive == Some(true) {
                                VERDICT_ALIVE
                            } else {
                                VERDICT_FAIL
                            },
                        )
                    })
                    .collect(),
            }
        } else {
            let verdict = if completed >= minimum {
                VERDICT_FAIL
            } else {
                VERDICT_INCONCLUSIVE
            };
            // 代表失败 = probe_id 序里第一个失败探测点：跨轮稳定，面板的
            // 失败原因列不在探测点之间来回跳。
            let first = &rows[0];
            NodeAggregate {
                verdict,
                alive: false,
                delay_ms: None,
                egress_country: rows.iter().find_map(|row| row.egress_country.clone()),
                reason: Some(if verdict == VERDICT_FAIL {
                    first.reason.clone().unwrap_or_default()
                } else {
                    INSUFFICIENT_PROBES.to_string()
                })
                .filter(|reason| !reason.is_empty()),
                best_probe: Some(first.probe_id.clone()),
                completed_probes: completed,
                minimum_completed_probes: minimum,
                probes: rows
                    .iter()
                    .map(|row| {
                        (
                            row.probe_id.clone(),
                            if row.alive == Some(true) {
                                VERDICT_ALIVE
                            } else {
                                VERDICT_FAIL
                            },
                        )
                    })
                    .collect(),
            }
        };
        nodes.insert(fingerprint, node);
    }

    let mut completed_probes: BTreeSet<String> = BTreeSet::new();
    for rows in by_fp.values() {
        for row in rows {
            completed_probes.insert(row.probe_id.clone());
        }
    }
    let mut expected: Vec<String> = probe_states
        .iter()
        .filter(|(_, state)| !offline(state))
        .map(|(probe_id, _)| probe_id.clone())
        .collect();
    expected.sort();
    let offline_probes: Vec<String> = probe_states
        .iter()
        .filter(|(_, state)| offline(state))
        .map(|(probe_id, _)| probe_id.clone())
        .collect();
    let mut unresolved_probes: BTreeSet<String> = BTreeSet::new();
    for probes in unresolved.values() {
        for probe_id in probes {
            unresolved_probes.insert(probe_id.clone());
        }
    }
    let summary = RoundSummary {
        strategy,
        minimum_completed_probes: minimum,
        completed_probes: completed_probes.iter().cloned().collect(),
        missing_probes: expected
            .into_iter()
            .filter(|probe_id| !completed_probes.contains(probe_id))
            .collect(),
        offline_probes,
        unresolved_probes: unresolved_probes.into_iter().collect(),
        alive: nodes
            .iter()
            .filter(|(_, node)| node.verdict == VERDICT_ALIVE)
            .map(|(fp, _)| fp.clone())
            .collect(),
        failed: nodes
            .iter()
            .filter(|(_, node)| node.verdict == VERDICT_FAIL)
            .map(|(fp, _)| fp.clone())
            .collect(),
        inconclusive: nodes
            .iter()
            .filter(|(_, node)| node.verdict == VERDICT_INCONCLUSIVE)
            .map(|(fp, _)| fp.clone())
            .collect(),
        nodes: nodes.len(),
    };
    RoundAggregate { nodes, summary }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(minimum: i64) -> AggregationSection {
        AggregationSection {
            strategy: STRATEGY_ANY_ALIVE.to_string(),
            minimum_completed_probes: minimum,
            late_result_seconds: 90,
            interval_minutes: 30,
        }
    }

    fn obs(probe_id: &str, fingerprint: &str, alive: Value) -> Value {
        json!({"probe_id": probe_id, "fingerprint": fingerprint, "alive": alive})
    }

    /// 三条语义的正例：任一活即活；全部失败且足数判败；不足数 inconclusive。
    #[test]
    fn the_three_semantics_hold() {
        let states = BTreeMap::from([
            ("p1".to_string(), "online".to_string()),
            ("p2".to_string(), "online".to_string()),
        ]);
        // 任一活即活（即使另一个探测点失败、完成数只有 1 < minimum=2）。
        let aggregate = aggregate_round(
            &[
                obs("p1", "fp-a", json!(true)),
                obs("p2", "fp-a", json!(false)),
            ],
            &cfg(2),
            &states,
        );
        let node = aggregate.observation_for("fp-a").unwrap();
        assert_eq!(node.verdict, VERDICT_ALIVE);
        assert_eq!(node.best_probe.as_deref(), Some("p1"));

        // 全部失败且足数 → fail。
        let aggregate = aggregate_round(
            &[
                obs("p1", "fp-b", json!(false)),
                obs("p2", "fp-b", json!(false)),
            ],
            &cfg(2),
            &states,
        );
        let node = aggregate.observation_for("fp-b").unwrap();
        assert_eq!(node.verdict, VERDICT_FAIL);
        // 观测行没带 reason：代表失败原因如实为 None（Python first["reason"]）。
        assert_eq!(node.reason.as_deref(), None);

        // 全部失败但不足数 → inconclusive + insufficient_probes。
        let aggregate = aggregate_round(&[obs("p1", "fp-c", json!(false))], &cfg(2), &states);
        let node = aggregate.observation_for("fp-c").unwrap();
        assert_eq!(node.verdict, VERDICT_INCONCLUSIVE);
        assert_eq!(node.reason.as_deref(), Some(INSUFFICIENT_PROBES));
    }

    /// no-verdict 行不计入完成数；只有 no-verdict 行的节点是 inconclusive。
    #[test]
    fn a_no_verdict_row_never_counts_toward_completion() {
        let states = BTreeMap::from([("p1".to_string(), "online".to_string())]);
        let aggregate = aggregate_round(&[obs("p1", "fp-a", Value::Null)], &cfg(1), &states);
        let node = aggregate.observation_for("fp-a").unwrap();
        assert_eq!(node.verdict, VERDICT_INCONCLUSIVE);
        assert_eq!(node.completed_probes, 0);
        assert_eq!(aggregate.summary.unresolved_probes, vec!["p1"]);
    }

    /// 确定性：失败代表原因按 probe_id 序，不随回答顺序变。
    #[test]
    fn the_representative_failure_is_deterministic() {
        let states = BTreeMap::new();
        let flipped = [
            obs("zeta", "fp", json!(false)),
            obs("alpha", "fp", json!(false)),
        ];
        let straight = [
            obs("alpha", "fp", json!(false)),
            obs("zeta", "fp", json!(false)),
        ];
        let first = aggregate_round(&flipped, &cfg(1), &states);
        let second = aggregate_round(&straight, &cfg(1), &states);
        assert_eq!(
            first.observation_for("fp").unwrap().best_probe,
            second.observation_for("fp").unwrap().best_probe
        );
        assert_eq!(first.summary.failed, vec!["fp"]);
    }

    /// 摘要的诚实口径：missing = 注册表里在 answering 集合之外的非离线成员。
    #[test]
    fn the_summary_names_missing_and_offline_probes() {
        let states = BTreeMap::from([
            ("p1".to_string(), "online".to_string()),
            ("p2".to_string(), "offline".to_string()),
            ("p3".to_string(), "online".to_string()),
        ]);
        let aggregate = aggregate_round(&[obs("p1", "fp", json!(true))], &cfg(1), &states);
        assert_eq!(aggregate.summary.completed_probes, vec!["p1"]);
        assert_eq!(aggregate.summary.missing_probes, vec!["p3"]);
        assert_eq!(aggregate.summary.offline_probes, vec!["p2"]);
    }

    /// 过滤：旧轮次 / malformed / 未知探测点 / digest 不符各归其位；
    /// 空 digest 不拒；带 gate 的 digest 不符进 deferred。
    #[test]
    fn the_filter_routes_every_rejection_class() {
        let known = BTreeSet::from(["p1".to_string()]);
        let rows = [
            json!({"probe_id": "p1", "fingerprint": "fp", "alive": true, "round_id": 5, "payload_digest": "d1"}),
            json!({"probe_id": "p1", "fingerprint": "fp", "alive": true, "round_id": 4}),
            json!({"probe_id": "", "fingerprint": "fp", "alive": true}),
            json!({"probe_id": "pX", "fingerprint": "fp", "alive": true}),
            json!({"probe_id": "p1", "fingerprint": "fp", "alive": true, "payload_digest": "other"}),
            json!({"probe_id": "p1", "fingerprint": "fp", "alive": true, "payload_digest": ""}),
        ];
        let outcome = filter_observations(&rows, Some(5), Some(&known), Some("d1"), None);
        assert_eq!(
            outcome.accepted.len(),
            2,
            "current round + empty digest accepted"
        );
        let whys: Vec<&str> = outcome.rejected.iter().map(|r| r.why).collect();
        assert_eq!(
            whys,
            vec![
                "stale_round",
                "malformed",
                "unknown_probe",
                "digest_mismatch"
            ]
        );

        // digest 门禁属于 payload 层时：不符行 deferred（有效，仅不比列）。
        let outcome = filter_observations(&rows, Some(5), Some(&known), Some("d1"), Some("engine"));
        // accepted = 本轮两条（含空 digest）；digest 不符的 1 条 deferred；
        // 其余 3 条照旧拒绝。
        assert_eq!(outcome.accepted.len(), 2);
        assert_eq!(outcome.deferred.len(), 1);
        assert_eq!(outcome.deferred[0].why, "digest_deferred");
        assert_eq!(outcome.rejected.len(), 3);
    }

    /// 本地观测包装与规整互逆：本地单探测点与远程多探测点同一条路。
    #[test]
    fn the_local_observation_shares_the_aggregation_path() {
        let local = local_observation(9, "fp-1", Some(true), Some(120), None, Some("JP"), "");
        let as_value = serde_json::to_value(&local).unwrap();
        let states = BTreeMap::new();
        let aggregate = aggregate_round(&[as_value], &cfg(1), &states);
        assert_eq!(aggregate.summary.completed_probes, vec![LOCAL_PROBE_ID]);
        assert_eq!(
            aggregate.observation_for("fp-1").unwrap().verdict,
            VERDICT_ALIVE
        );
        assert_eq!(aggregate.summary.alive, vec!["fp-1"]);
    }

    /// 字符串布尔与 0/1 数字都读成结论（存储行 0/1 与 JSON true/false 同权）。
    #[test]
    fn boolean_coercion_reads_storage_rows_and_json() {
        let states = BTreeMap::new();
        let aggregate = aggregate_round(
            &[
                json!({"probe_id": "p1", "fingerprint": "fp", "alive": 1}),
                json!({"probe_id": "p2", "fingerprint": "fp2", "alive": "true"}),
                json!({"probe_id": "p3", "fingerprint": "fp3", "alive": 0}),
            ],
            &cfg(1),
            &states,
        );
        assert_eq!(aggregate.summary.alive, vec!["fp", "fp2"]);
        assert_eq!(aggregate.summary.failed, vec!["fp3"]);
    }
}
