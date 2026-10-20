# TEST_REPORT_RUST

Rust 侧的真实测试证据。每次提交批次追加一节；只记录实际执行过的命令与输出。
Python 基线见 `workstreams/13_TEST_COMPATIBILITY.md`（R0: 568 通过 / 2 跳过）。

---

## CI 基线修复 + 线上部署复核 — 2026-10-20（工作区，未提交）

起点：同一工作区 CI 三门全红——`cargo test --workspace --no-fail-fast` = 429 passed / 4 failed，
`cargo fmt --all -- --check` 在 `crates/probe-api/src/lib.rs:12` 报 use 顺序，
`cargo clippy --workspace --all-targets -- -D warnings` 在 `crates/probe-mihomo/src/exit.rs:138`
报 needless_borrow。本文档上一节写的「Rust 257 通过」已过期（该工作区实为 433 项）。

### 修复（每项都先复现失败，再验证通过）
1. F1 `probe-api/src/lib.rs:657`——`host_port()` 对含凭据/query/fragment 的 URL 返回空。
   改为显示专用 `display_host_port`（`probe-mihomo/src/controller.rs`）：容忍这些部分但绝不回显，
   严格解析仍是请求 URL 的唯一来源；旧断言「一律 None」按新语义改写，另加 10 组对照用例。
2. F2（安全）`probe-engine/src/publish/tests.rs:379`——导出根逃逸未被拦截。根因是 Windows verbatim
   路径上 `PathBuf::join("..")` 会词法弹出父目录，旧检查看不到 ParentDir。落盘入口收敛到 `CheckedRoot`
   （绝对路径 + 逐级 reparse 检查 + 独占所有权 + component 级前缀比较）。
3. F3 `probe-mihomo/src/config_check/tests/native.rs:165`——用例把非规范临时路径喂给内核校验器，
   helper 的契约断言直接 panic 提前退出，runner 于是返回 Ok。改为传 canonical 路径，并把断言加强为
   「错误原因必须是 deadline」，意外早退不能再假绿。
4. F4 `probe-storage/src/migrations.rs:255`——v3→v8 的旧表 SQL 逐字节快照随 0007/0008 加列过期。
   改为 `nodes`/`results` 断言声明列清单（PRAGMA table_info，cid 顺序），其余表保留逐字节强断言。
   反证：删掉 0007 对 nodes 的两条 ALTER → 8 个用例失败（7 个真实读写报 no such column: exit_ip）。

### 最终门禁（2026-10-20，工作区）
```text
cargo fmt --all -- --check                              -> 退出 0
cargo clippy --workspace --all-targets -- -D warnings    -> 退出 0
cargo test --workspace --no-fail-fast                    -> 435 passed / 0 failed
  probe-api 9 / probe-config 28 / probe-dns 60 / probe-domain 41 / probe-engine 122
  probe-mihomo 44 / probe-source 47 / probe-storage 61 / probe-substore 23 / probe-cli 0
python -X utf8 -m unittest discover -s tests             -> Ran 596 tests, OK (skipped=2)
cargo check --target x86_64-unknown-linux-gnu -p probe-engine -p probe-mihomo --all-targets
                                                          -> 退出 0（部署目标平台；覆盖 cfg(unix) 分支）
```

### 线上部署复核（ct186 = 216.167.28.249:10052）
- 部署面：`probe-cli` sha256 `b0dd462a…` 与本工作区 `target/x86_64-unknown-linux-gnu/release/probe-cli`
  逐字节一致；systemd = probe-kernel(mihomo v1.19.32) / probe-api(127.0.0.1:8088) /
  probe-tunnel(probe.bucher.dpdns.org) / probe-round.timer（每小时一轮）。
- 断点：round 7–18（2026-10-19T16:54Z 起）每轮 0 节点。原因：Sub-Store 组合订阅 `probe` 的 5 条
  remote 成员指向已失效域名 `probe.eminet.ggff.net`（CF 530 / error 1033），而接替域名
  `probe.bucher.dpdns.org` 没有 `/api/export/*`（R8 发布面未实现，404）。
- 恢复（用户授权的数据源改指）：把该 collection 的成员改指 5 条 `-local`（318 条代理，与账本
  round 6 的 312 个名字重叠 265 个）→ 手动触发 round 19：
  `tested 462/462 variants: 314 nodes, ok 64, failed 249, inconclusive 1`，14 分 12 秒完成；
  节点状态由 unknown 247 收敛为 dead 242 / alive 65 / pending 3 / unknown 4，exit_history 新增 75 行。
- 未验证：真内核 L2/L3 真流量端到端仍只有 mock 证据；`probe-*-local` 快照的生成轮次未考据。

### R8 发布闭环接线 + 线上落地（同日第二批）
- 接口冻结（`probe-config`）：`PublishSection{enabled,push_to_substore,prefix,hostname,add_region_tag,token,dir}`、
  `PublishSection::export_root(root)`、`SourceSpec.export`（Python 语义：只有显式 `false` 才关）。
- 引擎：`select_source_exports`（只导出存活节点的**原始域名字典**、同名变体折叠、`[CC]` 幂等覆写、
  dialer 必须在本文件内闭合，否则连同依赖者一起丢弃并记 `UnclosedDialer`）+ `publish_round`
  （`PublishRoundRequest{publish,sources,collected,verdicts,guard,measured,actor}`）。
- 跳过口径与 Python 对齐（engine.py:2275-2313）：只有 `enabled=false`、`guard==PreservePreviousState`
  （存活下限护栏）或 `MarkRoundInconclusive && !measured` 才整轮不发布；**部分覆盖照常发布**
  （线上教训：round 19/20 有 1 个 inconclusive 节点就整轮跳过 → 永远不产出）。
- API：`GET /api/export/{key}.yaml`（publish token 或 admin，常量时间；空 token 一律 401；正文为 active snapshot 原文）、
  `GET /api/v1/exports`、`/api/v1/exports/{key}/history`、`POST /api/v1/exports/{key}/rollback`（admin，CAS）。
- 线上验证（ct186=216.167.28.249:10052，二进制 sha256 `3aaf4400…`）：
  - 配置改为 5 个真实上游源（pokenmen/prologue/purens/233pio/saki）+ `publish.dir=/opt/probe/exports`（0700）。
  - round 22：`161 nodes, ok 126, failed 32, inconclusive 3`，轮末发布 4 个源（18/83/19/2；saki 上游 500 → NoAliveNodes 保留上一份）。
  - `active_exports` 4 行、`export_snapshots` 4 行（sha256/count 一致）、导出目录 4 个 0600 不可变文件。
  - `GET /api/export/<key>.yaml?token=…`：本机与公网（`probe.bucher.dpdns.org`）均 200，正文逐字节一致；
    无 token/错 token 401、未知 key 404。
  - Sub-Store 5 条成员 URL 指回新域名后，collection `probe` 下载 200 / 122 条代理（= 四份导出之和）；
    122 条里 108 条的 `[CC]` 与 `exit_history` 实测出口国别一致，14 条因所选变体没有出口身份而保留上游标签（保守行为，待跟进）。
- **真实内核验收（parent integration gate，2026-10-20）**：把 4 份已发布导出逐个交给线上真实的
  `mihomo v1.19.32` 做 `mihomo -t -d <tmp> -f <export>.yaml`，4/4 **ACCEPTED**；对照内核正在跑的
  `/opt/probe/core/config.yaml` 同样 ACCEPTED。内核口径解析出的节点数 19/18/83/2 = 122，与
  Sub-Store collection 下载到的 122 条一致。
