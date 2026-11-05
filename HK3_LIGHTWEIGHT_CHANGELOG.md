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

## P3-1 — 观察聚合层 + engine 接线 + front_dead 语义修复（2026-11-05）

- **执行者**：实现 Agent（P3a）
- **文件**：新增 `mihomo_test/aggregation.py`（约 340 行纯函数）、`tests/test_aggregation.py`（57 用例）；修改 `mihomo_test/engine.py`（12 个函数/接线点）、`tests/test_logic.py`（5 处 front_dead 断言修正）。
- **变更**：
  1. **aggregation.py**：`aggregate_round`（§4.3 三规则：任一活即活 → 全败且达 minimum 才判败 → 不足则 inconclusive；探测点离线/未知/digest 不符/超时未回传 = 观测缺失不计完成数）；`normalize_observation`/`local_observation`（probe_id="local"）/`filter_observations`（round/known_probes/digest 校验，违规只入 rejected 不采用）；聚合完成才对每节点每轮跑一次 policy.apply。
  2. **engine 接线**：`config_digest(cfg)`（DIGEST_SECTIONS 六段，Job 与 API 共用）；`_probe_jobs`/`_dispatch_probe_jobs`（契约钩子，延迟 import probe_dispatch，缺模块/异常 → warn 返回 {} 不让轮次失败）；`_collect_probe_observations`（在 publish 阶段下发 Job → 轮询回传 → 落 local 观测 → 聚合；等待窗口夹紧到轮次剩余预算，探测点全离线不阻塞）；`_converge_bucket` 增可选 observation 参数（inconclusive 不推 streak，写 results 行 inconclusive/insufficient_probes）；`_apply_and_publish` 走 `_round_observations` 聚合。
  3. **front_dead 修复（§5.4）**：`_record_chain_failures` 去掉 policy.apply——results 行照写 fail+front_dead+chain，节点行只刷 display/proto/server/category，status/consec_fail 原样保留。同步修正 test_logic.py 5 处旧断言（3744/3759/3769/4441/4527）为新语义，均带 §5.4 注释。
  4. **灰度第 1 步（§16）**：probes==[] → 不下发不等待，只落一条 local 观测，节点结论与改造前逐字段一致（用例直接对比 `_score_bucket`+`_resolve_exit` 手算结果）。
- **原因**：指导 §15 Phase 3 + §4.3 聚合门禁 + §5.4 前置失败不推进目标死亡计数。
- **影响**：front_dead 不再推进 streak（行为变更，指导明文要求）；多探测点接入但默认 probes=[] 行为不变；本地诊断信息（ip_alive/detail/attempts）仍由 _score_bucket 取。
- **风险**：中。engine 核心改动；已用 57 新用例 + P1 35 契约用例 + 605 基线回归锁死。
- **测试**：`tests.test_aggregation` 57 OK；全量 **770 OK (skipped=2)**（662 + P3a 57 + P3b ~51）。
- **回滚**：git revert 本批。
- **与 P3b 的接口裁决（§17.8）**：① dispatch_round 返回 `{probe_id: "ok"|"err"}`，`_dispatched_probe_ids` 兼容、err 状态绝不等待；② probe_observations.payload_digest 存 ProbeResult 整体 sha256 而非 config_digest，config_digest 校验交还 API 层（verify_result 在唯一拥有完整报文处比对报文自带 config_digest），本地单探测点模式下引擎侧才严格拒绝。已记入代码注释与用例。
- **已知限制**：真实多探测点端到端需 agent CLI + 可达探测点（P3b 交付后联调）；无 docker，compose 集成 NOT_RUN。

## P3-2 — 探测点分发 + Probe Agent 模式 + Controller API（2026-11-05）

