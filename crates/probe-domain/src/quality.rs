//! Pure quality statistics and scoring, independent of alive/dead policy.
//!
//! Inputs are an ordered observation window, not a source of current availability.
//! Missing values stay missing; invalid observations return errors. This module
//! neither performs transfers nor enforces traffic budgets; those belong to the engine.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Minimum successful, identified observations needed to estimate exit stability.
pub const MIN_EXIT_SAMPLES: usize = 2;

/// One completed bounded transfer. Both bytes and elapsed time must be positive.
///
/// The engine chooses and enforces the byte/time budget before constructing this
/// measurement. Only completed transfers on successful samples are accepted.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TransferMeasurement {
    /// Actual payload bytes received, not an advertised content length.
    pub bytes: u64,
    /// Measured elapsed time in milliseconds, finite and strictly positive.
    pub elapsed_ms: f64,
}

/// One observation; slice order defines the order used for jitter.
///
/// Failure latency and exit metadata are validated but excluded from statistics.
/// A failed sample cannot contain a completed transfer. An absent measurement
/// must be `None`, never a fabricated zero.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QualitySample {
    /// Whether this observation succeeded.
    pub success: bool,
    /// Finite, nonnegative latency in milliseconds; a measured zero is allowed.
    pub latency_ms: Option<f64>,
    /// Nonblank, opaque canonical identity (for example an exit IP).
    /// Equality is exact and case-sensitive; canonicalization is the caller's job.
    pub exit_identity: Option<String>,
    /// Optional completed, bounded-transfer measurement.
    pub transfer: Option<TransferMeasurement>,
}

/// Distribution of only the successful observations with recorded latency.
///
/// Quantiles use linear interpolation at zero-based rank `(n - 1) * p`
/// (Hyndman-Fan type 7). Thus an even-sized median averages the middle values.
/// Empty distributions have no values; a singleton has quantiles but no jitter.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LatencyStatistics {
    /// Count of successful observations with latency, not all successes.
    pub sample_count: usize,
    /// Smallest recorded latency, in milliseconds.
    pub min_ms: Option<f64>,
    /// Interpolated 50th percentile, in milliseconds.
    pub median_ms: Option<f64>,
    /// Interpolated 90th percentile, in milliseconds.
    pub p90_ms: Option<f64>,
    /// Interpolated 95th percentile, in milliseconds.
    pub p95_ms: Option<f64>,
    /// Largest recorded latency, in milliseconds.
    pub max_ms: Option<f64>,
    /// Mean absolute successive difference: `sum(|x[i] - x[i-1]|) / (n-1)`.
    /// Uses input order after omitting failed/missing latencies, bridging gaps.
    /// Requires at least two recorded successful latencies; units are milliseconds.
    pub jitter_ms: Option<f64>,
}

/// Explicitly defined window aggregates; all rates and stability are in `[0, 1]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QualityStatistics {
    /// All observations, including failures and successes without measurements.
    pub sample_count: usize,
    /// Number of observations whose `success` flag is true.
    pub success_count: usize,
    /// `success_count / sample_count`; absent for an empty window.
    pub success_rate: Option<f64>,
    /// Successful-latency distribution and successive-difference jitter.
    pub latency: LatencyStatistics,
    /// Number of successful observations carrying an exit identity.
    pub exit_sample_count: usize,
    /// Modal identity count divided by `exit_sample_count`, not a transition rate.
    /// Missing identities are omitted; fewer than `MIN_EXIT_SAMPLES` yields `None`.
    pub exit_stability: Option<f64>,
    /// Number of successful, completed transfer measurements.
    pub transfer_sample_count: usize,
    /// Aggregate payload megabits/second: `sum(bytes) * 0.008 / sum(elapsed_ms)`.
    /// A megabit is decimal (1,000,000 bits). No transfers means `None`.
    pub throughput_mbps: Option<f64>,
}

/// Relative metric weights. Each must be finite and nonnegative.
///
/// Defaults weight the six non-throughput metrics equally. Zero disables a
/// metric's contribution, not its measurement or validation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QualityWeights {
    /// Weight of explicitly supplied current availability.
    pub availability: f64,
    /// Weight of median successful latency.
    pub median_latency: f64,
    /// Weight of p95 successful latency.
    pub p95_latency: f64,
    /// Weight of successful-latency jitter.
    pub jitter: f64,
    /// Weight of window success rate.
    pub success_rate: f64,
    /// Weight of observed exit stability.
    pub exit_stability: f64,
    /// Weight of measured throughput, disabled by default.
    pub throughput: f64,
}

impl Default for QualityWeights {
    /// Return equal non-throughput weights with throughput disabled.
    fn default() -> Self {
        Self {
            availability: 1.0,
            median_latency: 1.0,
            p95_latency: 1.0,
            jitter: 1.0,
            success_rate: 1.0,
            exit_stability: 1.0,
            throughput: 0.0,
        }
    }
}

