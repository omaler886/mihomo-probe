//! Deterministic tests of numeric conventions, validation and coverage gating.

use super::*;

/// Build a successful sample with the supplied latency in milliseconds.
fn latency_sample(latency_ms: f64) -> QualitySample {
    QualitySample {
        success: true,
        latency_ms: Some(latency_ms),
        ..Default::default()
    }
}

/// Build ordered successful observations from the supplied latency values.
fn latency_samples(values: &[f64]) -> Vec<QualitySample> {
    values.iter().copied().map(latency_sample).collect()
}

/// Build a successful identified observation without latency or throughput.
fn exit_sample(identity: &str) -> QualitySample {
    QualitySample {
        success: true,
        exit_identity: Some(identity.to_owned()),
        ..Default::default()
    }
}

/// Build a successful completed transfer with the supplied bytes and milliseconds.
fn transfer_sample(bytes: u64, elapsed_ms: f64) -> QualitySample {
    QualitySample {
        success: true,
        transfer: Some(TransferMeasurement { bytes, elapsed_ms }),
        ..Default::default()
    }
}

/// Return a default scoring policy with all metric weights disabled.
fn zero_weight_config() -> QualityConfig {
    QualityConfig {
        weights: QualityWeights {
            availability: 0.0,
            median_latency: 0.0,
            p95_latency: 0.0,
            jitter: 0.0,
            success_rate: 0.0,
            exit_stability: 0.0,
            throughput: 0.0,
        },
        ..Default::default()
    }
}

/// Assert an optional measurement exists and is finite and approximately expected.
fn assert_close(actual: Option<f64>, expected: f64) {
    let actual = actual.expect("expected a measured value");
    assert!(actual.is_finite(), "nonfinite value: {actual}");
    assert!(
        (actual - expected).abs() <= 1e-12 * expected.abs().max(1.0),
        "expected {expected}, got {actual}"
    );
}

/// Extract a complete score from supplied samples/availability under the supplied policy.
fn score(samples: &[QualitySample], availability: Option<bool>, config: &QualityConfig) -> f64 {
    evaluate_quality(samples, availability, config)
        .unwrap()
        .score
        .expect("expected sufficient coverage")
}

/// Empty inputs have counts but no measurements, normalized components, or score.
#[test]
fn empty_window_preserves_every_missing_measurement() {
    let report = evaluate_quality(&[], None, &QualityConfig::default()).unwrap();
    assert_eq!(report.statistics, QualityStatistics::default());
    assert_eq!(report.components, QualityComponents::default());
    assert_eq!(report.weight_coverage, 0.0);
    assert_eq!(report.score, None);
}

/// A single latency defines every quantile, but not jitter or exit stability.
#[test]
fn singleton_has_quantiles_but_no_inferred_repetition() {
    let sample = QualitySample {
        exit_identity: Some("exit-a".into()),
        ..latency_sample(123.0)
    };
    let stats = summarize_samples(&[sample]).unwrap();
    assert_eq!(stats.sample_count, 1);
    assert_eq!(stats.success_count, 1);
    assert_close(stats.success_rate, 1.0);
    assert_eq!(stats.latency.sample_count, 1);
    for quantile in [
        stats.latency.min_ms,
        stats.latency.median_ms,
        stats.latency.p90_ms,
        stats.latency.p95_ms,
        stats.latency.max_ms,
    ] {
        assert_close(quantile, 123.0);
    }
    assert_eq!(stats.latency.jitter_ms, None);
    assert_eq!(stats.exit_sample_count, 1);
    assert_eq!(stats.exit_stability, None);
    assert_eq!(stats.throughput_mbps, None);
}