- 已知限制：`probe-*-local` 快照未清理（推模式接线后会被覆盖成新内容）；
  未验证：真内核 L2/L3 真流量端到端仍只有 mock 证据。

### 第三批：调度器 / 读取面 / 耗时入账 / 推模式 / 前置校验（2026-10-20 晚）

- **G09 调度器**：`probe-engine::scheduler::{SchedulerDecision, decide, reap_orphan_rounds,
  parse_stored_instant, OrphanLedger, STARTUP_GRACE, ORPHAN_NOTE}`（纯函数 + 固定时间戳用例）
  + `impl OrphanLedger for Storage`（真 SQLite 回收用例；孤儿轮 `duration_s=0` 对齐 Python）
  + `probe-api::spawn_scheduler`（20s tick，`serve()` 启动即拉起，`spawn_scheduler_every` 供测试注入）。
  `/api/v1/status` 新增 `schedule:{enabled,interval_minutes,next_run}`。
- **G14 读取面**：`GET /api/v1/nodes`（keyset `(source,fingerprint)`、16 字段白名单、`server` 只出 host）、
  `/nodes/{fp}`（+状态历史）、`/rounds`、`/rounds/{id}`、`POST /rounds/{id}/cancel`（在飞 CAS 槽：200/409/404）。
  `/api/v1/status` 的 `rounds.last` 补全计数与 `duration_s`（面板三张卡不再显示 '-'）；publish token 对读取面一律 401。
- **耗时入账**：`probe-storage::{finish_round_timed, finish_round_full_timed}`（duration 与关闭同一条 UPDATE，
  不新增失败面）+ runner 侧 `measured_seconds`（Python `round(x,1)`；NaN/inf/负数 clamp 到 0.0，时钟回拨读 0）。
- **G13 推模式**：`probe-engine::publish::push_round_exports`（只推本轮 Published 源、正文取活跃快照、
  payload 九字段对齐 Python、prune 三重作用域 = 前缀 + `-local` + `source==local`、list 失败一律不删、
  所有后端失败只记 `PushRecord` 绝不影响轮次）。`probe-source::SubAdmin::list_subs` 新增。
- **G03 前置校验**：`probe-engine::preflight::{make_testable, KernelValidator, MihomoValidator, MAX_PRUNE}`——
  内核 `-t` 校验 → 按**账本身份**（`fingerprint_proxy`，忽略名字）剪掉被拒节点并重写重试（Python `make_testable`）
  → 仍被拒则 `Rejected` 让调用方 block 本轮；`Degraded`（无 MIHOMO_BIN 且无 docker）只跳过不失败；
  内核文本 200 字符有界并把 secret 换成 `[redacted]`。CLI 与 API 两条轮次路径都已接线。

门禁（同一工作区）：
```text
cargo fmt --all -- --check                              -> 退出 0
cargo clippy --workspace --all-targets -- -D warnings    -> 退出 0
cargo test --workspace --no-fail-fast                    -> 509 passed / 0 failed
  probe-api 32 / config 32 / dns 60 / domain 41 / engine 156
  probe-mihomo 44 / source 50 / storage 71 / substore 23
python -X utf8 -m unittest discover -s tests             -> Ran 596 tests, OK (skipped=2)
```
- 环境限制（如实记录，非代码问题）：跑 `probe-substore`（rquickjs）相关构建/测试时，本机多次出现 rustc 原生崩溃
  （`0xC0000409` STATUS_STACK_BUFFER_OVERRUN）与运行器退出；随后 `target/` 出现损坏产物
  （proc-macro2 报 "only metadata stub found for rlib dependency core"，交叉构建报 158 个 E0463 类错误）。
  处置：`cargo clean` + `CARGO_BUILD_JOBS=2` + `CARGO_INCREMENTAL=0` 全量重建交叉目标。
  上述 host 门禁是在产物损坏之前实测通过的；交叉构建失败属于产物/环境损坏。

### 第三批线上落地（ct186，2026-10-20T10:0xZ）
- 二进制：`cargo clean` 后全量重建的 `probe-cli`，sha256 `3e790e0f…`（与损坏前同一次构建**逐字节一致**，可复现），
  已部署到 `/opt/probe/bin/probe-cli`；`probe-api.service`/`probe-round.service` 增加 `MIHOMO_BIN=/opt/probe/bin/mihomo` drop-in。
- **进程内调度器已接管**：`systemctl disable --now probe-round.timer`（单元保留作手动回退），
  启动 4 秒后即由调度器自动开轮（journal：`scheduler started a round round_id=27`）。
- round 27（API 路径，trigger=schedule）：`141 nodes, ok 106, failed 33, inconclusive 2`，`duration_s=111.6`（**该列首次被写入**）；
  轮末 4 源发布（14/70/16/1，saki 上游 500 → NoAliveNodes），随后 **4 条 `<prefix>-<key>-local` 推回 Sub-Store**
  （journal：`export pushed into Sub-Store … name=probe-pokenmen-local node_count=14`）；
  Sub-Store 侧核对 `probe-pokenmen-local`：`source=local`、`14 proxies`、`displayName=pokenmen 测活 (14)`。
- 读取面实测：`/api/v1/status` 带 `schedule{enabled,interval_minutes,next_run}` 与完整 `rounds.last`；
  `/api/v1/nodes?limit=2` 200（16 字段白名单）、`/api/v1/rounds?limit=2` 200、未知节点/未知轮次 404、
  `POST /api/v1/rounds/{未知}/cancel` 404（标准错误体）、publish token 与空 token 访问读取面均 401。
- 收尾：`schedule.interval_minutes` 回到 60（`next_run` 11:05:29Z），systemd 定时器已停用，三个服务 active。
- 未验证：真内核 L2/L3 真流量端到端（G07）；`_local` 之外的旧快照清理；`/metrics`、迁移模式、UI 仍未做（见 17 审计矩阵）。

### 第四批：`/metrics` 与 CLI 进程级测试（2026-10-20 深夜）

- **G16 `/metrics`**（`probe-api/src/metrics.rs` + `probe-storage::MetricsSnapshot`）：admin 鉴权、`text/plain; version=0.0.4`、`no-store`；
  数字全部在抓取时从账本现算（三条索引查询），因此没有第二份计数器会漂移或在重启后丢失：
  `probe_rounds_finished_total` / `_suspect_total` / `_inconclusive_total` / `_with_duration_total`、
  `probe_nodes{status=...}`、`probe_exports_active`、`probe_scheduler_enabled`、`probe_scheduler_interval_minutes`、
  `probe_round_last_{total,ok,failed,duration_seconds}`。标签只有 `status`（≤5 值）；标签值按文本规范转义，
  并有一条「恶意 status 不能注入新指标行」的用例。
- **probe-cli 首个测试**（此前该 crate 测试数为 0）：`crates/probe-cli/tests/db_commands.rs` 以**真二进制**跑
  `db migrate / check / backup / rollback / verify`：全新账本迁移→检查（`0 pending`、`integrity_check: ok`）→备份→
  **备份后写入一条轮次**→未加 `--yes` 的回滚被拒（exit 2，且账本未被改动）→加 `--yes` 回滚成功（写后那条消失、
  回滚自带 verify）→`db verify` 通过；另有「缺失账本 check 退 1 并点名路径」「未知子命令退 2」「缺 `--from` 退 2」。
