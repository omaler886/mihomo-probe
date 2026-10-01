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
- [ ] R10：Python↔Rust 双跑数据比对工具（读同库，校验计数与语义）

## 测试证据
- Python 锚点：MigrationTest/TimestampTest；Rust 侧 R2 建临时库迁移测试。

## 下一步
- R2。
