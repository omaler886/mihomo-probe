# ARCHITECTURE — probe × 官方 Sub-Store 扩展架构（第二轮）

- 日期：2026-09-28。输入：`PLAN.md`、`MIGRATION_MAPPING.md`（§0/§2）、`FEATURE_INVENTORY.md`（域 9/10/11、附录 B）、
  `OFFICIAL_CAPABILITY_MAP.md`（§4/§5/§8）、`SECURITY_REVIEW.md`、`mihomo_test/server.py`（全文）、
  `mihomo_test/db.py:list_nodes`、`mihomo_test/policy.py`、`tests/test_hardening.py`。
- 本批编码范围 = `MIGRATION_MAPPING.md` §2 的 N-01~N-04，其余一概不动。
- 官方基线：Sub-Store backend @ `e08f1b1`（Node 24，Express，AGPL-3.0）。**官方核心零修改**，全部接入走两个官方扩展点。

---

## 1. 总体拓扑

```
 订阅上游(多源)          ┌─────────────────────────────────────────────────────────┐
      │                 │                probe 服务（mihomo_test，独立部署）          │
      ▼                 │                                                         │
 engine.run_round ──实测──▶ mihomo 内核容器（docker，loopback:19190）               │
      │                 │                                                         │
      ├─ 收敛/状态机 ─────────▶ db.nodes 账本 (source,fingerprint)                  │
      │                 │            │                                            │
      └─ 导出构建 ──▶ data/exports/<key>.yaml                                        │
                        │              │                                          │
                        │              ▼                                          │
                        │   substore_bridge.probe_nodes_payload(rows)  (N-03)     │
                        │              │                                          │
                 ┌──────┴──────────────┴───────────┐                              │
                 ▼ server.py (do_GET)              ▼ server.py (do_GET)           │
   GET /api/export/<key>.yaml            GET /api/probe/nodes  (N-01)             │
   ClashMeta YAML（含完整节点凭据）          只读账本快照 JSON（无凭据）                  │
   publish.token（既有作用域）             publish.token（本批扩入，见 §5）              │
                 │                                     │                        │
                 └──────────────┬──────────────────────┘                        │
                                │  HTTPS（公网面，cloudflared 隧道）                │
                                └───────────────────────────────────────────────┘
         ┌───────────────────────────┼───────────────────────────────┐
 路径 B  │ 远程订阅上游（零改动复用）      │ 路径 A  Script Operator（N-02）  │
         ▼                           ▼                               │
 ┌───────────────────────────────────────────────────────────────┐   │
 │           官方 Sub-Store backend @ e08f1b1（零修改）              │   │
 │                                                               │   │
 │ sub "probe-<key>" (source:remote, url=/api/export/<key>.yaml?token=…) │
 │ collection "probe" = [probe-<key>…]                            │   │
 │      │  produceArtifact：下载(缓存1h) → parse → process[]       │   │
 │      ▼                                                        │   │
 │ process: [ QuickSetting?, {type:"Script Operator",            │   │
 │            args:{probe_url, probe_token, mode, missing}} ]    │   │
 │      │  async operator(proxies, targetPlatform, context)       │───┘ fetch /api/probe/nodes
 │      │  （probe_filter.script.js：match→filter/annotate）        │      + X-Auth-Token 头
 │      ▼                                                        │
 │  过滤 dead / 标注 [CC]·dead ──▶ producers（ClashMeta/sing-box/…） │
 │  ──▶ /download/… 输出、collection 合并、artifact(Gist)/cron      │
 └───────────────────────────────────────────────────────────────┘
```

数据流一句话：

- **路径 B**：官方 Sub-Store 把 probe 的 `/api/export/<key>.yaml?token=<publish.token>` 当普通远程订阅拉（per-sub
  `ua`/`proxy`/`timeout` 可配，缓存 1h）。probe 端零新增，`500 即不存在`、`零节点必 500` 两个官方怪癖已被
  `store.py._is_missing` 与「零存活跳过联动」显式建模（FEATURE_INVENTORY 移植事实 2）。
- **路径 A**：官方订阅的 `process[]` 里挂 Script Operator，粘贴 `probe_filter.script.js`；脚本在 produce 时
  **fetch 一次** `/api/probe/nodes`（token 走 `X-Auth-Token` 头），按 name→server 匹配节点后过滤/改名。
  probe 因此扮演官方文档里 http-meta 的「本地测试执行器」角色（OFFICIAL_CAPABILITY_MAP §5.2）。

