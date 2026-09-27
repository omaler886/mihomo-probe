# Sub-Store 官方能力调研报告（upstream-research）

> 调研目的：为把本仓库（mihomo-probe，Python 测活中心）的测活 / ipmap 等能力合并移植到官方 Sub-Store 架构，摸清官方后端架构、扩展点与测试设施。
> 调研日期：2026-09-28。所有结论均附证据（命令输出 / 源码路径 / 文档 URL）。克隆位置：`D:/ChatGPT/.tmp_upstream/`（浅克隆，未动本仓库任何文件）。

---

## 1. 仓库清单与锁定的 commit SHA

| 角色 | 仓库 URL | HEAD commit SHA（master/main） | 最近提交时间 | 获取方式 |
| --- | --- | --- | --- | --- |
| 后端（主仓库，含 backend/） | https://github.com/sub-store-org/Sub-Store | `e08f1b19f436b604fcafbf14a6743d635824e391` | 2026-09-27 13:54:03 +0800 `feat: #652 调整 Loon tls-profile 相关逻辑` | `git ls-remote <url> HEAD` + `git clone --depth 1` |
| 前端（PWA） | https://github.com/sub-store-org/Sub-Store-Front-End | `4bd7b0acbe65466b933ccea14fc61dbebe036ead` | 2026-09-22 13:06:19 +0800 `feat: 协议过滤支持 EasyTier` | 同上 |
| 官方文档（VitePress） | https://github.com/sub-store-org/doc | `8d3c62b2be076221bbc8b064407f85bc3bb971a6` | 2026-09-22 16:18:53 +0800 `members 页改为浏览器端实时加载…` | 同上 |
| HTTP-META（官方文档推荐的配套测活服务，社区作者 xream 维护） | https://github.com/xream/http-meta | `dd633b6433d5e0d2765d568592dc37520cc23aa0`（仅 ls-remote 验证存在，未克隆） | 未核实 | `git ls-remote <url> HEAD` |

**关键澄清：官方后端不是 `sub-store-org/Sub-Store-Backend`。** 该仓库不存在：

```
$ git ls-remote https://github.com/sub-store-org/Sub-Store-Backend HEAD
remote: Repository not found.
fatal: repository 'https://github.com/sub-store-org/Sub-Store-Backend/' not found
```

后端源码就在主仓库 `Sub-Store` 的 `backend/` 子目录（pnpm workspace，见 `backend/pnpm-workspace.yaml`；仓库里 dependabot 分支名 `dependabot/npm_and_yarn/backend/axios-1.6.0` 亦佐证）。后端仓库其他分支：`dev`（`2aa3cbd9d24c775e14f379015a536559d4c76019`）、`release`（`40462041c0e70baa44dca2a654a391550210ea56`，CI 推送构建产物）、`master`（默认，`e08f1b1…`）。

org 下其余仓库（GitHub API `GET /orgs/sub-store-org/repos` 输出）：`Sub-Store-Manager-Cli`（docker CLI 包装）、`resource`（静态资源）、`Sub-Store-Front-End-New`（2023 年停更的旧前端）、`subcase`（Android 包装）、`SubDock`（原生跨平台运行时管理器）、`.github`。

获取命令记录（2026-09-28）：

```bash
git ls-remote https://github.com/sub-store-org/Sub-Store HEAD
git ls-remote https://github.com/sub-store-org/Sub-Store-Front-End HEAD
git ls-remote https://github.com/sub-store-org/doc HEAD
git ls-remote https://github.com/xream/http-meta HEAD
git clone --depth 1 https://github.com/sub-store-org/Sub-Store            # → /d/ChatGPT/.tmp_upstream/Sub-Store
git clone --depth 1 https://github.com/sub-store-org/Sub-Store-Front-End  # → /d/ChatGPT/.tmp_upstream/Sub-Store-Front-End
git clone --depth 1 https://github.com/sub-store-org/doc                  # → /d/ChatGPT/.tmp_upstream/doc
```

文档站已在线验证可达：`curl -s -o /dev/null -w "%{http_code}" -L https://sub-store-org.github.io/doc/` → `200`。官方前端 demo/托管为 Vercel 上的 `https://sub-store.vercel.app`（前端 CI 用 vercel-action 直接部署，见 §7）。主仓库 README 的文档入口仍指向 GitHub Wiki：https://github.com/sub-store-org/Sub-Store/wiki 。

