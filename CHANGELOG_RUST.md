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

---

## R20 — 整轮护栏语义修正（2026-10-21）

**触发**：ct186 线上复核发现 **36/36 轮全部 `inconclusive`**。根因：一个长期 500 的上游
（saki）使 `preserved_sources` 每轮非空 → 每轮都按 ADR-0005 以外的条件标记 inconclusive。
连锁后果有两个，都是生产级缺陷：

1. **存活下限护栏被永久禁用**：`suspect = !inconclusive && round_is_suspect(...)`，
   inconclusive 恒真 ⇒ suspect 永远为 false ⇒ 即使一轮 ok=0/275 也照常发布。
2. **存活基线链被清空**：`previous_alive_count` 跳过 suspect 与 inconclusive 轮，
   全部轮次被跳过 ⇒ 基线恒为 0 ⇒ 即便护栏可达，`round_is_suspect` 对 `alive_prev<=0`
   也恒返回 false（首轮永不 suspect 的设计）。双重失效。

### 修复（`crates/probe-engine/src/round.rs`）
- `inconclusive` 收窄回 **ADR-0005 写入条件本身**：`!has_commit || ok+failed == 0`
  （内核不可达 / 拒绝 reload / 空测量 / 取消 / 工作线程崩溃 / controller_error / 落账失败）。
  **数据源拉取失败不在清单上**：被测过的节点仍是有效测量。
- 数据源拉取失败改为**在 note 里点名**（`; sources not fetched: saki`），节点状态照旧保留
  （不变量未变），操作者从轮次行就能看到窟窿，而不是一句笼统的 "incomplete coverage"。
- `GuardDecision::MarkRoundInconclusive` 的 doc 与 CLI 文案同步更正
  （旧文案 "incomplete coverage; affected sources require previous exports" 已失真）。

### 修复（运行报错与文案，`crates/probe-cli` / `crates/probe-api`）
- serve 启动横幅删掉失真的 "shadow; Python remains the default implementation"——
  ct186 上 Rust 已是唯一实现，该句让线上日志自相矛盾。
- `/metrics` 的 `probe_rounds_inconclusive_total` HELP 同步收窄语义。

### 测试
- 新增 `a_fetch_failed_source_is_named_but_not_inconclusive`（健康轮 + saki 拉取失败 →
  ApplyConvergence、note 点名、轮次行 inconclusive=0）。
- 新增 `a_fetch_failed_source_does_not_mask_the_suspect_guard`（基线 10 + saki 拉取失败 +
  ok=0 → 仍必须 suspect/PreservePreviousState）——本批的回归用例。
- 更新 `uncertain_dns_preserves_the_node_and_isolates_its_source`：节点级保留不变
  （streak 不推进、状态原样），轮级从 MarkRoundInconclusive 改为 ApplyConvergence
  （该轮测到了 1 个活节点，是有效测量；无基线时首轮永不 suspect）。
- 其余 ADR-0005 用例（refused reload / 空轮 / 取消 / panic / controller_error / 内核不可达）
  语义未动，原断言全部原样通过。

### 影响范围
- 轮次行的 `inconclusive` 语义收窄：历史行不迁移（旧行仍是 1，仅不再被当作基线）；
  部署后第一轮 conclusive 轮即恢复基线链，第二轮起护栏恢复效力。
- 发布语义不变：partial coverage 照发（Python parity，publish_round 的 `measured` 分支）。
- Python 侧零改动（Python 从不因数据源拉取失败标记轮次）。

### 回滚
- `git revert` 本提交；轮次行/账本无格式迁移，导出快照不受影响。

---

## R21 — 四路对抗审查的修正批（2026-10-21）

同一会话对**已部署但未提交**的 R8–R19 代码做了四路并行对抗审查（probe-api /
probe-engine / probe-cli+config+mihomo / storage+source+dns），证实 20 项缺陷，
全部修复并带测试；1 项（NaN 端口）实测证伪后按实际行为钉住。

### 安全（HIGH）
1. **`core.secret` 0644 → 0600**（probe-config）：`fs::write` 落盘 0644，任何本机
   用户可读内核 controller 的 bearer secret。改 `create_new(0600)`，且像 Python 一样
   **每次读取都 chmod 修复**历史遗留文件（ct186 线上那颗 0644 的会在重启后自愈）。
   测试：`core_secret_file_is_never_world_readable`。
