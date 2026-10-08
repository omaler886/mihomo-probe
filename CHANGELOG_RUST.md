# CHANGELOG_RUST

Rust 全量重构的变更台账。每批含影响范围与回滚方法；总控文档见
`GLM_5.3_Flash_mihomo_probe_Rust_full_rewrite.md`，逐工作流状态见 `workstreams/`。

格式：每批一节，`影响范围` / `回滚` 必填。

---

## R0 — 基线审计 + P0 整改（2026-09-30）

基线：HEAD 03cb6ea，Python 3.12.10，`python -m unittest discover -s tests -v`
→ **Ran 554 tests, OK (skipped=2)**（基线输出留档于会话诊断，不入库）。

### 新增
- `workstreams/00..16*.md`：17 个工作流工作台（审计结论、设计决策、待办、证据）。
- `GLM_5.3_Flash_mihomo_probe_Rust_full_rewrite.md` 入库（总控文档）。
- `SECURITY_CREDENTIAL_ROTATION.md`：凭据轮换台账（只记位置与动作，不记值）。
- `tools/scan_secrets.py`：secret 扫描门禁（工作区 + `--history` 全历史；本次运行：工作区与 13 个历史提交均 clean）。
- `tests/test_hardening.py`：StatusRedactionTest（4 例）、MaskablePatchTest（3 例）、
  MixedPortDefaultTest（2 例）、QueryTokenCompatTest（3 例）。

### 修复（P0）
1. **/api/status 泄密（SECURITY_REVIEW S-12 → P0）**：`server.redacted_config` 原先只掩
   `auth.token`/`publish.token`，Telegram bot token、webhook URL、Sub-Store 后端密钥路径
   随 5 秒轮询明文外送。现全部掩码（backend 保留 scheme+host、路径掩码）。
   影响范围：仅 `/api/status` 响应；`exports[].url` 仍带 publish token（面板复制用途，读-only）。
   配套：`config.validate_patch` 对 `config.MASKABLE_PATHS` 丢弃哨兵 `"***"`——
   设置表单回读掩码值再保存**不会**把真实凭据覆盖成掩码。
2. **core.mixed_port 缺省 KeyError（ARCHITECTURE 陷阱清单 / 总控 §2.3）**：
   `DEFAULTS["core"]["mixed_port"]` = `MIHOMO_TEST_MIXED_PORT` 环境变量或 19194
   （部署文档现网值）；`NUMERIC_BOUNDS` 增加 `(1024, 65535)` 夹取。
   新装环境首轮不再死于 `core.build_config` 直接下标。