---

## 2. 后端运行时要求与启动方式

**版本与栈**（证据：`backend/package.json`，version `2.42.2`，license AGPL-3.0）：

- Node 版本：**24.15.0**（仓库根 `.node-version` 文件内容，前后端相同；README 明确"后端测试过的 Node 版本见 `.node-version`"）
- 包管理：**pnpm**（`"packageManager": "pnpm@11.0.9"`，`preinstall: npx only-allow pnpm`；CI 用 `corepack enable`）
- Web 框架：Express 4（自带魔改版 `src/vendor/express.js` + `patches/http-proxy@1.18.1.patch`）
- HTTP 客户端：undici ^8.8.0；定时：cron ^3.1.6；GeoIP：@maxmind/geoip2-node ^5.0.0；通知：shoutrrr-ts；加密：age-encryption、jsrsasign
- 多运行时设计：同一份 `src/` 同时跑在 QX / Loon / Surge / Egern / Stash / Shadowrocket / Node（`src/vendor/open-api.js` 的 `ENV()` 返回 `isQX/isLoon/isSurge/isNode/...`）；Node 只是其中一种 runtime，平台差异通过 `src/runtime/{fs,net,tls,worker-threads,child-process,...}.js` 抽象，Node API 一律经 `eval('process')` / `eval('require(...)')` 调用（为了能打进非 Node 的 bundle）。

**启动方式**（证据：`backend/package.json` scripts + `README.md` Development 节）：

```bash
cd backend && pnpm i
# 开发（esbuild watch + nodemon）：
SUB_STORE_BACKEND_API_PORT=3000 pnpm esbuild:dev
SUB_STORE_BACKEND_API_PORT=3000 pnpm run --parallel "/^dev:.*/"
# 打包产物：
pnpm bundle:esbuild     # → sub-store.min.js 等
pnpm serve              # node sub-store.min.js
# 测试：
pnpm test               # mocha src/test/**/*.spec.js
```

- 监听端口/地址：`SUB_STORE_BACKEND_API_PORT`（默认 3000）、`SUB_STORE_BACKEND_API_HOST`（默认 `::`）— `src/restful/index.js` `serve()`。
- 数据持久化：JSON 文件 `${SUB_STORE_DATA_BASE_PATH||'.'}/sub-store.json`（+ `root.json`）— `src/vendor/open-api.js` `persistCache()`：
  ```js
  const basePath = eval('process.env.SUB_STORE_DATA_BASE_PATH') || '.';
  this.node.fs.writeFileSync(`${basePath}/${this.name}.json`, data, ...)
  ```
- Docker：官方仓库内无 Dockerfile（未核实官方镜像构建源）；官方文档推荐 Docker 镜像为 `xream/sub-store`（普通版 / 带 `http-meta` tag 的测活版），见 `doc/advanced/http-meta.md`。

---

## 3. HTTP API 面（路由表，以源码为准）

路由注册入口：`backend/src/restful/index.js`（`register*Routes` 逐个挂载）。逐文件 grep `$app.(get|post|put|patch|route)` 结果汇总：

**订阅 / 组合订阅**
| 路由 | 方法 | 源码 |
| --- | --- | --- |
| `/api/subs` | GET/POST/PUT | `subscriptions.js` |
| `/api/sub/:name` | GET/PATCH/DELETE | `subscriptions.js` |
| `/api/sub/flow/:name` | GET（流量信息） | `subscriptions.js` |
| `/api/collections` | GET/POST/PUT | `collections.js` |
| `/api/collection/:name` | GET/PATCH/DELETE | `collections.js` |

**下载 / 分享输出（核心输出面）** — `download.js`
| 路由 | 说明 |
| --- | --- |
| `GET /download/:name`、`/download/:name/:target` | 单条订阅输出，`target` 指定产出平台（缺省按 User-Agent 判定，`getPlatformFromHeaders`） |
| `GET /download/collection/:name[/:target]` | 组合订阅输出 |
| `GET /share/sub/:name[/:target]`、`/share/col/:name[/:target]`、`/share/file/:name` | 分享链接，需 `?token=`（`token.js` 校验，可携带 age 公钥加密输出 `age-output.js`） |