- 门禁：`cargo fmt --all -- --check` 0；`cargo clippy --workspace --all-targets -- -D warnings` 0；
  `cargo test --workspace --no-fail-fast` = **520 passed / 0 failed**
  （api 36 / cli 4 / config 32 / dns 60 / domain 41 / engine 156 / mihomo 44 / source 52 / storage 72 / substore 23）；
  `python -X utf8 -m unittest discover -s tests` -> Ran 596, OK (skipped=2)。
  线上（ct186）：新二进制 `cf81a1b1…` 已部署，`GET /metrics` 实测 200、
  `content-type: text/plain; version=0.0.4; charset=utf-8`、`cache-control: no-store`、15 条指标行，无 token 401。
- **G02 边界**（同批）：`probe-source` 的 Sub-Store 客户端把重定向限制为 5 跳（此前用 reqwest 默认），
  响应体改为**流式读取**并在 8 MiB 处拒绝（此前 `Response::text()` 无上限）——
  两条都带用例（自指的 302 循环必须失败为 redirect；超限正文必须报 exceeds，而不是先缓冲进内存）。
- **G15 只读面板**（`probe-api/src/lib.rs` 的 `PANEL_HTML`，9.8 KB，无外部依赖）：
  概要卡（状态 / 轮次 / 存活 / 失败 / 总数 / 耗时 / 下次轮次）、带状态+来源筛选与游标翻页的节点表、
  轮次表、导出表（每个 key 一个「下载」按钮，用 admin 凭据直接取 `/api/export/<key>.yaml`）。
  渲染全程 `textContent`（新增用例断言面板里**不存在** `innerHTML`），页面不含任何写操作入口；
  JS 用 `node --check` 做过语法检查，线上 `/` 与 `https://probe.bucher.dpdns.org/` 均 200。
- **G20 供应链门禁**（同轮）：
  - `cargo audit` 实测出 **7 个漏洞**：`rustls 0.23.45`（4 条，含会话密钥经 Debug 泄露、ECH 降级、
    brotli 解压放大到 1 GiB）与 `zip 2.4.2`（3 条，含 SUID/SGID 保留、解压软链逃逸，CVSS 9.1）。
    处置：`cargo update -p rustls` → 0.23.46；`zip` 2.2 → **9.0.5**（调用点只需兼容编译，
    且我们从未调用被点名的 `extract()`，只用 `enclosed_name()` + 逐条写盘）。现在 **0 漏洞 / 0 警告**。
  - 顺带清掉未维护依赖：`serde_yaml`（deprecated）→ `serde_yaml_ng` → 仍带 `unsafe-libyaml` →
    最终换 **`serde_norway` 0.9.42**（维护中的 fork，后端 `unsafe-libyaml-norway`），advisory 归零。
  - 新增 `tools/license_check.py`：读 `cargo metadata --locked`，对 245 个第三方包做 SPDX 表达式求值
    （`OR`/`AND`/`WITH`/括号 + 老式 `MIT/Apache-2.0` 归一），白名单 12 条；当前 **clean**。
  - 新增 `tools/sbom.py`：`cargo metadata --locked` → CycloneDX 1.4 JSON（245 components、purl、license），
    输出确定性（仅 timestamp 变动），CI 里作为 artifact 上传。
  - CI（`.github/workflows/ci.yml`）改为 `--all-features` 的 clippy/test，并新增 license / advisories / SBOM 三步。
- **G07 真流量取证（同轮）**：`verify.chain_payload` 增加 `scope: "chain" | "all"`（默认 `chain`，保持 Python 语义；
  `all` 让每个存活节点都真拉一次页面）。线上 ct186 置 `scope=all` 后跑 round 29：
  133 节点 / ok 96，**150 个变体各完成一次真实页面拉取，合计 8,005,808 字节**（单条约 372 B–52 KB），
  **2 个「延迟通过但真实 TLS 拉不动」的节点被判 `payload_fail`**——正是 Python 记录的那类假活；
  同时 L2 出口验证留下 150 条观测（83 个不同出口 IP、18 个国家）。轮次耗时 111.6s → 174.4s，
  仍在 25 分钟看门狗之内。用例：`scope=all` 时直连节点的 payload 失败必须变成 `payload_fail` 判决（而非 uncertain），
  `scope=chain` 时直连节点完全不发真实流量。
- **面板：出口与质量视图（同轮）**：
  - 节点表新增**实测出口列**（`exit_ip` + `exit_asn`，来自 L2 出口验证）；
  - 点击任一节点行 → 详情区显示**质量窗口**与状态变更历史；质量由既有 `probe-domain::quality::summarize_samples`
    计算（该模块此前无生产调用方），窗口取该节点最近 50 条 results，**按时间正序**喂入（抖动定义在相邻序上）：
    样本数/成功数/成功率、min/median/p90/p95/max 延迟、抖动、出口数与出口稳定度。
  - 新读取面：`Storage::recent_results(source, fingerprint, limit)`（keyset 无关，round_id DESC + limit 校验）；
    `GET /api/v1/nodes/{fp}` 响应新增 `quality`（空窗口为 `null`，**不编造 0**）。
  - 线上实测（节点 `233pio/🇼🇸 TW-Taichung…`）：`window 2 / success_rate 1.0 / median 214ms / p95 250.9ms /
    jitter 82ms / exits 2 / exit_stability 0.5` —— 两条样本两个不同出口 IP，稳定度如实反映。
- **导出快照保留策略（同轮）**：`publish.keep_snapshots`（默认 `None` = 全留；`0` 视为未设置，
  绝不会被读成「全部删除」）。每轮发布后（**即使本轮因护栏跳过发布也照跑**）执行一次保留：
  `Storage::prune_export_snapshots` 只删「不在最新 N 条、且不是 active」的行（active 还被 DB 触发器二次保护），
  然后 `prune_publications` 按 `<sha256(key)>-<sha256(content)>.yaml` 命名规则删除**不再被任何行引用**的文件；
  不是该命名格式的文件（README、备份、别人的东西）一律不碰。
  - 线上取证：keep=3 时 round 31 触发 `export retention pruned superseded snapshots round_id=31 snapshots=14 files=14 keys=4`，
    账本 26 行/26 文件 → **13 行/13 文件**（`prologue` 保留 4 条，因为它的 active 快照比最新 3 条更老——active 永不删）；
    随后恢复 keep=20，导出接口仍 200、4 个 active key 完整。
  - 用例：三次发布 → `keep=1` 只留 active（删 2 行 2 文件、外来文件无恙、再跑一次为 no-op）；配置侧覆盖 `0 → None`。
- **面板：来源 / 护栏 / 保留量（同轮）**：
  - `/api/v1/status` 新增 `sources`（按来源的 total/alive/dead，来自新 `Storage::source_summary()`）与
    `exports: {active, snapshots}`（`active_export_count()` + `snapshot_count()`），全部是索引计数，无节点名/地址。
  - 面板：新增「来源」表（含存活率）、轮次表新增「护栏」列（suspect/inconclusive/dropped/restored 标注）、
    概要卡新增「导出 key / 快照」。
  - 线上实测：`sources` 5 行（233pio 10/7、pokenmen 18/18、probe 314/65、prologue 126/98、purens 43/33），
    `exports {active: 4, snapshots: 13}`，round 31 显示 `inconclusive: true`、`suspect: false` —— 与账本一致。