---

## 2. 模块布局（文件白名单）

本批**允许触及/新增**的文件全集，除此之外不得改动任何文件（尤其：官方 `backend/**`、`mihomo_test/web/`、
`mihomo_test/{config,db,engine,core,store,ipmap,ui,notifier,doh,policy}.py`、Dockerfile、docker-compose.yml）：

| 文件 | 动作 | 职责（一句话） |
|---|---|---|
| `mihomo_test/server.py` | 触及（两处最小 diff） | ① `auth_ok` 的 publish 作用域扩入 `/api/probe/nodes`（§3.2）；② `do_GET` 新增一个路由分支（§3.1）。其余逻辑不动 |
| `mihomo_test/substore_bridge.py` | 新增（N-03） | `probe_nodes_payload(rows)` 纯函数 + 字段/状态白名单常量——payload 形状的单一事实来源，server 端点与测试共用 |
| `substore_bridge/probe_filter.script.js` | 新增（N-02，新目录） | Sub-Store Script Operator 脚本：调 probe API，按账本过滤 dead / 标注国别，契约见 §4 |
| `tests/test_substore_bridge.py` | 新增 | N-03 纯函数行为 + **N-01 端点 HTTP 面用例（ProbeNodesEndpointTest：无 token 401 / publish token 200 / auth token 200 / publish token 仍打不开 `/api/status` 与 `/api/config` / 响应形状 / 不含敏感字段，沿用 `_LiveServer` 风格，见 §3.5）** + N-02 的 Node 24 行为 harness（mock fetch + 官方 operator 签名） |
| `MIGRATION_GUIDE.md` | main Agent 写（N-04） | 路径 B/A 分步配置、$arguments 填法、回滚、凭据轮换与历史清洗前置项。需要 §3/§4 的契约文字，直接引用本文件 |

（B4 修订，REVIEW R-02：原白名单含 `tests/test_hardening.py`「仅追加用例」一行——实际实现中 N-01 的 16 个
端点用例由 tests Agent 独立决定集中落位 `tests/test_substore_bridge.py`，`test_hardening.py` 未被触碰，
该行已按实际实现更正并入上表第一行。）

约束重申：不为「看起来完整」改任何 deprecate 项（MIGRATION_MAPPING §3）；`db.py` **不需要改**——
`db.list_nodes(source=None, status=None)`（db.py:472）已同时支持两个过滤参数。

---

## 3. N-01 端点契约：`GET /api/probe/nodes`

### 3.1 路由

- 方法/路径：`GET /api/probe/nodes`（固定路径，无尾参）。
- 位置：`server.py do_GET` 的既有 try 块内，插在 `if path == "/api/nodes":` 分支之后——自动享受既有的
  「鉴权前置（401 先于路由）」与「异常兜底 500」。
- 解析 query 沿用 `/api/stats` 的写法：`urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)`。

### 3.2 鉴权：publish.token 作用域扩展（最小 diff）

现状：`auth_ok`（server.py:117-135）接受 `auth.token`；仅当 path 以 `/api/export/` 开头时**追加**接受
`publish.token`。扩展方式——把判断提成一个小谓词，`auth_ok` 只改一行：

```python
# 模块级，放在 SECURITY_HEADERS 附近
def _publish_scoped(path):
    """Paths a publish token may read: exports (prefix) + the probe node snapshot.

    `/api/export/` 是前缀匹配（key 可含任意合法字符）；`/api/probe/nodes` 是精确匹配，
    避免 `/api/probe/nodes/anything` 之类的尾随路径意外落入只读作用域。
    """
    return path.startswith("/api/export/") or path == "/api/probe/nodes"


def auth_ok(handler, cfg, path=None):
    accepted = [str(cfg.get("auth", {}).get("token") or "")]
    if path and _publish_scoped(path):
        accepted.append(str(cfg.get("publish", {}).get("token") or ""))
    presented = _presented_tokens(handler)
    return any(_matches(c, t) for t in accepted for c in presented)
```

- 语义不变量（测试锁死）：publish token 依旧打不开 `/api/status`、`/api/config`、任何 POST（AuthLogicTest
  `test_the_publish_token_reads_exports_but_nothing_else` 不回退）；空 publish token 依旧拒绝（空 token 一律
  拒绝，`_matches` 对空串返回 False）；三传法（`?token=` / `X-Auth-Token` / `Bearer`）自动继承
  （`_presented_tokens` 不改）。