常用 query 参数（`download.js` 源码）：`target/platform`、`produceType`、`includeUnsupportedProxy`、`proxy`、`noCache`、`mergeSources`、`ignoreFailedRemoteSub`、`ua`、`content`、`prettyYaml`；SurgeMac 还支持 `mihomoExternal/mihomoMerge/mihomoLocalPort`（借助 mihomo 外部控制器输出 Surge 不支持的协议）。

**Artifacts（产物托管到 Gist）与同步** — `artifacts.js`、`sync.js`
| 路由 | 方法 |
| --- | --- |
| `/api/artifacts` | GET/POST；`/api/artifact/:name` GET/PUT/PATCH/DELETE；`/api/artifacts/restore` GET |
| `/api/sync/artifacts`、`/api/sync/artifact/:name` | GET（同步到 Gist） |

**文件 / 模块 / 令牌 / 归档**
| 路由 | 方法 | 源码 |
| --- | --- | --- |
| `/api/files`、`/api/file/:name`、`/api/wholeFile(s)/:name` | GET/POST/PUT/PATCH/DELETE | `file.js`（file 支持 `mihomoConfig`/`mihomoProfile` 类型，Script Operator 对其走 `main(config)` 补丁式处理） |
| `/api/modules`、`/api/module/:name` | GET/POST/PUT/PATCH/DELETE | `module.js` |
| `/api/token`（POST 签发）、`/api/token/:token`（DELETE）、`/api/tokens`（GET） | | `token.js`（jsrsasign 签名的分享令牌） |
| `/api/archives`、`/api/archive/:id`、`/api/archives/:id/restore` | GET/DELETE/POST | `archives.js` |

**工具 / 其他**
| 路由 | 说明 |
| --- | --- |
| `POST /api/preview/sub`、`/api/preview/collection`、`/api/preview/file` | 预览处理结果（`preview.js`） |
| `POST /api/sort/{subs,collections,artifacts,files,tokens,archives}` | 排序（`sort.js`） |
| `GET/PATCH /api/settings` | 设置（`settings.js`） |
| `POST /api/utils/node-info` | 查询节点**服务器（入口）IP** 归属地：源码固定调 `http://ip-api.com/json/<server>?lang=`（`node-info.js`）。**是入口 IP 查询，不是经代理的出口落地检测** |
| `POST /api/proxy/parse`、`/api/rule/parse` | 解析分享链接（`parser.js`） |
| `GET /api/utils/env`、`/api/utils/backup`、`/api/utils/refresh` | 运行环境 / Gist 备份动作 / 刷新（`miscs.js`） |
| `GET/POST /api/storage` | 整库 JSON 导出 / 导入（`miscs.js`） |
| `GET/DELETE /api/logs` | 日志（`logs.js`） |
| `POST /api/utils/age/key-pair`、`/api/utils/age/public-key` | age 加密密钥（`age.js`） |
| `GET /` | 返回运行环境信息（`miscs.js`） |

**没有 `/api/...` 形式的测活 / 节点可用性检测端点**（全量路由清单里无此类目）。

---

## 4. 数据处理管线与内建 Operator

**管线**（`src/restful/sync.js` `produceArtifact()`，`src/core/proxy-utils/index.js` `processFn()`）：

```
订阅配置(sub.process) → 下载远程订阅(多 URL 换行分隔；ua/proxy/timeout 可配；缓存 1h, key=url+ua)
  → preprocess(各格式预处理) → ProxyUtils.parse(解析 SS/SSR/VMess/VLESS/Trojan/Hysteria2/TUIC/… URI 及 Clash/Surge/Loon/QX 配置)
  → 逐个应用 operator(item.type → PROXY_PROCESSORS 注册表；Script 类经 loadScriptItem 加载)
  → ProxyUtils.produce(targetPlatform) → 响应输出 或 artifact 上传 Gist
组合订阅 = 合并多条 sub 后再走 collection.process
```

- Operator 注册表是**硬编码对象**，`src/core/proxy-utils/processors/index.js` 末尾 `export default { 'Useless Filter': …, 'Script Operator': ScriptOperator, … }`；运行时经 `PROXY_PROCESSORS[item.type]` 派发（`core/proxy-utils/index.js:193`），未注册类型报 `Unknown operator`。**没有动态插件注册机制。**
- Producer（输出格式，`src/core/proxy-utils/producers/`）：Clash（弃用）、Clash.Meta(mihomo)、Stash、Surge、SurgeMac、Surfboard、Loon、QX、Egern、Shadowrocket、**sing-box**、V2Ray、URI、JSON。