- **配置自检（同轮，并明确不做「配置编辑」）**：
  - 背景：`Config::load` 对**读不到或非 JSON** 的文件一律回退成默认值（Python `stored = {}`），
    这是正确的运行时行为，但意味着 `config.json` 里一个手抖就可能让服务静默以默认值运行（来源没了、发布凭据没了），
    **日志里一句话都不会说**。
  - 新增 `Config::parse(&str) -> (Config, json_valid)`（`load` 改为读文件后调用它），
    以及 `GET /api/v1/config/validate`：解析**进程启动时那个文件**，返回 `json_valid`、生效摘要、
    以及与当前运行值的差异列表（`schedule`/`publish`/`sources`/`verify.chain_payload`/`core`/`chain`）；
    所有凭据只输出 `*_configured` 布尔，绝不回显。
  - 面板新增「配置」卡：`正常` / `未重启`（文件与运行值不一致）/ `JSON 损坏`，点击展示差异。
  - 线上实测：`json_valid true`、`matches_running true`、差异为空，摘要含 `keep_snapshots 20`、
    `payload_scope all`、`schedule 60`；**响应体不含 admin token 也不含 publish token**。
  - **配置编辑（写入）明确不做**：它需要写文件 + 重启语义，而文件坏掉的后果正是上面那个静默回退——
    在有 SSH 的前提下，面板写入只会把一个已有故障模式搬到浏览器里。审计矩阵里记的是这个理由，而不是一个永远不做的 TODO。
- **模糊测试（同轮，G20 尾项）**：本机没有 nightly，`cargo-fuzz`/libFuzzer 用不了，
  所以把「随机输入下不崩」做成**确定性变异测试**进 CI（固定种子 + 固定变异计划，失败可复现）：
  - `probe-config`：`Config::parse` × 2000 次（改字节/截断/插入/追加 0x00 0xFF/反转），断言永不 panic；
  - `probe-mihomo`：`parse_trace` × 1000 次，断言键非空且已 trim、值不含换行且已 trim；
  - `probe-source`：`parse_proxies` × 1000 次，断言不 panic、Ok 时每个条目都是对象。
  - 第一次跑就抓到真问题：`parse_trace` 对 `ip=…\r=` 这类输入会保留**行内 `\r`**；
    查 Python `partition("=").strip()` 行为一致，故**保持parity**并把「为什么允许」写进断言注释，
    而不是偷偷改解析行为。
  - libFuzzer 的跑法记在下方「手工验证」小节，供有 nightly 的环境深挖。
- **crate 签名**：评估结论是**当前不适用**——唯一的产物分发方式是从操作机 SSH 拷二进制（CI 产物由平台完整性保护），
  真正适用的控制是「上传前后哈希一致」，这一步本来就在部署流程里（每次部署都打印两端 sha256 并核对）。
  等出现 GitHub Release / 包分发时再引入签名，届时 SBOM 与签名一起出。

- **链式路径真流量取证（同轮，G07 最后一个未验证分支）**：在 ct186 上起了一个**完全隔离**的实例
  （`/tmp/chain` 自有 root/DB、自有 mihomo：控制器 19191、车道 19300/19301、混合端口 19397/19398），
  配置 `chain.enabled=true`、前置来源 `pokenmen` 按名单取 2 条真实节点、`test_plain_nodes=true`（客户端路径模式）、
  `verify.chain_payload.scope=chain`、发布关闭。
  - 结果：**76 个变体、74 条 chain 记录、24 条存活、24 条完成真实页面拉取、合计 790,896 字节**；
    两个前置均 alive（147ms / 146ms），chain 记录的出口 IP 随前置不同而不同（`42.200.173.215` vs `42.200.172.135`）——
    说明流量确实从不同前置出去；单条 payload 达到 65,536 字节上限。
  - 过程中修掉两处真问题：① `probe-cli round` 从未初始化 tracing，前置池告警**全部静默**（已加，默认 `warn`，`RUST_LOG` 可调）；
    ② `probe-cli --help` 直接报 `unknown argument`（已加用法说明，未知命令也打印用法）。
  - 踩到并记录：隔离实例必须改 `core.mixed_port`（默认 19194 与生产内核冲突 → reload 整包失败 → 全部 `front_dead`）；
    `chain.front_source` 在 JSON 里是**嵌套对象** `{kind,name}`，写成扁平键会被静默忽略。

- **质量抽样（同轮，G07 最后一项）**：新增 `quality` 配置段（`enabled` 默认 false、`sample_size` 默认 5、
  `max_bytes` 默认 256 KiB、`timeout_s` 15），在轮次**验收完成之后**对**分散抽样**的存活节点各做一次有上限的真实传输，
  写入新表 `quality_samples`（迁移 0009，`PRIMARY KEY (round_id, source, fingerprint)` 使重跑幂等），
  并在节点详情里给出 `transfers` 与 `throughput_mbps`（十进制 Mbps，聚合口径同 `probe-domain::quality`）。
  - 设计取舍：抽样用「每隔 k 个取一个」而不是「取前 k 个」——同一订阅的前几个节点通常同属一家、机型最便宜，
    只测它们不构成对整轮的抽样；抽样失败**不改变判决**（吞吐探测超时说明不了节点死活，轮次已经判过了）。
  - 线上实测（round 33，281 节点 / ok 239 / 402.7s）：**3 条样本、合计 185,837 字节 / 4,289ms**
    （`purens` 90,917B/2,907ms、`prologue` 94,548B/920ms、`233pio` 372B/462ms），
    节点详情实测 `transfers: 1, throughput_mbps: 0.25`。
  - 用例：① 存储层「同一轮重复写入是 no-op、limit=0 被拒」；② 引擎层「4 个存活 + 1 个失败、sample_size=2 →
    取到 a/c 而不是 a/b、失败节点绝不探测、`enabled=false` 时零流量」；③ API 层「1 MB / 100 ms → 80 Mbps」。

- **依赖策略门禁（同轮，G20 收尾）**：新增 `tools/dependency_policy.py`（**不引入 `cargo deny`**——它的配置 schema 随版本变，
  而我在本机无法固定版本验证；写一份没跑过的 deno.toml 塞进 CI 只会在某天变红）。策略用 `cargo metadata --locked` 直接判定：
  - **来源**：注册表依赖必须来自 crates.io；不允许 git 依赖；不允许工作区之外的 path 依赖；
  - **禁令**：`openssl` / `openssl-sys` / `native-tls` / `libgit2-sys` / `cloudflare-zlib` 五个名字，
    每一条都对应本项目**主动关掉的门**（rustls 是唯一 TLS 栈）；
  - **可重现性**：注册表依赖不得用通配符版本 `*`；
  - **只报告不拦截**：13 个多版本 crate（健康依赖图里正常）、40 个带 build script 的 crate（构建必需）。
  - `--selftest` 用合成图证明每条规则都会真的触发（6 条规则全中），避免「策略写了但从不生效」。
  - 实测：`dependency policy: clean (243 third-party crate(s), 5 denied name(s), 0 wildcard registry requirement(s))`。
  - 过程中修掉一个自己的 bug：cargo 写的是**绝对** path，且仓库路径含非 ASCII，字符串前缀比较在 Windows 上不稳；
    改成「path 依赖的目标必须是本工作区成员」——语义更准，也不受编码影响。

- **跨进程轮次锁（同轮，G06 的 process exclusion）**：新增 `probe_engine::RoundLock`——
  `<root>/data/round.lock` 用 `create_new`（POSIX `O_EXCL` / NTFS `CREATE_NEW`）原子创建，写入 `pid=` 与起始时间；
  文件年龄超过看门狗预算即视为陈旧并夺取一次；`Drop` 只删**自己 pid** 的那份（防止慢轮次被夺取后误删别人的锁）。
  服务端 `spawn_round` 与 `probe-cli round` 都取锁。
  - 线上取证（round 34，263 节点 / ok 220 / 379.8s）：轮次运行中锁文件为 `pid=16456 started=…`，
    同时手工执行 `probe-cli round` → **exit 2**，输出 `round refused: another round is already running (pid=16456 started=…)`；
    轮次结束后锁文件消失。
  - 用例：第二次取锁被拒且信息含持有者 pid；把锁文件 mtime 改到 2 小时前后可夺取、Drop 后文件消失。