/// Type-7 percentiles interpolate rather than taking the nearest observed rank.
#[test]
fn even_and_odd_distributions_use_documented_percentiles() {
    for (values, median, p90, p95, maximum) in [
        (vec![40.0, 10.0, 30.0, 20.0], 25.0, 37.0, 38.5, 40.0),
        (vec![50.0, 10.0, 30.0, 20.0, 40.0], 30.0, 46.0, 48.0, 50.0),
        (vec![10.0, 110.0], 60.0, 100.0, 105.0, 110.0),
    ] {
        let stats = summarize_samples(&latency_samples(&values)).unwrap();
        assert_close(stats.latency.min_ms, 10.0);
        assert_close(stats.latency.median_ms, median);
        assert_close(stats.latency.p90_ms, p90);
        assert_close(stats.latency.p95_ms, p95);
        assert_close(stats.latency.max_ms, maximum);
        assert_eq!(stats.latency.sample_count, values.len());
    }
}

/// Jitter uses the recorded order and bridges omitted failure/missing measurements.
#[test]
fn jitter_is_mean_absolute_successive_difference_not_sorted_spread() {
    let samples = [
        latency_sample(10.0),
        QualitySample {
            success: false,
            ..latency_sample(900.0)
        },
        QualitySample {
            success: true,
            ..Default::default()
        },
        latency_sample(100.0),
        latency_sample(20.0),
    ];
    let stats = summarize_samples(&samples).unwrap();
    assert_eq!(stats.sample_count, 5);
    assert_eq!(stats.success_count, 4);
    assert_eq!(stats.latency.sample_count, 3);
    assert_close(stats.success_rate, 0.8);
    assert_close(stats.latency.median_ms, 20.0);
    assert_close(stats.latency.max_ms, 100.0);
    assert_close(stats.latency.jitter_ms, 85.0);
}

/// Failed measurements affect only success rate, never latency or exit statistics.
#[test]
fn failed_sample_metadata_is_not_successful_measurement() {
    let samples = [
        QualitySample {
            success: false,
            ..latency_sample(0.0)
        },
        QualitySample {
            success: false,
            ..exit_sample("exit-b")
        },
        QualitySample {
            exit_identity: Some("exit-a".into()),
            ..latency_sample(50.0)
        },
    ];
    let stats = summarize_samples(&samples).unwrap();
    assert_close(stats.success_rate, 1.0 / 3.0);
    assert_close(stats.latency.min_ms, 50.0);
    assert_eq!(stats.latency.sample_count, 1);
    assert_eq!(stats.exit_sample_count, 1);
    assert_eq!(stats.exit_stability, None);
}

/// An all-failed window has a real zero success rate, not synthetic zero measurements.
#[test]
fn all_failed_window_can_score_zero_only_for_observed_success_rate() {
    let samples = [QualitySample::default(), QualitySample::default()];
    let mut config = zero_weight_config();
    config.weights.success_rate = 1.0;
    let report = evaluate_quality(&samples, None, &config).unwrap();
    assert_eq!(report.statistics.success_count, 0);
    assert_close(report.statistics.success_rate, 0.0);
    assert_eq!(report.statistics.latency, LatencyStatistics::default());
    assert_eq!(report.statistics.exit_stability, None);
    assert_eq!(report.statistics.throughput_mbps, None);
    assert_eq!(report.components.availability, None);
    assert_close(report.score, 0.0);
}

/// Explicit current availability is not inferred from historical successes or failures.
#[test]
fn current_availability_is_separate_from_sample_success_rate() {
    let mut config = zero_weight_config();
    config.weights.availability = 1.0;
    assert_eq!(score(&[], Some(true), &config), 100.0);
    assert_eq!(score(&latency_samples(&[20.0]), Some(false), &config), 0.0);
    assert_eq!(
        score(&[QualitySample::default()], Some(true), &config),
        100.0
    );
    assert_eq!(
        evaluate_quality(&latency_samples(&[20.0]), None, &config)
            .unwrap()
            .score,
        None
    );
}