- `auth_ok` 的 docstring 同步补一句：probe 节点快照与导出同属 publish 作用域（理由见 §5）。

### 3.3 Query 参数（均可选）

| 参数 | 取值 | 行为 |
|---|---|---|
| `source` | 任意字符串（来源 key，精确等值） | 透传 `db.list_nodes(source=…)`；**不做白名单校验**——未知 source 返回空 `nodes`（对只读发布面，空答案是正确答案，且避免向 publish 作用域暴露 `cfg["sources"]` 语义） |
| `status` | `alive` \| `pending` \| `dead` \| `unknown` \| `excluded` | 透传 `db.list_nodes(status=…)`；**不在白名单内 → 400**（沿用 `/api/run` mode 白名单的边缘拒绝风格） |

不提供 `limit`/分页：账本行数上界 = 上游订阅节点数（nodes 表不随轮次膨胀，膨胀的是 results 且已按 500 轮修剪），
一次快照是脚本的预期消费形状，截断反而制造「看起来全、其实少」的静默差异。MIGRATION_MAPPING N-01 risk 提到的
「count 上限」按此裁量不实现（见 §9 修正 3）。

### 3.4 响应形状

成功（200，`_send` 默认 `application/json; charset=utf-8`）：

```json
{
  "ok": true,
  "generated_at": "2026-09-28T12:34:56",
  "count": 2,
  "nodes": [
    {"name": "hk-01", "source": "air", "status": "alive", "country": "HK",
     "delay_ms": 234, "consec_fail": 0, "server": "a.example.com",
     "proto": "vless", "category": "direct"}
  ]
}
```

- `generated_at`：`db.now()`（UTC，`%Y-%m-%dT%H:%M:%S`，秒精度无时区后缀——与全仓时间戳约定一致，
  test_logic TimestampTest 锁定）。由 N-03 构建器填，端点不自行生成。
- 字段类型：`name/source/status/category` = string（`category` 若账本行尚未盖章——迁移补列的 NULL——按全仓
  约定归一为 `"direct"`，与 `db.list_nodes` 的 `n.category || "direct"` 同款，B4 按 REVIEW R-05 显式成文）；
  `country/server/proto` = string 或 `null`（账本可能还没写上，
  如本轮尚未完成归属查询）；`delay_ms` = int 或 `null`（从未测成）；`consec_fail` = int。
- `status` 枚举 = `policy.py` 五常量（ALIVE/PENDING/DEAD/UNKNOWN/EXCLUDED）。
- **字段白名单（单一事实来源 = N-03 的 `PROBE_NODE_FIELDS`）**，恰好 9 个：
  `name, source, status, country, delay_ms, consec_fail, server, proto, category`。
  db 行 → payload 映射：`display→name`、`last_delay_ms→delay_ms`，其余同名；`fingerprint`、`last_reason`、
  `first_seen/last_seen/last_ok/total_ok/total_fail/ip_alive/ip_total` **故意排除**——fingerprint 是连接参数
  sha256（非明文凭据但属派生标识，排除以保持「本端点无凭据」承诺干脆）；`last_reason` 可能内嵌内核错误细节
  （S-17 同类信息面）；其余对脚本无用途。
- 空账本：**200** + `{"ok": true, "generated_at": "…", "count": 0, "nodes": []}`。
  不用 404：脚本必须能区分「路径/鉴权错」（fail-open 但要日志）与「账本就是空」（正常态），后者是合法快照。
  与导出 YAML 的 404-注释体语义（server.py:404）互不模仿——那是「key 尚无导出」的拉取端怪癖，不是 JSON API 的。

### 3.5 错误形状与头（全部自动继承，无新代码）

| 场景 | 响应 | 来源 |
|---|---|---|
| 无/错 token | `401 {"error": "unauthorized", "hint": "append ?token=<your token>"}` | do_GET 既有 401 前置 |
| `status` 非法 | `400 {"error": "status 必须是 alive/pending/dead/unknown/excluded 之一，收到 'xyz'"}` | 新分支内自建，对齐 `/api/run` 的中文 400 风格 |
| 路径打错 | `404 {"error": "not found", "path": "…"}` | do_GET 兜底 |
| 未预期异常 | `500 {"error": "<Type>: <msg>"}` | do_GET 兜底（S-17 既有风格，本批不修） |

- `Cache-Control: no-store`、`Vary: Origin`、`SECURITY_HEADERS`（nosniff/DENY/no-referrer/CSP）由 `_send`
  无条件附带；CORS 精确白名单回显由 `cors_origin` 处理——**新端点零额外代码**。
