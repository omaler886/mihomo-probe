# mihomo_test 安全与许可证审计报告（第一轮）

- 审计人：security-license 子 Agent
- 日期：2026-09-28
- 范围：工作仓库 `D:\ChatGPT\代理测活`（只读审计，本报告是唯一产出文件）
- 方法：全仓凭据扫描（含未跟踪文件）、git 历史抽查（`git log -p --all`）、HTTP 出口点 / subprocess / 文件服务 / 鉴权逐点核对、依赖与上游许可核对
- 脱敏约定：本报告不出现任何敏感值明文，一律以「文件:行号 + 类型 + 前 4 字符 + …」表示

严重级定义：P0=立即泄露风险（提交即泄露或已泄露）｜P1=高危｜P2=中｜P3=低/加固建议。

---

## 一、发现清单

### S-01（P0）已提交文件含真实节点凭据（vless 分享链接）
- 证据：`tools/chain_diagnose.py:31-36` —— `TARGET_URIS` 内含 4 条真实 vless 分享链接：真实 UUID（分别以 `<UUID 片段>`、`<UUID 片段>` 开头）、REALITY 公钥/short-id（`pbk=<公钥片段>`、`pbk=<公钥片段>`）、真实服务器域名/IP；`FRONT_URI` 含自建前置节点真实 UUID（`<UUID 片段>`）及自建前置域名（`<前置域名>`）。
- 状态：**该文件已被 git 跟踪，内容已进入提交 初始提交，即 git 历史中已存在这些凭据**（`git log -p --all` 可见）。
- 建议：
  1. 视这些 UUID/REALITY 参数已泄露，**先在订阅侧/节点侧换发凭据（轮换 UUID、更换 pbk/sid）**；
  2. 将 `TARGET_URIS`/`FRONT_URI` 改为从环境变量或本地未跟踪文件读取；
  3. 若基线要推到公开/共享远端，必须改写历史（`git filter-repo` / BFG 清洗 初始提交）。

### S-02（P0）已提交文件以 proxy 字典形式重复同一批真实凭据
- 证据：`tools/chain_test2.py:26-59` —— `EDGETUNNEL`/`T2/T3/T4` 节点字典内嵌与 S-01 相同的真实 UUID（`<UUID 片段>`、`<UUID 片段>`、`<UUID 片段>`）、自建前置域名与 REALITY 参数。
- 状态：已跟踪、已在 HEAD/历史中。
- 建议：同 S-01；即使 S-01 改掉，此文件不改则历史与工作区仍含泄露源。

### S-03（P0）已提交文件含自建前置节点凭据
- 证据：`tools/front_debug.py:22-25`、`tools/front_ablation.py:22-33` —— `FRONT_URI`/`BASE` 内嵌自建前置真实 UUID（`<UUID 片段>`）、前置域名与 xhttp/host 配置。
- 状态：已跟踪、已在 HEAD/历史中。
- 建议：同 S-01，前置节点凭据一并轮换。

### S-04（P0）DOM 快照含真实发布令牌（publish token）
- 证据：`dom_live.html`（约第 5000+ 行的导出地址区）与 `dom_offline.html`（约 8800+ 行）—— 多处 `?token=<token 片段>` 的导出 URL（/api/export/alphasub.yaml、betasub.yaml、gammasub.yaml、deltasub.yaml）。这是面板真实 publish token 的明文快照。
- 状态：未跟踪、未入历史；但 `reports/`、`dom_*.html` 均不在 .gitignore 覆盖范围内。
- 缓解事实：经比对，快照未包含 admin token（bootstrap 块未捕获），泄露面为只读发布令牌。
- 建议：
  1. `dom_*.html` 删除或加入 .gitignore，禁止提交；
  2. 在 vps 部署侧轮换 publish token（快照已在本机明文留存多日）。

### S-05（P0）诊断目录含真实订阅 URL（内嵌密钥路径）
- 证据：`.tmp_diag/subs.json`（313KB）—— 20 个 remote 订阅对象的 `url` 字段为真实订阅地址，含密钥路径（如 `<订阅域名>/link/<密钥>`）；`.tmp_diag/cols.json` 同类。
- 状态：`.tmp_diag/` 未被 .gitignore 覆盖（当前 ignore 只有 `.tmpcheck/`）。
- 建议：`.tmp_diag/` 整体加入 .gitignore；该目录在本机仍属敏感留存，建议用后清理。

