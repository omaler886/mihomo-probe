# HK3 轻量化改造 · 修改历史（HK3_LIGHTWEIGHT_CHANGELOG）

> 依据：《HK3 轻量化改造_多探测点_Gist_Web_SubStore人工推送_实施指导》（D:\HK3_轻量化改造_多探测点_Gist_Web_SubStore人工推送_实施指导.md）
> 前序基础：Gist-only 重构 B1–B8（已入库至 2c22db6）+ B6 测试/B7 文档（工作区未提交）。
> 每条记录含：编号、时间、执行者、文件、变更、原因、影响、风险、测试、审查、回滚。

---

## P0-1 — 基线冻结（2026-11-05）

- **执行者**：主 Agent
- **时间**：2026-11-05（Asia/Shanghai）
- **基线 commit**：`2c22db6`（B8：落地重命名 + 发布去重 + Sub-Store 测活产物清理）
- **文件**
  - 新增本文件 `HK3_LIGHTWEIGHT_CHANGELOG.md`。
  - 基线测试日志：`baseline-rust.log`、`baseline-python.log`（仓库根，未纳入 git）。
- **基线测试结果（实跑）**
  - `cargo test --workspace`：**709 passed / 0 failed**（28 个测试目标，含 doc-tests），exit 0。
  - `python -m unittest discover -s tests`：**605 passed, OK (skipped=2)**，50.2s，exit 0。
- **数据库 schema 基线**：migrations LATEST = 12（0010 sources/artifacts、0011 source_runs、0012 source_run_publish）。
- **配置样本**：`.env.example`；配置 schema 现状 = Python `mihomo_test/config.py` 与 Rust `crates/probe-config/src/lib.rs` 双实现；`chain.enabled` 默认 false（已有测试钉住）；无 `probes[]`/`aggregation`/`rename` 段、无 schema_version 3。
- **data/ 目录**：不存在（仓库为干净工作区，无运行期数据需备份）。指导 §15 Phase 0 的「备份 data/」在本基线下为空动作，如实记录。
- **未提交前序产物**：B6（crates/probe-{api,engine,storage}/tests/、tests/test_substore_write_zero.py、workstreams/19_ACCEPTANCE_MAP.md）与 B7（ARCHITECTURE_GIST_ONLY.md、MIGRATION_GIST_ONLY.md、四份 M 文档）留在工作区，HK3 各 Phase 在其之上继续，不抢先提交前序批次。
- **原因**：指导 §15 Phase 0 要求冻结基线（commit/测试/schema/配置样本）并新建本 CHANGELOG。
- **影响**：无代码行为变化。
- **风险**：无。
- **测试**：上述两条命令即本条目测试证据。
- **审查**：基线数字与 CHANGELOG_REFACTOR.md B8 条目（cargo 709 / python 605）一致。
- **回滚**：删除本文件即可（纯文档）。

## P1-1 — 死亡逻辑 characterization tests（2026-11-05）

- **执行者**：实现 Agent（P1）
- **文件**：新增 `tests/test_death_logic_contract.py`（770 行，35 用例）。engine.py/policy.py/config.py/db.py/test_logic.py 零改动。
- **变更**：按指导 §7 八条语义锁死现有死亡收敛状态机——①身份=连接参数指纹（改名不分裂行、别名一轮只推 1 次 streak）；②单次成功立即恢复 alive+清零；③连续失败达阈值（默认 3）才 dead；④单轮失败只 pending/保持 unknown 不删除；⑤excluded 与 controller_error 等基础设施错误不计 streak；⑥整轮 suspect 不发布且保留上轮导出、suspect 轮不作基线；⑦无来源/拉取空/Controller 不可达/超时/配置校验失败均不批量推进节点失败；⑧重连迁移不清 nodes/consec_fail/last_ok/rounds/results，追加表不扰动账本。
- **原因**：指导 §15 Phase 1「先锁死死亡逻辑，未通过前不得改 engine 主链路」。
- **影响**：纯新增测试，零产品行为变化。
- **风险**：无。
- **测试**：`python -m unittest tests.test_death_logic_contract -v` → 35 OK。
- **审查**：待独立审查 Agent 复核（见 P1/P2 联合审查条目）。
- **回滚**：删除该文件。
- **记录（§17.8 冲突裁决）**：`_orig_fp` 实际语义=剥 DROP_FIELDS 后做连接参数指纹，不做端口类型归一（"443" 与 443 指纹不同）；真正保护来自 collect_entries/make_testable 只算一次 fp 并复用。测试按 docstring 实际契约断言，未改代码。

## P2-1 — config schema v3 + DB 三表 + 迁移（2026-11-05）