- **收尾审计（同轮）**：逐行核对审计矩阵，补齐 9 行的证据与结论（G01/G03/G06/G08/G10/G11/G17/G18/G21），
  把 G13 的缺口改写成明确的一句话（不实现 Sub-Store 侧 operator 配置），并在矩阵开头写明当前状态与**仅剩的 4 项**。

- **未测节点优先（同轮，G09）**：`run_round` 开头按账本状态重排任务——`pending`/`unknown`（以及**账本里根本没有行**的节点）
  排在最前，其余保持原有相对顺序（`sort_by_key` 稳定排序，只在两个 rank 之间排序）。
  动机是**取消/超时下的覆盖率**：一轮若被看门狗中断，尾部节点永远轮不到；按未测优先，每轮都在推进没人见过的节点，
  而不是反复重测列表头部。`Ledger::node_statuses` 有新默认实现（返回空 = 不重排），`Mutex<Storage>` 用一次查询提供真实状态。
  - 线上取证（round 35）：`INFO probe_engine::round: unmeasured nodes scheduled first unmeasured=19 measured=327`。
  - 用例：① `alive/pending/dead` + 一个**账本里不存在**的节点 → 顺序必须变成 `pending, 不存在, alive, dead`；
    ② 空账本 → 顺序完全不变（排序只在有信息时发生）。
  - 顺带确认：Python 侧既没有 pending 优先也没有 dead backoff，所以这两项是本项目的增强，不是 parity 缺口。

- **事件日志（同轮，G14 最后一项）**：`events` 表在 Python 时代由 `db.log()` 写入并裁剪到最新 2000 条，
  但 Rust 侧**从来没有写过也没有读过**——它只是一张躺在 schema 里的死表。本轮接通：
  - `Storage::record_event(level, message)`（消息按字符截断到 500）+ `recent_events(limit)`（新到旧），
    写入后按 Python 语义裁剪到最新 2000 行；
  - API 在轮次生命周期的关键点写事件：`round N started (trigger=…)`、`round N closed: total/ok/failed/inconclusive/dropped`、
    来源拉取失败（warn，带原始错误摘要）、轮次失败（error）；写事件走独立线程，**绝不因为写日志失败而影响轮次**；
  - 新端点 `GET /api/v1/events?limit=`（admin），形状与 Python 一致 `{"events": [...]}`，limit 非法回 400 而不是静默截断；
  - 面板新增「事件」表（时间/级别/内容，warn 黄、error 红），加载时取最新 20 条。
  - 线上取证（round 36）：部署前 `events` 0 行 → 触发轮次后立刻出现
    `[warn] source fetch failed: saki: HTTP 500 …` 与 `[info] round 36 started (trigger=api)`，`/api/v1/events?limit=5` 返回 200。
  - 用例：存储层（新到旧、截断、limit=0 被拒）与 API 层（两行事件、顺序、`limit=0` → 400）。

### 手工验证：libFuzzer（需要 nightly + Linux/WSL）
```bash
cargo install cargo-fuzz
cargo +nightly fuzz init            # 只做一次，生成 fuzz/
# 目标：fuzz_targets/config_parse.rs -> probe_config::Config::parse
#       fuzz_targets/trace_parse.rs  -> probe_mihomo::exit::parse_trace
cargo +nightly fuzz run trace_parse -- -max_total_time=300 -rss_limit_mb=2048
```
种子语料就是上面三个测试里的 `SEEDS`；变异测试发现的每个输入都应当加进语料并补一条回归用例。

---

## 安全与轮次可靠性补丁 — 2026-10-09（工作区，未提交）

基于 `c2a88fd`，默认实现仍为 Python；本批未连接 VPS、未部署、未推送。
起点实跑：Python 573 总计（571 通过 / 2 跳过），Rust 243 通过。

### 复现与修复
- Python：新库首次调度饥饿、线程启动失败占住 BUSY、畸形/截断 JSON 被当作空
  命令执行、负数/重复长度与无正文上限、URL 用户信息及 fragment 泄露、
  host 保留型脱敏地址回写覆盖真实后端、共享库忽略 Rust inconclusive。
- Rust：拒绝 reload 仍测旧别名、converge/baseline 错误绕过关闭轮次、
  结果写入失败但节点状态已提交、取消/空轮次被当成有效基线、变体计数
  抬高下轮护栏、失败抹掉分类、状态地址字符串切分漏出完整 URL。
- 已定位缺陷的回归先在修前代码上确认失败；修复后补测取消准备阶段、提交边界、
  worker panic、控制器中途故障、按计划汇总结果及 API 密钥失败保护。
- SQLite 用真实触发器分别中止 results 与 node_state_history 的写入，
  确认已修改节点、已插入节点、结果与状态变更历史一起回滚；移除测试触发器后
  同一连接可成功提交，失败计数只增加一次。
- Python HTTP 测试使用本地回环服务，校验 400/408/413 与断开连接；
  配置保存测试显式模拟 Sub-Store 清理调用，避免外部网络依赖。

### 最终门禁
```text
python -X utf8 -m unittest discover -s tests
  Ran 592 tests in 42.364s; OK (skipped=2)
  = 590 passed / 0 failed / 2 skipped，新增 19 项

cargo test --workspace --locked
  probe-api 8 / probe-cli 0 / probe-config 24 / probe-dns 22
  probe-domain 12 / probe-engine 75 / probe-mihomo 26
  probe-source 46 / probe-storage 21 / probe-substore 23
  = 257 passed / 0 failed，新增 14 项

cargo fmt --all -- --check
  无输出，退出 0
cargo clippy --workspace --all-targets --locked -- -D warnings
  零告警，退出 0
python -X utf8 tools/scan_secrets.py
  secret scan: clean (tracked files)
```

构建后的 probe-engine 测试二进制以 `round::tests --test-threads=4` 连续运行
10 次，每次 28 passed，10/10 通过。内嵌 Sub-Store 全量测试约 190 秒，
与本批起点一致，未跳过其慢用例。

### 行为边界
- Python POST 正文限 2 MiB、15 秒，空正文兼容；不接受 Transfer-Encoding，
  不再把格式错误转成空命令。配置保存响应使用与状态轮询相同的脱敏投影。
- Rust 的 results/nodes/history 在同一事务内提交，轮次关闭仍是独立操作。
  若数据库连关闭操作也拒绝，返回明确错误，仍需后续孤儿轮回收处理。
- 取消在同步提交前生效；进入提交后不把已落账结果重新标成取消。
- 本批不增加数据库迁移、不改变 publish token 作用域、不切换默认服务。
  真 Mihomo 端到端、DNS/ECS 接入、出口验证、发布执行面与 shadow 对账仍待完成；
  不能把本地回归通过描述成 Rust 迁移或线上测活验证完成。

---

## R1 — 2026-09-30（HEAD：R1 提交）

### 环境
- rustc/cargo 1.98.1，工具链 `stable-x86_64-pc-windows-msvc`
  （本机默认 `x86_64-pc-windows-gnu` 缺 C 编译器，rusqlite bundled / windows-sys 无法构建；
  MSVC 侧有 VS 2022，构建正常。CI 在 ubuntu 上不受影响。）
- 命令一律 `cargo +stable-x86_64-pc-windows-msvc …`；CI 使用 `cargo test --workspace --locked`。