/// Exit stability requires two identities and is a modal fraction, not transition stability.
#[test]
fn stable_changing_and_missing_exits_use_only_identified_successes() {
    let stats = summarize_samples(&[exit_sample("a"), exit_sample("a")]).unwrap();
    assert_close(stats.exit_stability, 1.0);
    for identities in [["a", "b"], ["b", "a"]] {
        let samples: Vec<_> = identities.into_iter().map(exit_sample).collect();
        assert_close(summarize_samples(&samples).unwrap().exit_stability, 0.5);
    }
    let samples = [
        exit_sample("a"),
        exit_sample("b"),
        QualitySample {
            success: true,
            ..Default::default()
        },
        exit_sample("a"),
        QualitySample {
            success: false,
            ..exit_sample("b")
        },
    ];
    let stats = summarize_samples(&samples).unwrap();
    assert_eq!(stats.exit_sample_count, 3);
    assert_close(stats.exit_stability, 2.0 / 3.0);
    let missing = summarize_samples(&latency_samples(&[10.0, 20.0])).unwrap();
    assert_eq!(missing.exit_stability, None);
}

/// Throughput uses decimal Mbps and ratio of totals, not mean per-transfer rates.
#[test]
fn throughput_uses_payload_bytes_total_time_and_decimal_megabits() {
    let single = summarize_samples(&[transfer_sample(1_000_000, 1000.0)]).unwrap();
    assert_close(single.throughput_mbps, 8.0);
    let samples = [
        transfer_sample(1_000_000, 1000.0),
        transfer_sample(1_000_000, 3000.0),
    ];
    let stats = summarize_samples(&samples).unwrap();
    assert_eq!(stats.transfer_sample_count, 2);
    assert_close(stats.throughput_mbps, 4.0);
    assert_eq!(stats.latency.sample_count, 0);
}

/// Throughput remains measurable when its default weight is disabled.
#[test]
fn throughput_is_optional_and_does_not_change_default_scores() {
    let without = latency_samples(&[10.0, 20.0]);
    let mut with = without.clone();
    with[0].transfer = Some(TransferMeasurement {
        bytes: 1_000_000,
        elapsed_ms: 1000.0,
    });
    let config = QualityConfig::default();
    assert_eq!(config.weights.throughput, 0.0);
    let absent = evaluate_quality(&without, Some(true), &config).unwrap();
    let present = evaluate_quality(&with, Some(true), &config).unwrap();
    assert_eq!(absent.components.throughput, None);
    assert!(present.components.throughput.is_some());
    assert_eq!(absent.weight_coverage, present.weight_coverage);
    assert_eq!(absent.score, present.score);
}

/// Missing metrics reduce weight coverage but never become synthetic component values.
#[test]
fn missing_measurements_gate_score_at_inclusive_minimum_coverage() {
    let samples = latency_samples(&[100.0]);
    let mut config = QualityConfig::default();
    let report = evaluate_quality(&samples, None, &config).unwrap();
    assert_eq!(report.weight_coverage, 0.5);
    assert!(report.score.is_some());
    assert_eq!(report.components.jitter, None);
    assert_eq!(report.components.exit_stability, None);
    assert_eq!(report.components.availability, None);
    config.minimum_coverage = 0.5001;
    assert_eq!(
        evaluate_quality(&samples, None, &config).unwrap().score,
        None
    );
    let missing = [QualitySample {
        success: true,
        ..Default::default()
    }];
    config.minimum_coverage = 0.5;
    let report = evaluate_quality(&missing, None, &config).unwrap();
    assert_close(Some(report.weight_coverage), 1.0 / 6.0);
    assert_eq!(report.score, None);
    assert_eq!(report.components.median_latency, None);
}

/// Unequal weights govern coverage and observed normalization without missing-value imputation.
#[test]
fn coverage_is_weighted_and_score_renormalizes_only_observed_metrics() {
    let mut config = zero_weight_config();
    config.weights.availability = 3.0;
    config.weights.success_rate = 1.0;
    let report = evaluate_quality(&[], Some(true), &config).unwrap();
    assert_close(Some(report.weight_coverage), 0.75);
    assert_close(report.score, 100.0);
    let report = evaluate_quality(&[QualitySample::default()], None, &config).unwrap();
    assert_close(Some(report.weight_coverage), 0.25);
    assert_eq!(report.score, None);
    let report = evaluate_quality(&[QualitySample::default()], Some(true), &config).unwrap();
    assert_close(Some(report.weight_coverage), 1.0);
    assert_close(report.score, 75.0);
}

