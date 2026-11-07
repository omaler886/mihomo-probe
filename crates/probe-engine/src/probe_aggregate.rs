//! 轮末聚合消费（G-3 语义闭环，workstreams/32；Python `_dispatch_and_collect`
//! 等待循环 + `_converge_bucket` 的 observation 注入口的 Rust 镜像）。
//!
//! 时序（与 Python 相同）：本地测量完成 → fold →
//! 1. 本地观测落表（`probe_id="local"`，与远程观测同一条聚合路径——本地单
//!    探测点兼容模式因此与多探测点零差异，灰度第 1 步 no-op）；
//! 2. 等待远程回传：轮询 `get_observations(round_id)`，直到「已回传的非本地
//!    探测点 ⊇ waitable」或「截止 + late 宽限到点」；waitable 剔除 dark
//!    （disabled 或心跳超过 2×interval 的）探测点——探测点自身的故障表现为
//!    观测缺失，绝不计入完成数，更不计入节点失败（§4.3 末段）；
//! 3. `aggregation::aggregate_round` 折叠本地 + 远程观测，按三条不可协商
//!    语义改写 folded 节点：
//!     * alive → ok=true，delay = 最快探测点，出口国别跟随聚合；
//!     * fail → ok=false，代表失败原因 = probe_id 序第一条（跨轮稳定）；
//!     * inconclusive → `is_inconclusive=true`——persist 的 converge 过滤器
//!       会跳过它，账本状态与 consec_fail 原样保留（§7.7：本轮对该节点
//!       一无所知，不是失败）；
//!     * 没有任何观测的指纹保留本地 verdict（Python observation=None 的
//!       pre-refactor 路径）。
//! 4. 返回轮级摘要（completed/missing/offline/unresolved），调用方喂进
//!    canonical status payload 的 `probes` 键（探测点面板的诚实口径）。

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use probe_config::{AggregationSection, ProbeSpec};
use probe_storage::ObservationRow;
use serde_json::{json, Value};

use crate::aggregation::{aggregate_round, filter_observations, LOCAL_PROBE_ID};
use crate::limits::RoundCtx;
use crate::round::FoldedNode;

/// 等待循环的轮询间隔（Python time.sleep(1.0)）。
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// 轮次预算尾部保留（Python reserve = 60s）：等待窗口不吃完 watchdog，
/// 已测得的 verdict 不能因等待被整轮作废。
const BUDGET_RESERVE: Duration = Duration::from_secs(60);

/// 一个探测点是否会回答本轮（Python `_probe_is_dark`）：disabled 或心跳
/// 年龄超过 `2 × interval` 的都不等——为离线盒子等满窗口会拖死每一轮。
fn probe_is_dark(
    row: Option<&probe_storage::ProbeRow>,
    stale_after_s: i64,
    now_epoch: i64,
) -> bool {
    let Some(row) = row else {
        return false; // 注册表里还没有行的探测点：等它一次（可能刚上线）。
    };
    if !row.enabled || row.state == "disabled" {
        return true;
    }
    match row.last_seen_at.as_deref().and_then(parse_epoch) {
        Some(seen) => now_epoch - seen > stale_after_s,
        None => false,
    }
}

fn parse_epoch(stamp: &str) -> Option<i64> {
    let format = time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");
    // 存储值恒为 UTC（utc_now() 无时区后缀）：naive 解析后假设 UTC。
    time::PrimitiveDateTime::parse(stamp, &format)
        .ok()
        .map(|dt| dt.assume_utc().unix_timestamp())
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 观测行 → aggregate_round 的输入形状（serde 同形序列化）。
fn row_to_value(row: &ObservationRow) -> Value {
    json!({
        "round_id": row.round_id,
        "probe_id": row.probe_id,
        "fingerprint": row.fingerprint,
        "alive": row.alive,
        "delay_ms": row.delay_ms,
        "reason": row.reason,
        "egress_country": row.egress_country,
        "payload_digest": row.payload_digest,
    })
}

/// 一次轮末聚合消费的全部输入（借用束；与 `CanonicalRoundRequest` 同范式）。
pub(crate) struct AggregateConsumeRequest<'a> {
    pub round_id: i64,
    pub probes: &'a [ProbeSpec],
    pub aggregation: &'a AggregationSection,
    /// 本轮派发被接受的探测点 id（空 = 未派发）。
    pub dispatched: &'a [String],
    pub config_digest: &'a str,
    pub probe_deadline: Option<&'a str>,
    pub folded: &'a mut [FoldedNode],
    pub ledger: &'a dyn crate::Ledger,
    pub ctx: &'a RoundCtx,
}