### 1. cargo fmt
```
cargo +stable-x86_64-pc-windows-msvc fmt --all          # 执行
cargo +stable-x86_64-pc-windows-msvc fmt --all -- --check   # 通过（无输出）
```

### 2. clippy（-D warnings，CI 同参）
```
cargo +stable-x86_64-pc-windows-msvc clippy --workspace --all-targets -- -D warnings
→ Finished `dev` profile … in 1.45s   （零告警；一处 clamp-like 模式已改为 .clamp(1, 32)）
```

### 3. cargo test --workspace --locked
```
probe-api     5 passed   (鉴权矩阵/零秘密状态/round 起一次且必闭合/ct 比较/healthz-readyz)
probe-config  6 passed   (缺文件/坏 JSON→默认值、覆盖合并、空 token 拒绝、core.secret 一次生成)
probe-domain  2 passed   (RoundSummary JSON 往返、错误展示)
probe-mihomo  8 passed   (内核配置逐字节快照、空组 DIRECT、失败分类对拍 _reason_from、
                          mock 内核：Bearer 必带/reload 204/delay 成功与 504 timeout、控制器失联归类)
probe-storage 4 passed   (round 行往返、UTC 时间戳与 Python db.now() 同形、open_rounds、空账本)
probe-cli     0 tests (bin) — 由下方冒烟覆盖
合计 25 passed / 0 failed（含 doc-tests 0）
```

### 4. CLI 端到端冒烟（临时根 .tmp_diag/smoke-root，测毕删除）
```
probe-cli --root <tmp> status   → "no rounds recorded yet"
probe-cli --root <tmp> round    → round 1: slice round: config at <tmp>\core\config.yaml;
                                  slice: controller unreachable (http://127.0.0.1:19190)
probe-cli --root <tmp> status   → last round: #1 trigger=cli finished_at=2026-09-30T14:28:25(note 如实)
probe-cli --root <tmp> serve    → /healthz {"ok":true}（无鉴权）
                                  /api/v1/status（带 token）→ {"service":"mihomo-probe-rs","rounds":…,
                                  "kernel":{"api_host_port_only":"127.0.0.1:19190"}}   # 零秘密
                                  /api/v1/status（无 token）→ 401
                                  POST /api/v1/rounds（带 token）→ 202，任务闭合 round 2（note=unreachable）
                                  WARN 日志: controller unreachable during slice round round_id=2
生成的 core/config.yaml 首 11 行与 Python build_config 逐字段一致
（mixed-port: 19194 / allow-lan: true / bind-address: 127.0.0.1 / external-controller: 127.0.0.1:19190 / secret 自动生成）
data/ 产出 state.db(+WAL) / core.secret(0600 语义) / config.json 未被改写
```

### 已知限制（如实记录）
- 本机无 Docker / 无真 mihomo 内核：`mihomo -t` 校验与真内核 reload 在 vps 复核（R3 门禁）。
- Controller 对 127.0.0.1:1（无监听）的连接失败归类为 Controller 错误——由单测锚定。
- 快照测试发现的 serde_json key 顺序差异已用 `preserve_order` 对齐 Python `json.dumps` 文档序。

---

## R2 — 2026-09-30

### cargo fmt / clippy
```
cargo +stable-x86_64-pc-windows-msvc fmt --all -- --check   # 通过
cargo +stable-x86_64-pc-windows-msvc clippy --workspace --all-targets -- -D warnings
→ Finished（一处 needless_mut_ref 于测试代码已改）
```

### cargo test --workspace --locked（30 通过 / 0 失败）
```
probe-storage 9: 迁移版本连续性、幂等重放、Python 旧库迁移后数据逐行保全+新表就位+integrity ok、
                 备份快照一致性、open_without_migrating 不变更状态、round/时间戳原有 4 例
probe-mihomo 8 / probe-api 5 / probe-config 6 / probe-domain 2（与 R1 相同，回归通过）
```

### db 子命令端到端演练（Python 形态 legacy 库，数据为占位符）
```
db check   → applied 0, pending 2, integrity ok, required tables missing
db migrate → backup written: state.db.bak-<ts>；2 applied；counts: nodes 1 / rounds 1 / results 1（legacy 数据保全）
db verify  → integrity ok + 全表计数
Python 复读 → nodes/rounds/results 原值不变，export_snapshots 可见
db rollback → 缺文件 rc=1；无 --yes rc=2；--yes 恢复迁移前快照，check 如实报“缺表”（备份早于迁移，语义正确）
```

---

## R3 — 2026-10-01

### fmt / clippy
```
cargo fmt --all -- --check   # 通过
cargo clippy --workspace --all-targets -- -D warnings   # Finished（校验器补 120s 超时+kill）
```

### cargo test --workspace --locked（45 通过 / 0 失败，新增 15）
- lanes(3)：端口区间、lane_count 夹取(0→1/99→32)、组名/监听名与 Python 逐字符一致
- exit(6)：trace 解析（ip/loc/colo、垃圾行、空键）、经车道读出口身份（reqwest+Proxy::all 对 axum mock 代理，
  绝对 URI 代理形态即真实 mihomo 入站所见）、空 trace 判失败、fetch 字节上限（1MiB 体 64KiB 帽，提前停）、
  拨号失败有界错误（≤160 字符）
- config_check(6)：退出码→Passed/Failed、无 MIHOMO_BIN 且无 docker → Degraded（显式降级，绝不读作通过）、
  culprit 定位（引号名优先/短名永不 token 匹配——"jp" 历史 bug/长名边界匹配（BageVM-Tokyo 不匹配
  BageVM-Tokyo-2）/server 兜底）
- controller 新增 select：PUT /proxies/{group}（200/204 ok，非 2xx 带正文报错）
- 回归：R1/R2 全部用例通过

---

## R5（并发部分）— 2026-10-01

新 crate `probe-engine`，本批只含分层并发闸门（`src/limits.rs`）。
新增依赖：`tokio` 补 `sync`/`time` 特性、`tokio-util 0.7.19`（CancellationToken）。
`tokio-util` 的 `sync` 模块在 0.7 中**不受 feature 门控**——写 `features = ["sync"]`
会报 `tokio-util does not have that feature` 并中止解析（实测踩过）。

### fmt / clippy
```
cargo fmt --all -- --check                                  # 通过
cargo clippy --workspace --all-targets -- -D warnings       # Finished（零 lint；仅 Windows
                                                            # 增量目录 os error 5 噪声）
```

### cargo test --workspace --locked（**58 通过 / 0 失败**，新增 13）
```
probe-engine 13:
  global_limit_is_enforced                      global=2 / 8 个独立源与地址，全局峰值恰为 2
  per_source_limit_serialises_a_single_source   同源 4 任务峰值 1；另有 3 个对照组任务
                                                （独立源+独立 IP）证明 global 确实放行了 >1
  per_server_ip_limit_serialises_one_address    同 IP 4 任务峰值 1；同样带对照组
  all_three_layers_hold_at_once                 3 源 × 3 IP，三层同时生效，峰值均不越界
  permits_return_to_the_pool                    12 任务后 available_permits 复原
  cancel_fails_a_queued_job_instead_of_waiting  取消唤醒阻塞中的 acquire，返回 Cancelled
  check_refuses_the_ledger_write_after_cancel   check() 在取消后拒绝写账本
  mixed_load_finishes_within_budget             60 任务 / 4 源 / 3 IP，5s 内排空（死锁哨兵；
                                                理论下限 ~0.5s）
  diagnose_lane_runs_while_the_fast_lane_is_saturated  快车道打满时诊断车道仍可获取
  idle_registry_entries_are_pruned              2098 个不同 IP 后注册表 < 256（跨多轮剪枝窗口）
  zero_limits_degrade_to_serial_instead_of_hanging  全 0 上限夹取为 1，不死锁
  effective_limits_clamp_at_the_ceiling         usize::MAX / 0 / 99999 的夹取
  from_concurrency_keeps_the_deployed_ceiling   concurrency=20 → global 仍为 20
其余 crate（probe-api 5 / config 6 / domain 2 / mihomo 23 / storage 9）回归通过
```