**内建 Operator / Filter 全清单**（`processors/index.js`，按 `name:` 字段）：

| 类型 | 名称 | 与本仓库相关性 |
| --- | --- | --- |
| 筛选 | Conditional Filter | 组合条件筛选 |
| 筛选 | Useless Filter | 剔除"流量/过期/网址"信息节点 |
| 筛选 | Region Filter | 按节点名里的**国旗 emoji** 匹配（HK/TW/US/SG/JP/UK/DE/KR），非真实 IP 判定 |
| 筛选 | Regex Filter | 正则筛选 |
| 筛选 | Type Filter | 按协议类型 |
| 筛选 | **Script Filter** | ★ 脚本筛选（返回布尔值） |
| 操作 | Quick Setting Operator | 批量设置属性（udp/tfo/skip-cert-verify…） |
| 操作 | Flag Operator | 按名称关键词增删国旗 emoji（`utils/geo.js` getFlag/removeFlag，纯字符串匹配） |
| 操作 | Handle Duplicate Operator | 去重 |
| 操作 | Sort Operator / Regex Sort Operator | 排序 |
| 操作 | Regex Rename Operator / Regex Delete Operator | ★ **节点改名**（官方路径就是它 + Script Operator） |
| 操作 | **Script Operator** | ★ 脚本操作（见 §5） |
| 操作 | Add Proxies From Subscription Operator | 从其他订阅/mihomo 配置文件并入节点 |
| 操作 | **Resolve Domain Operator** | DNS 解析节点域名：写 `_resolved/_resolved_ips/_IPv4/_IPv6/_IP/_IP4P/_domain` 字段；支持 系统/Google/Cloudflare/Ali/Custom DNS、EDNS、并发与缓存、IPv6（`processors/index.js:1183` 起）。**只做 DNS 解析，不做连通性检测** |
| 响应 | RESPONSE_TRANSFORMER | 修改输出响应头/状态码/内容（需设置 `SUB_STORE_FRONTEND_BACKEND_PATH` 才启用） |

### 与本仓库（mihomo-probe）功能逐项对照（重点结论）

1. **官方后端没有任何内建的"节点测活 / 可用性检测 / 延迟测试 / 测速 / 落地 IP 检测" operator 或 API 端点。** 证据：`processors/index.js` 全部 17 个处理器如上；`restful/` 全部路由如上；grep `alive|latency|check|liveness` 于 processors 无结果。官方 README 的功能清单亦无测活项。
2. **官方生态的测活 = Script Operator（扩展点）+ 外部 HTTP-META 服务。** HTTP-META（https://github.com/xream/http-meta，已验证存在）是"一个本地 HTTP 代理服务，让 Sub-Store 可以在本地执行节点测试类脚本（测活、测延迟、测速、GPT/UDP 检测、落地/入口检测等）"；官方文档 `doc/advanced/http-meta.md` 明确：Docker 用带 `http-meta` tag 的 `xream/sub-store` 镜像（内置，默认 `127.0.0.1:9876`，env `PORT` 可改），脚本中以 `http_meta_protocol=http&http_meta_host=127.0.0.1&http_meta_port=9876&http_meta_start_delay=…&http_meta_proxy_timeout=…` 参数把测试请求发给它。即：**mihomo 内核跑在 HTTP-META 里，Sub-Store 后端本体不含内核**。
3. **节点改名**：官方有 Regex Rename Operator / Flag Operator / Script Operator，能力完备，无需本仓库补充。
4. **通知渠道（官方有 Telegram）**：Node 端 `$.notify`（`src/vendor/open-api.js:280-350`）读 `SUB_STORE_PUSH_SERVICE`：
   - 值为 `http(s)://` URL 模板 → GET 请求，`[推送标题]`/`[推送内容]` 占位符替换；
   - 其他值 → 按 **shoutrrr-ts** service URL 处理（`eval('import("shoutrrr-ts")')`）。
   - 官方文档 `doc/advanced/push.md` 与 README 给出 **Telegram 两种写法**：`telegram://<token>@telegram?chats=-1001234567890`（shoutrrr）与 `https://api.telegram.org/bot<API_KEY>/sendMessage?chat_id=…`（URL 模板）。README 还列出 shoutrrr-ts 全部支持家族：Generic Webhook、Bark、Discord、Gotify、Google Chat、IFTTT、Join、Mattermost、ntfy、OpsGenie、Pushover、Pushbullet、Rocket.Chat、Slack、MS Teams、Telegram、Zulip；**Matrix/SMTP 不支持**。
   - 触发时机：仅绑定在定时任务完成/失败（SYNC_CRON、PRODUCE_CRON、UPLOAD/DOWNLOAD_CRON，`src/restful/sync.js` 多处 `$.notify`）；API 层无独立"发通知"端点。
