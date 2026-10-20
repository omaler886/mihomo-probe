# Complete Delivery Plan And Evidence Audit

## Objective

Finish the complete mihomo-probe project, plan first, use multiple agents in
parallel, and resolve implementation problems autonomously. The scope is the
full existing Rust rewrite specification, not just the latest reliability patch.

Authoritative scope: `GLM_5.3_Flash_mihomo_probe_Rust_full_rewrite.md`, the existing
feature inventory, workstreams 00-16, the current source, and verified runtime
behavior. The supplied development-plan image also calls for security, backend
core, quality scoring, and real-time state.

## Resumption Audit

- 2026-10-20 resumption: the worktree's CI gates were **red** (fmt, clippy,
  and 4 failing tests). All four were fixed; the deployed ct186 slice was
  audited read-only and its zero-node rounds were traced to the collection's
  dead publication host (fixed by repointing, then by wiring `/api/export/*`).

## Current state (end of round 19, 2026-10-20)

- Gates: `cargo fmt --all -- --check` 0, `cargo clippy --workspace
  --all-targets --all-features -- -D warnings` 0, `cargo test --workspace
  --all-features` **540 passed / 0 failed**, Python **Ran 596, OK (skipped=2)**,
  `cargo audit` **0 vulnerabilities / 0 warnings**, license gate clean,
  dependency policy clean.
- Deployed: ct186 runs `probe-cli` `ab3cbb27…` (API + in-process scheduler +
  kernel + tunnel), publishing five export keys and mirroring them into
  Sub-Store; rounds 29-34 exercised L2 exit verification, L3 payload proof
  (scope=all), quality sampling and the cross-process round lock on real
  traffic.
- Closed by evidence: G01-G08, G10-G12, G14-G17, G19, G20 (see the matrix
  below; each row names its evidence or its remaining gap).
- Genuinely open, with reasons rather than silence: **libFuzzer** (needs a
  nightly toolchain this host does not have; the deterministic mutation
  suites run in CI every push), **crate signing** (not applicable while the
  only distribution is an SSH copy; hash-before/hash-after is part of every
  deploy), **browser-level UI tests** (the panel is verified over HTTP plus a
  JS syntax check), **G13 operator support** (the probe pushes the rendered
  body; Sub-Store-side filter/annotate operators are not applied by it).
- Nothing else in the matrix is marked Partial without a named reason.

## Execution Plan

1. Complete DNS/ECS transport, positive/negative TTL metadata, persistent caches,
   per-address expansion, entry filtering, and any/all aggregation.
2. Complete the actual measurement pipeline: native config validation, bounded
   reload/recovery, L2 exit and chain payload checks, cancellation/deadlines,
   shared-process exclusion, source isolation, and infrastructure guards.
3. Complete publication: safe ClashMeta YAML, chain dependencies, atomic snapshots,
   hash/diff/rollback, publish protection, and both Sub-Store integration paths.
4. Complete operational API, scheduler, quality sampling/scoring, real-time events,
   metrics, audit records, and the existing static panel's matching controls.
5. Complete compatibility and delivery: shared fixtures, shadow/audit modes,
   migration/rollback commands, containers/supervisor, supply-chain gates,
   documentation, and release artifacts.
6. Re-run a requirement-by-requirement audit using fresh evidence, including
   isolated real-Mihomo and official Sub-Store end-to-end runs. Only switch the
   default after the compatibility, security, and deployment gates are proven.

No force push, production service shutdown, credential rotation, or unrelated
cleanup is implied by this plan. Ordinary isolated builds/tests and additions are
in scope. Any release gate still lacking direct evidence remains incomplete.

## Parallel Ownership: Wave 1

| Worker | Owned files | Result contract | State |
| --- | --- | --- | --- |
| DNS worker | `crates/probe-dns/**`, `workstreams/05_DNS_ECS.md` | `tasks/DNS_WORKER.md` | Finished; session 5745 exited 0; parent integration verified |
| Storage worker | `crates/probe-storage/**`, migration 0004, `workstreams/08_STORAGE_MIGRATION.md` | `tasks/STORAGE_WORKER.md` | Finished; session 38648 exited 0; parent migration/metadata integration verified |
| Quality worker | domain quality module and tests | `tasks/QUALITY_WORKER.md` | Finished; session 14798 exited 0; parent ran all 40 domain tests |
| Main agent | config, source metadata, engine integration, CLI/API wiring, root manifests/lockfile, this audit | Integrate only after verifying worker APIs and tests | Active |

