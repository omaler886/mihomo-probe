# MIGRATION_GUIDE — Sub-Store 接入与外发手册（N-04）

> 面向对象：接手本仓库的运维/后续 Agent。契约细节以 `ARCHITECTURE.md` §3/§4 为准；
> 功能映射以 `MIGRATION_MAPPING.md` 为准；审查结论以 `REVIEW.md` 为准。
> 官方基线：`sub-store-org/Sub-Store` backend@`e08f1b1`（文档站 <https://sub-store-org.github.io/doc/>）。

---

## 1. 外发硬前置（在任何 push 之前，SECURITY_REVIEW §三.A）

当前仓库**只能留在本机/私有环境**。推送到任何共享/公共远端前必须完成：

1. **凭据轮换**（这些值已进 git 历史，视同泄露）：
   - `tools/chain_diagnose.py` / `chain_test2.py` / `front_debug.py` / `front_ablation.py` 内嵌的真实节点
     UUID（`<UUID 片段>`、`<UUID 片段>`、`<UUID 片段>`）、REALITY 公钥/short-id（`pbk=<公钥片段>` 等）、自建前置域名；
   - `dom_live.html` / `dom_offline.html` 快照中的真实 publish token（`<token 片段>`，已在 vps 侧明文留存，**在面板
     设置里换掉 publish.token**）。
2. **历史清洗**：`git filter-repo`（或 BFG）清洗上述四个文件后强推（如有远端）。
3. 工作区遗留敏感文件（`.tmp_diag/`、`reports/`、`dom_*.html`、`chain-alive-*.md`、`console_*.txt`）已被
   .gitignore 挡住且未入库；确认本地留存是否还需要，不需要就删。

## 2. 路径 B：probe 作为官方 Sub-Store 的远程订阅上游（零改动，推荐先做）

probe 每轮把收敛后的活节点写到 `data/exports/<key>.yaml`，官方把它当普通远程订阅拉即可。

1. 在 Sub-Store 前端「订阅」→ 新建，`url` 填：
   `https://<面板域名>/api/export/<key>.yaml?token=<publish.token>`
   （`publish.token` 是只读令牌，在 probe 面板「订阅输出」区可直接复制完整链接；`<key>` 见 probe 面板
   「数据源」每行的导出名。）
2. 需要 `target=ClashMeta` 以外的输出（sing-box/Surge/Loon…）时，交给官方 producer 处理——在官方订阅的
   下载链接上带 `?target=sing-box` 等，probe 侧不做任何改动。
3. 行为注意（均为官方语义，`store.py`/`engine.link_substore` 已显式建模）：
   - 官方对资源有 **1 小时缓存**，验收时在官方下载 URL 上加 `noCache` 强制拉新；
   - 官方对「零节点订阅」返回 **HTTP 500**——某来源还没有活节点时先别建订阅（probe 的
     link_substore 联动本来就跳过零存活来源）；
   - 官方对不存在资源也是 500 而非 404，排查时别被状态码误导。
4. **回滚**：删除该远程订阅即回到原状，probe 侧无任何状态变化。

## 3. 路径 A：Script Operator 消费 probe 账本（本批交付的扩展面）

给**任意**订阅（不限于 probe 导出的）按测活账本过滤死节点 / 标注实测国别。

1. 前提：probe 服务可达（`GET /api/probe/nodes` 已随 <B3 提交> 上线，publish.token 或 auth.token 皆可访问）。
2. 在 Sub-Store 目标订阅的「操作(process)」里添加**脚本操作**，粘贴
   `substore_bridge/probe_filter.script.js` 全文，并在订阅链接参数（`$arguments`）里传：
   ```
   probe_url=https://<面板域名>/api/probe/nodes
   probe_token=<probe 的 publish.token>
   mode=filter          # filter | annotate | both
   missing=keep         # keep | drop
   ```