- `test_hardening` 用例断言方式沿用 `HttpSurfaceTest`：`_LiveServer` 起真实回环实例 →
  `code, body, headers = self.srv.get(path)` → `json.loads(body)` 后对形状/字段逐一 `assertEqual` /
  `assertNotIn(token, body)`。

---

## 4. N-02 脚本契约：`substore_bridge/probe_filter.script.js`

### 4.1 签名与执行模型

```js
async function operator(proxies, targetPlatform, context) { /* … */ return proxies; }
```

- 依赖官方**函数式** Script Operator 契约：整组节点数组进、数组出，每次 produce 调用一次
  （OFFICIAL_CAPABILITY_MAP §5：`createDynamicFunction('operator', script, $arguments, $options)`，包装器为
  async）。**fetch 只在顶层调一次**，绝不在逐节点循环内。
- 允许触碰的最小面：`$arguments`、`fetch`、`AbortController/setTimeout`（Node 24 原生）、`JSON`、`console.log`。
  **不使用** ProxyUtils 任何高级能力（MMDB/download/yaml/doh…）、不使用 `$substore`、`context`/`targetPlatform`
  不读（标注与平台无关）——把与官方沙箱演化的耦合压到最低。

### 4.2 `$arguments` 表

| 键 | 类型 | 默认 | 语义 |
|---|---|---|---|
| `probe_url` | string（完整 URL） | **必填** | probe 服务 `/api/probe/nodes` 绝对地址（公网/隧道面）。**不要**把 token 拼进它——token 走请求头 |
| `probe_token` | string | 必填（缺省见 4.4） | probe 的 **publish.token**；以 `X-Auth-Token` 请求头发送（服务端三传法之一），避免进反代 access log（S-16 对本端点的主动缓解） |
| `mode` | `filter` \| `annotate` \| `both` | `filter` | 行为开关，见 4.5 |
| `missing` | `keep` \| `drop` | `keep` | 无记录/API 失败时的策略，见 4.4 |

`$arguments` 来源遵循官方约定（Script Operator args 或订阅链接 query，以官方 `doc/script/usage` 为准）；
脚本只读键名，不关心载体。

### 4.3 匹配算法（写死）

对每个入站节点 `p`（字段 `p.name`、`p.server` 来自官方 parse 产物）：

1. **name 精确**：`p.name` 与 payload `nodes[].name` 严格等值（`Map`，O(1)）。
   注意：probe 自家导出开了 `publish.add_region_tag` 时节点名是**前缀**形式 `[CC] name`（engine.py:2059），
   与账本 display 名不等——这类命中失败是**预期**的，由第 2 步兜底接住。
2. **server 兜底**：取 `p.server`，`toLowerCase()` 并剥尾部 `.`，在 payload `nodes[].server`（同样归一化）上匹配：
   - 恰 1 条记录 → 命中；
   - 多条记录且**全部 status 一致** → 命中（取其一，status/国别一致即等价）；
   - 多条记录状态**互相矛盾** → 视为「无记录」，走 missing 策略（防误杀：账本无 port 列、无法区分同 host
     多端口节点，宁可不裁）。
   - `p.server` 缺失或账本侧 `server` 为 null → 视为无记录。
3. **无 port 参与**：probe 账本 nodes 表没有 port 列（`db.list_nodes` 字段清单为证），`(server,port)` 兜底不可
   实现——见 §9 修正 1。port 不参与匹配是**设计事实**，不是省略。

### 4.4 失败与缺省策略（失败开放，但配置错误 fail-loud）

| 情形 | 归类 | 行为 |
|---|---|---|
| `$arguments.probe_url` 缺失 | **配置级错误** | `throw new Error("probe_filter: $arguments.probe_url 缺失")`，让本次 produce **显式失败** |
| `$arguments.probe_token` 缺失/错误（401/403） | 运行级失败 | 按 missing 策略，`console.log("[probe_filter] probe 401 → missing=…")` |
| fetch 抛错/超时（5s，AbortController；沙箱无 AbortController 则退化为无超时直连） | 运行级失败 | 按 missing 策略并 console.log |
| HTTP 非 2xx | 运行级失败 | 按 missing 策略 |
| 响应体非 JSON / `ok !== true` / `nodes` 非数组 | 运行级失败 | 按 missing 策略 |
| 200 但 `count===0` / 节点在账本无记录 | **无记录** | 按 missing 策略 |
| 命中记录 | 正常 | 按 mode 处理 |