/// No threshold permits all-missing or all-zero-weight inputs to receive a score.
#[test]
fn zero_coverage_threshold_does_not_invent_evidence() {
    let mut config = QualityConfig {
        minimum_coverage: 0.0,
        ..Default::default()
    };
    assert_eq!(evaluate_quality(&[], None, &config).unwrap().score, None);
    config = zero_weight_config();
    config.minimum_coverage = 0.0;
    let report = evaluate_quality(&latency_samples(&[0.0, 0.0]), Some(true), &config).unwrap();
    assert_eq!(report.score, None);
    assert_eq!(report.weight_coverage, 0.0);
    config.weights.throughput = 1.0;
    assert_eq!(
        evaluate_quality(&latency_samples(&[10.0]), Some(true), &config)
            .unwrap()
            .score,
        None
    );
}

/// Negative and nonfinite latency values fail explicitly, even on failed observations.
#[test]
fn invalid_latency_is_never_silently_omitted_or_scored() {
    for value in [-1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        for success in [false, true] {
            let sample = QualitySample {
                success,
                ..latency_sample(value)
            };
            assert!(matches!(
                summarize_samples(&[QualitySample::default(), sample]),
                Err(QualityError::InvalidSample {
                    index: 1,
                    field: "latency_ms",
                    ..
                })
            ));
        }
    }
}

/// Blank identities and invalid or contradictory completed transfers are errors.
#[test]
fn invalid_exit_and_transfer_measurements_are_rejected() {
    for identity in ["", " \t\n"] {
        assert!(matches!(
            summarize_samples(&[exit_sample(identity)]),
            Err(QualityError::InvalidSample {
                field: "exit_identity",
                ..
            })
        ));
    }
    for sample in [
        transfer_sample(0, 1000.0),
        transfer_sample(1, 0.0),
        transfer_sample(1, -1.0),
        transfer_sample(1, f64::NAN),
        transfer_sample(1, f64::INFINITY),
        transfer_sample(1, f64::NEG_INFINITY),
        QualitySample {
            success: false,
            ..transfer_sample(1, 1.0)
        },
    ] {
        assert!(matches!(
            summarize_samples(&[sample]),
            Err(QualityError::InvalidSample { .. })
        ));
    }
}

/// Every weight independently rejects negative and nonfinite values, including disabled data.
#[test]
fn all_weight_fields_validate_even_when_their_measurements_are_missing() {
    for invalid in [-1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        for metric in 0..7 {
            let mut config = zero_weight_config();
            let weights = &mut config.weights;
            let fields = [
                &mut weights.availability,
                &mut weights.median_latency,
                &mut weights.p95_latency,
                &mut weights.jitter,
                &mut weights.success_rate,
                &mut weights.exit_stability,
                &mut weights.throughput,
            ];
            *fields.into_iter().nth(metric).unwrap() = invalid;
            assert!(matches!(
                evaluate_quality(&[], None, &config),
                Err(QualityError::InvalidConfig { .. })
            ));
        }
    }
}

/// Every reference is positive/finite, while coverage is finite and within the unit interval.
#[test]
fn invalid_references_and_coverage_are_rejected() {
    for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        for metric in 0..4 {
            let mut config = zero_weight_config();
            let references = &mut config.references;
            let fields = [
                &mut references.median_latency_ms,
                &mut references.p95_latency_ms,
                &mut references.jitter_ms,
                &mut references.throughput_mbps,
            ];
            *fields.into_iter().nth(metric).unwrap() = invalid;
            assert!(matches!(
                evaluate_quality(&[], None, &config),
                Err(QualityError::InvalidConfig { .. })
            ));
        }
    }
    for invalid in [-0.1, 1.1, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let config = QualityConfig {
            minimum_coverage: invalid,
            ..Default::default()
        };
        assert!(matches!(
            evaluate_quality(&[], None, &config),
            Err(QualityError::InvalidConfig {
                field: "minimum_coverage",
                ..
            })
        ));
    }
}