2. **fetch.rs 传输错误泄漏后端 URL**（probe-source）：reqwest 的 Display 内嵌完整请求
   URL，而部署的后端路径 `ridge.eminet.ggff.net/<secret>` **本身就是凭据**；连接失败/
   重定向错误把它原样写进轮次 note/events（共享账本、面板可见）。四处调用点全部改为
   只分类不引用（probe-dns resolver 同款），`kind_of` 增加 `is_redirect`。
   测试：`transport_failures_never_leak_the_backend_url`（连一个不存在的端口断言无 URL）。

### 正确性（HIGH/MED）
3. **链式实测存活节点被发布层整体丢弃**（probe-engine collect+publish）：
   `originals` 建在 expand 之后的条目上（first-wins），链式变体携带的
   `dialer-proxy: __FRONT0__` 成了节点的「原始」形式 → `close_dialers` 按
   UnclosedDialer 丢掉每个链式验证过的节点；client-path 模式下整源每轮 NoAliveNodes。
   改为从 **expand 之前的上游字典**建 originals，并覆盖 direct 孪生的派生指纹。
   测试：`originals_carry_upstream_dicts_for_both_fold_keys`。
4. **DEFERRED 事务在 Python 共库写入时 BUSY_SNAPSHOT**（probe-storage）：
   `commit_measurements`/`converge_nodes`/`record_results` 先读后写，WAL 快照升级
   失败**立即**返回（实测 0.00s，绕过 busy_timeout）→ 整轮测量丢弃。全部改
   `TransactionBehavior::Immediate`（record_quality_samples 本来就是）。
5. **孤儿收割把 ok=0 当可信基线**（probe-engine scheduler + storage）：
   崩溃残留的 open row 被收割时 `inconclusive=0` → `previous_alive_count` 读到 0 →
   下一轮（最可能仍在故障中的那轮）护栏失效。收割改 `finish_round_full_timed` +
   `inconclusive=1`；note/计数/时长保持逐字节 Python 兼容。
   另：收割器现在**跳过本进程在飞的轮次**（deadline 取消后的 drain 窗口），
   调度器的 round.lock 抢占加了 120s drain 余量。测试：`a_running_round_is_never_reaped…`
   + 真库用例断言 `inconclusive` 与基线跳过。
6. **region 标签重写可产出重名节点 → 整源每轮发布失败**（probe-engine publish）：
   `[SG] 01`（实测出口 HK）与 `[HK] 01` 都发布成 `[HK] 01`，dedupe 比的是改写前的
   名字 → 拓扑校验拒收 → 该源导出永久停更。改按**发布名** dedupe。
   测试：`a_region_tag_collision_supersedes_the_loser_instead_of_failing_the_source`。
   （ct186 线上 `add_region_tag: true`，属已生效缺陷。）
7. **迁移注册表只看版本号**（probe-storage migrations）：0004–0009 部署期间换过字节，
   版本号相同而内容不同的库会静默跳过迁移。现在 apply 时**逐行比对 name**，
   不一致/未知版本号立即报错（启动期点名版本）。
   测试：`a_registry_row_with_a_foreign_name_fails_loudly` / `an_unknown_higher_version_is_refused`。
8. **API 阻塞点**（probe-api）：发布尾部持全局 storage mutex 在 async worker 上跑
   fsync 重的 `publish_round` → 1 核运行时全 API（含 cancel 路由、deadline 计时器）
   冻结在发布期间；内核 preflight（最多 6×120s 同步子进程）同理且不可取消。发布尾部
   改 `spawn_blocking`（锁在阻塞线程内取放）；preflight 改 `spawn_blocking` +
   **ctx 已取消则一次尝试都不花**。测试：`a_cancelled_round_never_starts_preflight`。
9. **回滚绕过引擎物化路径 + 三次全量加载**（probe-api exports + storage）：
   旧处理器直接调 `storage.rollback_export`，激活的指针可指向无文件的快照；且为一次
   存在性检查在锁内三次加载+哈希 ≤8MiB 正文。改走 `probe_engine::rollback_export`
   （物化→校验→CAS），预检查改为两个元数据读取（`active_export_id` /
   `export_snapshot_exists`）。现有回滚测试改用可写 root 真实走物化路径。
10. **`cmd_status` 新装环境 panic**（probe-cli）：`Connection::open` 会**创建**空
    state.db，随后 `expect("query")` 死于 no such table，留下孤儿库。改先查路径存在，
    查询错误退出码 1 而非 panic。
11. **`substore` 子命令端口默认 8088 与文档/内嵌默认 8299 相悖**（probe-cli）：
    与 serve 默认冲突。改显式给过 `--port` 才覆盖，否则 8299。