3. 参数语义（详见 ARCHITECTURE §4）：

   | 参数 | 取值 | 语义 |
   |---|---|---|
   | `mode` | `filter`（默认） | 仅删除账本状态 `dead` 的节点；`pending/unknown/excluded` 与无记录节点一律保留（「不测 ≠ 死」，失败开放不误杀） |
   | | `annotate` | 存活节点名尾追加 ` [CC]`（实测国别），dead 追加 ` ·dead`；幂等（剥旧尾缀再追加） |
   | | `both` | 先 filter 后 annotate |
   | `missing` | `keep`（默认） | probe 不可达 / 节点无记录 → 保留 |
   | | `drop` | 无记录即删（严格模式：probe 挂了输出会清空，慎用） |
   | `probe_url` | 必填 | 缺失=配置错误，脚本 **throw** 让本次 produce 显式失败（不静默空转） |
   | `probe_token` | 可选 | 只经 `X-Auth-Token` 头发送，**不要**拼进 URL |

4. 验收（关掉 N-02 的「保留意见」，REVIEW 已知限制 1）：
   - 官方前端用 `POST /api/preview/sub`（或订阅预览页）看 operator 前后节点数与改名结果；
   - 断言：dead 节点被删、pending/unknown 保留；`[CC] 前缀名` 经 server 兜底命中；annotate 二次运行幂等；
   - 把 probe 停掉再 produce 一次：`missing=keep` 时输出应与上次一致（失败开放），且官方日志出现
     `[probe_filter] probe 不可达…`；
   - 验收结果回填 `TEST_REPORT.md`（新增「真机验收」节）。
5. **回滚**：在该订阅的 process 列表里删除脚本项即可；probe 侧零状态。官方若升级 Script Operator 契约
   （签名/`$arguments`/fetch 能力），只需改 `probe_filter.script.js` 一个文件（ARCHITECTURE §8）。

## 4. 部署与环境陷阱（新环境第一次起服务前必读）

- **`core.mixed_port` 不在 config.DEFAULTS**（`core.py:236` 直接下标读取；vps 是靠已部署 config.json 里的
  该键才能跑）。新环境按默认配置启动会在 `build_config` KeyError——部署时在 `data/config.json` 补
  `"core.mixed_port": <内核 HTTP 入站端口>`（vps 用 19194），或把 core.py 的读取改成 `cfg.get(..., 默认值)`
  （后者超出本批范围，见 REVIEW 建议节）。
- 三容器全 host 网络 + `MIHOMO_TEST_ROOT` 与宿主路径同位（`mihomo -t` 的 bind-mount 由宿主 daemon 解析），
  见 FEATURE_INVENTORY「移植决策最重要的 5 条架构事实」第 5 条。
- 已知遗留安全项：S-12（`/api/status` 轮询载荷携带 bot token/webhook URL，建议后续单独端点+打码）、
  S-13（`config.py`/`build_web.py` 默认面板域名硬编码——并入公开发行版前改占位符并走
  `MIHOMO_TEST_HOSTNAME` 环境变量；vps 现网依赖该默认值，本批不动）。
- 面板/面板令牌的安全模型（auth/publish 双 token、CORS 精确白名单）沿用 `server.py` 现状，见
  SECURITY_REVIEW「正面确认」清单。

## 5. 已知限制（来自 REVIEW.md 第五节，审核时先读）

1. N-02 脚本未在真实官方 backend 的 produce 管线内端到端跑过（harness 模拟官方契约）；
2. 真实内核/真实部署零涉及（531 用例全离线；test_live 需 vps 环境）；
3. 无 Node 的机器上 21 个 JS 用例整组 skip——CI 上显式断言 Node 存在；
4. 新端点未显式断言 Bearer 传法（`_presented_tokens` 未改动，其他面已覆盖）；
5. JS byName 对账本同名多记录取最后一条（未文档化的边缘，影响极小）；
6. annotate 模式下 `missing=drop` 无效果（纯改名从不删节点——契约如此）；
7. 外发前置（第 1 节）未完成前仓库不得外推。