### S-06（P0）诊断目录含真实节点完整配置（raw Clash YAML）
- 证据：`.tmp_diag/sub_try.txt` 与 `.tmp_diag/sub_v6_raw.txt`（内容相同）—— 真实订阅原始正文：节点 UUID（`<UUID 片段>`）、12 个真实 server IPv6 字面量、端口等完整凭据；`.tmp_diag/vpscode.tgz`、`deployed.tgz` 为部署代码打包件；`.tmp_diag/cd1/`、`cd2/` 为浏览器 profile 快照（含浏览历史类隐私）。
- 建议：同 S-05，`.tmp_diag/` 整体 ignore 并清理；tgz 内如需留档先脱敏。

### S-07（P0）reports/ 目录含订阅 URL 与节点完整凭据
- 证据：
  - `reports/ipmap-demo-v6.md:1` —— 标题即真实订阅地址（`https://sub.example.com/a9K2…/291a…`，密钥路径明文）；
  - `reports/ipmap-demo-v6.json` —— 12 个节点的 `orig_proxy` 完整凭据（uuid、reality-opts、cipher、server），且 `title` 字段同为该订阅地址。
- 状态：`reports/` 未跟踪且**不在 .gitignore**，极易随基线提交泄露。
- 建议：`reports/` 加入 .gitignore；已生成的两份 ipmap 报告先脱敏（去掉 title 的订阅 URL、JSON 去掉 orig_proxy）再留存。

### S-08（P1）测试夹具使用真实节点凭据
- 证据：`tests/test_ipmap.py:32`（真实 server IPv6 `2001:db8::…`）、`:38`（真实节点 UUID `<UUID 片段>`，与 S-06 订阅正文中的 UUID 相同）、`:45/:51/:59/:61`（真实 IPv4 与真实域名 `<域名片段>`）。
- 状态：未跟踪新文件，按计划将随基线提交。
- 建议：把这些夹具值换成 RFC 5737（192.0.2.0/24、2001:db8::/32）文档地址与随机 UUID；仅此改动后该文件可提交。

### S-09（P2）ipmap 产物设计上携带订阅 URL 与节点凭据
- 证据：`mihomo_test/ipmap.py:435-436`（`title = args.url`，订阅地址直接进入报告标题/JSON）、`:502-504`（`{**m, "row": ...}` 把 `orig_proxy` 完整凭据写入 `data/ipmap/<key>.json`）。
- 影响：S-07 的根因——只要有人把产物拷出 `data/`（如拷到 reports/）就泄露。
- 建议：JSON 产物剥离 `orig_proxy`（保留 mihomo 别名 ↔ 实测出口映射即可）；报告标题用 `--key` 而非订阅 URL；`md` 标题已同问题（`ipmap.py:285-287`）。

### S-10（P2）链式测活结果文档泄露前置服务器地址与节点清单
- 证据：`chain-alive-r365.md:5`（真实前置域名 `<前置域名>`）、:11-80（70 个真实节点名称表）。
- 状态：未跟踪、不在 .gitignore。无凭据，但属基础设施信息 + 订阅内容快照。
- 建议：加入 .gitignore 或将域名/节点名脱敏后再提交。

### S-11（P2）应用容器挂载 docker.sock，面板令牌等价主机 root
- 证据：`docker-compose.yml:76`（`/var/run/docker.sock:/var/run/docker.sock`）；`mihomo_test/core.py:331-357`（应用经 docker CLI 操纵内核容器）。
- 代码注释已明确该风险（`mihomo_test/server.py:27-30`）。应用容器有 `mem_limit` 但无 `no-new-privileges`（应用侧）。
- 建议：如可行改用 docker socket 代理（按白名单暴露 `containers/*` 端点）；至少维持现有「面板令牌绝不外泄」纪律，并为应用容器补 `security_opt: no-new-privileges:true`。

### S-12（P2）/api/status 每次轮询回传 Telegram bot token、webhook URL、Sub-Store 后端地址
- 证据：`mihomo_test/server.py:172-193`（`redacted_config` 仅掩 `auth.token`、`publish.token`）；`mihomo_test/web/app.js:1154-1158`（设置表单确实回读这些值）。
- 影响：面板每 5 秒轮询一次的响应体内长期携带 bot token（`tg…` 形态）与含密钥路径的 Sub-Store 后端 URL；一次日志误存/一次转发即泄露。
- 建议：设置值单独走一个显式端点（或 `?reveal=1` 请求），轮询载荷里对这些字段一律 `***`。