- **执行者**：实现 Agent（P3b）
- **文件**：新增 `mihomo_test/probe_dispatch.py`（525 行）、`tests/test_probe_dispatch.py`（1138 行，53 用例）；修改 `mihomo_test/server.py`（+约 330 行）、`mihomo_test/__main__.py`（agent 子命令）。engine.py/aggregation.py/config.py/db.py/web/ 零改动。
- **变更**：
  1. **probe_dispatch**：`dispatch_round`（urllib POST 每 Job 到 endpoint+"/jobs"；Bearer token 取自 probes[].token_env + X-Probe-Timestamp + X-Probe-Signature（HMAC-SHA256，body+5 分钟时间窗）+ Content-Type；超时/连接失败/5xx/缺 token/非 HTTPS endpoint 全部 warn 后 continue 不抛，观测缺失由聚合层当「未完成」兜底；token 与请求体凭据从不进日志）；`verify_result`（BAD_PAYLOAD→ROUND_STALE→UNKNOWN_PROBE→DIGEST_MISMATCH→RESULT_LATE 五项校验，违规只 db.log 审计、绝不落观测/推进死亡计数；合法则 record_observation 幂等 + touch_probe_seen；deadline 落盘 data/probe_deadlines.json 跨重启仍拦过期回传）；`sign_request/verify_signature`（纯函数，compare_digest）；`config_digest`（优先 engine 版，回退六 section 逐字对齐，等价性有测试钉死）。
  2. **server /api/v1 路由 11 条**：读（admin）dashboard / rounds?limit / rounds/{id} / nodes?q&status&proto&source&page&page_size / probes（db∪cfg 合并视图，不回显 token）；写（probe token，Bearer 常量时间比较，与 admin 两套互不通）heartbeat / jobs/{round_id}/result；写（admin）gist/publish + substore/{preview,confirm-token,push}（501/PUSH_CONFIRM_REQUIRED 占位，P4/P5 落地，写 publish_audit）。写接口统一鉴权+Content-Type(415)+体积上限(413)+审计 ID(uuid4)+稳定错误码。
  3. **agent 模式**：`python -m mihomo_test agent --id --token-env [--port --controller ...]`；GET /healthz、POST /jobs（验签后 202、后台单工作线程、并发 409 BUSY）→ core.prepare/build_config 到临时目录 → 一次性内核容器 → Core.delay 逐节点 → 组 ProbeResult（egress_country=null）→ 回传 Controller → finally 停容器+rmtree 临时配置、启动清扫；agent 分支先于 cfgmod.load()——不连 Sub-Store、不写 Gist、不读业务配置。
