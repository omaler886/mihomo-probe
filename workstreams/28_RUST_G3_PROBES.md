# 28 — Rust 移植 G-3：多探测点聚合 + 探测点回传面（两批）

> 关闭 `workstreams/21_RUST_PARITY.md` 的 **G-3** 的核心面：聚合纯函数
> （any_alive / minimum_completed_probes / inconclusive 三条语义）+ 探测点
> 注册表/回传面（migration 0013 三张表 + HMAC 签名 + 五类结果校验 + agent
> API 端点）。验收沿用 19 号写法，每条 = 树内指名测试。

## 批次 A（G-3a，聚合核心）

| 文件 | 内容 |
|---|---|
| `crates/probe-engine/src/aggregation.rs` | `mihomo_test/aggregation.py` 的 Rust 镜像：`Observation` 规整（字符串布尔/0-1 数字同权）、`filter_observations`（stale_round/malformed/unknown_probe/digest_mismatch/deferred 五类分流）、`aggregate_round`（any_alive + 完成数下限 + inconclusive + 轮级摘要 completed/missing/offline/unresolved）、`local_observation`（本地单探测点与远程多探测点同一条聚合路径） |
| `crates/probe-config/src/lib.rs` | `ProbeSpec`（id/name/enabled/endpoint/token_env/weight/tags，白名单重建：未知键丢弃、非法 id/重复 id/坏 env 名跳过——明文凭据键名永远进不了内存结构）+ `AggregationSection`（strategy/minimum_completed_probes/late_result_seconds，clamp 0..64 / 0..3600） |

## 批次 B（G-3b，回传面）

| 文件 | 内容 |
|---|---|
| `migrations/0013_probe_observations.sql` | `probes` / `probe_observations`（PK round+probe+fingerprint，幂等覆盖）/ `publish_audit`（G-4 用）三张表，SQL 与 Python `V3_SCHEMA` 同构 + `rounds.probe_config_digest`（dispatch 登记列） |
| `crates/probe-storage/src/probes.rs` | Python `db.py` 最小访问函数的镜像：`upsert_probe`（只动传了的字段）、`touch_probe_seen`（0 = unknown probe）、`record_observation`、`get_observations`、`record_publish_audit`/`list_publish_audit`、`probe_round_state`（存在/已结算/最新三态） |
| `crates/probe-engine/src/probe_dispatch.rs` | `sign_request`/`verify_signature`（hex HMAC-SHA256 over `ts+"\n"+body`，300s 窗，与 Python 同字节）、`config_digest`（六段 sorted-JSON sha256，键序无关）、`clean_result_item`、`verify_result`（BAD_PAYLOAD/ROUND_STALE/UNKNOWN_PROBE/DIGEST_MISMATCH/RESULT_LATE 五类拒绝，全部 audit-only） |
| `crates/probe-api/src/probe_plane.rs` | `POST /api/v1/probes/{id}/heartbeat` + `POST /api/v1/probes/{id}/jobs/{round}/result`：probe Bearer 凭证（与 admin token 刻意分离，§13.1），serve 启动即同步注册表进账本，错误码/HTTP 映射照抄 Python `V1_ERROR_STATUS` |
| `crates/probe-api/src/lib.rs` | `ServeConfig`/`AppState` 增 `probes` 快照；probe 路由挂在 unguarded 组（自带 probe 认证） |

## 设计裁决

1. **三条聚合语义逐字对齐 Python**（§4.3）：任一活即活（先于最小完成数——
   下限拦的是「凭太少证据判死」）；全部失败且足数才判败；不足数 inconclusive
   且 reason=`insufficient_probes`（与节点自身失败原因刻意不同）。no-verdict
   行永不计入完成数（§7.7）。
2. **失败代表原因确定性**：probe_id 序第一条，跨轮稳定，面板列不跳。
3. **观测表不是状态账本**：`probe_observations` 只存原始观测，不参与死亡
   计数推进——`nodes` 仍是唯一状态账本（§12）。
4. **digest 门禁的分层**：dispatch 登记进 `rounds.probe_config_digest`，列空 =
   尚未启用 dispatch → 任何非空 digest 接受（与 Python expected_digest=None
   的重算口径等价）；列非空则强制比对。
5. **两套凭证**：probe token 只从 `token_env` 命名的环境变量读（请求、配置、
   响应里都不存在本体）；admin token 永远进不了 probe 路由（测试钉住）。
6. **本轮未做（诚实边界）**：controller→agent 的 dispatch HTTP 发送与
   Rust `agent` 模式（`__main__.py` 的 /jobs 接收器镜像）。协议层（签名、
   Job/ProbeResult 形状、错误码）已全部就位，Python agent 与 Rust 控制器
   可互通；接线属下一批（round 语义变更需独立验收）。

## 验收映射（每条 = 树内指名测试）

| # | 行为 | 测试 | 状态 |
|---|---|---|---|
| 1 | 三条聚合语义 | aggregation `the_three_semantics_hold` | covered |
| 2 | no-verdict 不计完成数 | `a_no_verdict_row_never_counts_toward_completion` | covered |
| 3 | 失败代表确定性 | `the_representative_failure_is_deterministic` | covered |
| 4 | 摘要 missing/offline | `the_summary_names_missing_and_offline_probes` | covered |
| 5 | 过滤五类分流 + digest deferred | `the_filter_routes_every_rejection_class` | covered |
| 6 | 本地观测同路径 | `the_local_observation_shares_the_aggregation_path` | covered |
| 7 | probes 白名单重建 | probe-config `the_probe_registry_parses_with_the_whitelist` | covered |
| 8 | 签名同字节 + 时间窗 | probe_dispatch `the_signature_matches_the_python_bytes` | covered |
| 9 | 六段 digest 键序无关 | `the_config_digest_is_key_order_independent` | covered |
| 10 | 五类结果拒绝 | `the_verifier_routes_every_rejection_class` | covered |
| 11 | 注册表 upsert 只动传入字段 | probes `upsert_probe_never_resets_seen_state` | covered |
| 12 | 心跳 0 = unknown probe | `a_heartbeat_on_an_unknown_probe_touches_nothing` | covered |
| 13 | 观测幂等覆盖 + 有序读回 | `observations_overwrite_and_read_back_ordered` | covered |
| 14 | agent 端点冻结类 + 幂等落观测 | probe_plane `the_result_route_maps_the_frozen_classes` 等 4 条 | covered |
| 15 | admin token 进不了 probe 路由 | probe_plane `a_heartbeat_stamps_the_ledger_row`（admin 断言） | covered |

## 门禁（2026-11-08 实跑）

| 门禁 | 结果 |
|---|---|
| `cargo test --workspace` | 31 目标全 ok、**778 passed / 0 failed**（766 + 12 新增） |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过 |
| `cargo fmt --all -- --check` | 通过 |
| `tools/scan_secrets.py` | clean |