12. **serve 的 SUBSTORE_BACKEND 冲突警告在 tracing init 之前发出 → 静默丢弃**
    （probe-cli）：订阅器初始化移到 cmd_serve 顶部。
13. **docker 137/139 当作配置失败**（probe-mihomo）：容器被 OOM/SIGKILL（128+n）时
    `docker run` 的退出码不是配置判定，读成 `Failed` 会以「内核拒绝配置」错误诊断
    阻塞轮次。docker 路径 `code >= 125` 全部归 Degraded。
14. **`find_word` 把 `.` 当名字的一部分**（probe-mihomo）：Python 的
    `(?<![\w-])…(?![\w-])` 里 `.` 是边界 —— `proxy 'v1.tokyo' …` 匹配不到名为
    `tokyo` 的节点 → 整轮 Rejected。名字层对齐（`.` 归边界）；**server 层保留更严的
    规则**（`sub.edge.example.test` 不得点名 `edge.example.test`，Python 是裸子串会误杀，
    拒绝猜更安全——按实际行为重写了三个用例的语义注释）。
15. **CLI 值旗标缺值/坏值静默吞**（probe-cli）：`--root` 截断会静默用兜底根、
    `--port 8O88` 静默 8088。缺值/不可解析一律 exit 2。--help 的默认根从 /opt/probe
    改为真实值 /srv/mihomo-test（resolve_root 的兜底）。

### 证伪一项
- **`port: .nan` 会拨 0 端口**（审查推断）：实测 serde_norway→serde_json::Value 根本
  表达不了 NaN，`port` 键整体消失 → 节点以 "missing port" 被丢弃，与 Python 的
  ValueError 殊途同归，**不会**走到饱和转换。`coerce_port` 保留 `is_finite` 过滤作
  纵深防御；测试 `a_nan_or_infinite_port_never_reaches_the_kernel` 按真实行为钉住。

### 低危修正（LOW）
- `metrics` 的 `probe_round_last_*` 改读**最新已关闭**轮次（in-flight 轮期间不再
  全零误报，`last_finished_round` 新读接口）。
- config self-check 端点不再回显 `trace_url` / `chain_payload.url`（免费操作员字符串，
  可带凭据；降为 `*_configured` 布尔，模块 doc 的承诺自洽）。
- `--help` 给 `db backup` 加了「服务停止后再备份」的提示（在线 backup 对共享账本
  会因外部提交反复重启拷贝）。

### 验证
- `cargo fmt --all -- --check` 0；`cargo clippy --workspace --all-targets
  --all-features -- -D warnings` 0；`cargo test --workspace --locked
  --all-features` 全绿（engine 169 / api 46 / source 55 / storage 76 / mihomo 46 /
  config 36 / substore 23 / pipeline 23 / db_commands 4 …）；
- Python `python -X utf8 -m unittest discover -s tests`：Ran 596, OK (skipped=2)；
- `tools/scan_secrets.py` / `tools/license_check.py` / `tools/dependency_policy.py` clean。

### 影响范围
- 无 schema 迁移（0009 不变）；`core.secret` 权限修复在服务重启后自愈线上文件。
- 回滚 API 行为不变（仍 CAS），但快照文件缺失时不再激活无文件指针。

### 回滚
- `git revert` 本提交；`inconclusive` 收窄与收割标记对历史行不回填。

---

## R22 — CI 首跑修正（2026-10-21）

`d330ed0` 推送后 CI 首次在新代码上跑（此前 R8–R19 从未入过 CI），两个 job 各挂一处，
均已修复：

1. **python job**：`test_invalid_body_framing_never_starts_a_round` 的
   重复 Content-Length 子用例 ERROR——**新版 http.client（3.12.14+ 的请求走私加固）
   客户端拒绝发送**重复/冲突的 Content-Length，子用例在客户端就抛了，根本没到服务器。
   本地 3.12.10 能发，CI 的 3.12.15 不能 → 「本地绿、CI 红」的新变体。
   修复：测试改走**裸 socket** 发送预构请求字节（`raw_bytes_post`/`framing_request`
   两个新测试助手），7 个 framing 用例全部不经 http.client——被测的是**服务器**的
   拒绝行为，敌意帧必须真的上线。全平台版本无关。
