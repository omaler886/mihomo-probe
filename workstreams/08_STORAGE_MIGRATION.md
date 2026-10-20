# SQLite 与数据迁移

## 目标
SQL migration 体系 + Python SQLite 无损迁移（总控 §11）。

## 当前实现审计（db.py，731 行）
- 连接：单连接 + RLock；WAL、busy_timeout=30000、synchronous=NORMAL（older FS 降级容忍）。
- 表：nodes(source,fingerprint PK; status/consec_fail/total_ok/total_fail/last_*/ip_alive/ip_total/category)、rounds(id,started_at,finished_at,trigger,total,ok,failed,dropped,restored,suspect,note,duration_s,mode)、results(round_id,source,fingerprint,...,category)、events(滚动 2000)、ip_geo、domain_views。
- 迁移：代码内 PRAGMA table_info 检查 + ALTER ADD COLUMN（rounds.mode、category 双表、ip_alive/ip_total）；指纹化重构为 DROP 重建 + 备份（.bak-<ts>）。
- 时间：全部 UTC（db.now），读侧 to_epoch 用 timegm——55/56 轮 finished_at 早于 started_at 的时区事故已根治。
- 容量：results 按 KEEP_RESULT_ROUNDS=500 轮裁剪；events 2000 条。
- 查询卫生：IN 变量 400 分块（SQLITE_MAX_VARIABLE_NUMBER 999 兼容）、results(source,round_id DESC) 索引覆盖 5 秒轮询。

## 设计决策
- ADR-0001：切片用 rusqlite(bundled 6.x)——Windows 无系统 sqlite，bundled 构建可复现；R2 评估 sqlx（编译期校验 SQL + 异步）与迁移框架（refinery/手写 schema_migrations）。总控建议 sqlx，不否决，延后到迁移批次。
- **ADR-0003（R2 定稿）**：迁移框架取手写运行器（`schema_migrations` 登记 + `include_str!` 内嵌 SQL）而非 refinery/sqlx-migrate——迁移文件须与测试过的二进制同源，编译期内嵌消除部署漂移面；sqlx 仍留给 R5+ 的查询层复评。
- **ADR-0003b**：总控建议表中的 `sources` 与 `observations` 改名**推迟到 R10**——兼容期 config.json 是源注册表、`results` 是共享逐轮表；改名会让 Python 读取方在 shadow 期失效。Rust 侧新增 node_state_history/export_snapshots/config_audit/security_audit 四表（0002）。
- migrations/ 目录 + schema_migrations 表；迁移前自动备份；probe-cli db check/migrate。
- Rust 表结构在 Python 现有 schema 上扩展（rounds/observations/node_state_history/exports/export_snapshots/config_audit/security_audit 等），不删历史数据。

## 外部方案 §2.5（PRAGMA / 索引）核对 — 2026-10-01
外部 Go 方案给了三条 PRAGMA 与三条索引建议。逐条核对结果：**基本已满足，无可执行缺口**。

| 方案建议 | 核对结果 | 证据 |
|---|---|---|
| `journal_mode=WAL` | ✅ 已有 | `crates/probe-storage/src/lib.rs:21` |
| `synchronous=NORMAL` | ✅ 已有 | `lib.rs:23` |
| `busy_timeout=5000` | ✅ 已有且更宽（30000） | `lib.rs:22`；与 Python `db.py` 一致 |
| `foreign_keys=ON` | ⚠️ **不适用** | `migrations/*.sql` 中**没有任何 `REFERENCES` / `FOREIGN KEY`**——schema 不靠外键约束，开了也无对象可约束。**不加**（加了只是徒增一次 PRAGMA） |
| `UNIQUE INDEX (source_id, fingerprint)` | ✅ 已是主键 | `0001_python_compat.sql:31` `PRIMARY KEY (source, fingerprint)` |
| `INDEX (round_id, node_id)` | ✅ 实质覆盖 | `0001:61` `idx_results_round ON results(round_id)`；另有 `idx_results_node(source, fingerprint)`、`idx_results_source_round(source, round_id DESC)`。复合 `(round_id, node_id)` 仅在「同时按两者等值查」时更优，当前无此查询模式 → **不加** |
| `INDEX (status, consecutive_failures)` | ✅ 已有等价 | `idx_nodes_source(source)`；节点表规模为数百行，全表扫描成本可忽略 → **不加** |