- **执行者**：实现 Agent（P2）
- **文件**：`mihomo_test/config.py`（+312）、`mihomo_test/db.py`（+189）、`tests/test_config_v3.py`（新建，21 用例）、`tests/test_logic.py`（2 处最小断言修正）。
- **变更**：
  1. **config v3**：DEFAULTS 增 `schema_version:3`、`probe_policy`（interval/timeout/concurrency/dead_after_consecutive_failures/round_guard_ratio/round_guard_min_alive）、`probes:[]`（空=本地单探测点兼容模式）、`aggregation`（any_alive/minimum_completed_probes=1/late_result_seconds=90）、`rename`（模板 `{country} | {display_name}`、冲突后缀 ` #{n}`、preserve_original_name）、`gist`（固定三文件名 + token_env=GITHUB_GIST_TOKEN + secret）；substore 增 `target_name=mihomo-probe-alive`、`manual_push_only=true`。
  2. **validate_config_v3**：启动/保存/测试前同一套门禁——manual_push_only 非 true（含缺失）拒绝启动；probes[].id 唯一且匹配 `^[a-z0-9][a-z0-9_-]{1,31}$`；token 只允许环境变量名、禁明文字段；chain.enabled 缺失强制 false；dead 阈值从既有 policy 派生（不清任何 streak）。
  3. **migrate_config_v2_to_v3**：纯函数一次性迁移（probe_policy 六键从 schedule/test/policy 现值派生、显式值不回退；chain 缺失置 false 并报告提示手动开启；幂等 no-op）；迁移前 `config.json.bak-v3-<UTC>` 只备一次。
  4. **DB 三表**（指导 §12 SQL 逐字）：probes / probe_observations / publish_audit，`_add_v3_tables` 幂等创建于 `_migrate` 最早期，首建前 WAL checkpoint + `<db>.bak-v3-<UTC>` 快照只一次；nodes/rounds/results 不重建不清空。新增 upsert_probe/touch_probe_seen/record_observation（幂等覆盖）/get_observations/record_publish_audit/list_publish_audit。
  5. load() 末尾统一 validate（失败拒绝启动）；update() 保存路径过同一门禁；新增 take_migration_report() 供轮次/UI 报告。
- **原因**：指导 §15 Phase 2（§3 配置验证 + §12 数据库迁移）。
- **影响**：旧 config.json 首次启动自动迁移并落盘（有备份）；新增校验失败会拒绝启动/保存（设计意图）；probe_policy 键 engine 暂不读取（v4 收口，plan §4.1）。
- **风险**：低。迁移幂等 + 双备份（config 与 sqlite 各一份 .bak-v3）。
- **测试**：新 21 用例全绿；全量 `python -m unittest discover -s tests` → **661 OK (skipped=2)**（605 基线 + P1 35 + P2 21）；py_compile 四文件 OK；`tools/scan_secrets.py --include-untracked` clean。
- **既有断言修正**：test_logic.py MigrationTest 2 处 glob 排除 `.bak-v3-*`（v3 快照是指导 §12 要求的备份，允许与旧 rebuild 备份并存）。
- **回滚**：git revert 本次改动；删除 `data/config.json.bak-v3-*`、`data/state.db.bak-v3-*` 与三张新表即可（旧版忽略新表）。

## P1/P2-R1 — Wave 1 审查观察项收口（2026-11-05）

- **执行者**：主 Agent（依据独立审查 Agent 8d595bfd 报告观察项 1）
- **文件**：`mihomo_test/config.py`、`tests/test_config_v3.py`。
- **变更**：`validate_config_v3` 的 probes[] 条目校验由「四个已知明文字段名黑名单」升级为「键白名单」——新增 `PROBE_ENTRY_KEYS`（id/name/enabled/endpoint/token_env/weight/tags，指导 §3 探测点形状），未列出的键一律拒绝；新增 1 条用例（authorization/apikey/access_key/cookie 拒绝）。
- **原因**：审查发现黑名单拦不住非标准命名的敏感键，运维手写后会过闸并随 /api/status 到前端（低危，默认 probes=[] 需误配才触发）。白名单 fail-closed，与 normalize_sources 同范式。
- **影响**：手写配置多写键会被拒（fail-closed，方向正确）；DEFAULTS 与迁移产物只产白名单内键，无误拒。
- **风险**：低。
- **测试**：`tests.test_config_v3` 22 OK；全量 **662 OK (skipped=2)**（661 + 新 1）。
- **审查**：审查 Agent 报告 A-G 全通过、无阻断项后实施。
- **回滚**：git revert 本次改动。
- **审查报告存档要点**：A §3 五条代码+测试双侧钉死；B §12 三表 SQL 逐字一致、备份在前、nodes 不重建；C §7 八条 35 用例读断言核实（引擎级全走真实状态机+真实 SQLite，mock 仅在网络/内核边界）；D §13 新键无泄漏路径（本条目收口其唯一缺口）；E CHANGELOG 数字与独立复跑一致（56/661）；F 独立复跑通过；G 迁移必然落盘、既有合法配置不误拒、probes=[] 一致性由全量回归实证。观察项 2（executescript 未显式单事务，与既有 SCHEMA 同范式）与 3（v2 显式 manual_push_only=false 迁移置 true 且有报告，属升级路径合理裁决）记录不改。
