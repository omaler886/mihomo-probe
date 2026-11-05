# 23 — HK3 轻量化改造 · 最终报告（P0–P7）

> 依据：《HK3 轻量化改造_多探测点_Gist_Web_SubStore人工推送_实施指导》§15 Phase 7 / §18。
> 工作区基线：HEAD `1210999` + 本批（P7）。全程批次：P0 `—`、P1/P2 `5450252`、Wave1 `377c624`、
> P3 `f8fadae`、P4 `a58aa74`、P5 `f9ec735`、P6 `1210999`、P7 本批。
> 诚实声明：本报告只登记有真实执行证据的内容；NOT_RUN 单独列出，不与通过混写。

## 1. 交付总览

| Phase | 交付物 | 提交 |
|---|---|---|
| P0 基线冻结 | HK3_LIGHTWEIGHT_CHANGELOG.md + baseline 日志（cargo 709 / python 605） | 随后批携带 |
| P1 死亡逻辑契约 | tests/test_death_logic_contract.py（35 用例，八条语义锁死） | 5450252 |
| P2 config v3 + DB 三表 | validate_config_v3、2→3 迁移（双 .bak-v3 备份）、probes/probe_observations/publish_audit | 5450252 |
| P3 聚合 + agent + API | aggregation.py、probe_dispatch.py（HMAC+防重放）、agent CLI、/api/v1 11 路由、front_dead 修复 | f8fadae |
| P4 canonical + Gist | canonical_export.py、gist_store.py（四文件/幂等门禁钉在 canonical sha）、engine 接线、POST /api/v1/gist/publish | a58aa74 |
| P5 人工 Sub-Store | manual_publish.py（preview/confirm-token 防重放/update-only push）、三端点、**删除自动回灌链** | f9ec735 |
| P6 Web 控制台 | 五视图路由（总览/节点/探测点/发布中心/设置）、发布中心全流程、探测点启停、节点诊断复制、compose probe-agent 角色 | 1210999 |
| P7 清理交付 | 本报告、22_ROLLOUT_ROLLBACK.md、TEST_REPORT §12 终跑数字、README/ARCHITECTURE 收口、B6/B7 产物收尾入库 | 本批 |

## 2. §18 最终验收清单（逐条证据）

| # | 验收项 | 结论 | 证据 |
|---|---|---|---|
| 1 | chain 默认关闭，开启后行为正确 | **PASS** | config v3 校验：chain.enabled 缺失强制 false（validate_config_v3 + 迁移置 false 并报告）；既有 chain 行为测试未回退 |
| 2 | 多探测点共用一份配置、一个账本、一个 canonical export | **PASS** | config 唯一事实来源（config.py v3）；账本单 SQLite；canonical 唯一产物 `data/exports/mihomo-alive.yaml`（test_canonical_export 21 用例 + engine muted-source 后门测试） |
| 3 | 节点不因探测点增多重复推进死亡 | **PASS** | 聚合三规则（any_alive/全败达 minimum 才败/不足 inconclusive）+ policy.apply 每节点每轮恰好一次（test_aggregation 57 用例）；探测点离线/未知/digest 不符不计完成数 |
| 4 | 重命名稳定、不改连接参数 | **PASS** | canonical_export 只动 name 字段（连接字段逐字复制 + digest 排除 name 的测试）；冲突 #2/#3 稳定排序 |
| 5 | 死亡收敛与整轮护栏回归全过 | **PASS** | P1 契约 35 + 605 基线回归全绿（832 总量，见 §3） |
| 6 | Gist 只维护固定文件，无扩散订阅 | **PASS** | gist_store 四文件 + 首建一次 + PATCH 只触目标文件（FixedFilesTest/FirstPublishTest）；canonical 幂等门禁（NoChangeTest 3 用例） |
| 7 | Web 可清楚查看探测状态/节点明细/失败原因 | **PASS** | P6 五视图（总览探测点表/最近 20 轮；节点表含连败/近况/最近原因 + 诊断复制；探测点页） |
| 8 | Sub-Store 只能人工预览+二次确认后推送 | **PASS** | preview → 60s 一次性 confirm-token（绑定 target+digest+节点数）→ push；重放/过期/绑定不符/无 token 各自 409 独立错误码（test_manual_publish 17 + 端点 4 用例） |
| 9 | 自动推送路径已删除或证明不可达 | **PASS** | `_publish_sources` 轮末调用点已删；`/api/push` 无 token 一律 409（test_hardening + test_logic PushEndpointTest 重写钉住） |
| 10 | 敏感 token/凭据不进日志/状态 JSON/前端 | **PASS** | gist mask()（PAT/Bearer/查询参数/hex/UUID）+ 探测点 token 不回显（/api/v1/probes 不含 token 值）+ confirm token 值不入审计摘要 + backend 只给 origin（§13.6）+ scan_secrets clean |
| 11 | 有配置迁移/数据库迁移/灰度/回滚文档 | **PASS** | P2 双 .bak-v3 自动备份 + take_migration_report；22_ROLLOUT_ROLLBACK.md（灰度 G1–G5 + 按批回滚表）；MIGRATION_GUIDE.md（B 系列既有） |
| 12 | 全量单元/集成/UI 测试有真实执行记录 | **PARTIAL** | 单元/契约全绿有实跑记录（§3）；集成（compose/双进程）与 UI（Playwright）NOT_RUN——本机无 docker、无第二台探测点主机，见 §4 |