结论：**R2 的 schema 无需因外部方案改动**。若 R10 双跑后发现真实慢查询，再按实测加索引
（索引不是越多越好，写路径要为每个索引付代价）。

## 待办清单
- [x] R1：切片 round 表读写（兼容现有列）
- [x] R2：migrations + 迁移 CLI（db check/migrate/verify/backup/rollback）+ 回滚演练
- [x] R2：核对外部方案 §2.5 的 PRAGMA/索引建议（结论：无缺口，见上）
- [x] R5：`record_results`——**单事务**批量写 `results`（外部方案 §2.5 与本文档都要求的
      "每轮一个写事务"；Python 是逐节点 `record_result` 各提交一次）；
      另有 `finish_round_with_counts`（写 total/ok/failed）与 `results_for_round`（读侧）
- [ ] R5+：`RoundSummary` 增加 `failed` 字段——`finish_round_with_counts` 已写入，
      但 `last_round()` 读不到，双跑比对会少一个字段
- [ ] R10：Python↔Rust 双跑数据比对工具（读同库，校验计数与语义）

## 测试证据
- Python 锚点：MigrationTest/TimestampTest；Rust 侧 R2 建临时库迁移测试。

## 下一步
- R2。

## Cache Storage Handoff (2026-10-09)

Storage-worker scope is implemented and locally verified. Parent integration and
overall acceptance remain pending; this entry does not close the wider R5/R10 work.

### Changed Files

- `crates/probe-storage/src/cache.rs`: exported cache records, five `Storage`
  methods, input validation, UTC conversion, and transactional geo writes.
- `crates/probe-storage/src/cache/tests.rs`: cache and compatibility regressions.
- `crates/probe-storage/src/lib.rs`: only the cache module declaration and record
  re-exports were added by this worker. Pre-existing atomic measurement commits,
  category preservation, and rollback regression tests were retained unchanged.
- `crates/probe-storage/src/migrations.rs`: version 4 registration, required-table
  registration, and a version-3 upgrade regression.
- `migrations/0004_dns_cache.sql`: additive `dns_cache` table with key/payload/time
  constraints and an expiry index. Migrations 0001-0003 and `ip_geo` are unchanged.
- `workstreams/08_STORAGE_MIGRATION.md`: this handoff.

No dependencies, manifests, root lockfile, other crates, Python, global settings,
credentials, or deployment data were changed. No commits or deployments were made.

### Public Contract

Both records are available from `probe_storage` and `probe_storage::cache`:

```rust
pub struct DnsCacheRecord {
    pub value_json: String,
    pub stored_at: i64,
    pub expires_at: i64,
}
pub struct GeoCacheRecord {
    pub ip: String,
    pub country: Option<String>,
    pub isp: Option<String>,
    pub checked_at: i64,
}
```

The following inherent methods return the existing `probe_domain::DomainResult`:

```rust
read_dns_cache(&self, key: &str, now: i64) -> DomainResult<Option<DnsCacheRecord>>
write_dns_cache(&self, key: &str, record: &DnsCacheRecord) -> DomainResult<()>
prune_dns_cache(&self, now: i64) -> DomainResult<usize>
read_ip_geo(&self, ips: &[String], now: i64, max_age_secs: u64) -> DomainResult<Vec<GeoCacheRecord>>
write_ip_geo(&self, records: &[GeoCacheRecord]) -> DomainResult<usize>
```

- DNS keys must contain exactly 64 ASCII hexadecimal characters. Case is preserved;
  the engine should produce one consistent digest representation. Validation errors
  never echo the supplied key, so an accidentally supplied resolver URL is not logged.
- `cache::MAX_DNS_CACHE_VALUE_BYTES` is 1,048,576 UTF-8 bytes. Bundled SQLite validates
  JSON syntax; neither storage nor the migration reserializes payloads. Positive,
  negative, empty-answer, and JSON `null` payloads round-trip byte-for-byte.
- UTC Unix timestamps must be in `0..=253402300799`; DNS expiry must follow storage
  time. A hit requires `stored_at <= now < expires_at`. Missing, expired, and
  future-dated rows are misses. Invalid timestamp metadata and corrupt/oversized
  live payloads return `DomainError::Storage`, as do invalid arguments and SQL errors.
  Pruning deletes rows with `expires_at <= now` and returns the deleted count.