/// Lower median and p95 latency improve their respective scores.
#[test]
fn latency_scores_are_monotonic_and_references_are_tunable() {
    let fast = latency_samples(&[20.0, 40.0]);
    let slow = latency_samples(&[100.0, 200.0]);
    let mut config = zero_weight_config();
    config.weights.median_latency = 1.0;
    assert!(score(&fast, None, &config) > score(&slow, None, &config));
    config.weights.median_latency = 0.0;
    config.weights.p95_latency = 1.0;
    assert!(score(&fast, None, &config) > score(&slow, None, &config));
    config.references.p95_latency_ms = 195.0;
    assert_close(Some(score(&slow, None, &config)), 50.0);
    config.weights.p95_latency = 0.0;
    config.weights.median_latency = 1.0;
    config.references.median_latency_ms = 150.0;
    assert_close(Some(score(&slow, None, &config)), 50.0);
}

/// Less jitter, fewer failures, and a more stable exit each improve their isolated score.
#[test]
fn jitter_success_and_exit_scores_are_monotonic() {
    let mut config = zero_weight_config();
    config.weights.jitter = 1.0;
    assert!(
        score(&latency_samples(&[100.0, 100.0]), None, &config)
            > score(&latency_samples(&[50.0, 150.0]), None, &config)
    );
    config.references.jitter_ms = 100.0;
    assert_close(
        Some(score(&latency_samples(&[50.0, 150.0]), None, &config)),
        50.0,
    );
    config = zero_weight_config();
    config.weights.success_rate = 1.0;
    assert!(
        score(&latency_samples(&[20.0, 20.0]), None, &config)
            > score(
                &[latency_sample(20.0), QualitySample::default()],
                None,
                &config
            )
    );
    config = zero_weight_config();
    config.weights.exit_stability = 1.0;
    assert!(
        score(&[exit_sample("a"), exit_sample("a")], None, &config)
            > score(&[exit_sample("a"), exit_sample("b")], None, &config)
    );
}

/// Higher throughput improves its enabled score and its tunable midpoint maps to 50.
#[test]
fn throughput_scoring_is_monotonic_and_reference_has_mbps_units() {
    let mut config = zero_weight_config();
    config.weights.throughput = 1.0;
    config.references.throughput_mbps = 8.0;
    let fast = [transfer_sample(2_000_000, 1000.0)];
    let slow = [transfer_sample(1_000_000, 1000.0)];
    assert!(score(&fast, None, &config) > score(&slow, None, &config));
    assert_close(Some(score(&slow, None, &config)), 50.0);
}

/// Finite extreme measurements and weights must not overflow quantiles, jitter or scores.
#[test]
fn large_finite_inputs_keep_scores_and_statistics_finite() {
    let samples = latency_samples(&[0.0, f64::MAX, 0.0, f64::MAX]);
    let mut config = QualityConfig::default();
    config.weights.availability = f64::MAX;
    config.weights.success_rate = f64::MAX;
    config.references.median_latency_ms = f64::MAX;
    config.references.p95_latency_ms = f64::MAX;
    config.references.jitter_ms = f64::MAX;
    let report = evaluate_quality(&samples, Some(true), &config).unwrap();
    assert_close(report.statistics.latency.median_ms, f64::MAX / 2.0);
    assert_close(report.statistics.latency.jitter_ms, f64::MAX);
    assert_close(report.components.jitter, 0.5);
    assert!(report.statistics.latency.p95_ms.unwrap().is_finite());
    assert!((0.0..=100.0).contains(&report.score.unwrap()));
    assert!((0.0..=1.0).contains(&report.weight_coverage));
}

/// Unrepresentable transfer rates or aggregate durations return errors, not zero/infinity.
#[test]
fn unrepresentable_throughput_is_an_explicit_error() {
    let samples = [transfer_sample(1, f64::MAX), transfer_sample(1, f64::MAX)];
    assert!(matches!(
        summarize_samples(&samples),
        Err(QualityError::InvalidAggregate { .. })
    ));
    let sample = transfer_sample(u64::MAX, f64::from_bits(1));
    assert!(matches!(
        summarize_samples(&[sample]),
        Err(QualityError::InvalidAggregate { .. })
    ));
}