### S-13（P3）部署源站域名硬编码在已跟踪代码中
- 证据：`mihomo_test/config.py:300`（默认 `MIHOMO_TEST_HOSTNAME` 为真实面板域名 `<面板域名>`）、`tools/build_web.py:38`（`DEFAULT_API_BASE` 同域）、`tools/verify_measure_switch.py:27`。README 亦多处提及。
- 影响：非凭据，但公开仓库将直接暴露面板与隧道源站；配合 S-04 类令牌泄露会放大风险。
- 建议：默认值改占位符（如 `panel.example.com`），真实值走环境变量/.env。

### S-14（P3）ipmap 订阅拉取无 SSRF 校验（当前仅 CLI 可达）
- 证据：`mihomo_test/ipmap.py:321-331`（`fetch_subscription` 接受任意 URL：无 scheme 白名单、无内网地址拦截、urllib 默认跟随重定向；有 30s 超时与 2 次 UA 兜底）。
- 缓解：该入口仅经 `python3 -m mihomo_test ipmap --url …`（`mihomo_test/__main__.py:36`）由运维本地触发，面板未暴露（`server.py` 无对应路由）。
- 建议：若未来接到面板，必须加 `https` 白名单 + 解析后拒绝私网/环回地址；当前保持 CLI-only 即可。

### S-15（P3）管理员可配置的出站 URL 无 scheme 校验（管理员侧 SSRF 面）
- 证据：`mihomo_test/notifier.py:55,72-84`（telegram/webhook URL）、`mihomo_test/doh.py:102-111`（DoH resolver）、`mihomo_test/store.py:32-66`（Sub-Store backend，`path.startswith("http")` 时还允许绝对 URL 覆盖）。
- 评估：全部处于 admin token 之后的 `/api/config` 写入路径，且 `config.py` 的 `reject_self_reference` 已阻断「订阅源指向自身导出」的回环；风险限于管理员自伤。
- 建议：低优先级——对 webhook/DoH/backend 加 `http(s)` scheme 校验即可。

### S-16（P3）发布令牌经 URL query 传递
- 证据：`mihomo_test/engine.py:2296-2304`（`/api/export/<key>.yaml?token=…`）、`server.py:108-114`（接受 query token）。
- 缓解：响应头已设 `Referrer-Policy: no-referrer`、`Cache-Control: no-store`（`server.py:43-52`）；这是 Sub-Store 远端订阅的兼容形态，难以完全避免。
- 建议：文档标注令牌会出现在反代访问日志中；日志侧注意脱敏。

### S-17（P3）500 响应回传原始异常文本
- 证据：`mihomo_test/server.py:407-408,527-528`（`{"error": f"{type(exc).__name__}: {exc}"}`）。
- 评估：仅在鉴权之后触发（`do_GET` 的 401 先于处理），信息面为内部路径/异常类型。
- 建议：对外仅返回简短错误，详情进 `db.log`。

### S-18（P3）运维脚本把参数拼进远端 shell 命令串
- 证据：`tools/pull_file.py:12-15`（`base64 -w0 {remote}`，remote 来自 argv）、`tools/push_run.py`（远端路径由本地文件名拼接）。
- 评估：本地运维个人工具、参数即操作者本人输入，无提权面；但习惯不良。
- 建议：远端路径固定白名单目录 + `shlex.quote`。

### S-19（P3）仓库无 LICENSE 文件
- 证据：仓库根目录无 LICENSE/COPYING（`git ls-files` 确认）。
- 建议：合并移植到官方 Sub-Store（AGPL-3.0，见下节）前，先决定本项目文件的许可归属；并入上游则随上游 AGPL-3.0。

