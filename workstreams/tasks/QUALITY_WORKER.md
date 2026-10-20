# Quality Worker Contract

Implement the pure quality statistics/scoring portion of the complete-delivery
goal. The parent is concurrently integrating real round/DNS behavior.

Ownership: only `crates/probe-domain/src/quality.rs`, a matching test submodule,
the `pub mod quality;` export in `crates/probe-domain/src/lib.rs`, and a factual
quality subsection in `workstreams/07_EXIT_VERIFICATION.md`. Preserve every other
existing edit in lib.rs. Do not edit manifests/lockfiles, other code, or Python.

Do not spawn agents/chats, commit, push, deploy, inspect deployment/credential
data, or change global settings. Do not scan outside the workspace. If the
sandbox prevents Rust tooling, report that limitation and let the parent run
the compiler; do not spend the task searching the machine for toolchains.
Use apply_patch for edits and existing dependencies only.

Read specification sections 8-10. Provide meaningful, actually tested pure
functions, with public input/output types using serde where appropriate:

- Samples with success, optional latency, optional exit identity, and optional
  bounded-transfer bytes/time; current availability is a separate optional input.
- Statistics: total/success counts, success rate, min/median/p90/p95/max, jitter
  with a documented formula, exit stability with a documented minimum sample
  requirement, and optional measured throughput.
- Failed samples contribute to success rate but not successful-latency statistics.
  Missing/invalid/nonfinite numeric data must not become perfect or zero-valued
  measurements. Return explicit validation errors for contradictory/invalid input.
- Configurable nonnegative finite weights for availability, median latency, p95,
  jitter, success rate, exit stability, and optional throughput; explicit tunable
  reference values for mapping latency/jitter/throughput into [0,1].
- Result components must retain missing metrics as None, include observed-weight
  coverage, and expose no complete score when coverage is below a configured
  minimum (default 0.5). All-missing or all-zero-weight inputs have no score.
- Scores stay in [0,100]. Throughput is optional, disabled by default weight.
  The engine, not this pure crate, enforces actual traffic budgets.
- Deterministic tests for empty/single/even/odd samples, percentile definitions,
  failed samples, missing data, invalid weights/NaN/Infinity, monotonic scoring,
  stable/changing exits, throughput units, and minimum-coverage gating.

Keep APIs small, readable, and fully documented. No IO, clocks, web or database
dependencies in probe-domain. Choose precise names and explain numeric conventions.
Update only your owned workstream subsection with actual evidence and remaining
integration work. Final report: APIs/files, commands/results, limitations.