- `missing=keep`（默认）：保留节点——probe 挂了/节点不在账本 ≠ 节点死了，**失败开放不误杀**。
- `missing=drop`：严格模式，无记录即删（订阅只想要「probe 见过且未判死」的子集时用，慎用）。
- 配置错误 fail-loud 的理由：本仓库吃过「静默死亡管线」的亏（FEATURE_INVENTORY 移植事实 2：两套旧管线静默
  死亡的根因）。fail-open 只应吸收**瞬时**故障（probe 重启、网络抖动），不应吸收把 URL 敲错的**确定性**配置
  手误——后者宁可让 produce 红掉一次，也不许过滤器悄悄失效。

### 4.5 mode 两行为的确切语义（写死）

状态取值 = `policy.py` 五枚举。**filter 的删除条件是且仅是「命中记录且 `status === "dead"`」**：

| 命中 status | filter | annotate |
|---|---|---|
| `alive` | 保留 | 名尾追加 ` [CC]`（`country` 归一为大写；非两字母/为 null 则不加） |
| `pending` / `unknown` | 保留（尚未收敛，不裁） | 不变 |
| `excluded` | 保留（**不测 ≠ 死**：受限 ISP 等运维主动排除，出口端自会处理） | 不变 |
| `dead` | **删除**（唯一删除条件） | 名尾追加 ` ·dead` |
| 无记录（含矛盾兜底） | missing 策略 | missing=keep 时不变；missing=drop 时已删 |

- `mode=annotate`：只改名不删节点。后缀追加**幂等**——追加前先剥掉本脚本上次留下的尾缀
  （`/ ·dead$/` 与 `/ \[[A-Z]{2}\]$/` 各一次），防同名校验/二次处理叠加成 `foo ·dead ·dead`。
  用尾缀而非前缀，避免与 probe 导出的既有前缀标签 `[CC] ` 打架。
- `mode=both`：先按 filter 删，再对幸存者按 annotate 规则改名（dead 已删，故 both 模式不会出现 `·dead`）。
- 返回值：`annotate` 原数组（原地改 `p.name`）；`filter`/`both` 返回筛后新数组。空入站数组直接原样返回。

### 4.6 参考骨架（实现以此为蓝本，行为以本节文字为准）