### 时序用例稳定性
```
for i in 1..10: cargo test -p probe-engine   → 10/10 轮 13 passed，无抖动
```

### 独立审查（只读 subagent）发现并修正的 7 项
1. 模块文档把 `diagnose` 画成 `global` 的子层，与实现矛盾——`acquire_diagnose` 不取
   global，诊断车道在 global 打满时仍可并发。**已改为平级并写明峰值是 `global + diagnose`**。
2. `Permit` 注释称"逆序归还"，实际 `Vec` 正序 drop（global 先放）。已改注释。
3. 剪枝测试断言 `ips <= 1024` 过弱（只要剪枝跑过一次就必过）。改为 2098 个 IP + `< 256`。
4. 死锁哨兵 20s 过松（理论下限 ~0.5s）。降到 5s。
5. per_source / per_server_ip 两测缺正向断言，可能因上层顺带串行而假阳性。
   **加对照组后暴露了原断言设计错误**（4 个任务同源时 global 峰值必然是 1），已修正。
6. `REGISTRY_PRUNE_AT` 的理由写成"长生命周期进程累积"——`Gate` 随轮次建销，不成立。
   已改为"限制单轮内的分配"。
7. §2.4 的"节点集合每轮都变"是未实测的前提。已改写为不依赖该前提的论证。

### 已知限制（如实记录）
- 本批只交付**闸门本身**，尚无调用方：未接入实际轮次流水线，因此
  "取消后不写账本"目前由 API 约定保证，不是由类型系统强制。
  **→ 已由下一节（R5 轮次编排）解决：闸门已接进 `run_round`，并有对应单测。**
- **`diagnose` 不在 `global` 之下**（有意为之：否则会排在它所绕开的拥塞后面）。
  因此单轮在飞峰值为 `global + diagnose`，配置两层时要一起算。
- `Limits::from_concurrency(20)` 的 per_source/per_server_ip/diagnose 是**比例初值**，
  未用真机数据调优（06 已列为待办）。
- 同一任务对同一 key 取两次会自锁（信号量不可重入）；链式节点必须传前置地址。
  这是调用方契约，**代码层无法强制**，只能靠文档与 code review。
- 未做 `-race` 等价检查（Rust 无 `-race`；Miri/loom 选型在 14 的 R12 批次）。
- 单元测试全部用 `multi_thread` 运行时，但仍在单机 4 线程下跑；真机高并发未验证。

---

## R5（RoundCtx 接入流水线）— 2026-10-01

把分层并发闸门接进真实轮次路径。此前轮次逻辑**存在两份**（`probe-cli::cmd_round`
与 `probe-api::start_round`），都是同一段占位代码，且都没走 `RoundCtx` —— 闸门没有调用方。

### 新增/修改
- `crates/probe-engine/src/round.rs`（新）：`run_round` / `run_round_with_ctx`，
  trait `NodeTester` / `Ledger` / `KernelPrep`，`KernelDelayTester` / `ControllerPrep` /
  `NotPrepared`，`RoundSettings`（把「哪些配置决定一轮怎么跑」收在一处）。
- `crates/probe-storage`：`ResultRow`、`record_results`（**单事务**批量写入）、
  `finish_round_with_counts`、`results_for_round`，常量 `VERDICT_OK/FAIL/EXCLUDED`。
- `crates/probe-config`：新增 `TestConfig`（`targets` / `expected_status` / `timeout_ms` /
  `concurrency`），默认值与 Python `config.DEFAULTS["test"]` 一致。
- `crates/probe-cli`、`crates/probe-api`：轮次路径改为调用 `run_round`，删掉两份重复占位。

### fmt / clippy
```
cargo fmt --all -- --check                                # 通过
cargo clippy --workspace --all-targets -- -D warnings     # 0 errors
```

### cargo test --workspace --locked（**79 通过 / 0 失败**，新增 17）
```
probe-engine 25（+12）:
  a_round_records_every_verdict_and_closes_once        一次开行/一次关行/一批写入
  failures_are_counted_and_carry_their_class           失败计数与 FailureKind 落 row
  the_gate_actually_bounds_the_node_phase              per_server_ip=1 真的串行（闸门确实生效）
  distinct_addresses_still_run_concurrently            对照组：不同地址仍并发
  an_unreachable_kernel_skips_the_node_phase_and_still_closes  内核没了就不测，行照样关
  a_refused_reload_still_tests_nodes                   reload 被拒仍测（内核可能在跑旧配置）
  cancellation_drops_results_but_still_closes_the_row  取消：0 结果写入，行仍关闭
  a_round_already_cancelled_writes_nothing_but_closes  开始前已取消
  the_note_never_carries_a_controller_message           note 不含错误正文/URL
  detail_truncation_is_character_safe                  200 字符截断不 panic（CJK 安全）
probe-config 11（+5）：test 段默认值对齐 Python、空 targets 回落、全 http 无首选目标、覆盖
probe-storage 13（+4）：results 往返、空批次不写、finish_with_counts 写 total/ok
probe-api 5 / probe-mihomo 23 / probe-domain 2：回归通过
```

### CLI 端到端冒烟（临时根 `.tmp_diag/smoke-r5`）
```
probe-cli --root <tmp> round
  → round 1: kernel unreachable; 0 node(s) not tested
probe-cli --root <tmp> status
  → last round: #1 trigger=cli finished_at=Some("2026-10-01T14:17:20")
                note=Some("kernel unreachable; 0 node(s) not tested")
账本直查：rounds 1 行（total/ok/failed 均为 0，finished_at 有值）；
          results 0 行；SELECT ... WHERE finished_at IS NULL → 空
```

### 时序用例稳定性
```
cargo test -p probe-engine  × 10 → 10/10 轮 23 passed
cargo test -p probe-api     × 10 → 10/10 轮  5 passed
```

### 独立审查（只读 subagent）发现并修正的 6 项
1. **[高] `in_flight` 非全路径清理**：清理写在 spawned task 尾部，task panic 即跳过
   → `POST /api/v1/rounds` 永久 409。改为 RAII drop guard `InFlight`（unwind 也会释放），
   并把 handler 里 `storage.lock().expect(...)` 的 panic 面收成显式 500。
2. **[中] 全 http 目标与 Python 分歧**：我原把"没有 https 目标"当作 blocked 跳过节点阶段。
   实读 `engine.py:247` 后确认：`https_required = urls[0].startswith("https://")`（分区之后），
   **全 http 列表时 Python 会把 http 通过判为存活**。已对齐，并新增
   `TestConfig::https_required()` 把这个规则写进代码。
3. **[中] 单事务"失败不留半截"没有证据**：新增用例用 `BEFORE INSERT` 触发器让**第二行**中止，
   断言 `results` 一行不剩，且后续批次仍可用 —— 这是唯一能证明回滚的写法。
4. **[中] `unchecked_transaction` 的隐含依赖未写明**：它取 `&self`、绕过借用检查，
   只在"每连接一个 `Mutex`"下安全。已在 doc 里写明"不要把 `Storage` 交给两个线程/两个并发 future"。