### 正面确认（无发现，防扩散性检查通过）
- 路径穿越：`/api/export/<key>.yaml` 经 `config.validate_key`（`config.py:47-59`，拒绝 `/ \ : * ? " < > |`、控制字符、点开头）+ `engine.read_export` 的 `path.parent != EXPORT_DIR` 双重校验（`engine.py:2542-2554`），`../` 变体全部落到 404/None。**未发现穿越漏洞。**
- 命令注入：全仓（mihomo_test/、tools/、tests/）无 `shell=True`、无 `os.system`；docker CLI 调用均为 argv 列表形式（`core.py:373-380,475-489`，`ipmap.py:378-392`），容器名/路径来自管理员配置而非未鉴权输入。
- 鉴权：空令牌一律拒绝（`server.py:101-105,117-135`）；`auth.token` 与只读 `publish.token` 严格分离，`/api/export/` 才接受 publish token（`server.py:131-135`），Sub-Store 联动只写入 publish token 而非 admin token（`engine.py:2411-2416,2448-2449`）；比较用 `hmac.compare_digest`（UTF-8 字节）。
- CORS：精确匹配白名单后**回表白名单项而非请求 Origin**（`server.py:62-85`），preflight 未鉴权但无凭据头（`server.py:288-308`）。
- 前端安全：CSP `script-src 'self'` 无 unsafe-inline（`server.py:43-52`）；bootstrap JSON 转义 `<`/U+2028/9（`ui.py:64-87`）；静态资源白名单（`ui.py:38-42`）；CDN 构建不内嵌令牌（`ui.py:17-19`）。
- 内核面：mihomo 配置强制 `bind-address: 127.0.0.1`、external-controller 仅环回（`core.py:235-251`）；容器 `no-new-privileges` + 内存限制（docker-compose.yml、ipmap.py:384-386）；内核 secret 独立生成并 `chmod 600`（`config.py:640-646`）。
- 敏感日志：`db.log` 各调用点仅含节点名/计数/失败原因，未发现订阅正文、token、节点凭据进入日志；`/api/status` 已掩两个面板令牌（S-12 范围除外）。
- `.env` 不存在于工作区（仅 `.env.example`，全部为占位符/空值，`git check-ignore` 确认 `.env`、`data/`、`core/`、`dist/`、`.local/` 被忽略）。
- `reports/round-432/433-report.html`、`console_live.txt`、`console_offline.txt`：无 token、无 UUID、无节点凭据（仅本机 Chrome 日志/统计页），无 P0/P1 问题。

---

## 二、依赖与许可证

| 组件 | 版本/形态 | 许可证 | 结论 |
|---|---|---|---|
| PyYAML（requirements.txt 唯一第三方运行时依赖，钉版本 `PyYAML==6.0.3`） | 6.0.3 | MIT | 可随任意许可项目再分发，含并入 AGPL-3.0 的 Sub-Store |
| 其余 Python 代码 | 标准库（urllib/json/sqlite3/unittest 等） | Python Software Foundation License | 无兼容问题 |
| 基础镜像 `python:3.11-slim` | Dockerfile 第 6 行 | PSF + Debian 打包许可 | 无兼容问题 |
| `docker:cli`（COPY docker CLI） | Dockerfile 第 4 行 | Apache-2.0（Docker CLI/Moby） | 无兼容问题 |
| `metacubex/mihomo:latest`（运行镜像，docker-compose.yml:21） | latest | MIT（MetaCubeX/mihomo，经 GitHub API 核实） | 无兼容问题 |
| `cloudflare/cloudflared:latest`（docker-compose.yml:79） | latest | Apache-2.0 | 无兼容问题 |
| 官方 Sub-Store（sub-store-org/Sub-Store） | 合并目标 | **AGPL-3.0**（经 GitHub API 核实） | 见下 |

**Sub-Store 许可兼容性核对结论：**
- 本仓库代码**未嵌入任何官方 Sub-Store 源码/脚本**。`link_substore.py` 与 `mihomo_test/store.py` 是对 Sub-Store HTTP API 的原创客户端封装（REST 调用 + Clash YAML 处理），`engine.link_substore` 亦为原创；仅存在 API 互操作，不构成衍生作品问题。
- 将 mihomo_test 功能并入官方 AGPL-3.0 仓库时，PyYAML（MIT）、各镜像（MIT/Apache-2.0）许可均兼容；无 GPL/AGPL 冲突链。
- 唯一动作项：S-19——本项目尚无自有 LICENSE 声明；并入上游后随上游 AGPL-3.0 即可，若还想单独发布则需先自行选定许可。

---

## 三、基线提交安全结论

**结论：当前状态「不能」直接做基线提交。** 存在两类阻塞：

### A. 已在 git 历史中的泄露（P0，与本次提交无关但决定「基线能否外发」）
- `tools/chain_diagnose.py`、`tools/chain_test2.py`、`tools/front_debug.py`、`tools/front_ablation.py` 四个已跟踪文件含真实节点 UUID/REALITY 参数/自建前置凭据（S-01~S-03），已随提交 初始提交 进入历史。
- 处置：**先轮换这批节点凭据**（订阅侧换 UUID、前置节点换发），然后把四个文件中的真实链接/字典改为环境变量或占位符；若基线要推送到任何共享/公开远端，历史必须用 `git filter-repo`/BFG 清洗后才推。清洗+轮换完成前，仓库只能留在本机/私有环境。

### B. 未跟踪文件逐个裁决