impl QualityWeights {
    /// Return named weights for validation; takes self and preserves metric order.
    fn entries(self) -> [(&'static str, f64); 7] {
        [
            ("weights.availability", self.availability),
            ("weights.median_latency", self.median_latency),
            ("weights.p95_latency", self.p95_latency),
            ("weights.jitter", self.jitter),
            ("weights.success_rate", self.success_rate),
            ("weights.exit_stability", self.exit_stability),
            ("weights.throughput", self.throughput),
        ]
    }
}

/// Finite, strictly positive reference values, each mapping to component score 0.5.
///
/// For latency/jitter, score is `reference / (reference + measurement)`.
/// For throughput, score is `measurement / (reference + measurement)`.
/// Defaults are tunable policy choices, not network performance guarantees.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QualityReferences {
    /// Median latency midpoint, in milliseconds; default 200.
    pub median_latency_ms: f64,
    /// P95 latency midpoint, in milliseconds; default 500.
    pub p95_latency_ms: f64,
    /// Jitter midpoint, in milliseconds; default 50.
    pub jitter_ms: f64,
    /// Throughput midpoint, in decimal megabits/second; default 100.
    pub throughput_mbps: f64,
}

impl Default for QualityReferences {
    /// Return explicit latency, jitter and throughput midpoints.
    fn default() -> Self {
        Self {
            median_latency_ms: 200.0,
            p95_latency_ms: 500.0,
            jitter_ms: 50.0,
            throughput_mbps: 100.0,
        }
    }
}

/// Scoring policy; invalid configuration is rejected even for missing metrics.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QualityConfig {
    /// Nonnegative finite relative weights.
    pub weights: QualityWeights,
    /// Positive finite mapping midpoints with explicit units.
    pub references: QualityReferences,
    /// Minimum observed/configured weight ratio, inclusive, in `[0, 1]`.
    /// Defaults to 0.5. Even zero never permits an entirely unobserved score.
    pub minimum_coverage: f64,
}

impl Default for QualityConfig {
    /// Return default weights/references and a 50% coverage requirement.
    fn default() -> Self {
        Self {
            weights: QualityWeights::default(),
            references: QualityReferences::default(),
            minimum_coverage: 0.5,
        }
    }
}

impl QualityConfig {
    /// Validate all weights, references and coverage in self, returning the first error.
    fn validate(self) -> Result<(), QualityError> {
        for (field, value) in self.weights.entries() {
            if !value.is_finite() || value < 0.0 {
                return Err(QualityError::InvalidConfig {
                    field,
                    reason: "must be finite and nonnegative",
                });
            }
        }
        for (field, value) in [
            (
                "references.median_latency_ms",
                self.references.median_latency_ms,
            ),
            ("references.p95_latency_ms", self.references.p95_latency_ms),
            ("references.jitter_ms", self.references.jitter_ms),
            (
                "references.throughput_mbps",
                self.references.throughput_mbps,
            ),
        ] {
            if !value.is_finite() || value <= 0.0 {
                return Err(QualityError::InvalidConfig {
                    field,
                    reason: "must be finite and strictly positive",
                });
            }
        }
        if !self.minimum_coverage.is_finite() || !(0.0..=1.0).contains(&self.minimum_coverage) {
            return Err(QualityError::InvalidConfig {
                field: "minimum_coverage",
                reason: "must be finite and in [0, 1]",
            });
        }
        Ok(())
    }
}

/// Normalized components in `[0, 1]`. Missing metrics remain `None`, even if disabled.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QualityComponents {
    /// Explicit current availability: true maps to 1, false to 0, unknown to None.
    pub availability: Option<f64>,
    /// Normalized median latency.
    pub median_latency: Option<f64>,
    /// Normalized p95 latency.
    pub p95_latency: Option<f64>,
    /// Normalized jitter, absent when fewer than two usable latencies exist.
    pub jitter: Option<f64>,
    /// Observed window success rate, independent of current availability.
    pub success_rate: Option<f64>,
    /// Observed modal exit fraction.
    pub exit_stability: Option<f64>,
    /// Normalized measured throughput, present even when its weight is zero.
    pub throughput: Option<f64>,
}

impl QualityComponents {
    /// Pair self's normalized metrics with the supplied weights for aggregation.
    fn weighted(&self, weights: QualityWeights) -> [(Option<f64>, f64); 7] {
        [
            (self.availability, weights.availability),
            (self.median_latency, weights.median_latency),
            (self.p95_latency, weights.p95_latency),
            (self.jitter, weights.jitter),
            (self.success_rate, weights.success_rate),
            (self.exit_stability, weights.exit_stability),
            (self.throughput, weights.throughput),
        ]
    }
}