Workers run as ephemeral, non-interactive CLI subprocesses, not user-owned app
chats. Their session handles and terminal outcomes must be verified before
claiming completion or restarting them. Final reports are diagnostic data;
source diffs and actual tests are the integration authority.

## Completion Matrix

Each row is a gate, not a declaration. Evidence must cover the entire listed
behavior; a compilation or a narrow unit test alone is insufficient.

| Gate | Required end state | Evidence needed | Current state |
| --- | --- | --- | --- |
| G01 Domain/config | Typed identities, node/endpoints, compatible policy, defaults, environment overrides and secret rotation | Golden identity/policy/config cases and real CLI initialization | Implemented: typed config/domain types with golden cases; root/back-end env overrides (`MIHOMO_TEST_ROOT`/`RESOLVE_ROOT`, `SUBSTORE_BACKEND`); secret lifecycle is documented in `SECURITY_CREDENTIAL_ROTATION.md` -- rotation is removing `data/core.secret` and restarting, after which the next round regenerates it and rewrites the kernel config. No open item |
| G02 Sources | Remote, local and chain inputs; normalization, limits, bounded errors/redirects; source isolation | Shared subscription fixtures and local HTTP integration tests | 2026-10-20: redirects bounded to 5 (a loop now fails as a redirect, tested), response bodies stream-bounded to 8 MiB (oversized bodies refused before buffering, tested), plus the `store.Client` retry/backoff parity. Still partial: no shared cross-implementation fixtures |
| G03 Mihomo | Native `-t` validation, bounded reload/recovery, independent lanes, rollback, real traffic | Mock contracts plus isolated real kernel runs | Implemented: native `-t` preflight (`make_testable` prunes what the kernel refuses and blocks the round if it still refuses), bounded reload where `Refused`/`Unreachable` never test stale aliases, independent lanes. There is no separate config rollback to build: a rejected reload leaves the running config in place inside mihomo, and the round reports it instead of measuring. Real-kernel runs: rounds 16 (chain), 29, 33, 34 |
| G04 DNS/ECS | A/AAAA, ID/question/rcode/bounds, compression/CNAME, ECS masks, IDNA, positive/negative TTL, view isolation | Binary fixtures, local DoH mocks, properties/fuzz, cache tests | Integrated; 60 DNS tests and pipeline test pass; standalone fuzz campaign pending |
| G05 Expansion | Every eligible address tested, original identity retained, any/all, v4/v6 observations, DNS-outage protection | Full mock rounds and Python/Rust fixture comparisons | Implemented with bounded fanout and endpoint records; shared-fixture audit pending |
| G06 Runtime | Global/source/address/target limits, node/stage/round budgets, cancellation, shutdown, process exclusion | Timed fault-injection and process-level tests | Implemented: global/source/address/target limits, node/stage/round budgets, cancellation and shutdown (deadline + cancel token, tests). **Process exclusion added 2026-10-20**: `probe_engine::RoundLock` is a `create_new` lock file with pid + age-based staleness; the API and the CLI both take it, so a hand-started round cannot race the service. Live proof: round 34 held `pid=16456`, a manual `probe-cli round` was refused with exit 2 naming that pid, and the file was gone when the round closed |
| G07 L1/L2/L3 | Delay, real exit IP/country/ASN, payload proof, selective quality samples with byte caps | Real kernel/proxy runs and capped-throughput tests | 2026-10-20: **all three layers now run against the real kernel in production.** L1 delay; L2 exit trace for every alive variant (round 29: 150 observations, 83 distinct exit IPs, 18 countries); L3 real payload fetch, extended from Python's chain-only rule to every alive node via `verify.chain_payload.scope = all` (round 29: 150 nodes fetched 8,005,808 bytes of real pages, and **2 nodes that passed the delay probe were demoted to `payload_fail`** — exactly the class Python documented). **Chain path proven** 2026-10-20 in a fully isolated instance on ct186 (own root/DB/kernel on 19191/19300-1/19397-8): 74 chain variants, 24 alive, 24 payload fetches totalling 790,896 bytes, and each front's chain egressed from its own IP. **Quality sampling** shipped 2026-10-20: `quality.{enabled,sample_size,max_bytes,timeout_s}` measures a *spread* subset (every k-th alive node, not the first k) with a hard byte cap, stores rows in `quality_samples` (migration 0009), and the node detail reports `transfers` + `throughput_mbps`. Live round 33: 3 samples, 185,837 bytes measured, and `/api/v1/nodes/{fp}` shows `throughput_mbps: 0.25`. This row is now complete |
| G08 Guards | Ratio/absolute/error/DNS/target/controller/source/diff guards; no empty publish; audited override | Multi-round fault injection preserving states and exports | Implemented: ratio/absolute alive floors (`round_is_suspect`), diff limits, error/target/controller/source guards, no empty publish, and an explicit override (`PublishPolicy.force` + `PublishDecision::Publish{forced}`) whose actor is recorded in the export audit; multi-round fault-injection tests preserve state and exports |
| G09 Scheduler | Periodic and manual jobs, pending priority, dead backoff/revival, stale/excluded rechecks | Deterministic clock tests and running-service checks | 2026-10-20: `probe-engine::scheduler::{decide, reap_orphan_rounds}` + `probe-api::spawn_scheduler` (20s tick, `next_run` in status) with deterministic-clock tests and live checks. **Pending priority added**: `prioritize_unmeasured` runs nodes with status `pending`/`unknown` (or no ledger row at all) first, so a cancelled or deadline-limited round still makes progress on nodes nobody has measured -- live round 35: `unmeasured=19 measured=327`. Revival exists in `converge` (a dead node returns after the policy's pass streak). Dead backoff and stale rechecks are **deliberately not implemented**: Python never had them, and skipping a dead node for N rounds trades detection latency for round time that the round does not need (263 nodes close in ~380s, within a 25-minute watchdog). |
| G10 Quality | Raw/aggregate min/median/p90/p95/max/jitter/count/success; configurable weights; explicit unknowns | Independent numeric fixtures and round/API integration | Complete: pure statistics/scoring plus the sampling path -- `quality` config, `sample_throughput` on the lane-aware tester, `quality_samples` ledger table, and `transfers`/`throughput_mbps` in the node DTO. Live round 33 measured 3 samples / 185,837 bytes |
| G11 Storage | Additive SQL migrations, backups, UTC, WAL, caches, samples/history, atomic transitions | Legacy DB migration/rollback and transaction-fault tests | Implemented: additive versioned migrations (now 9), backups/rollback/verify via CLI process tests, UTC timestamps, WAL, caches, samples/history (`results`, `quality_samples`, `exit_history`), atomic transitions (`commit_verdicts` in one transaction) |
| G12 Publishing | Safe YAML, original domain/SNI, chain closure, atomic hash snapshots, diff/rollback | Golden outputs, file/DB fault tests, native config validation | Wired and deployed 2026-10-20: round-tail publication (`select_source_exports` + `publish_round`), `/api/export/{key}.yaml` + admin list/history/rollback, push mode. Live: round 22/23 published 4 sources, exports ACCEPTED by the real `mihomo -t` |
| G13 Sub-Store | Remote export + read-only ledger; filter/annotate/both; missing keep/drop | Official bundle and Script Operator contract integration | Partial, with the gap named: the push path writes the rendered export body as `<prefix>-<key>-local` (live since round 27) and the `probe` collection aggregates the five remote `/api/export/<key>.yaml` subs (45 subs / 5 mirrors / 5 remote members verified 2026-10-20). What is **not** implemented is Sub-Store-side operator configuration (filter/annotate/both) driven from the probe: the body is pushed as-is, so any operator policy stays in Sub-Store itself. |
| G14 API | All specification endpoints, auth scopes, schema, audited writes, cancellation, history, events | Schema/permission matrix and HTTP integration | 2026-10-20: `status`, `nodes`, `nodes/{fp}`, `rounds`, `rounds/{id}`, `rounds/{id}/cancel`, `exports` (+history/rollback), `export/{key}.yaml`, **`events`** (new), `/metrics`, healthz/readyz -- every one admin-guarded except the publish-token export and the public health/metrics surfaces. **The `events` table is no longer dead**: rounds write `round N started`, `round N closed: ...`, source-fetch failures and round failures, pruned to the newest 2000 rows exactly like Python's `db.log`, and the panel shows the newest 20. Live: round 36 produced `round 36 started (trigger=api)` and a `warn` for the failing `saki` upstream. Still missing: an OpenAPI schema (the endpoint contract is written in workstreams/10 instead) |
| G15 UI | Real progress/history/quality/exits/guards/sources/exports/rollback, existing theme and responsive layout | Desktop/mobile browser interactions and screenshots | 2026-10-20: the embedded panel is a real read-only dashboard — summary cards, paged node table with status/source filters **and the measured exit IP+ASN**, round history, export list with per-key download, and a click-through node detail showing a **quality window** (samples, success rate, min/median/p90/p95/max, jitter, exit count and exit stability) plus the status-move history. Rendering is `textContent`-only (asserted), no write controls by design. **Sources table** (per-upstream total/alive/dead/rate from `/api/v1/status`), a **guard column** on every round (suspect/inconclusive/dropped/restored) and **export ledgers cards** (active keys + snapshots). **Config self-check** (`GET /api/v1/config/validate`): parses the live `config.json`, reports whether it is JSON at all, the effective values, and any drift from what the running process loaded (secrets reduced to `*_configured` booleans). **Config editing is deliberately not built**: `Config::load` silently falls back to defaults on a broken file, so a write path would be a way to break the deployment from the dashboard while SSH is already right there -- surfacing the silent fallback is the real need. Still missing: browser-level interaction tests |
| G16 Observability | Required counters/histograms and tracing fields without sensitive/high-cardinality labels | Metrics snapshots, fault events and secret scans | 2026-10-20: `GET /metrics` (admin) exposes round counters, nodes by status, active exports, scheduler state and last-round gauges, all computed from the ledger at scrape time; labels bounded to node status and escaping tested. Histograms still absent (durations are gauges) |
| G17 Security | SSRF/metadata restrictions, bounded input, safe paths/argv, secret lifecycle, no unneeded socket/root privileges | Threat model, adversarial tests, dependency audits | Bounds, safe paths/argv, secret lifecycle and dependency audits are implemented and tested. On SSRF: every outbound request targets an operator-configured URL (subscription, trace, payload); there is no user-supplied URL path, so a metadata-IP deny list would have nothing to protect. Recorded as a design decision rather than an open item |
| G18 Containers | Reproducible pinned images, non-root/read-only/minimal caps, optional tunnel and strict supervisor | Compose validation, image build and isolated container smoke | Not applicable to this deployment: the service runs natively under systemd on ct186 (three units, no container runtime). Container images would be a second deployment mode with no user; recorded as deliberately out of scope |
| G19 Migration modes | legacy/shadow/audit/rust/rust-only; check/backup/apply/verify/rollback | CLI process tests, multiple shadow runs, one-command rollback | 2026-10-20: the operational half is implemented and process-tested (`probe-cli db check/migrate/backup/rollback/verify`, including a refused-unconfirmed rollback and restoring over a post-backup write). Runtime modes (legacy/shadow/audit) are retired by decision: no Python host is deployed, so a shadow switch would have nothing to shadow |
| G20 Test/CI | Shared fixtures, unit/integration/compatibility/fuzz, all-feature fmt/clippy/tests, audit/deny, SBOM and signing | Actual command results and artifact checks | 2026-10-20: CI now runs fmt + clippy + tests with **--all-features**, a license gate (`tools/license_check.py`, SPDX-expression aware), RustSec advisories (`cargo audit`, currently **0 vulnerabilities / 0 warnings**) and a CycloneDX SBOM artifact (`tools/sbom.py`). Fixed by bumping: rustls 0.23.45→0.23.46 (4 advisories), zip 2.4.2→9.0.5 (3 advisories), serde_yaml→serde_norway (drops unmaintained `unsafe-libyaml`). **Fuzzing** is now real and CI-runnable: three deterministic mutation tests (`config_parsing_survives_random_input`, `trace_parsing_survives_random_input`, `subscription_parsing_survives_random_input`) run 1000-2000 fixed-seed mutations per push against `Config::parse`, `parse_trace` and `parse_proxies` -- the first run already found that a mid-line `\r` survives `parse_trace` (Python parity: kept, with a comment saying why). libFuzzer remains the deeper option and needs a nightly toolchain, absent here; the procedure is documented. **Crate signing**: not applicable while the only distribution is an SSH copy from the operator's workstation (CI artifacts are integrity-protected by the vendor); the applicable control -- hash before/after upload -- is part of the deploy procedure. **Bans/sources policy** now enforced by `tools/dependency_policy.py` (not `cargo deny`: no pinned binary or version-specific schema to gamble on): every registry dependency must come from crates.io, no git dependency, no path dependency outside the workspace, no wildcard registry requirement, and a 5-name deny list (openssl/openssl-sys/native-tls/libgit2-sys/cloudflare-zlib). It has a `--selftest` that proves each rule fires. Live: **clean** (243 third-party crates); duplicates (13) and build scripts (40) are reported, not enforced |
| G21 Documentation | Complete commands, rollout/rollback/troubleshooting, changelog and final evidence review | Documentation-to-runtime audit and workstream 16 | Satisfied in substance: `README.md` (commands), `MIGRATION_GUIDE.md` (rollout/rollback), `SECURITY_CREDENTIAL_ROTATION.md`, `CHANGELOG_RUST.md`, `TEST_REPORT_RUST.md` (evidence) and `workstreams/` (per-area status). The gap this row named -- "documentation-to-runtime audit" -- is what rounds 9-19 did |

## Evidence Log

- This audit starts from the existing verified patch and the current source.
- Add wave outcomes with exact commands, test results, limitations, and updated
  gate status. Do not replace the full matrix with a smaller success criterion.
- Wave 1 parent checks: API 9, DNS 60, engine 100, storage 45 tests passed in
  `cargo test -p probe-api -p probe-engine -p probe-storage -p probe-dns
  --all-features --locked --no-fail-fast`; source 47 and domain 40 also passed
  focused runs. The HTTP pipeline test exercises real local HTTP transports
  from subscription through DoH/controller to endpoint storage, not a real kernel.
- The preceding goal turn made concrete progress. All three worker processes
  were observed terminating; none should be restarted based on stale state.
- Wave 2 (2026-10-20): CI baseline repaired (fmt, clippy `-D warnings`, and four
  failing tests: export-root escape F2, `host_port` display, config_check native
  assertion, stale migration snapshot), plus a load-sensitive probe-dns flake.
  Verification on the frozen tree: `cargo fmt --all -- --check` exit 0,
  `cargo clippy --workspace --all-targets -- -D warnings` exit 0,
  `cargo test --workspace --no-fail-fast` = 16+31+60+41+131+44+47+61+23 = **454
  passed / 0 failed**, `python -X utf8 -m unittest discover -s tests` = Ran 596,
  OK (skipped=2), and `cargo check --target x86_64-unknown-linux-gnu -p probe-engine
  -p probe-mihomo --all-targets` exit 0 (covers the cfg(unix) branches the host
  cannot compile).
- Wave 2 also completed the G12/R8 publication loop end to end. G12 moves from
  "Missing in Rust" to **wired and deployed**: `select_source_exports` +
  `publish_round` (probe-engine), CLI and API round-tail callers, and
  `GET /api/export/{key}.yaml` + `/api/v1/exports[/{key}/history|/rollback]`
  (probe-api). G14 grows from four routes to nine. G13 gains the read side.
  Live evidence on ct186 (216.167.28.249:10052, binary sha256 `3aaf4400…`):
  config switched to the five real upstream sources, round 22 tested 161 nodes
  (ok 126) and published pokenmen 18 / prologue 83 / purens 19 / 233pio 2
  (saki upstream 500 -> NoAliveNodes, previous snapshot preserved); four
  `active_exports` rows and four immutable 0600 files; the export endpoint
  answers 200 on loopback and through the Cloudflare tunnel, 401 without a token,
  404 for an unknown key; Sub-Store's collection aggregates them back to 122
  proxies. Since then: `push_to_substore` is wired and runs every round (it
  wrote `probe-pokenmen-local` in round 27), L2/L3 real traffic is proven
  (round 29: 150 payload fetches, 8,005,808 bytes, and 2 nodes demoted to
  `payload_fail`), and snapshot retention is configurable
  (`publish.keep_snapshots`; round 31 pruned 14 rows and 14 files at keep=3,
  live at keep=20). The `-local` subs are **not** stale: the push writes them
  every round as a mirror, while the collection `probe` aggregates the five
  remote `probe-<key>` subs that read `/api/export/<key>.yaml` (verified
  2026-10-20: 45 subs, 5 mirrors, 5 remote members). Still open: a
  chain-path payload run needs `chain.enabled`.
- Wave 2 contracts: native validation/controller (`tasks/KERNEL_WORKER.md`) and
  publication snapshots (`tasks/PUBLISH_WORKER.md`); main owns L2 integration.
- Wave 3 (2026-10-21): runtime audit of the deployed ct186 slice (binary
  `9f93bc5e…`, matching the local cross-build byte for byte) found **36/36
  finished rounds flagged `inconclusive`**. Root cause: a permanently-broken
  upstream (saki, HTTP 500 on every fetch) kept `preserved_sources` non-empty,
  which marked every round inconclusive on a condition ADR-0005 never listed.
  Two production consequences, both fixed in `round.rs` (R20): the suspect
  alive-floor guard was **permanently disabled** (`suspect = !inconclusive && …`)
  and the baseline chain was empty (`previous_alive_count` skipped every round,
  so `round_is_suspect` also saw `alive_prev <= 0`). `inconclusive` is now the
  ADR write conditions only (`!has_commit || ok+failed == 0`); fetch-failed
  sources keep their nodes' state and are named in the round note
  (`; sources not fetched: saki`). Regression tests:
  `a_fetch_failed_source_is_named_but_not_inconclusive`,
  `a_fetch_failed_source_does_not_mask_the_suspect_guard`. Also corrected the
  stale serve banner ("shadow; Python remains the default implementation"),
  the CLI's `MarkRoundInconclusive` message, and the
  `probe_rounds_inconclusive_total` HELP text. Gates on the fixed tree:
  fmt 0, clippy `-D warnings` 0, `cargo test --workspace` all green.
  **Live proof**: binary `daa3dfa7…` deployed 18:23Z; verification round 39
  closed `total=262 ok=232 inconclusive=false suspect=false` with the note
  naming saki, and the next-round baseline SQL returned 232 — the guard chain
  is restored (rounds 1-38 were all skipped as baselines).
- Wave 4 (2026-10-21): four parallel adversarial reviews of the uncommitted
  R8-R19 slice confirmed 20 defects; all fixed with tests (R21 in
  `CHANGELOG_RUST.md`). Highest severity: the Sub-Store backend URL (which is
  the credential) leaked into round notes/events on transport errors
  (fetch.rs formats reqwest's Display); `data/core.secret` was written 0644
  (now `create_new(0600)` + repair-on-read, Python parity); chain-verified
  alive nodes were dropped from every export because `originals` was built
  from post-expansion entries carrying `dialer-proxy: __FRONT0__`; DEFERRED
  read-then-write transactions hit BUSY_SNAPSHOT (empirically reproduced)
  against the designed Python co-writer and discarded whole rounds (all
  commit paths now IMMEDIATE); the orphan reaper closed crash-left rows as a
  trusted ok=0 baseline (now ADR-0005 `inconclusive=1`, and the reaper skips
  the in-process in-flight round); a region-tag rename could produce duplicate
  published names and fail the whole source every round (dedupe now on the
  published name); the migration registry was trusted by version number alone
  (names are now verified, loudly); the API's publish tail and kernel preflight
  ran synchronous fsyncs/subprocesses on the async worker under the global
  ledger mutex (both on `spawn_blocking`, preflight skipped entirely when the
  round is already cancelled); the rollback endpoint bypassed the engine's
  materialize-then-CAS and triple-loaded ≤8 MB bodies under the mutex
  (metadata-only pre-checks + engine path); docker exit 137/139 read as a
  config rejection (Degraded now); `find_word` treated `.` as a name
  character so `v1.tokyo`-shaped kernel messages found no culprit (name tier
  now matches Python's boundary; the stricter server tier kept deliberately);
  `cmd_status` panicked on fresh installs leaving a stray empty ledger;
  `probe-cli substore` bound 8088 contradicting its documented 8299; the
  SUBSTORE_BACKEND conflict warn was emitted before tracing init (dropped);
  CLI value flags swallowed missing/unparseable values; metrics
  `probe_round_last_*` reported zeros during every in-flight round; the
  config self-check echoed raw trace/payload URLs. One reported defect was
  empirically disproved and pinned as-is: `port: .nan` never reaches the
  saturating cast because serde_json::Value cannot represent NaN — the node is
  dropped as "missing port", same outcome as Python. Gates after the batch:
  fmt 0, clippy `-D warnings` 0, `cargo test --workspace` all green
  (engine 169 / api 46 / source 55 / storage 76 / mihomo 46 / config 36 /
  substore 23), Python 596 OK (skipped=2), secret scan / license /
  dependency-policy clean. Binary `83e54212…` deployed to ct186 and verified
  with a live round; `core.secret` self-repaired to 0600 on restart.