5. **IP 归属 / 出口检测能力**：
   - **MMDB（MaxMind GeoLite2 Country/ASN）**：`src/utils/geo.js` `MMDB` 类（`geoip(ip)→ISO`、`ipaso(ip)→组织`、`ipasn(ip)→ASN`），env `SUB_STORE_MMDB_COUNTRY_PATH` / `SUB_STORE_MMDB_ASN_PATH` + `SUB_STORE_MMDB_CRON`/`_COUNTRY_URL`/`_ASN_URL` 定时更新（`restful/index.js`）；**`MMDB` 直接挂在 `ProxyUtils` 上暴露给脚本**（`core/proxy-utils/index.js` `export const ProxyUtils = { …, MMDB, … }`）。文档：`doc/advanced/mmdb.md`。
   - 官方仓库自带示例脚本 `backend/scripts/`：`demo.js`、**`ip-flag.js` / `ip-flag-node.js`（IP 归属打国旗：批量并发 + `ProxyUtils.isIP` + geo 查询）**、`media-filter.js`、`udp-filter.js`、`tls-fingerprint.js`、`fancy-characters.js`、`revert.js`、`vmess-ws-obfs-host.js`。
   - `/api/utils/node-info`：入口 IP 归属地（ip-api.com），非落地检测。
   - **真正的"出口落地检测"官方不做，交给脚本 + HTTP-META + MMDB**（文档原话："落地 / 入口检测：配合 MMDB 本地数据库可节约大量请求时间"；社区脚本：落地检测 https://zhetengsha.eu.org/blog/posts/1269 、入口检测 /posts/1358 、完整示例 /posts/1415 、测活完善版 /posts/1210 、测速 /posts/1258 、丢包率 /posts/6149 、UDP 检测 /posts/1431 ）。

---

## 5. 扩展点分析（三条移植路径的可行性）

### 路径 A：Script Operator / Script Filter（官方第一扩展点，文档最全）

- 实现位置：`processors/index.js` `ScriptOperator`（约 435-530 行）/ `ScriptFilter`（约 1497 行）。两种写法（`doc/script/overview.md`）：
  - 快捷脚本：直接操作当前节点 `$server`（如 `$server.name = ...`）；
  - 函数式：`function operator(proxies, targetPlatform, context) { ... return proxies }`（与快捷写法二选一）。
- Node 端执行（`nodeFunc`）：脚本被包进 `async function operator(input, targetPlatform, context)`，逐节点以 `$server` 执行；**mihomo 配置类文件（`mihomoConfig/mihomoProfile`）则传入完整 config 并调用脚本的 `main(config)` 做补丁式改写**。动态执行入口 `createDynamicFunction('operator', script, $arguments, $options)`。
- 脚本可用环境（证据：`ProxyUtils` 导出清单 + `scripts/demo.js` + `doc/script/api.md`）：`$server`、`$arguments`（订阅链接 `?` 参数，如 HTTP-META 的 `http_meta_host=...`）、`$options`、`context`（含 source/raw）、`ProxyUtils`（**含 `MMDB`、`download/downloadFile`、`parse/produce`、`isIP/isIPv4/isIPv6`、`yaml`、`doh`、`getISO` 等**）、`scriptResourceCache`（48h 默认缓存的脚本资源缓存）、`$substore` 全局（`scripts/ip-flag-node.js` 用法）。
- **对本仓库的可行性：高。** mihomo-probe 的测活/ipmap 逻辑可封装为脚本：脚本内调用本测活服务的 HTTP API（本服务自带 mihomo 内核，可完全替代 HTTP-META 的角色），按返回结果改名/加后缀（延迟/出口国家）/过滤死节点。官方文档把"节点测试"明确列为脚本的第一个"常见用途"（`doc/script/overview.md`："节点测试：测活、测延迟、节点测速、UDP 检测、落地/入口检测等"）。
- 局限：脚本运行在订阅处理管线内，无独立调度（需 `SUB_STORE_PRODUCE_CRON` 定时处理订阅预热缓存，或 artifact 的 per-artifact cron）；不支持复杂状态存储（只有 scriptResourceCache）；测活并发/超时受 Node 进程约束。
- 文档 URL：`https://sub-store-org.github.io/doc/script/overview`（及 `/script/usage`、`/script/api`、`/script/examples`）、官方示例 `https://github.com/sub-store-org/Sub-Store/blob/master/scripts/demo.js`、Wiki《脚本使用说明》。