| 文件/目录 | 裁决 | 依据 |
|---|---|---|
| `mihomo_test/ipmap.py`、`mihomo_test/web/` | **可提交** | 无硬编码凭据（S-13 的默认域名建议顺手改占位符） |
| `tests/test_hardening.py`、`tests/_isolation.py`、`tests/diag_round_suite.py` | **可提交** | 扫描无真实凭据 |
| `tests/test_ipmap.py` | **脱敏后提交** | S-08：真实 UUID/真实 server IPv6/真实域名换文档地址 |
| `tools/build_web.py`、`tools/verify_measure_switch.py` | **脱敏后提交** | S-13：默认 API base 改占位符 |
| `tools/chain_ab.py`、`tools/chain_verify.py`、`tools/deploy_files.py`、`tools/deploy_pages.py`、`tools/pull_file.py`、`tools/push_run.py`、`tools/v6_chain_cause.py`、`tools/v6_front_scan.py` | **可提交** | 无凭据（chain_verify 用 192.0.2.1 文档地址，pull/push 属 S-18 低危加固项） |
| `dom_live.html`、`dom_offline.html` | **禁止提交，删除或 ignore** | S-04：真实 publish token（`<token 片段>`）；并建议在部署侧轮换该 token |
| `.tmp_diag/`（整目录，含 subs.json、sub_try.txt、sub_v6_raw.txt、tgz、cd1/cd2 等） | **必须加入 .gitignore，禁止提交** | S-05/S-06：20 条真实订阅 URL + 完整节点凭据 + 浏览器 profile；建议用后清理 |
| `reports/` | **必须加入 .gitignore（或脱敏后提交）** | S-07：ipmap 报告含订阅 URL 与 `orig_proxy` 完整凭据；round-*.html 无敏感值，可留可弃 |
| `chain-alive-r365.md` | **建议 ignore（或脱敏后提交）** | S-10：真实前置域名 + 70 节点名，无凭据 |
| `console_live.txt`、`console_offline.txt` | **不建议提交（无敏感值）** | 仅本机 Chrome 日志，属噪音 |

### C. .gitignore 需新增的行
```
.tmp_diag/
reports/
dom_*.html
chain-alive-*.md
```

### D. 必须脱敏/轮换的内容汇总
1. **轮换**：publish token（S-04，快照已明文留存）；S-01~S-03 涉及的全部节点 UUID 与 REALITY 参数、自建前置节点凭据。
2. **脱敏**：`tests/test_ipmap.py` 夹具（S-08）；`tools/build_web.py`/`config.py` 默认域名（S-13）；如确需保留 ipmap 产物，去掉订阅 URL 与 `orig_proxy`（S-09）。

### E. 底线回答
- **功能代码本身（mihomo_test/*、web/、多数 tests/tools）是干净的**，鉴权/CORS/路径穿越/命令注入面经核查无 P0/P1 漏洞；
- 阻塞基线的全部是**数据文件与四个已跟踪的凭据文件**：完成第三节 A/B/D 三步（轮换、ignore、脱敏、必要时清洗历史）后，基线提交即可安全进行。

---

## 四、整改状态（B4 回填，2026-09-28）

| 项 | 状态 | 落点 |
|---|---|---|
| S-04 dom 快照含 publish token | **已整改（不入库）** | `.gitignore` 新增 `dom_*.html`；token 轮换待部署侧（MIGRATION_GUIDE §1.1） |
| S-05/S-06 .tmp_diag/ 含订阅 URL 与节点凭据 | **已整改（不入库）** | `.gitignore` 新增 `.tmp_diag/`；本地留存清理由使用者决定 |
| S-07 reports/ 含订阅 URL 与 orig_proxy 凭据 | **已整改（不入库）** | `.gitignore` 新增 `reports/` |
| S-08 测试夹具真实凭据 | **已整改** | 基线提交 B1：test_ipmap.py 真实 UUID/IPv6/IPv4/域名 → 占位 UUID + RFC 5737/2001:db8 文档地址（连带修复审查补充发现的 test_logic.py:2927、tools/chain_verify.py:70 同款真实 UUID） |
| S-01~S-03 已跟踪 tools 脚本凭据（已在 git 历史 初始提交） | **未整改（外发硬前置）** | 凭据轮换 + `git filter-repo`/BFG 清洗，见 MIGRATION_GUIDE §1；完成前仓库不得推共享/公共远端 |
| S-10 chain-alive-r365.md | **已整改（不入库）** | `.gitignore` 新增 `chain-alive-*.md` |
| S-13 默认面板域名硬编码（P3） | **本批有意不动** | vps 现网依赖 `config.py` 默认值生成联动 URL，改默认即行为回归；已固化为公开发行版前置项（MIGRATION_GUIDE §4） |
| S-09/S-12/S-14~S-19（P2/P3） | **未整改，未放大** | 本批（B3）零触碰相关代码；新端点经 reviewer 核对零泄露面 |