/// Statistics, normalized components, and an explicitly coverage-gated score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualityReport {
    /// Validated sample aggregates.
    pub statistics: QualityStatistics,
    /// Observed components without imputation for missing measurements.
    pub components: QualityComponents,
    /// Sum of observed positive weights / sum of all configured weights.
    /// Defined as zero if all weights are zero; always finite and in `[0, 1]`.
    pub weight_coverage: f64,
    /// Observed-weight-normalized mean times 100, in `[0, 100]`.
    /// Absent below minimum coverage or without any observed positive weight.
    /// Missing components never enter the mean as zero or perfect measurements.
    pub score: Option<f64>,
}

/// Invalid inputs are errors, not missing values or synthetic measurements.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QualityError {
    /// Invalid/contradictory observation; index is zero-based in the input slice.
    #[error("sample {index}, {field}: {reason}")]
    InvalidSample {
        /// Index of the offending sample.
        index: usize,
        /// Invalid measurement field.
        field: &'static str,
        /// Required invariant.
        reason: &'static str,
    },
    /// Invalid scoring policy.
    #[error("{field}: {reason}")]
    InvalidConfig {
        /// Invalid weight, reference, or threshold.
        field: &'static str,
        /// Required invariant.
        reason: &'static str,
    },
    /// Finite input measurements whose aggregate is not representable.
    #[error("{field}: {reason}")]
    InvalidAggregate {
        /// Aggregate that could not be computed.
        field: &'static str,
        /// Numeric failure, never silently clamped to a measurement.
        reason: &'static str,
    },
}

/// Aggregate ordered samples, returning statistics or the first validation error.
///
/// Failures count toward success rate only; even discarded metadata is validated.
/// All-failed windows have success rate zero but no latency/exit/throughput values.
pub fn summarize_samples(samples: &[QualitySample]) -> Result<QualityStatistics, QualityError> {
    let mut success_count = 0;
    let mut latencies = Vec::new();
    let mut exit_counts = BTreeMap::<&str, usize>::new();
    let mut transfers = Vec::new();
    for (index, sample) in samples.iter().enumerate() {
        validate_sample(index, sample)?;
        if !sample.success {
            continue;
        }
        success_count += 1;
        if let Some(latency) = sample.latency_ms {
            latencies.push(latency);
        }
        if let Some(identity) = sample.exit_identity.as_deref() {
            *exit_counts.entry(identity).or_default() += 1;
        }
        if let Some(transfer) = sample.transfer {
            transfers.push(transfer);
        }
    }
    let exit_sample_count = exit_counts.values().sum();
    let exit_stability = if exit_sample_count >= MIN_EXIT_SAMPLES {
        exit_counts
            .values()
            .max()
            .map(|count| *count as f64 / exit_sample_count as f64)
    } else {
        None
    };
    Ok(QualityStatistics {
        sample_count: samples.len(),
        success_count,
        success_rate: (!samples.is_empty()).then(|| success_count as f64 / samples.len() as f64),
        latency: summarize_latencies(latencies),
        exit_sample_count,
        exit_stability,
        transfer_sample_count: transfers.len(),
        throughput_mbps: measure_throughput(&transfers)?,
    })
}

/// Evaluate ordered samples with separate current availability and a scoring policy.
///
/// Returns validated aggregates, components and a coverage-gated score. Availability
/// is never inferred from the window. Recomputing aggregates here prevents caller-
/// supplied contradictory statistics from entering the scorer.
pub fn evaluate_quality(
    samples: &[QualitySample],
    current_availability: Option<bool>,
    config: &QualityConfig,
) -> Result<QualityReport, QualityError> {
    config.validate()?;
    let statistics = summarize_samples(samples)?;
    let references = config.references;
    let components = QualityComponents {
        availability: current_availability.map(|available| if available { 1.0 } else { 0.0 }),
        median_latency: statistics
            .latency
            .median_ms
            .map(|value| normalize_cost(value, references.median_latency_ms)),
        p95_latency: statistics
            .latency
            .p95_ms
            .map(|value| normalize_cost(value, references.p95_latency_ms)),
        jitter: statistics
            .latency
            .jitter_ms
            .map(|value| normalize_cost(value, references.jitter_ms)),
        success_rate: statistics.success_rate,
        exit_stability: statistics.exit_stability,
        throughput: statistics
            .throughput_mbps
            .map(|value| normalize_cost(references.throughput_mbps, value)),
    };
    let (weight_coverage, score) = combine_components(&components, config);
    Ok(QualityReport {
        statistics,
        components,
        weight_coverage,
        score,
    })
}