2. **rust job**：`a_missing_core_secret_never_writes_an_unauthenticated_config`
   FAILED——R21 的 `core_secret` 重写引入的真缺陷：Linux 上
   `open(O_CREAT|O_EXCL)` 对**已存在的目录**也返回 EEXIST，AlreadyExists 分支
   被当作「文件已存在」处理，函数返回了一个**从未落盘**的 secret → 轮次照跑、
   内核配置无鉴权——正是该用例钉住不许发生的事。Windows 上 `fs::write` 对目录
   报错所以本地全绿。
   修复：AlreadyExists 分支改为**读回验证**——只有「可读且非空」的才返回 Ok
   （目录/不可读/空文件一律 Err 阻塞）；空文件按 Python `write_text` 语义
   原地截断重生成。新增两个用例：`an_empty_secret_file_is_healed_and_the_value_is_persisted`
   （痊愈值必须与盘上一致）与 `a_directory_named_like_the_secret_never_yields_a_secret`
   （unix 下钉住 EISDIR 路径与「不 chmod 目录」）。

教训：**Windows 本地绿 ≠ Linux CI 绿**又添两条——`http.client` 的版本化加固、
`O_EXCL` 对目录的 EEXIST 语义。ct186（生产机）从现在起兼任 Linux 测试床：
交叉编译 test 二进制传上去跑，是唯一能在推送前复现 CI 环境的方式。

### 验证
- 本地：fmt/clippy/workspace tests/Python 596 全绿（config 37 / api 46 / mihomo 46）。
- Linux（ct186 实测）：`probe_config` 与 `probe_api` 的 Linux test 二进制交叉编译后
  在生产机上运行，`a_missing_core_secret_never_writes_an_unauthenticated_config` 与
  新增两个 secret 用例全部通过。

### 回滚
- `git revert` 本提交。

---

## R23 — CI 第三跑 + ETXTBSY 生产事故复盘（2026-10-21）

1. **`0611948` 的 rust job 挂在 Clippy**：CI 的 clippy lint 的是 **Linux 目标**，
   `#[cfg(unix)]` 分支 Windows 本地永远看不到（2026-10-05「本地绿 CI 红」的同一陷阱，
   这次是 cfg 而非 clippy 版本）。core_secret unix 分支的 `return Ok(secret)` 是
   match 尾表达式 → needless_return。修复 + **新增本地门禁**：
   `cargo clippy --target x86_64-unknown-linux-gnu --workspace --all-targets
   --all-features -- -D warnings` 必须零告警（`0ccb690`）。
   python job 在 `0611948` 已转绿（framing 裸 socket 修复生效）。
2. **`0ccb690` 与部署二进制的行为差异为零**（去掉一个尾位置 return 关键字）；
   线上 `c92304…` 对应 `0611948` 的树，行为上等价 HEAD。

### ⚠️ ETXTBSY 生产事故（两次，均已恢复）：部署程序缺陷

- **现象**：v3 二进制 swap 后 probe-api 反复 `203/EXEC`，journal 写
  `Failed to execute …: Text file busy`；systemd 每 5s 重启一次，服务全停。
- **根因**：分片断点续传的 SFTP 上传在服务端留下 **sftp-server 进程持有 `.new`
  的写句柄**（客户端侧 sftp.close()/连接断开并不能保证服务端句柄立即释放；
  被 kill 的连接留下孤儿句柄）。`mv .new → probe-cli` 把**被写句柄钉住的 inode**
  换到服务路径上 → 任何 execve 都吃 ETXTBSY。**二进制本身没有问题**——两次都是
  上传机制造成的。
- **恢复**（两次都立即生效）：`cp 备份 probe-cli.tmp && mv probe-cli.tmp probe-cli`
  ——rename 换出**新 inode**，孤儿句柄继续钉着旧 inode，新文件可 exec。
- **新部署规则**（写进流程）：swap 前必须 ① `pkill -f sftp-server`（此刻不应有
  活客户端）② `fuser -v <root>/bin/probe-cli.new` 证明**无写句柄** ③ swap 后
  `systemctl is-active` + `healthz` 双检，**不健康立即 rename 回滚**。
- 事故时间线：21:48 首次 swap 即 ETXTBSY（30 分钟后恢复）；22:25 第二次 swap
  仍是孤儿句柄（90 秒内 rename 恢复）；22:37 按新规则 swap 成功，
  round 44 全绿（ok 203/293、inconclusive=false、导出+push 正常）。

### 影响 / 遗留
- ct186 现运行 `c92304…`（= `0611948` 树，行为等价 HEAD `0ccb690`）。
  secret 文件 0600，护栏链与基线正常（round 44 基线 203）。
- `0ccb690` 待推送的网络窗口（本地出网故障期间用重试循环）。

---
  Python 侧无影响。