```js
// 头部注释模板（用途 + 用法示例，实现时照抄并保持与 ARCHITECTURE §4 同步）：
// probe_filter.script.js — Sub-Store Script Operator：按 probe 测活账本过滤/标注节点
//
// 用途：消费独立 probe 服务（mihomo_test）的只读账本端点 GET <probe_url>，
//   按 name（精确）→ server（兜底，多记录状态矛盾视为无记录）匹配当前订阅节点。
//   mode=filter   仅删除 status=dead 的节点（pending/unknown/excluded 保留）
//   mode=annotate 存活节点名尾追加 " [CC]"（实测国别），dead 追加 " ·dead"，幂等
//   mode=both     先 filter 后 annotate
//   missing=keep  probe 不可达/节点无记录时保留（默认，失败开放不误杀）
//   missing=drop  无记录即删（严格模式，慎用）
//   probe_url 缺失为配置错误：直接抛错让本次 produce 显式失败（不静默失效）。
//
// 用法：Sub-Store 订阅 → 操作(process) → 添加「脚本操作」，粘贴本文件全文，传参：
//   probe_url=https://probe.example.com/api/probe/nodes
//   probe_token=<probe 的 publish.token>   （以 X-Auth-Token 头发送，勿拼进 URL）
//   mode=filter&missing=keep
// 契约细节：仓库 ARCHITECTURE.md §3/§4。

async function operator(proxies, targetPlatform, context) {
  const args = (typeof $arguments === "object" && $arguments) || {};
  const mode = ["filter", "annotate", "both"].includes(args.mode) ? args.mode : "filter";
  const missing = args.missing === "drop" ? "drop" : "keep";
  const url = String(args.probe_url || "").trim();
  if (!url) throw new Error("probe_filter: $arguments.probe_url 缺失");
  if (Array.isArray(proxies) && proxies.length === 0) return proxies;  // 空订阅不 fetch（配置错误已在上行抛出，fail-loud 优先）

  let nodes = null;                       // null = 「按 missing 策略」的失败/无记录态
  try {
    const ctrl = typeof AbortController === "function" ? new AbortController() : null;
    const timer = ctrl ? setTimeout(() => ctrl.abort(), 5000) : null;
    const headers = args.probe_token ? { "X-Auth-Token": String(args.probe_token) } : {};
    const resp = await fetch(url, { headers, signal: ctrl ? ctrl.signal : undefined });
    if (timer) clearTimeout(timer);
    if (resp.ok) {
      const body = JSON.parse(await resp.text());
      if (body && body.ok === true && Array.isArray(body.nodes)) nodes = body.nodes;
    }
    if (!nodes) console.log(`[probe_filter] probe 应答不可用（HTTP ${resp.status}）→ missing=${missing}`);
  } catch (e) {
    console.log(`[probe_filter] probe 不可达（${(e && e.name) || e}）→ missing=${missing}`);
  }

  const keepMissing = missing === "keep";
  if (!nodes) return keepMissing ? proxies : [];   // missing=drop：probe 不可用即清空（严格模式的代价）

  const byName = new Map(nodes.map(n => [n.name, n]));
  const byServer = new Map();
  for (const n of nodes) {
    if (!n.server) continue;
    const k = String(n.server).toLowerCase().replace(/\.$/, "");
    (byServer.get(k) || byServer.set(k, []).get(k)).push(n);
  }
  const hit = (p) => {
    const exact = byName.get(p.name);
    if (exact) return exact;
    if (!p.server) return null;
    const cands = byServer.get(String(p.server).toLowerCase().replace(/\.$/, "")) || [];
    if (cands.length === 0) return null;
    const first = cands[0].status;
    if (cands.every(c => c.status === first)) return cands[0];
    return null;                          // 同 host 多记录状态矛盾 → 视为无记录
  };
  const aliveTag = (n) => (n.country && /^[A-Za-z]{2}$/.test(String(n.country)))
    ? ` [${String(n.country).toUpperCase()}]` : "";
  const strip = (s) => s.replace(/ ·dead$/, "").replace(/ \[[A-Z]{2}\]$/, "");

  if (mode === "annotate") {
    for (const p of proxies) {
      const n = hit(p);
      if (!n) continue;
      if (n.status === "dead") p.name = strip(p.name) + " ·dead";
      else if (n.status === "alive") p.name = strip(p.name) + aliveTag(n);
    }
    return proxies;
  }
  // filter / both：删除且仅删除 dead；missing=keep 时无记录保留
  return proxies.filter(p => {
    const n = hit(p);
    if (!n) return keepMissing;
    if (n.status === "dead") return false;
    if (mode === "both" && n.status === "alive") p.name = strip(p.name) + aliveTag(n);
    return true;
  });
}
```

（骨架里 `strip` 的正则与 missing=drop 短路以最终实现为准，语义必须与本节表格一致。）

### 4.7 测试（Node 24，本机自带）

1. 门禁：`node --check substore_bridge/probe_filter.script.js`。
2. 行为 harness（`tests/test_substore_bridge.py` 里 subprocess 调 Node，或独立 `tests/substore_bridge/*.mjs`）：
   mock 全局 `fetch` + 提供 `$arguments`，以官方签名 `operator(proxies, "ClashMeta", {source:"…"})` 调用，断言：
   - filter：dead 删、alive/pending/unknown/excluded 留、missing=keep 留、missing=drop 删；
   - annotate：` [HK]` / ` ·dead` 后缀、幂等（二次调用不叠加）、country=null 不加缀；
   - fetch 非 200 / 抛错 / 非 JSON / 空 nodes：missing 两策略各自行为；
   - probe_url 缺失：抛错；
   - name 前缀标签节点（`[HK] foo`）经 server 兜底命中；同 host 矛盾记录 → 视为无记录；
   - token 仅出现在请求头（mock 断言 `init.headers["X-Auth-Token"]`），未拼进 URL。

---

## 5. 兼容与数据安全

**为什么 `/api/probe/nodes` 可以放进 publish 作用域：**

- 对照基线：publish.token 的既有作用域 `/api/export/<key>.yaml` 回送的是 **ClashMeta 完整导出**——每个节点带
  uuid/password/reality pbk+sid/server/port 等全部连接凭据。这个更宽的暴露面**今天已经存在**且被同一 token
  保护。新端点只回送 `name/server/status/延迟/国别/计数` 这类测量元数据（§3.4 白名单），信息面是导出 YAML 的
  真子集（连 server 地址都在导出里已可见）。把新端点并入该作用域因此**不放宽任何既有风险**，反而让
  「publish.token = 只读、面向 Sub-Store 的凭据」这个语义更完整：Sub-Store 想消费 probe，拿这一个 token 就够。