/// Tiny positive observed weights are not confused with disabled metrics at zero threshold.
#[test]
fn extreme_weight_ratios_keep_observed_mean_independent_of_coverage_underflow() {
    let mut config = zero_weight_config();
    config.minimum_coverage = 0.0;
    config.weights.availability = f64::from_bits(1);
    config.weights.throughput = f64::MAX;
    let report = evaluate_quality(&[], Some(true), &config).unwrap();
    assert_eq!(report.weight_coverage, 0.0);
    assert_close(report.score, 100.0);
    config.minimum_coverage = 0.5;
    assert_eq!(
        evaluate_quality(&[], Some(true), &config).unwrap().score,
        None
    );
}

/// Multiplying every configured weight by the same positive scale preserves the result.
#[test]
fn uniform_weight_scaling_preserves_coverage_and_score() {
    let mut config = zero_weight_config();
    config.weights.availability = 2.0;
    config.weights.success_rate = 1.0;
    config.weights.throughput = 1.0;
    let samples = [QualitySample::default()];
    let before = evaluate_quality(&samples, Some(true), &config).unwrap();
    config.weights.availability *= 1e300;
    config.weights.success_rate *= 1e300;
    config.weights.throughput *= 1e300;
    let after = evaluate_quality(&samples, Some(true), &config).unwrap();
    assert_close(Some(after.weight_coverage), before.weight_coverage);
    assert_close(after.score, before.score.unwrap());
}

/// Score and component bounds hold over deterministic combinations of measured inputs.
#[test]
fn score_bounds_hold_across_latency_failure_availability_and_exit_combinations() {
    for latency in [0.0, 0.001, 20.0, 5000.0, f64::MAX] {
        for availability in [None, Some(false), Some(true)] {
            for success in [false, true] {
                let samples = [
                    QualitySample {
                        success,
                        exit_identity: Some("a".into()),
                        ..latency_sample(latency)
                    },
                    QualitySample {
                        exit_identity: Some("b".into()),
                        ..latency_sample(10.0)
                    },
                ];
                let config = QualityConfig {
                    minimum_coverage: 0.0,
                    ..Default::default()
                };
                let report = evaluate_quality(&samples, availability, &config).unwrap();
                assert!((0.0..=100.0).contains(&report.score.unwrap()));
                assert!((0.0..=1.0).contains(&report.weight_coverage));
                assert!(report
                    .components
                    .weighted(config.weights)
                    .into_iter()
                    .filter_map(|(component, _)| component)
                    .all(|value| (0.0..=1.0).contains(&value)));
            }
        }
    }
}

/// Serde preserves missing metrics as null and applies documented partial policy defaults.
#[test]
fn public_inputs_outputs_and_config_roundtrip_with_serde() {
    let mut sample = latency_sample(100.0);
    sample.transfer = Some(TransferMeasurement {
        bytes: 1_000_000,
        elapsed_ms: 1000.0,
    });
    let encoded = serde_json::to_string(&sample).unwrap();
    assert_eq!(
        serde_json::from_str::<QualitySample>(&encoded).unwrap(),
        sample
    );
    let config: QualityConfig =
        serde_json::from_str(r#"{"weights":{"availability":2.0}}"#).unwrap();
    assert_eq!(config.weights.availability, 2.0);
    assert_eq!(config.weights.throughput, 0.0);
    assert_eq!(config.weights.success_rate, 1.0);
    assert_eq!(config.references, QualityReferences::default());
    assert_eq!(config.minimum_coverage, 0.5);
    let report = evaluate_quality(&[sample], None, &config).unwrap();
    let encoded = serde_json::to_string(&report).unwrap();
    assert!(encoded.contains("\"jitter\":null"));
    assert!(encoded.contains("\"score\":null"));
    assert_eq!(
        serde_json::from_str::<QualityReport>(&encoded).unwrap(),
        report
    );
}