- `ip_geo(ip,country,isp,checked_at)` retains Python's exact schema. SQLite converts
  `checked_at` to/from UTC `YYYY-MM-DDTHH:MM:SS`; a normalized round-trip rejects
  malformed dates, calendar overflow, offsets, fractions, and other formats.
- Geo freshness requires `0 <= now - checked_at < max_age_secs`; zero max age yields
  no hits, and `u64::MAX` does not overflow. Future, expired, malformed, unknown-country,
  wrong-type, and invalid-UTF-8 legacy rows are skipped. Database operation errors
  remain errors. Invalid requested IPs are skipped; valid input strings are
  deduplicated and queried in 400-IP chunks.
- Geo writes validate literal IPv4/IPv6 addresses, assigned uppercase ISO 3166-1
  alpha-2 country codes, and timestamps. `country: None` is not cached and cannot
  overwrite a known record. Invalid nonempty country codes are errors. Duplicate
  IPs use the last known-country record; the count reports successful upsert
  operations, not distinct IPs. One transaction covers the batch and rolls back
  both preceding inserts and updates on validation or SQL failure. Empty batches
  are no-ops. IP text is preserved for compatibility with Python's keys.

### Verification

The default rustup Cargo shim could not initialize its home in the restricted
sandbox (`os error 183`). The installed 1.99.0 MSVC toolchain worked directly;
the environment overrides below were process-local, not global configuration.
Commands were run from the repository root with the existing lockfile:

```powershell
$toolchain = 'C:\Users\ckai7\.rustup\toolchains\1.99.0-x86_64-pc-windows-msvc\bin'
$env:RUSTC = "$toolchain\rustc.exe"
$env:RUSTDOC = "$toolchain\rustdoc.exe"
$env:PATH = "$toolchain;$env:PATH"
& "$toolchain\cargo.exe" test --locked --offline -p probe-storage
& "$toolchain\cargo.exe" check --locked --offline -p probe-storage --all-targets
& "$toolchain\cargo.exe" clippy --locked --offline -p probe-storage --all-targets --no-deps -- -D warnings
& "$toolchain\rustfmt.exe" --check --edition 2021 crates/probe-storage/src/cache.rs crates/probe-storage/src/migrations.rs
git diff --check -- crates/probe-storage migrations/0004_dns_cache.sql workstreams/08_STORAGE_MIGRATION.md
```

- Baseline before this worker's edits: 21 tests passed, including the existing
  atomic measurement rollback regressions.
- Final storage tests: **45 passed, 0 failed**; doc-tests: 0 tests, successful.
- All-targets check, strict Clippy, focused rustfmt check, and scoped whitespace
  check: passed. Formatting was limited to the cache module (including its tests)
  and migration runner; the shared `lib.rs` was not reformatted.
- Coverage includes fresh files, legacy Python schema/data, v3-to-v4 migration
  without changing prior schema objects, repeated opens, DNS TTL boundaries,
  separate resolver digest keys/upserts, invalid/future timestamps, negative
  payloads, byte-size bounds, direct-SQL constraints, and corrupt rows.
- Geo coverage includes exact Python UTC text, strict age boundaries, invalid
  legacy rows, 2,405 unique IPs requested three times with SQLite forcibly capped
  at 999 variables, empty batches, unknown results, and rollback after a later
  invalid row or an actual SQLite trigger failure.
- A newly added invalid-UTF-8 legacy-row regression first reproduced a batch read
  error, then passed after conversion failures were made row-level cache misses.

### Parent Integration And Limits

- The engine still owns SHA-256 key derivation across domain/type/view/resolver/ECS,
  JSON payload serialization/deserialization and semantic validation, cache policy,
  and prune scheduling. Storage tests prove isolation of distinct supplied keys,
  not correctness of engine key derivation or prevention of secrets inside payloads.
- Country validation deliberately rejects lowercase, reserved/unknown codes such
  as `ZZ`, and unassigned provider-specific codes. Callers should normalize valid
  codes and represent failed/unknown lookups with `None`, not a fabricated country.
- Future timestamps may be written because write APIs do not accept a reference
  clock; neither cache returns those rows before the read caller's `now` reaches them.
- No live database, network, Python runtime, or workspace-wide test/format run was
  performed. Compatibility was verified using temporary SQLite fixtures matching
  the Python schema and timestamp format. Parent owns engine wiring, wider
  integration, live acceptance, and the remaining R5/R10 requirements above.