/// 执行轮末聚合消费；`Some(摘要)` = 聚合发生了（调用方把摘要喂进 status
/// payload），`None` = 兼容模式跳过（无探测点/无节点）。
pub(crate) async fn consume(request: AggregateConsumeRequest<'_>) -> Option<Value> {
    let AggregateConsumeRequest {
        round_id,
        probes,
        aggregation,
        dispatched,
        config_digest,
        probe_deadline,
        folded,
        ledger,
        ctx,
    } = request;
    if folded.is_empty() || (probes.is_empty() && dispatched.is_empty()) {
        return None;
    }
    let round_text = round_id.to_string();

    // 1) 本地观测落表：本地测量是聚合的一名普通探测点（§4.3）。
    for node in folded.iter() {
        if node.is_excluded {
            continue; // 排除节点没有测量结论（分类排除，未拨号）。
        }
        let _ = ledger.record_observation(
            &round_text,
            LOCAL_PROBE_ID,
            &node.fingerprint,
            Some(node.ok && !node.is_inconclusive),
            node.delay_ms,
            node.reason.as_deref(),
            node.exit
                .as_ref()
                .and_then(|identity| identity.country.as_deref()),
            "",
        );
    }

    // 2) 等待远程回传（有登记截止才有窗口；dark 探测点不等）。
    let now = now_epoch();
    let registry = ledger.list_probes();
    let stale_after_s = 2 * aggregation.interval_minutes.clamp(1, 1440) * 60;
    let waitable: BTreeSet<String> = dispatched
        .iter()
        .filter(|probe_id| {
            !probe_is_dark(
                registry.iter().find(|row| &row.probe_id == *probe_id),
                stale_after_s,
                now,
            )
        })
        .cloned()
        .collect();
    let known: BTreeSet<String> = dispatched
        .iter()
        .cloned()
        .chain(probes.iter().map(|probe| probe.id.clone()))
        .chain(std::iter::once(LOCAL_PROBE_ID.to_string()))
        .collect();
    if !waitable.is_empty() {
        let window_s = probe_deadline
            .and_then(parse_epoch)
            .map(|deadline| (deadline + aggregation.late_result_seconds.max(0) - now).max(0))
            .unwrap_or(0);
        if window_s > 0 {
            let deadline_at = Instant::now()
                + Duration::from_secs(window_s as u64)
                    .saturating_sub(BUDGET_RESERVE.min(Duration::from_secs(window_s as u64)));
            let mut collected: BTreeSet<String> = BTreeSet::new();
            loop {
                if ctx.is_cancelled() {
                    break;
                }
                let rows = ledger.get_observations(&round_text);
                let outcome = filter_observations(
                    &rows.iter().map(row_to_value).collect::<Vec<_>>(),
                    Some(round_id),
                    Some(&known),
                    Some(config_digest),
                    Some("engine"),
                );
                collected = outcome
                    .accepted
                    .iter()
                    .filter(|row| row.probe_id != LOCAL_PROBE_ID)
                    .map(|row| row.probe_id.clone())
                    .collect();
                if collected.is_superset(&waitable) || Instant::now() >= deadline_at {
                    break;
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            let missing: Vec<String> = waitable.difference(&collected).cloned().collect();
            if !missing.is_empty() {
                // §4.3：没回传的探测点不是节点失败——缺席不计入完成数。
                tracing::warn!(
                    round_id,
                    missing = %missing.join("、"),
                    "{} 个探测点本轮未回传，不计入完成数",
                    missing.len()
                );
            }
        }
    }

    // 3) 聚合：本地 + 远程走同一条 aggregate_round。
    let rows = ledger.get_observations(&round_text);
    let outcome = filter_observations(
        &rows.iter().map(row_to_value).collect::<Vec<_>>(),
        Some(round_id),
        Some(&known),
        Some(config_digest),
        Some("engine"),
    );
    let states: BTreeMap<String, String> = probes
        .iter()
        .map(|probe| {
            let row = registry.iter().find(|row| row.probe_id == probe.id);
            let state = match row {
                Some(row) if !row.enabled || row.state == "disabled" => "disabled",
                Some(row) => row.state.as_str(),
                None => "unknown",
            };
            (probe.id.clone(), state.to_string())
        })
        .collect();
    let aggregate = aggregate_round(
        &outcome
            .accepted
            .iter()
            .map(|row| serde_json::to_value(row).unwrap_or(Value::Null))
            .collect::<Vec<_>>(),
        aggregation,
        &states,
    );

    // 4) 改写 folded 节点（三条语义 + 没听过的指纹保留本地）。
    for node in folded.iter_mut() {
        if node.is_excluded {
            continue;
        }
        let Some(node_aggregate) = aggregate.observation_for(&node.fingerprint) else {
            continue;
        };
        match node_aggregate.verdict {
            crate::aggregation::VERDICT_ALIVE => {
                node.ok = true;
                node.is_inconclusive = false;
                node.delay_ms = node_aggregate.delay_ms;
                node.reason = None;
                // 出口国别跟随聚合（最快探测点的国别）；本地的 ip/asn 保留。
                if let Some(country) = &node_aggregate.egress_country {
                    let identity = node.exit.get_or_insert_with(Default::default);
                    identity.country = Some(country.clone());
                }
            }
            crate::aggregation::VERDICT_FAIL => {
                node.ok = false;
                node.is_inconclusive = false;
                node.delay_ms = None;
                node.reason = node_aggregate.reason.clone();
            }
            _ => {
                // inconclusive：本轮对该节点一无所知——persist 的 converge
                // 过滤器跳过它，账本状态与 consec_fail 原样保留（§7.7）。
                node.is_inconclusive = true;
                node.ok = false;
                node.delay_ms = None;
                node.reason = Some(crate::aggregation::INSUFFICIENT_PROBES.to_string());
            }
        }
    }

    // 5) 轮级摘要（探测点面板的诚实口径）。
    let summary = &aggregate.summary;
    Some(json!({
        "strategy": summary.strategy,
        "minimum_completed_probes": summary.minimum_completed_probes,
        "completed_probes": summary.completed_probes,
        "missing_probes": summary.missing_probes,
        "offline_probes": summary.offline_probes,
        "unresolved_probes": summary.unresolved_probes,
        "dispatched": dispatched,
        "alive": summary.alive.len(),
        "failed": summary.failed.len(),
        "inconclusive": summary.inconclusive.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregation::STRATEGY_ANY_ALIVE;
    use probe_config::ProbeSpec;
    use serde_json::json;
    use std::sync::Mutex;

    fn aggregation_cfg(minimum: i64) -> AggregationSection {
        AggregationSection {
            strategy: STRATEGY_ANY_ALIVE.to_string(),
            minimum_completed_probes: minimum,
            late_result_seconds: 90,
            interval_minutes: 30,
        }
    }

    fn node(fingerprint: &str, ok: bool) -> FoldedNode {
        FoldedNode {
            source: "air".into(),
            fingerprint: fingerprint.into(),
            display: "n".into(),
            category: Some("direct".into()),
            ok,
            delay_ms: if ok { Some(120) } else { None },
            reason: if ok { None } else { Some("timeout".into()) },
            protocol: Some("ss".into()),
            server: Some("s.example.invalid".into()),
            is_excluded: false,
            is_inconclusive: false,
            ip_alive: if ok { 1 } else { 0 },
            ip_total: 1,
            exit: None,
        }
    }

    /// 可编程假账本：观测行与注册表由测试注入。
    struct FakeLedger {
        observations: Mutex<Vec<ObservationRow>>,
        probes: Vec<probe_storage::ProbeRow>,
        recorded: Mutex<Recorded>,
    }

    impl FakeLedger {
        fn new() -> Self {
            Self {
                observations: Mutex::new(Vec::new()),
                probes: Vec::new(),
                recorded: Mutex::new(Vec::new()),
            }
        }

        fn with_remote(
            &self,
            probe_id: &str,
            fingerprint: &str,
            alive: Option<bool>,
            delay: Option<i64>,
        ) -> &Self {
            self.with_remote_reason(probe_id, fingerprint, alive, delay, None)
        }

        fn with_remote_reason(
            &self,
            probe_id: &str,
            fingerprint: &str,
            alive: Option<bool>,
            delay: Option<i64>,
            reason: Option<&str>,
        ) -> &Self {
            self.observations.lock().unwrap().push(ObservationRow {
                round_id: "7".into(),
                probe_id: probe_id.into(),
                fingerprint: fingerprint.into(),
                alive,
                delay_ms: delay,
                reason: reason.map(str::to_string),
                egress_country: Some("JP".into()),
                received_at: "2026-11-08T00:00:00".into(),
                payload_digest: "sha256:d".into(),
            });
            self
        }
    }

    impl crate::Ledger for FakeLedger {
        fn record_observation(
            &self,
            round_id: &str,
            probe_id: &str,
            fingerprint: &str,
            alive: Option<bool>,
            delay_ms: Option<i64>,
            reason: Option<&str>,
            egress_country: Option<&str>,
            payload_digest: &str,
        ) -> probe_domain::DomainResult<()> {
            self.recorded.lock().unwrap().push((
                round_id.into(),
                probe_id.into(),
                fingerprint.into(),
                alive,
            ));
            // 与真 Storage 同语义：落表后 get_observations 读得回（本地观测
            // 与远程观测同一条聚合路径的另一半）。
            self.observations.lock().unwrap().push(ObservationRow {
                round_id: round_id.into(),
                probe_id: probe_id.into(),
                fingerprint: fingerprint.into(),
                alive,
                delay_ms,
                reason: reason.map(str::to_string),
                egress_country: egress_country.map(str::to_string),
                received_at: "2026-11-08T00:00:00".into(),
                payload_digest: payload_digest.into(),
            });
            Ok(())
        }

        fn get_observations(&self, _round_id: &str) -> Vec<probe_storage::ObservationRow> {
            self.observations.lock().unwrap().clone()
        }

        fn list_probes(&self) -> Vec<probe_storage::ProbeRow> {
            self.probes.clone()
        }

        fn start_round(
            &self,
            _trigger: &str,
            _mode: Option<&str>,
        ) -> probe_domain::DomainResult<i64> {
            Ok(7)
        }

        fn commit_verdicts(
            &self,
            _round_id: i64,
            _verdicts: &[crate::NodeVerdict],
            _nodes: &[probe_storage::ConvergeNode],
            _policy: &probe_domain::Policy,
        ) -> probe_domain::DomainResult<(usize, probe_storage::ConvergenceSummary)> {
            Ok((0, probe_storage::ConvergenceSummary::default()))
        }

        fn previous_alive_count(&self) -> probe_domain::DomainResult<i64> {
            Ok(0)
        }

        fn finish_round(
            &self,
            _round_id: i64,
            _note: &str,
            _counts: crate::RoundCounts,
            _suspect: bool,
            _inconclusive: bool,
        ) -> probe_domain::DomainResult<()> {
            Ok(())
        }
    }

    fn ctx() -> RoundCtx {
        RoundCtx::new(crate::limits::Limits::from_concurrency(4))
    }

    /// any_alive：本地活 + 远程败 = 活（任一探测点说活就是活，先于最小完成数）。
    #[tokio::test]
    async fn any_alive_keeps_a_locally_alive_node_alive_against_one_remote_fail() {
        let ledger = FakeLedger::new();
        ledger.with_remote("p1", "fp-1", Some(false), None);
        let probes = vec![ProbeSpec {
            id: "p1".into(),
            ..ProbeSpec::default()
        }];
        let mut folded = vec![node("fp-1", true)];
        let summary = consume(AggregateConsumeRequest {
            round_id: 7,
            probes: &probes,
            aggregation: &aggregation_cfg(1),
            dispatched: &["p1".to_string()],
            config_digest: "sha256:d",
            probe_deadline: None,
            folded: &mut folded,
            ledger: &ledger,
            ctx: &ctx(),
        })
        .await;
        assert!(summary.is_some());
        assert!(folded[0].ok, "any_alive: local pass wins");
        assert!(!folded[0].is_inconclusive);
        // 本地观测已落表（probe_id=local）。
        let recorded = ledger.recorded.lock().unwrap();
        assert!(recorded
            .iter()
            .any(|(_, probe_id, fp, alive)| probe_id == "local"
                && fp == "fp-1"
                && *alive == Some(true)));
    }

    /// 本地败 + 远程活 = 活，delay 取最快探测点，国别跟随聚合。
    #[tokio::test]
    async fn a_remote_pass_revives_a_locally_failed_node() {
        let ledger = FakeLedger::new();
        ledger.with_remote("p1", "fp-1", Some(true), Some(88));
        let probes = vec![ProbeSpec {
            id: "p1".into(),
            ..ProbeSpec::default()
        }];
        let mut folded = vec![node("fp-1", false)];
        consume(AggregateConsumeRequest {
            round_id: 7,
            probes: &probes,
            aggregation: &aggregation_cfg(1),
            dispatched: &["p1".to_string()],
            config_digest: "sha256:d",
            probe_deadline: None,
            folded: &mut folded,
            ledger: &ledger,
            ctx: &ctx(),
        })
        .await;
        assert!(folded[0].ok, "any_alive: the remote pass IS the verdict");
        assert_eq!(folded[0].delay_ms, Some(88), "delay from the best probe");
        assert_eq!(
            folded[0]
                .exit
                .as_ref()
                .and_then(|exit| exit.country.as_deref()),
            Some("JP"),
            "egress country follows the aggregate"
        );
        assert_eq!(folded[0].reason, None);
    }

    /// 全部失败且足数 → fail，代表原因 = probe_id 序第一条（跨轮稳定）。
    #[tokio::test]
    async fn all_failed_with_enough_probes_fails_with_the_stable_reason() {
        let ledger = FakeLedger::new();
        ledger.with_remote("zeta", "fp-1", Some(false), None);
        ledger.with_remote_reason("alpha", "fp-1", Some(false), None, Some("refused"));
        ledger.with_remote_reason("alpha", "fp-1", Some(false), None, Some("refused"));
        let probes = vec![
            ProbeSpec {
                id: "alpha".into(),
                ..ProbeSpec::default()
            },
            ProbeSpec {
                id: "zeta".into(),
                ..ProbeSpec::default()
            },
        ];
        let mut folded = vec![node("fp-1", false)];
        consume(AggregateConsumeRequest {
            round_id: 7,
            probes: &probes,
            aggregation: &aggregation_cfg(2),
            dispatched: &["alpha".to_string(), "zeta".to_string()],
            config_digest: "sha256:d",
            probe_deadline: None,
            folded: &mut folded,
            ledger: &ledger,
            ctx: &ctx(),
        })
        .await;
        assert!(!folded[0].ok);
        assert_eq!(
            folded[0].reason.as_deref(),
            Some("refused"),
            "representative reason = probe_id 序第一条（alpha），跨轮稳定"
        );
    }

    /// 完成数不足 → inconclusive：converge 过滤器跳过，账本状态原样保留。
    /// 本地是一路完成观测，remote 再添一路：minimum=3 时完成数 2 < 3。
    #[tokio::test]
    async fn insufficient_probes_marks_the_node_inconclusive_and_skips_convergence() {
        let ledger = FakeLedger::new();
        ledger.with_remote("p1", "fp-1", Some(false), None);
        let probes = vec![ProbeSpec {
            id: "p1".into(),
            ..ProbeSpec::default()
        }];
        let mut folded = vec![node("fp-1", false)];
        consume(AggregateConsumeRequest {
            round_id: 7,
            probes: &probes,
            aggregation: &aggregation_cfg(3),
            dispatched: &["p1".to_string()],
            config_digest: "sha256:d",
            probe_deadline: None,
            folded: &mut folded,
            ledger: &ledger,
            ctx: &ctx(),
        })
        .await;
        assert!(
            folded[0].is_inconclusive,
            "streak below the floor reads as inconclusive"
        );
        assert!(!folded[0].ok);
        assert_eq!(
            folded[0].reason.as_deref(),
            Some(crate::aggregation::INSUFFICIENT_PROBES)
        );
    }

    /// 没有任何观测的指纹保留本地 verdict（observation=None 路径）。
    #[tokio::test]
    async fn a_fingerprint_no_probe_answered_for_keeps_the_local_verdict() {
        let ledger = FakeLedger::new();
        ledger.with_remote("p1", "fp-other", Some(true), Some(50));
        let probes = vec![ProbeSpec {
            id: "p1".into(),
            ..ProbeSpec::default()
        }];
        let mut folded = vec![node("fp-1", false)];
        let summary = consume(AggregateConsumeRequest {
            round_id: 7,
            probes: &probes,
            aggregation: &aggregation_cfg(1),
            dispatched: &["p1".to_string()],
            config_digest: "sha256:d",
            probe_deadline: None,
            folded: &mut folded,
            ledger: &ledger,
            ctx: &ctx(),
        })
        .await;
        assert!(!folded[0].is_inconclusive);
        assert_eq!(
            folded[0].reason.as_deref(),
            Some("timeout"),
            "local verdict stands"
        );
        let summary = summary.unwrap();
        assert_eq!(summary["completed_probes"], json!(["local", "p1"]));
        assert_eq!(summary["missing_probes"], json!([]));
    }

    /// 摘要的诚实口径：missing/offline 来自注册表，unresolved 来自 no-verdict。
    #[tokio::test]
    async fn the_summary_names_missing_offline_and_unresolved_probes() {
        let ledger = FakeLedger::new();
        ledger.with_remote("p1", "fp-1", None, None);
        let probes = vec![
            ProbeSpec {
                id: "p1".into(),
                ..ProbeSpec::default()
            },
            ProbeSpec {
                id: "p2".into(),
                ..ProbeSpec::default()
            },
        ];
        let mut ledger = ledger;
        ledger.probes = vec![
            probe_storage::ProbeRow {
                probe_id: "p1".into(),
                name: "p1".into(),
                enabled: true,
                last_seen_at: Some("2026-11-08T00:00:00".into()),
                version: None,
                state: "online".into(),
            },
            probe_storage::ProbeRow {
                probe_id: "p2".into(),
                name: "p2".into(),
                enabled: false,
                last_seen_at: None,
                version: None,
                state: "disabled".into(),
            },
        ];
        let mut folded = vec![node("fp-1", true)];
        let summary = consume(AggregateConsumeRequest {
            round_id: 7,
            probes: &probes,
            aggregation: &aggregation_cfg(1),
            dispatched: &["p1".to_string()],
            config_digest: "sha256:d",
            probe_deadline: None,
            folded: &mut folded,
            ledger: &ledger,
            ctx: &ctx(),
        })
        .await
        .unwrap();
        assert_eq!(summary["unresolved_probes"], json!(["p1"]));
        assert_eq!(summary["offline_probes"], json!(["p2"]));
        // 本地活是一路完成观测：any_alive 直接成立（远程 no-verdict 不计完成
        // 数，只进 unresolved——§7.7）。
        assert!(folded[0].ok, "any_alive: the local pass IS the verdict");
        assert!(!folded[0].is_inconclusive);
    }

    /// 兼容模式：无探测点 + 无派发 → None，folded 原样（灰度第 1 步 no-op）。
    #[tokio::test]
    async fn the_compat_mode_skips_the_whole_consume() {
        let ledger = FakeLedger::new();
        let mut folded = vec![node("fp-1", true)];
        let summary = consume(AggregateConsumeRequest {
            round_id: 7,
            probes: &[],
            aggregation: &aggregation_cfg(1),
            dispatched: &[],
            config_digest: "sha256:d",
            probe_deadline: None,
            folded: &mut folded,
            ledger: &ledger,
            ctx: &ctx(),
        })
        .await;
        assert!(summary.is_none());
        assert!(folded[0].ok);
        assert!(
            ledger.recorded.lock().unwrap().is_empty(),
            "nothing recorded"
        );
    }

    /// digest 门禁：digest 不符的远程观测被拒（gate=engine → digest_mismatch），
    /// 不进聚合。
    #[tokio::test]
    async fn a_digest_mismatched_remote_observation_is_rejected() {
        let ledger = FakeLedger::new();
        let mut row = ObservationRow {
            round_id: "7".into(),
            probe_id: "p1".into(),
            fingerprint: "fp-1".into(),
            alive: Some(true),
            delay_ms: Some(88),
            reason: None,
            egress_country: Some("JP".into()),
            received_at: "2026-11-08T00:00:00".into(),
            payload_digest: "other-digest".into(),
        };
        ledger.observations.lock().unwrap().push(row.clone());
        row.payload_digest = "sha256:d".into();
        row.fingerprint = "fp-2".into();
        ledger.observations.lock().unwrap().push(row);
        let probes = vec![ProbeSpec {
            id: "p1".into(),
            ..ProbeSpec::default()
        }];
        let mut folded = vec![node("fp-1", false)];
        consume(AggregateConsumeRequest {
            round_id: 7,
            probes: &probes,
            aggregation: &aggregation_cfg(1),
            dispatched: &["p1".to_string()],
            config_digest: "sha256:d",
            probe_deadline: None,
            folded: &mut folded,
            ledger: &ledger,
            ctx: &ctx(),
        })
        .await;
        // fp-1 的远程观测 digest 不符被拒：本地 verdict 保留（failure）。
        assert!(
            !folded[0].ok,
            "the digest-mismatched pass never revives the node"
        );
        assert!(folded[0].is_inconclusive || folded[0].reason.is_some());
    }

    /// HashMap 引用保活（聚合内部类型标注用）。
    #[allow(dead_code)]
    type Recorded = Vec<(String, String, String, Option<bool>)>;
}