- **原因**：指导 §15 Phase 3 + §4.1/§4.2 Job 下发/结果回传 + §10 API 契约 + §13 安全。
- **影响**：新增面，不改既有 /api/* 路由与状态机；payload_digest=整个 ProbeResult 的 sha256，config_digest 校验在 API 层（与 P3a 分工）。
- **风险**：低（全部新增代码路径，engine 侧降级/异常均 warn 不失败）。
- **测试**：本模块 53 OK；全量 **772 OK (skipped=2)**（770 + 2 个 digest 对齐用例）；scan_secrets clean；socket stderr 噪音 0（修了 addCleanup LIFO 与超时桩点两处根因）。
- **回滚**：git revert 本批；data/probe_deadlines.json 可删，不影响账本。
- **交接**：dashboard 的 gist/substore 时间戳与 gist/publish、substore 三口为 501/null 占位，P4/P5 填。

## P4-1 — canonical 订阅产物 + Gist 固定四文件 + /api/v1/gist/publish（2026-11-06）

- **执行者**：实现 Agent（P4a：canonical_export/gist_store/engine 接线工作区半成品；P4b：本批收口——meta 透传、§8 幂等门禁、API 端点、dashboard 接线、测试）
- **文件**：新增 `mihomo_test/canonical_export.py`（274 行）、`mihomo_test/gist_store.py`（392 行）、`tests/test_canonical_export.py`（217 行）、`tests/test_gist_store.py`（387 行）；修改 `mihomo_test/engine.py`（canonical 接线 + `_publish_gist`）、`mihomo_test/server.py`（端点 + dashboard gist 块 + 磁盘产物读取）、`tests/test_probe_dispatch.py`（占位测试替换为 3 个端点用例）。
- **变更**：
  1. **canonical_export.build**（§4.4）：多来源 (proxies, groups) → 一份去重（连接参数指纹）、重命名（模板 {country} | {display_name}，冲突 #2/#3，只动 name 字段）、固定排序（country→protocol→normalized name→fingerprint）的 YAML；YAML 正文无每轮易变字段（内容不变 ⇒ sha 不变）；`source_aliases` 记多源同指纹。
  2. **engine 接线**：`_write_canonical_export` 只聚合 `export_keys` 内来源（muted 源不得从 canonical 后门出去）；产物写 `data/exports/mihomo-alive.yaml` + `.meta.json`（原子 tmp+replace）；`cleanup_exports` 保护 canonical 不被按源清理误删；`_apply_and_publish` 在发布成功后调 `_publish_gist`（fire-and-forget：失败只追加 note「本轮完成，发布失败」+ audit，绝不影响账本结算）。
  3. **gist_store.publish**（§4.5）：固定四文件（canonical/status/history/README）；GET 读远端 → **以 canonical 文件 sha 为幂等门禁** → 未变化零 PATCH 零 revision，变化则 PATCH 只含变化文件；首次空 Gist 一次 PATCH 建齐；429/5xx 有界退避、401/403/404/422 终态；token 仅从 `gist.token_env` 环境变量（回退 GITHUB_TOKEN_FILE）读；`mask()` 覆盖 PAT/Bearer/查询参数/十六进制/UUID；transport 注入测试不触网；history 滚动截断 100 轮。
  4. **§17.8 裁决（revision 语义）**：指导 §8「内容 digest 未变化时不提交新 revision」钉在 **canonical 文件**上——status/history/README 是随行文件，只在 canonical 变化的同一 PATCH 里更新；否则带 updated_at 的 status 会让每轮都产生新 revision，幂等门禁名存实亡。最新轮状态由 /api/v1/dashboard 实时给出。
  5. **meta 透传（P4b 修正）**：`_write_canonical_export` 返回 `(content, meta)` 成对直达发布器（此前传 meta=None 会丢 digest，status/README 与幂等门禁全瞎）；engine 在 status payload 显式写 digest。
  6. **POST /api/v1/gist/publish（P4b，501→落地）**：admin 鉴权 + JSON 门禁；从磁盘读 canonical 产物发布（不重渲染，§8 去重保证）；无产物 409 `NO_CANONICAL_ARTIFACT`、gist 关闭 409 `GIST_DISABLED`、配置错误 409、上游失败 502 `GIST_PUBLISH_FAILED`（error_code 透传）；每次调用（含拒绝）落 publish_audit；dashboard `gist` 块改由 publish_audit 供给（last_published_at/revision/consistent_with_stable_round），只统计 published/no_change。
- **原因**：指导 §15 Phase 4（§4.4/§4.5/§8/§10）。
- **影响**：订阅出口收敛为唯一 canonical 产物；Gist 一个 revision = 一次真实节点集变化；Sub-Store 仍零自动写（P5 前维持）。
- **风险**：中。engine 发布区改动；P1 契约 35 用例 + 全量回归锁死。
- **测试**：`tests.test_gist_store`+`test_probe_dispatch`+`test_canonical_export` 96 OK；全量 `python -m unittest discover -s tests` → **815 OK (skipped=2)**（809 基线 + P4 净增 6）；`cargo test --locked --workspace` 0 failed（未触 Rust，例行复跑）；`cargo fmt --all -- --check` 通过；`cargo clippy --locked --workspace --all-targets -- -D warnings` 通过；`tools/scan_secrets.py --include-untracked` clean。
- **审查**：待独立审查 Agent 复核（下一波）。
- **回滚**：git revert 本批；删除 `data/exports/mihomo-alive.*` 即可（Gist 侧内容由远端保留，无本地状态需回滚）。
- **交接**：dashboard 的 substore 块仍 null、/api/v1/substore/* 仍 501 占位 → P5；Rust 侧 canonical/Gist/聚合模型缺口记入 `workstreams/21_RUST_PARITY.md`（G-1..G-5）。

## P5-1 — 人工 Sub-Store 推送三步 + confirm-token 防重放 + 删除自动回灌链（2026-11-06）

- **执行者**：实现 Agent（P5）
- **文件**：新增 `mihomo_test/manual_publish.py`（约 200 行）、`tests/test_manual_publish.py`（约 190 行，17 用例）；修改 `mihomo_test/server.py`（三端点落地 + `/api/push` 收编 + 错误码表）、`mihomo_test/engine.py`（删轮末自动 push）、`tests/test_probe_dispatch.py`（占位测试替换为 4 个端点用例）、`tests/test_logic.py`（PushEndpointTest 重写为新契约）、`tests/test_hardening.py`（/api/push 400 门用例改为 409 token 门）。
- **变更**：
  1. **manual_publish.py**：`diff_preview`（指纹身份；改名不成对呈现；样例只含 name/type，§13.7 凭据不进预览/日志）；`issue_confirm_token`/`consume_confirm_token`（60s 一次性，绑定 target+digest+节点数；同 target 重签立即作废旧 token；已用/过期/绑定不符/未知分别返回 `CONFIRM_TOKEN_REUSED`/`CONFIRM_TOKEN_EXPIRED`/`CONFIRM_TOKEN_MISMATCH`/`PUSH_CONFIRM_REQUIRED`，进程内注册表 + 锁，防重放）；`push_canonical`（store.update_only，目标缺失 → `SUBSTORE_TARGET_MISSING` 拒绝新建）；`parse_proxies`（非法 YAML → `BAD_CONTENT`）。
  2. **三端点**：`POST /api/v1/substore/preview`（只读 diff；目标不存在如实标注 missing_target，非错误）；`confirm-token`（签发并审计 confirm_issued，token 值不入审计摘要）；`push`（消费 token → update-only → 审计 pushed/rejected_*/publish_failed）。错误码表新增 `NO_CANONICAL_ARTIFACT`/`GIST_DISABLED`/`PUSH_CONFIRM_REQUIRED`/`CONFIRM_TOKEN_*`/`SUBSTORE_TARGET_MISSING`(409)、`SUBSTORE_ERROR`(502)、`BAD_CONTENT`(400)，移除 `NOT_IMPLEMENTED`。
  3. **/api/push 收编**：删除旧「一键推送 per-source 导出」契约（publish.enabled 400 门一并移除），与 v1 push 共用 `_substore_push_core`（无 token 一律 409）。
  4. **删除自动回灌（指导 §0.1/§0.6）**：`_publish_sources` 的 `push_exports` 轮末调用点删除——Gist 发布、scheduler 轮末与 Sub-Store 写再无耦合；`push_exports` 本体保留为 CLI/旧门面（双开关门禁不变）。
  5. **§17.8 裁决（token 绑定维度）**：指导 §9.4 写「绑定 target+revision+digest」；canonical meta 不携带 Gist revision，故绑定实现为 **target+digest+节点数**（digest 即内容身份，节点数防「同 digest 不同规模」的构造混淆）；若后续 meta 增加 revision 可无缝加绑。