### 路径 B：独立部署集成 —— 把 mihomo-probe 当远程订阅上游（零改动，风险最低）

- 证据：单条订阅 `url` 字段支持**多行多个远程订阅**（`sync.js` `produceArtifact`：`url.split(/[\r\n]+/)` 逐个下载后合并；`doc/subscription/overview.md`："远程订阅链接：机场或服务商给的订阅地址（可多个）"），且可给每个订阅单独配 `ua`/`proxy`/`timeout`/失败策略。
- 做法：mihomo-probe 暴露测活收敛后的 YAML/URI 订阅端点 → 在 Sub-Store 建一条订阅指向它 → 后续的改名（Regex Rename）、地区分组（Region/Flag）、合并（collection）、多格式输出（mihomo/sing-box/surge producers）、分享链接、Gist 托管/定时同步全部复用官方能力。
- 另有 `SUB_STORE_BACKEND_DEFAULT_PROXY`（socks5/http）与 per-sub `proxy` 可控制 Sub-Store 拉取该上游时走哪条代理；`noCache` 可绕过 1 小时资源缓存强制拉新。

### 路径 C：把功能做进 Sub-Store 后端本体（fork 改造）

- 官方**没有插件系统**：operator 注册表是 `processors/index.js` 里的硬编码 `export default {...}`，新增 operator = fork 后在该对象加一项（照抄 `ResolveDomainOperator`/`ScriptOperator` 签名：`({args}, executionContext) => ({ name, func })`，`func: async proxies => proxies.map(...)`），必要时在 `restful/` 加路由（照 `node-info.js` 模式，15 行即可注册一条 API）。
- 代价：上游活跃（几乎每日提交，master 直发），fork 维护成本高；许可 **AGPL-3.0** 要求衍生分发开源。
- 折中（推荐）：**独立微服务 + 路径 B/A 接入**，即 mihomo-probe 保持独立进程（承担 HTTP-META 同等的"本地测试执行器"角色），不改上游一行代码、跟随上游更新零成本；本报告不虚构上游有"插件加载器"之类的机制。
- 另注：`SUB_STORE_DATA_URL_POST` 会在启动恢复数据后 `eval` 一段自定义 JS（`restful/index.js`），属启动期小后门，不适合承载测活。

---

## 6. 定时任务 / 鉴权 / 数据模型速查

**定时任务**（`restful/index.js` `serve()` 尾部 + `utils/artifact-cron.js`，库为 `cron` CronJob）：

| 环境变量 | 作用 |
| --- | --- |
| （artifact 自带 `sync` 字段） | per-artifact cron：`startArtifactCronJobs()` 按每个 artifact 配置的 crontab 定时 produce+同步 Gist |
| `SUB_STORE_BACKEND_SYNC_CRON` | 全量 artifact 同步（旧 `SUB_STORE_BACKEND_CRON`/`SUB_STORE_CRON` 已弃用，源码显式报错提示） |
| `SUB_STORE_PRODUCE_CRON` | 定时 produce：格式 `cron,sub|col,名称;…`（例 `0 */2 * * *,sub,a;0 */3 * * *,col,b`），用于脚本缓存预热 |
| `SUB_STORE_BACKEND_UPLOAD_CRON` / `DOWNLOAD_CRON` | Gist 全量备份 / 恢复 |
| `SUB_STORE_MMDB_CRON`（+ `_COUNTRY_PATH/_COUNTRY_URL/_ASN_PATH/_ASN_URL`） | 定时更新 GeoLite2 Country/ASN 库 |