3. **query token 标记兼容模式（总控 §2.3）**：鉴权三通道保持；新增 `server.auth_kind`
   区分 (query|header)×(admin|publish)。admin 域经 query token 认证 → 每进程一次 warn
   日志引导迁移 `X-Auth-Token`/`Bearer`；publish 域（/api/export/*、/api/probe/nodes）
   的 query token 是 Sub-Store 既定集成面，不受影响。401 hint 改推头部。
4. **install.sh 测试门禁（总控 §2.2）**：原 `docker exec … unittest … | tail -3 || true`
   —— 管道吞退出码 + 硬吞失败，红套件照常部署。改为构建后、替换现网栈之前用
   `docker compose run --rm --no-deps` 一次性容器跑套件，POSIX 安全取 rc，失败 exit 1
   且现网栈不动。
5. **Docker Socket 退役方案（总控 §2.3 / ADR-0002）**：三步走写入 `workstreams/12`；
   本批不改 compose 行为（Python 期保持现网稳定）。

### 文档一致性
- README / MIGRATION_GUIDE / ARCHITECTURE 中「mixed_port 不在 DEFAULTS、新装必踩」的
  迁移陷阱条目标注 R0 已修复（保留原文描述以留档陷阱成因）。

### 影响范围
- Python 默认实现的行为变化仅限：/api/status 掩码扩展、401 hint 文案、admin query token
  一次性 warn 日志、新装环境 mixed_port 有默认值。轮次/导出/联动逻辑零变化。
- 554+12 测试全部通过后提交；Rust 尚未引入。

### 回滚
- `git revert` 本提交即回到 03cb6ea 行为；无数据/格式迁移，无状态兼容问题。

---

## R1 — Rust workspace + 首条纵向切片（2026-09-30）

切片链路：`probe-cli` 读 config.json（兼容 Python 部署的 config 格式）→ 生成内核配置
（与 core.build_config 逐字段一致）→ 调 Mihomo Controller（/version、PUT /configs?force=true）
→ 写入一条 round（SQLite）→ `GET /api/v1/status` 返回。默认 Python 服务不受影响
（Rust 不接流量，shadow 形态）。

### 新增
- `Cargo.toml` + `Cargo.lock`（提交锁定，CI `--locked`）。
- `crates/probe-domain`：RoundId/RoundStatus/RoundSummary 最小领域模型（不依赖 IO）。
- `crates/probe-config`：兼容读 `data/config.json` + DEFAULTS 语义（含 mixed_port 默认 19194、
  core.secret 文件读取）；缺字段回退默认而非报错（对齐 Python `load()` 容错）。
- `crates/probe-mihomo`：ConfigBuilder（lanes/IN-NAME 规则/inline-JSON proxies 逐字段对齐）、
  `ReasonClassifier`（对拍 Python `_reason_from` 的 timeout/kernel_error/bad_request/
  unreachable/http_<status> 表）、Controller 客户端（version/reload，Bearer secret）。
- `crates/probe-storage`：rusqlite(bundled)（ADR-0001：Windows 可复现构建；R2 复评 sqlx），
  rounds 表读写（与 Python schema 同列）。
- `crates/probe-api`：axum `/healthz` `/readyz` `/api/v1/status`（零秘密）`POST /api/v1/rounds`；
  Bearer 鉴权；loopback 默认。
- `crates/probe-cli`：`serve` / `round` / `status` 子命令。
- `.github/workflows/ci.yml`：python（py_compile + unittest + secret scan）与 rust
  （fmt --check / clippy -D warnings / test --locked / secret scan）双 job 门禁。
- `TEST_REPORT_RUST.md`：cargo fmt/clippy/test 真实输出留档。

### 本机环境注记（如实记录）
- 本机默认工具链 x86_64-pc-windows-gnu 缺 C 编译器，rusqlite bundled 无法构建；
  本机构建用 `cargo +stable-x86_64-pc-windows-msvc`（VS 2022 在位）。CI（ubuntu）不受影响。
- serde_json 启用 `preserve_order`：Python `json.dumps` 按文档序输出 proxy 字段，
  生成的内核配置须与 Python 逐字节可比（快照测试锚定）。
- 冒烟：临时根上 `probe-cli status/round/serve` 全链路通过（round 行如实闭合、
  /api/v1/status 零秘密、401/202、WAL、core.secret 自动生成），详见 TEST_REPORT_RUST.md。

### 影响范围
- 全部为新增目录；不修改任何 Python 文件、不改动 compose。Python 服务照常部署运行。

### 回滚
- 删除 `crates/`、`Cargo.toml`、`Cargo.lock`、`TEST_REPORT_RUST.md`（后续批次按同法）。

---

## R2 — SQLite schema 与兼容迁移（2026-09-30）

### 新增
- `migrations/0001_python_compat.sql`：Python 兼容基础 schema（nodes/rounds/results/events/
  ip_geo/domain_views + 全部索引，与 db.py 逐字段一致，全部 IF NOT EXISTS——Python 写的库直接
  打开，Rust 建的库 Python 也能读）。
- `migrations/0002_probe_ledger.sql`：Rust 侧增量表（node_state_history / export_snapshots /
  config_audit / security_audit），纯 additive，不动 Python 写的任何表。
- `crates/probe-storage/src/migrations.rs`：版本化迁移运行器（`schema_migrations` 登记、
  逐版本事务、`include_str!` 编译期内嵌防漂移、`applied()` 只读）。
- `probe-cli db check|migrate|verify|backup|rollback`：
  - migrate = 自动在线备份（SQLite backup API，WAL 一致性快照）→ 应用 → verify；
  - rollback --from --yes = 恢复备份（存在 `-wal` 时拒绝——防止活写入者下恢复主文件造成损坏）；
  - check = 只读报告（已应用版本/pending/integrity/表计数），绝不顺手迁移。
- ADR-0003（workstreams/08）：`sources` 表与 `observations` 改名推迟到 R10 双跑决策——
  兼容期 config.json 仍是源注册表、`results` 仍是共享逐轮表，Python 读取方不受影响。

### 测试（cargo test --workspace --locked，30 通过）
- 全新库：两条迁移依序应用、幂等重放不重复。
- Python 旧库（legacy nodes/rounds/results 数据）迁移后：数据逐行保全、新表就位、
  integrity ok、Python sqlite3 仍可读。
- 备份一致性：备份后写入不进快照。
- open_without_migrating 对缺表库如实报错而非编造零。

### 端到端演练（本机，Python 形态 legacy 库 → Rust CLI）
`db check`(0 applied/2 pending) → `db migrate`(自动备份→2 applied→counts 含 legacy 数据)
→ `db verify` ok → Python sqlite3 读回 nodes/rounds/results 原值 → rollback 守卫
（缺文件 rc=1 / 缺 --yes rc=2 / --yes 恢复到迁移前快照）。

### 影响范围
- 仅 Rust 侧新增；Python 服务、现有部署数据零改动。`Storage::open` 现在自动迁移
  （对 Python 建的库即补 Rust 增量表）。

### 回滚
- `git revert` 本提交；已迁移的数据库无需回滚（增量表对 Python 无害），
  如需彻底还原用 `db migrate` 产出的 `.bak-*` 快照。

---

## R3 — Mihomo 控制器补全（2026-10-01）

### 新增（全部在 crates/probe-mihomo）
- `lanes` 模块：lane_count/lane_ports（夹取 1..=32、base_port+i）、组名 `__LANE<i>__`、
  监听名 `lane<i>` 与 Python 逐字符一致（shadow diff 可读性）。
- `exit` 模块：`egress(port, trace_url)` 经车道 loopback 入站拉 Cloudflare trace
  （reqwest Proxy::all，任何完整 HTTP 响应算过；空 trace 判失败）；
  `fetch(port, url, cap)` 真拉流（读上限防下载，拨号错/超时/TLS reset 为 Err ≤160 字符）；
  `parse_trace` 纯函数。这组能力是 R5 链式真实拉流校验与 R6 出口验证的地基。
- `config_check` 模块：`mihomo -t` 校验器——优先 `MIHOMO_BIN`（固定 argv、120s 超时+kill），
  其次 docker run 固定镜像（CLI 探测缓存 60s），都没有时返回 **Degraded**（与 Python 的
  "无 docker 静默跳过" 相反：降级必须显式记录，workstreams/04 既定语义）；
  `culprit_from` 移植 `_culprit_from`（引号名 > 长名边界匹配 > server 兜底），含 "jp" 误匹配
  回归测试；`find_word` 手写边界检查，零 regex 依赖。
- Controller 增 `select(group, name)`（PUT /proxies/{group}，R5 车道钉扎用）。

### 范围说明（如实记录）
- `make_testable` 完整裁剪循环**不在本批**：它依赖引擎的 entry/fp 数据模型（R5 一并落地）；
  本批交付其全部内核侧原语（config build/check、culprit 定位、delay、select）。

### 影响范围
- 仅 probe-mihomo 新增模块；Python 侧零改动。

### 回滚
- `git revert` 本提交（probe-mihomo 回到 R2 形态，切片功能不受影响）。

---

## 修复 — 车道 select group 分桶（2026-10-08）

> **不是 R 批次**，是一次生产事故修复；Rust 侧同步了同一改动。
> 代码在**前一个提交**（`fix(engine): 车道 select group 只列本车道节点`），
> 本节与该提交一起入账。细节见 `.workbuddy-ai/memory/2026-10-08.md` 与
> `reports/lane-split-fix-20261008.html`。

### 问题
线上 287 轮全部 `aborted: CoreError: TimeoutError: timed out`（2026-09-29 03:57Z 起，
面板与导出停在旧快照 9 天，看起来像"大量测出来实际不可用"，实为测活完全停摆）。
根因：`build_config` 给**每条车道**列出**全部节点** → 486 节点 × 16 车道 = **7776 个
组成员** → 459KB 配置 → `PUT /configs?force=true` 超过 HTTP 客户端 20s 超时 → 整轮作废
（`total=0`，一个节点都没测）。

### 修复
- Python `core.build_config`：车道 group 只列本车道切片 `names[i::lanes]`（空则落 `DIRECT`）。
- Python `engine`：`_run_round` 新增 `lane_of`（`index % lanes`，与配置同源）；
  `_verify_egress` / `_verify_chain_payload` 据此分桶。原先按 `queue[i::lanes]` 分桶，
  而 `queue` 是 `mapping` 的**子集且重排**，与配置位置不一致时 `core.select` 会在错误车道
  失败 → 节点静默丢失出口验证。
- Rust `probe-mihomo`：新增 `lanes::lane_members`（同一 `index % lanes` 切分），
  `build_config` 改用之；补 2 个测试。顺带修掉该函数引入的 clippy `needless_lifetimes`
  （说明上一批没跑过 clippy）。
- 测试：`test_every_lane_lists_every_proxy`（断言的正是导致事故的旧布局）重写为划分断言
  `test_every_proxy_lands_in_exactly_one_lane_group`；`test_live.py` 新增 `lane_members()`
  并改造两条车道用例。

### 效果
组成员 7776 → 486（−93.8%）；配置 459,518 → 173,772 字节（−62.2%）。
线上第 793/794/795 轮连续成功（`ok` = 224 / 209 / 212）。
Rust 侧新增 2 个测试（`lane_members` 的划分与边界用例）：全量 219 → 221。

### 已知限制
- 线上代码仍落后本地 HEAD（本批只部署了 `core.py` + `engine.py`）。
- `chain.front_pick` 只挑中 1 个前置（`max_fronts=3` 形同虚设），该前置一挂整轮链式全
  `front_dead`。

### 回滚
- `git revert` 该提交；线上回滚需
  `docker compose up -d --build --force-recreate mihomo-test`。

---

## R4 — probe-dns：DoH + ECS + 二进制 fixture（2026-10-08）

> **本批是补做批次**：R0–R3 后 R4 被跳过、直接进了 R5–R7；`crates/probe-dns/` 一直以
> **未提交的工作区状态**存在（未 `git add`，`Cargo.toml` 的成员行与 `Cargo.lock` 条目也未提交）。
> 本批把它整理、补齐、入账，所以提交时间晚于 R5–R7，内容属于 R4。

### 新增（全部在 crates/probe-dns）
- `wire` 模块：手写 DNS 报文，逐字段对齐 `mihomo_test/doh.py`——`build_query`（随机 id、
  RD 标志、OPT 记录 + ECS option、label 长度前缀编码）、`parse_ips`（QD/AN 计数、
  `skip_name` 跳过压缩指针、A=4B / AAAA=16B）。
- `resolver` 模块：`DohResolver::query`（GET `?dns=<base64url-nopad>`、
  `Accept: application/dns-message`）、`resolve_views`（逐视角 A+AAAA、视图内去重保序、
  失败视图产空列表）、`ViewConfig`。`rand_id()` 用 `getrandom::fill` 对齐 `os.urandom(2)`。
- `geo` 模块：`GeoClient::fetch_batch`——`ip-api.com/batch` 的 HTTP 半边，
  `urlopen(timeout=45)` 的等价物、失败重试一次（间隔 2s）、错误文本截 80 字符。
  **分块（90/批）与缓存归调用方**，与 Python 的分工一致。
- `fixtures/dns-packets/`：8 个二进制包 + `tools/make_dns_fixtures.py`（可复现生成）——
  A 记录（owner name 走压缩指针）、AAAA、CNAME→A、NXDOMAIN、恶意 rdlength、
  自指针 question、坏 label 长度、不足 12 字节的短包。

### 对 Python 的三处**有意**硬化（`workstreams/05` 已记）
1. 非零 RCODE → `WireError::Rcode`，不再与"空 answer"混同（Python 返回空列表）。
2. 包被截断 → `WireError::Truncated`，不再返回"部分答案"（Python 遇短记录 `break`）。
3. `MAX_NAME_JUMPS` 具名上限（Python 靠 `offset >= len(data)` 兜底）。
   调用方可见行为不变：`resolve_views` 对"查询失败"与"无记录"同样产空视图。

### 测试（cargo test --workspace --locked，243 通过 / 0 失败）
- `probe-dns` 22 通过：`wire` 18（含 8 个 fixture 用例）、`geo` 3、`resolver` 1。
- 全量 **243** = R7 后 219 + lane 修复 2（`lane_members` 的两个划分用例）+ 本批 22。
- `fmt --all -- --check` 无输出；`clippy --workspace --all-targets -- -D warnings` 零告警。

### 独立审查（只读 subagent，两轮）发现并修正的 11 项
**第一轮**（整理后立即跑）：
1. **[中] `GeoClient::fetch_batch` 无超时**：Python 是 `urlopen(timeout=45)`，缺了它一个
   挂住的端点会拖死整轮。已加 `BATCH_TIMEOUT_S = 45`。
2. **[中] 畸形 JSON body 不重试**：Python 的 `json.load(resp)` 与请求在同一个 `try` 里，
   所以 body 解析失败**也会重试**；Rust 原本用 `?` 直接返回，恰在"端点偶发返错页"这个
   场景上与 Python 分歧。已改为与传输错误同一路径重试，并加对照组测试。
3. **[中] `lib.rs` / `Cargo.toml` 有现在时假陈述**：原文写"engine owns the caches and the
   expansion (`classify_and_expand`)"，但 `probe-engine` 根本没依赖 `probe-dns`。
   已改为如实标注"未接入"。
4. **[中] 「坏标签」fixture 缺失**：md 待办点名要、docstring 也自称覆盖，但实际没有。
   已补 `bad-label-overrun.bin`（label 长度 0x41 = 65，既非合法 label 也非指针）。
5. **[低] `parse_ips` / `MAX_NAME_JUMPS` 的 doc 不准确**：前者说指针环会 `None → Truncated`
   （实际自指针 → `Ok([])`）；后者说"each hop must make progress towards the buffer start"
   （不跟随指针，该句是从"会解引用"的实现抄来的）。均已改正。
6. **[低] 冗余代码与依赖**：`skip_name` 与 `read_name` 的实现在整理前的原文里**逐字相同**
   （后者 doc 却声称 "following compression pointers with loop protection"），已合并为一个；
   `probe-domain` / `serde` / `tracing` 三个依赖全未使用，已删。

**第二轮**（修完后复核，抓到第一轮的漏网与我自己的新错误）：
7. **[中] `encode_name` 的 doc 仍是现在时假陈述**：写"idna-encode non-ASCII labels"，
   实现却是拒绝——第一轮清理假陈述时的漏网。
8. **[中] 顶注把两种情形混为一谈**（**第一轮修复时我自己引入的**）：写"a self-pointing
   pointer costs a single step and the walk then ends on the packet's own bounds"——自指针是在
   指针分支**直接返回**，与 `MAX_NAME_JUMPS` 无关；"Python relied on its buffer check"对自指针
   也不成立。已改为区分"指针一步结束"与"畸形 label 链被上限截断"。
9. **[中] 合法但非数组的 JSON 会被重试**（本次修复的副作用）：Python 的 `json.load` 会接受
   非数组 body（随后在 `lookup_countries` 的 `row.get` 上崩），Rust 直接反序列化到 `Vec`
   则归入重试。已在 doc 记为**有意的选择**（更严格的一侧）。
10. **[中] `query` 的 timeout / `ViewConfig.ecs_prefix` 没有 Python 默认值**（8 / 24），
    接入方必须显式补否则语义漂移。已加 `DEFAULT_TIMEOUT_S` / `DEFAULT_ECS_PREFIX` 常量。
11. **[低] 若干措辞/边界**：`MAX_NAME_JUMPS` doc 的 off-by-one（64 次迭代 → 最多 63 个标签）；
    `parse_ips` doc 的"malformed rdata is skipped"与"runs short is an error"自相矛盾；
    `geo` 的 `query` 过滤用 `as_str` 而非 Python 的真值判断（空串未丢）；`lib.rs` 说
    "probe-source's crate doc points here"属夸大。均已改正。

**审查者明确无法验证的一项**：`skip_name` 与 `read_name` 在合并前「逐字节相同」——
因这批代码此前未入库（`?? crates/probe-dns/`），无基线可 diff。依据是整理时读到的原文；
现状自洽，且与 Python 的单一 `_skip_name` 结构一致。

审查确认成立的：8 个 fixture 的字节逐一手工解析后与全部断言相符；`BATCH_TIMEOUT_S` /
重试次数与间隔 / 80 字符截断与 Python 逐项一致；删依赖无遗漏且 `serde_json` 够用
（`reqwest` 的 `.json()` 自带 serde 支持）；`rand_id` 对齐 `os.urandom(2)`；`build_query`
显式 qid 不影响双跑对账（id 本就随机且从不与响应比对）；`ecs_option` 的 prefix 边界等价；
`resolve_views` 对失败 query 的最终视图与 Python 一致（都无条件产出该视图，可能为空）。

### 影响范围
- 全部为新增 crate；`Cargo.toml` 只加一行成员、`Cargo.lock` 只加 probe-dns 条目。
- **Python 零改动**；无调用方，默认路径行为不变。

### 已知限制（如实记录）
- **未接入任何调用方**：`probe-engine` 未依赖 `probe-dns`，`classify_and_expand` 未实现，
  一轮仍"一个 server 测一个地址"。R4 剩下：engine 接入 + `domain_views`/`ip_geo` 缓存 +
  any/all 聚合。
- **无日志接缝**：返回 `Result<_, String>` 而不自己记日志，与 Python 分工一致。
- **非 ASCII 域名**：Python idna 编码成功、Rust 拒绝（无 `idna` 依赖），是真实分歧。
- **fuzz 未做**：md 目标是"fixture 与 fuzz"，本批只交付固定 fixture，未接 `cargo-fuzz`。

### 回滚
- `git revert` 本提交（回到没有 `crates/probe-dns/` 的状态）；无数据/格式迁移，
  Python 侧无影响。