5. **[低] 取消后 permit 是否复原无直接断言**：新增 `Gate::available_permits()`，
   在取消用例里断言归位（`(limits.global, limits.diagnose)`）。
6. **[低] 文档陈旧/措辞过强**：`16_FINAL_REVIEW` 仍写"未接流水线"；`06` 的"每条路径都关"
   未标注 `finish_round` 自身失败的例外。均已修。

审查同时确认成立的：`finish_round` 是开行后唯一的 `?`（其余失败都被 match 吞下再关行）；
取消用例非退化（`per_server_ip=1` 串行保证取消落在轮次中）；两条闸门用例互为对照
（纯串行实现过不了对照组，无限流实现过不了主用例）；`TestConfig` 四项默认值与
`config.py:239-247` 逐字一致；API 响应形状未变。

### 已知限制（如实记录）
- **轮次目前没有节点**：`jobs` 恒为空（节点采集 Sub-Store→fingerprint→变体尚未移植）。
  所以闸门虽然接上了，实际还没限到任何东西 —— 单测用假 tester 证明它能限。
- `KernelDelayTester` 只做**一次**尝试、只打分区后的**首个**目标。
  Python `test_one` 的 `max_attempts=3`、超时升级（`timeout_ms_retry`）、
  `TERMINAL_REASONS` 短路、以及失败后轮换目标均未移植；`attempts` 列固定写 1。
- 没有重试、没有整轮护栏（`GuardDecision` 是 R7）、没有出口验证（R6）。
  取消路径已具备且已测，但**没有对外触发点**（`POST /api/v1/rounds/{id}/cancel` 属 R9）。
- `RoundSummary` 未增加 `failed` 字段，`last_round()` 读不到 `failed`；
  `finish_round_with_counts` 写进去了但读侧暂不可见。
- `POST /api/v1/rounds` 的轮次行由 handler 同步开启（为在响应里返回 `round_id`），
  再交给 spawned task 关闭。若 `finish_round` 失败，行会留在打开态 —— 依赖
  `open_round_ids()` / 孤儿回收，日志会明确报出。**这是 `run_round` 唯一不关行的路径。**

---

## R4 — 2026-10-08（补做批；HEAD：R4 提交）

> R4 在 R0–R3 后被跳过、直接进了 R5–R7，`crates/probe-dns/` 一直以未提交的工作区状态
> 存在（未 `git add`，`Cargo.toml` 成员行与 `Cargo.lock` 条目也未提交）。本批整理、补齐、入账。

### fmt / clippy
```
cargo +stable-x86_64-pc-windows-msvc fmt --all
cargo +stable-x86_64-pc-windows-msvc fmt --all -- --check      # 无输出
cargo +stable-x86_64-pc-windows-msvc clippy --workspace --all-targets -- -D warnings
→ 0 error（判据 `grep -cE "^error"`；输出里的 os error 5 是 Windows 增量目录噪声，非 lint）
```
- 上一批（lane 分桶修复）遗留一处 `needless_lifetimes`（`crates/probe-mihomo/src/lanes.rs:47`），
  说明那批没跑过 clippy；本批一并修掉。

### cargo test --workspace --locked（**243 通过 / 0 失败**，本批新增 22）
```
probe-api      5 passed
probe-cli     24 passed
probe-config  20 passed
probe-dns     22 passed   ← 本批新增（wire 18 / geo 3 / resolver 1）
probe-domain  12 passed
probe-engine  67 passed
probe-mihomo  25 passed
probe-source  46 passed
probe-storage 19 passed
probe-substore 23 passed（耗时 190s，rquickjs 跑官方 bundle）
```
（243 = R7 后 219 + lane 修复 2 + 本批 22）

### probe-dns 的 22 个用例
```
wire（18）：报文头/ECS option 逐字节断言、无 ECS 则无 additional、非 ASCII label 拒绝、
           A/AAAA 顺序、跳过其它类型、非零 RCODE、短包、自指针 question、
           answer owner name 走指针；+ 8 个 fixture 用例（见下）
geo（3）：批量 POST 语义（本地 axum stub 回显）、429 一次重试后报错、畸形 body 也重试
resolver（1）：视图内去重保序、无 resolver 的视图被跳过、失败视图产空列表
```

### 二进制 fixture（`crates/probe-dns/fixtures/dns-packets/`，8 个）
由 `tools/make_dns_fixtures.py` 生成（无随机、每次写出同样字节）。逐包断言：
```
a-answer-compressed.bin    43B  owner name 指针→offset 12 → ["1.2.3.4"]
aaaa-answer.bin            56B  16B rdata → ["2001:db8::1"]
cname-then-a.bin           66B  ANCOUNT=2，跳过 CNAME → ["1.2.3.4"]
nxdomain.bin               30B  rcode 3 → Err(Rcode(3))
truncated-answer.bin       41B  rdlength 声称 4 实给 2 → Err(Truncated)
pointer-loop-question.bin  18B  question 自指针 → Ok([])
bad-label-overrun.bin      22B  label 长度 0x41(65) → Err(Truncated)
short-header.bin            6B  < 12 → Err(Truncated)
```
其中「恶意长度」「坏标签」是内联测试原本没覆盖的两类。

### 独立审查（只读 subagent，两轮）发现并修正的 11 项
（完整版见 CHANGELOG_RUST.md 的 R4 节；此处只列摘要）
- **第一轮 6 项**：`fetch_batch` 缺 45s 超时；畸形 JSON body 未重试；`lib.rs`/`Cargo.toml`
  现在时假陈述；「坏标签」fixture 实际缺失；`parse_ips`/`MAX_NAME_JUMPS` doc 不准确；
  `skip_name`/`read_name` 逐字重复 + 3 个未使用依赖。
- **第二轮 5 项**（含第一轮的漏网与我自己的新错误）：`encode_name` doc 仍是假陈述；
  **顶注把「自指针」与「label 链越界」混为一谈（第一轮修复时我自己引入的）**；
  非数组 JSON 被重试（有意选择，已记 doc）；`query` 超时与 `ecs_prefix` 缺 Python 默认值
  （已加 `DEFAULT_TIMEOUT_S`/`DEFAULT_ECS_PREFIX`）；`MAX_NAME_JUMPS` off-by-one 等措辞。

审查者明确**无法验证**的一项：`skip_name`/`read_name` 合并前「逐字节相同」——这批代码
此前未入库，无基线可 diff；依据是整理时读到的原文。

审查确认成立：8 个 fixture 字节逐一手工解析与断言相符；超时/重试/80 截断与 Python 逐项
一致；删依赖无遗漏且 `serde_json` 够用；`rand_id` 对齐 `os.urandom(2)`；`build_query`
显式 qid 不影响对账；`resolve_views` 失败 query 的最终视图与 Python 一致。

### 已知限制（如实记录）
- **未接入任何调用方**：`grep -rn "probe-dns" crates/*/Cargo.toml` 只命中它自己，
  `probe-engine` 未依赖它；`classify_and_expand` 未实现。轮次仍"一个 server 测一个地址"。
- **缓存未做**：`domain_views`(6h) / `ip_geo` 归调用方，本 crate 不持有。
- **`cargo-fuzz` 未接**：md 目标是"fixture 与 fuzz"，本批只交付固定 fixture。
- **非 ASCII 域名是真实分歧**：Python 的 idna 编码会成功、Rust 拒绝（无 `idna` 依赖）。
- 本机无真内核/容器，与既有批次相同。

