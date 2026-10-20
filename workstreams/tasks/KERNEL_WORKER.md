# Kernel Worker

Implement robust native validation and controller primitives for the full goal.
Own only `crates/probe-mihomo/src/config_check.rs`, its new test submodules,
`crates/probe-mihomo/src/controller.rs` and `workstreams/04_MIHOMO_CONTROLLER.md`.
Do not edit exit.rs, lanes.rs, manifests, lockfiles, engine, or other modules.
Preserve all existing changes. Use apply_patch. No commits, pushes, deployment,
global changes, credential reads, or other agents. Do not search outside the
workspace for toolchains; report a sandbox/toolchain limitation after one attempt.

Read current code and specification sections 5, 7, 15.

Required:
- Keep public Controller and ConfigCheck APIs compatible; add helpers where needed.
- Controller URL construction must use parsed URLs/query pairs, preserve an
  intended base path, encode names/expected status, reject invalid schemes and
  fragment/userinfo misuse, and never follow redirects or ambient proxies.
- Bound all response bodies and node/client timeouts; transport/auth/unknown
  proxy failures are infrastructure failures, not evidence of node death.
  Error strings must never expose endpoint URLs, credentials or response bodies.
- Native validator output must drain concurrently so a full stdout/stderr pipe
  cannot deadlock validation. Bound retained output, timeout and child lifetime;
  hide Windows subprocess windows. Never confuse an unavailable/timed-out
  validator with an actual rejected configuration.
- Add an explicit-binary validation helper for isolated tests while keeping
  validate(dir). Use fixed argv, null stdin, and only the explicit config path.
- Bound Docker availability probing. Remove the drifting `:latest` validator
  default; upstream v1.19.32 was verified via GitHub releases by the parent.
  Do not operate a live Docker daemon or arbitrary containers in tests.
- Fix Unicode-safe culprit matching (the current byte +1 advance can panic).
  Avoid ambiguous server/name matches that could prune the wrong proxy.
- Local mock controller tests and native helper-process tests must cover
  redirects, large/malformed responses, 401/403/404, encoded parameters,
  oversized output, exit failure, timeout/cleanup, and Unicode boundaries.

Do not change the actual default deployment. Document exact APIs and test results,
and clearly separate unavailable validation from passed validation. The parent
will wire these primitives into round preparation and run real Mihomo tests.