- **原因**：指导 §15 Phase 5（§4.6/§9.4/§10/§13.5）。
- **影响**：Sub-Store 唯一写路径 = 人工预览+二次确认+60s token；自动推送路径已删并有测试钉住不可达；`24h 零写`保证从「双开关默认关」升级为「结构上无自动写」。
- **风险**：中。/api/push 契约变更（旧客户端将得到 409 而非 400/200，属预期破坏性变更）；web/app.js 的「一键推送」按钮在 P6 重构前对新契约显示失败提示，属诚实降级。
- **测试**：`tests.test_manual_publish` 17 OK；`test_probe_dispatch`/`test_logic`/`test_hardening` 相关用例更新后全绿；全量 `python -m unittest discover -s tests` → **832 OK (skipped=2)**（815 + P5 净增 17）；`tools/scan_secrets.py --include-untracked` clean；`cargo test --locked --workspace` 28 个测试目标全 ok、0 failed（Rust 树本批零改动，例行复跑）。
- **审查**：待独立审查 Agent 复核（下一波）。
- **回滚**：git revert 本批。注意：回滚后轮末自动回灌恢复依赖 `push_to_substore`+`allow_legacy_substore_write` 双开关（默认关），无静默写风险。
- **交接**：Web 发布中心（Gist 区+Sub-Store 区 UI、二次确认弹窗）→ P6；`workstreams/21_RUST_PARITY.md` G-4 随本批从「无」变「Python 已落地、Rust 待移植」。