**鉴权方式**：后端 API **无内建用户名密码鉴权**。官方安全模型（`doc/reference/environment-variables.md`）：
- 裸后端"**不应该对外暴露**"，建议 `SUB_STORE_BACKEND_API_HOST=127.0.0.1`，只对外暴露前端端口（Docker 只发布 3001）；
- `SUB_STORE_FRONTEND_BACKEND_PATH`：前端→后端的**秘密路径前缀**（防扫描，文档示例 `/2cXaAxRGfddmGz2yx1wA`）；`SUB_STORE_BACKEND_MERGE=true` 可单端口合并前后端；
- `SUB_STORE_CORS_ALLOWED_ORIGINS` CORS 白名单（默认 `https://sub-store.vercel.app,http://substore.stash,https://substore.stash`）；
- 分享输出走签名 token（`POST /api/token` 签发，`/share/...?token=` 校验，jsrsasign 实现，可绑定 age 公钥做加密订阅）。
- 前端：Vue 3 + Vite（`Sub-Store-Front-End/package.json`：`"name": "sub-store-front-end", "version": "2.34.0"`），官方实例部署在 Vercel。

**数据模型**（`src/constants.js` 键名 + JSON 存储）：`sub-store.json` 顶层键 `schemaVersion, settings, subs[], collections[], files[], modules[], artifacts[], rules, tokens[], archives[]` 及 `#sub-store-logs`、`#sub-store-cached-resource`、`#sub-store-cached-headers-resource`、`#sub-store-cached-script-resource`。订阅对象关键字段（`doc/subscription/overview.md` + 源码）：`name`（内部引用名）、`displayName`、`url`（可多行）、`source`（local/remote）、`content`（本地内容）、`mergeSources`（localFirst/remoteFirst）、`ua`/`uaTransit`、`proxy`、`timeout`、`ignoreFailedRemoteSub`、`noFlow`、`noCache`、`subUserinfo`、`process[]`（operator 链）。组合订阅 = `subscriptions[]`（名称列表）+ `process[]`。artifact = `type(sub/col/file)+source+dest+sync(cron)+…`。

---

## 7. 官方测试设施与 CI

- **后端测试**：mocha + chai，`pnpm test` = `SUB_STORE_FRONTEND_BACKEND_PATH=/ mocha src/test/**/*.spec.js --require @babel/register --recursive`。**41 个 spec 文件**（`backend/src/test/`），覆盖：协议解析（uri/v2ray/qx/pipeline）、producers（structured/text）、processors（resolve-domain/process-context）、restful（download/settings/sync/token/file/mihomo-config 等）、utils（artifact-cron/artifact-sync-policy/cors/gist/flow/request-concurrency）、vendor（express-cors/open-api/open-api-notify）、runtime（builtin/runtime-manifest）。
- **后端 CI**（`.github/workflows/main.yml`）：push master 且 `backend/package.json` 变更时触发 → Node 24.15.0（读 `.node-version`）→ `pnpm i --no-frozen-lockfile` → **自动把最新 mihomo release 版本号写入 `src/utils/download.js` 和 `src/utils/flow.js` 的 `clash.meta/<version>` 字符串（订阅下载 UA）** → `pnpm test` → `pnpm bundle:esbuild` → GitHub Release（`sub-store.min.js`、`sub-store-0/1.min.js`、`cron-sync-artifacts.min.js`、`proxy-utils.esm.mjs`、`runtime-manifest.json` 等）并强推到 `release` 分支。
- **前端 CI**（`main.yml` + `update-vercel-project-settings.yml`）：package.json 变更 / PR 触发 → build → **vercel-action 部署生产**（即 sub-store.vercel.app）+ zip 产物发 Release。
- **文档 CI**（`doc/.github/workflows/deploy.yml`）：VitePress build → GitHub Pages（部署 URL 已验证 https://sub-store-org.github.io/doc/ 返回 200）。
- CI 中无 lint/类型检查步骤（eslint/prettier 配置在仓库但 CI 未跑）；前端仓库未见单测目录（未核实到前端测试）。

---

## 8. 上游同步注意事项

