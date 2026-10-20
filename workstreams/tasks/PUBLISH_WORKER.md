# Publication Worker

Implement durable exports/snapshot primitives for the full project goal.
Own only new `crates/probe-storage/src/publication.rs` and tests, new
`crates/probe-engine/src/publish.rs` and tests, `migrations/0006_publication.sql`,
and `workstreams/09_SUBSTORE_EXPORT.md`. Do not modify lib.rs exports, migration
registration, manifests/lockfiles, round.rs, config, API or Python: the parent
owns integration. Preserve all existing work. Use apply_patch. No commits,
pushes, deployment, credential reads, global changes, or additional agents.
Do not search the machine for toolchains. Modules are intentionally not registered
until handoff; report unexecuted tests honestly for the parent to run.

Read the existing export_snapshots/security_audit schema, Python export behavior,
specification sections 10-12, and current engine Collected/Job/NodeVerdict types.
Use existing workspace libraries; parent will add serde_json/sha2 to storage if
needed. No replacement of official Sub-Store conversion code.

Required storage primitives:
- Metadata-only snapshot summaries (no content in list responses), current export,
  history, lookup by key/id, atomic publication, and explicit audited rollback.
- Reuse existing export_snapshots content/hash/count columns. Add an active pointer
  table in migration 0006, preserving old snapshots and data. No changes to old SQL.
- Validate key, content/hash/count, enforce nonempty bounded exports. Publication
  and rollback must be transactions with audit entries containing only safe
  identifiers/hash/count/actor, never YAML or credentials. Wrong-key rollback
  and corrupt snapshots must be rejected. Re-publishing identical content should
  not create endless duplicate snapshots.

Required engine primitives:
- Render safe ClashMeta YAML from already-selected proxy dictionaries. JSON is
  valid YAML and may be used to guarantee scalar quoting (notably numeric-looking
  REALITY short IDs, booleans, Unicode and hashes). Preserve domains/SNI and refuse
  duplicate names or dangling dialer/group references. Support explicit groups
  if needed; do not silently strip topology.
- A clear PublishDecision for disabled/empty/guarded/diff-limit cases. Forced
  publication requires an explicit input and an audit record; never force zero.
- Atomically materialize exports under a caller-supplied root with checked path
  containment and restrictive file creation. DB active snapshot is authoritative.
  Prefer immutable versioned content files written before activation to avoid
  a partial file/metadata pair; old good export must survive write/DB failure.
- Compare snapshots without exposing credential values in diff output; report
  added/removed/changed identities or safe names/counts only.
- Focused tests: empty/guarded output, scalar round-trips, chain closure/collisions,
  size/path/symlink checks, failed writes/transactions, same-content idempotency,
  cross-key rollback rejection, bounded history and successful rollback.

The parent will select nodes using the complete round verdict, handle region
labels/front remapping, register modules/migration, add API routes, and run native
Mihomo/official Sub-Store validation. Keep handoff APIs clear and documented.