/// Validate one indexed sample, including metadata excluded due to failure.
fn validate_sample(index: usize, sample: &QualitySample) -> Result<(), QualityError> {
    let invalid = |field, reason| QualityError::InvalidSample {
        index,
        field,
        reason,
    };
    if let Some(latency) = sample.latency_ms {
        if !latency.is_finite() || latency < 0.0 {
            return Err(invalid("latency_ms", "must be finite and nonnegative"));
        }
    }
    if let Some(identity) = &sample.exit_identity {
        if identity.trim().is_empty() {
            return Err(invalid("exit_identity", "must not be blank"));
        }
    }
    if let Some(transfer) = sample.transfer {
        if !sample.success {
            return Err(invalid(
                "transfer",
                "completed transfer requires a successful sample",
            ));
        }
        if transfer.bytes == 0 {
            return Err(invalid("transfer.bytes", "must be strictly positive"));
        }
        if !transfer.elapsed_ms.is_finite() || transfer.elapsed_ms <= 0.0 {
            return Err(invalid(
                "transfer.elapsed_ms",
                "must be finite and strictly positive",
            ));
        }
    }
    Ok(())
}

/// Return latency aggregates from validated, ordered nonnegative finite values.
fn summarize_latencies(mut latencies: Vec<f64>) -> LatencyStatistics {
    let jitter_ms = if latencies.len() < 2 {
        None
    } else {
        // An incremental mean avoids overflowing a sum of finite differences.
        Some(
            latencies
                .windows(2)
                .enumerate()
                .fold(0.0, |mean, (index, pair)| {
                    let difference = (pair[1] - pair[0]).abs();
                    mean + (difference - mean) / (index + 1) as f64
                }),
        )
    };
    latencies.sort_unstable_by(f64::total_cmp);
    LatencyStatistics {
        sample_count: latencies.len(),
        min_ms: latencies.first().copied(),
        median_ms: percentile(&latencies, 0.5),
        p90_ms: percentile(&latencies, 0.9),
        p95_ms: percentile(&latencies, 0.95),
        max_ms: latencies.last().copied(),
        jitter_ms,
    }
}

/// Return a type-7 quantile of sorted finite values at p in `[0, 1]`, or None if empty.
fn percentile(sorted: &[f64], probability: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (sorted.len() - 1) as f64 * probability;
    let lower = rank.floor() as usize;
    let upper = rank.ceil() as usize;
    Some(sorted[lower] + (sorted[upper] - sorted[lower]) * rank.fract())
}

/// Compute aggregate Mbps from validated transfers; reject unrepresentable totals/rates.
fn measure_throughput(transfers: &[TransferMeasurement]) -> Result<Option<f64>, QualityError> {
    if transfers.is_empty() {
        return Ok(None);
    }
    let total_bytes: f64 = transfers.iter().map(|transfer| transfer.bytes as f64).sum();
    let total_elapsed_ms: f64 = transfers.iter().map(|transfer| transfer.elapsed_ms).sum();
    let throughput = total_bytes * 0.008 / total_elapsed_ms;
    if !total_elapsed_ms.is_finite() || !throughput.is_finite() || throughput <= 0.0 {
        return Err(QualityError::InvalidAggregate {
            field: "throughput_mbps",
            reason: "aggregate duration and rate must be finite and strictly positive",
        });
    }
    Ok(Some(throughput))
}

/// Map nonnegative value and positive reference to `reference / (reference + value)`.
fn normalize_cost(value: f64, reference: f64) -> f64 {
    // Ratio branches avoid overflowing the denominator for large finite inputs.
    if value <= reference {
        return 1.0 / (1.0 + value / reference);
    }
    let ratio = reference / value;
    ratio / (1.0 + ratio)
}

/// Return observed-weight coverage and a gated 0-100 mean for validated components/config.
fn combine_components(
    components: &QualityComponents,
    config: &QualityConfig,
) -> (f64, Option<f64>) {
    let pairs = components.weighted(config.weights);
    let maximum_weight = pairs.iter().map(|(_, weight)| *weight).fold(0.0, f64::max);
    let maximum_observed = pairs
        .iter()
        .filter(|(component, _)| component.is_some())
        .map(|(_, weight)| *weight)
        .fold(0.0, f64::max);
    if maximum_observed == 0.0 {
        return (0.0, None);
    }
    // Separate scales keep tiny observed weights usable even beside huge missing ones.
    let mut total_weight = 0.0;
    let mut observed_weight = 0.0;
    let mut weighted_score = 0.0;
    for (component, weight) in pairs {
        total_weight += weight / maximum_weight;
        if let Some(component) = component {
            let relative_weight = weight / maximum_observed;
            observed_weight += relative_weight;
            weighted_score += component * relative_weight;
        }
    }
    let coverage =
        ((maximum_observed / maximum_weight) * (observed_weight / total_weight)).clamp(0.0, 1.0);
    let score = if coverage >= config.minimum_coverage {
        Some((weighted_score / observed_weight * 100.0).clamp(0.0, 100.0))
    } else {
        None
    };
    (coverage, score)
}

#[cfg(test)]
mod tests;