- 不改变既有不变量：publish token 依旧不能写任何东西（POST 全家仍在 auth.token 之后）、依旧打不开
  `/api/status`/`/api/config`、空 token 依旧拒绝。
- payload 无任何配置回显：新端点不重蹈 S-12（`/api/status` 携带 bot token/backend URL）的覆辙——它根本不读
  `cfg` 的敏感区，字段白名单由 N-03 钉死并有 `assertNotIn` 型测试。

**脚本 token 不落库不写死：**

- 脚本文件内**无任何默认 token/URL**（`probe_token`/`probe_url` 全部来自 `$arguments`）；
- probe 侧不新增任何存储：token 仍只存在于 `data/config.json` 的 `publish.token`（既有），本批不新增
  配置键、不新增 env；
- 传给 Sub-Store 侧的凭据是 **publish.token，绝不是 auth.token**（MIGRATION_MAPPING §3 禁止事项）。
  它出现在 Sub-Store 的 operator args/订阅 query 里，与现状「导出 URL 内嵌 publish.token 写进
  `probe-<key>` 远程订阅对象」（engine.py:2304, 2411-2416）属同一暴露等级——publish.token 本就是设计为
  交给 Sub-Store 的只读凭据；auth.token 永不经此路径（`ExportTokenTest` 已锁）；
- 测试与文档示例一律用假 token（`p*32` 形态）。

---

## 6. 已知陷阱与迁移注意

1. **`core.mixed_port` KeyError 陷阱**（core.py:236）：`build_config` 用 `core_cfg["mixed_port"]` **直接下标**
   读内核入站端口，而 `config.DEFAULTS` **没有**这个键——它是部署特有覆盖（现部署 config.json 里有，
   FEATURE_INVENTORY 附录 B 显式标注「DEFAULTS 没有！」）。2026-09-20 的 round 99 就是 prune「未知键」把它
   删掉导致整轮 KeyError 中止，`DEAD_KEYS` 因此是显式清单而非「不在 DEFAULTS 即死」（config.py:348-356）。
   迁移含义（2026-09-30 更新）：**R0 已把 `core.mixed_port` 加入 DEFAULTS**（默认 19194，
   `MIHOMO_TEST_MIXED_PORT` 覆盖，`NUMERIC_BOUNDS` 夹取）——新环境不再必须手工补键；
   `DEAD_KEYS` 仍保持显式清单、`validate_patch` 仍不做 DEFAULTS 白名单（结论不变）。
   历史约束「本批不得顺手加进 DEFAULTS」针对当时批次，由 R0 按总控文档 §2.3 解除并带测试。
2. **SECURITY_REVIEW 携带项**（本批不修、不得恶化）：
   - S-09（P2，ipmap 产物设计上携带订阅 URL 与 `orig_proxy` 凭据）——ipmap 整域 deprecate 保留私有，产物不外发；
   - S-12（P2，`/api/status` 轮询载荷携带 TG bot token/webhook/backend URL）——留待后续批次；新端点已按
     §5 规避同类问题；
   - S-13（P3，`publish.hostname` 等默认值为真实域名）——外发前占位符化（config.py:300、tools/build_web.py:38）；
   - S-16（P3，query token 进反代日志）——对**既有**导出 URL 是既定兼容形态；对**新**端点，N-02 脚本改用
     `X-Auth-Token` 头传 token，不再新增 query token 用法。
3. **外发前置 = 凭据轮换 + 历史清洗**（SECURITY_REVIEW 三.A）：S-01~S-03 的四个已跟踪 tools 脚本凭据已进
   git 历史，S-04 的 publish token 快照已明文留存——**先轮换（节点 UUID/REALITY 参数/前置凭据/
   publish token），再 `git filter-repo`/BFG 清洗**，之后基线才可推任何共享远端。由 MIGRATION_GUIDE 承载为
   硬前置，不因本批是「纯增量」而豁免。
4. **零存活语义对齐**：官方「零节点订阅必 500」已被 link_substore 用「零存活跳过联动」建模；路径 A 脚本侧的
   对应物是 §3.4 的空账本 200 + missing=keep——两条路径都不会把「probe 还没跑过」渲染成「节点全死」。

---

## 7. 回滚

三处新增彼此独立、均可整体移除，**无 schema 变更、无数据迁移 → 无数据回滚**：