1. **仓库布局**：后端在主仓库 `backend/` 子目录（pnpm workspace + `patches/http-proxy` 补丁）；master 为开发分支（近乎每日提交，2026-09-27 仍有 feat），`release` 分支只放构建产物，另有 `dev` 分支。跟踪上游请以 `master` 为准。
2. **CI 自动改文件**：每次发版 CI 会把最新 mihomo 版本号写入 `backend/src/utils/download.js`、`src/utils/flow.js`（UA 字符串 `clash.meta/<version>`），rebase 时这两个文件的冲突可直接以上游为准。
3. **多运行时代码约定**：共享代码不得直接 `import` Node 模块，须走 `src/runtime/*` 抽象与 `eval('process')`/`eval('require(...)')` 模式（同一份代码要打包进 QX/Surge/Loon 等非 Node 环境）；新增依赖需考虑能否打进浏览器/Loon bundle（如 `shoutrrr-ts` 就是靠 esbuild 的动态 import 替换特殊处理的，见 `open-api.js` 注释）。
4. **版本独立**：后端 `2.42.2` 与前端 `2.34.0` 各自按 `package.json` 变更触发发版，无版本锁定关系。
5. **许可**：AGPL-3.0 —— 将本仓库能力合并移植进 Sub-Store 衍生分发时需遵守同等开源义务。
6. **文档随仓库演进**：`sub-store-org/doc` 与功能同步更新（2026-09-22 仍有提交），移植设计前以 `https://sub-store-org.github.io/doc/` 最新版为准；主仓库 README 的"文档"入口仍指向旧 GitHub Wiki，两者并存。
7. **HTTP-META 是独立第三方仓库**（xream/http-meta），不在 sub-store-org 组织内、不受官方发版节奏约束，但其协议（`http_meta_*` 脚本参数、默认端口 9876）已被官方文档固化为事实标准 —— 本测活服务若要平替它，应兼容这套参数约定。

---

## 附：本报告主要证据文件路径（浅克隆内）

```
D:/ChatGPT/.tmp_upstream/Sub-Store/backend/package.json          # 运行时/依赖/scripts
D:/ChatGPT/.tmp_upstream/Sub-Store/.node-version                 # Node 24.15.0
D:/ChatGPT/.tmp_upstream/Sub-Store/backend/src/main.js           # 入口
D:/ChatGPT/.tmp_upstream/Sub-Store/backend/src/restful/index.js  # 路由挂载/前端代理/share token/cron/mmdb
D:/ChatGPT/.tmp_upstream/Sub-Store/backend/src/restful/*.js      # 各路由模块（download/sync/artifacts/token/node-info…）
D:/ChatGPT/.tmp_upstream/Sub-Store/backend/src/core/proxy-utils/index.js             # parse/process/produce、ProxyUtils 导出
D:/ChatGPT/.tmp_upstream/Sub-Store/backend/src/core/proxy-utils/processors/index.js  # 17 个 operator + 注册表
D:/ChatGPT/.tmp_upstream/Sub-Store/backend/src/utils/geo.js      # getFlag/getISO/MMDB(geoip/ipaso/ipasn)
D:/ChatGPT/.tmp_upstream/Sub-Store/backend/src/vendor/open-api.js# $.notify/PUSH_SERVICE/persistCache
D:/ChatGPT/.tmp_upstream/Sub-Store/backend/src/constants.js      # 存储键
D:/ChatGPT/.tmp_upstream/Sub-Store/backend/scripts/              # demo.js、ip-flag.js 等官方示例脚本
D:/ChatGPT/.tmp_upstream/Sub-Store/.github/workflows/main.yml    # 后端 CI
D:/ChatGPT/.tmp_upstream/Sub-Store-Front-End/package.json        # Vue3+Vite 前端
D:/ChatGPT/.tmp_upstream/doc/advanced/http-meta.md               # HTTP-META 官方文档
D:/ChatGPT/.tmp_upstream/doc/advanced/{mmdb,push}.md
D:/ChatGPT/.tmp_upstream/doc/script/{overview,usage,api,examples}.md
D:/ChatGPT/.tmp_upstream/doc/subscription/{overview,processors,collection,local,conversion}.md
D:/ChatGPT/.tmp_upstream/doc/reference/environment-variables.md
D:/ChatGPT/.tmp_upstream/doc/.github/workflows/deploy.yml        # 文档 CI
```