## 3. 终跑门禁数字（P7 实跑）

| 门禁 | 结果 |
|---|---|
| `python -m unittest discover -s tests` | 832 OK / 0 failed / 2 skipped（基线 605 → P1+35 → P2+21 → P3+109 → P4+6 → P5+17 → P6 后 832；skipped 2 为既有预期跳过） |
| `cargo test --locked --workspace` | 28 个测试目标全 ok / 0 failed（P4 以来 Rust 树零改动，每波例行复跑） |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 通过 |
| `cargo fmt --all -- --check` | 通过 |
| `node --check mihomo_test/web/app.js` | 通过（P6） |
| `python tools/scan_secrets.py --include-untracked` | clean |
| compose YAML | `yaml.safe_load` 语法通过；容器级 NOT_RUN（无 docker） |

数字的批次明细（含每批增量）见 `HK3_LIGHTWEIGHT_CHANGELOG.md` P0–P7 各条与 `TEST_REPORT.md` §11/§12。

## 4. NOT_RUN（如实登记）

1. compose 容器级集成（Controller + 2 agent + 内核；指导 §14 集成清单）——无 docker 环境；agent 代码路径由 test_probe_dispatch 53 用例覆盖（HMAC/过期/digest/离线降级）。
2. Playwright UI 验收（§14）——UI 覆盖 = node --check + 渲染壳测试 + 端点级测试；人工核对清单在 22_ROLLOUT_ROLLBACK.md G1。
3. 真实 GitHub/真实 Sub-Store E2E——stub/transport 注入覆盖到 HTTP 语义；真机步骤属 MIGRATION_GIST_ONLY.md「需要用户在场」。
4. 24h soak / SQLite 故障注入——同 19_ACCEPTANCE_MAP §11.3-3。

## 5. 遗留与后续（不含在本工程内）

- **Rust 平价**：G-1→G-4（canonical/Gist 四文件/聚合/人工推送移植到 crates）——`workstreams/21_RUST_PARITY.md` 路线；在关闭 + 双跑验证前，"完全替代"不成立。
- G-6..G-10（§19_ACCEPTANCE_MAP 既有 partial 项）；B6-1 缓存头缺陷（probe-api 所有者）。
- P6 交接增强项：nodes 页服务端分页/排序（/api/v1/nodes 已备）、探测点拨号明细 UI 化（/api/v1/rounds/{id} observations）。

## 6. 结论

HK3 轻量化 P0–P7 的**代码与文档交付完成**，§18 十二条验收中 11 条 PASS、1 条 PARTIAL（缺集成/UI 与真机环节的执行环境，均有替代证据与人工核对清单）。工程状态：**可灰度**——按 22_ROLLOUT_ROLLBACK.md G1→G5 在 VPS 执行；「Rust 完全替代 Python」为独立后续工程（21 号 workstream），本报告不声称达成。