| 部分 | 移除动作 | 验证 |
|---|---|---|
| N-01 路由 + 鉴权扩展 | `server.py` 删 `do_GET` 的 `/api/probe/nodes` 分支；删 `_publish_scoped` 并把 `auth_ok` 还原为 `path.startswith("/api/export/")` | `tests/test_hardening.py` 既有用例全绿（AuthLogicTest 的「publish token 只读 exports」语义原样成立） |
| N-03 模块 | 删 `mihomo_test/substore_bridge.py` + `tests/test_substore_bridge.py` | `python -m unittest discover -s tests` 基线 479 用例不回退 |
| N-02 脚本 | 删 `substore_bridge/` 目录（含脚本与 harness） | Node 侧无残留引用 |

Sub-Store 侧回滚（N-04 指南承载）：删订阅 process 里的 Script Operator 项 / 删指向 probe 的远程订阅即恢复，
官方对象删除即可逆、无本地状态。probe 侧导出与账本自始至终不被本批触碰。

---

## 8. 上游同步

- **零修改官方核心 → 无同步负担**：路径 B 只是我们这侧提供 HTTP 端点、官方侧填 URL；路径 A 只是官方运行时
  执行一段我们提供的脚本。官方 master 日更（2026-09-27 仍有 feat）不会产生任何 rebase/冲突面；AGPL 义务仅在
  「分发官方衍生品」时触发，本架构不分发官方代码（OFFICIAL_CAPABILITY_MAP §8.5）。
- **观察点 1：download 路由语义**（路径 B 的全部依赖）：`GET /download/:name[/:target]` 的 query 语义
  （target/ua/proxy/timeout/noCache/ignoreFailedRemoteSub）、1h 资源缓存、`500 即不存在` 怪癖；另注意官方 CI
  每次发版自动改写 `src/utils/download.js`/`flow.js` 的 `clash.meta/<version>` UA（OFFICIAL_CAPABILITY_MAP
  §8.2——若未来路径 C 才需要 rebase 知识）。语义变化时复查两处既有建模：`store.py._is_missing` 与
  「零存活跳过联动」。
- **观察点 2：Script Operator 契约**（路径 A 的全部依赖）：函数式签名 `operator(proxies, targetPlatform,
  context)`、`$arguments` 注入来源、Node/QX/Loon 等多运行时下 `fetch`/`AbortController` 的可用性、脚本资源
  缓存行为（`scriptResourceCache` 48h——本脚本不用它，但要留意官方若改为强制走缓存会否影响新鲜度）。
  若官方收紧脚本网络能力或改签名，仅需改 `probe_filter.script.js` 一个文件——这正是 §4.1 限定最小 API 面
  的目的。引用：OFFICIAL_CAPABILITY_MAP §4（17 个 operator 无测活，官方不会内建竞争能力）、§5（三条路径
  论证）、§8（上游同步注意事项全文）。

---

## 9. 对 MIGRATION_MAPPING.md 的修正建议（供 main Agent 落笔）

1. **N-02 compatibility 字段**「节点匹配按 name 优先、(server,port) 兜底」→ 应改为「name 优先、**server 兜底**」：
   probe 账本 nodes 表无 port 列（db.py:478-480 字段清单），`(server,port)` 需 schema 变更才能实现，超出本批
   范围；server 兜底 + 「同 host 多记录状态矛盾视为无记录」已覆盖实际场景（含导出前缀标签 `[CC] name` 造成
   的 name 不等，engine.py:2059）。
2. **N-01 compatibility**「鉴权沿用现有模型：publish.token 或 auth.token 皆可」——准确，但建议补一句
   「publish 作用域 = `/api/export/`（前缀）∪ `/api/probe/nodes`（精确）」，与 `AuthLogicTest` 语义对齐。
3. **N-01 risk**「加 ?source= 过滤与 count 上限」→ count 上限按 §3.3 裁量**不实现**（截断制造静默差异；
   nodes 表行数有上界），仅保留 `source`/`status` 两个过滤参数。
4. **测试落点**：N-01 的 HTTP 面用例与 N-03/N-02 用例统一落在新增 `tests/test_substore_bridge.py`
   （N-01 = `ProbeNodesEndpointTest` 16 例；`test_hardening.py` 未触碰）。（B4 修订，REVIEW R-02/R-05：
   原写「N-01 用例落 test_hardening.py、该文件进入白名单」与实际实现不符——tests Agent 独立决定集中落位，
   本节已按实际实现更正。）
