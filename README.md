# mihomo-probe — 节点测活中心

在 VPS 上用**真实 mihomo 内核**周期性测试代理节点：能应答探针不算数，要真实出站、
出口验证、跨轮次收敛，最后把「确实还活着」的节点回灌 Sub-Store，并提供一个带
Cloudflare Tunnel 公网入口的 Web 面板。

它回答三个问题，而且每个问题都用实测而不是推断来回答：

1. **节点活不活** —— 经内核真实出站测延迟，失败按类型归类，跨轮次连续失败计数收敛；
2. **节点到底是谁** —— 双视角 DNS 解析 + 逐 IP 测试 + 实测出口国别（ipmap 甚至能把
   「配置里没写明的落地机」挖出来）；
3. **客户端拿到的订阅还剩多少** —— 护栏防误杀、导出永远带齐链式前置、零存活不发空订阅。

> 这是一套自用的运维工具，不是通用软件。它假设你有一台境外的 VPS、一个在用的
> Sub-Store 实例，并且知道自己为什么需要它。请遵守所在地的法律法规。

---

## 目录

- [核心特性](#核心特性)
- [架构](#架构)
- [快速开始](#快速开始)
- [为什么这样设计（实测先行的三条结论）](#为什么这样设计实测先行的三条结论)
- [收敛策略与判活规则](#收敛策略与判活规则)
- [链式代理（前置 → 落地）](#链式代理前置--落地)
- [IP 回显探测（ipmap）](#ip-回显探测ipmap)
- [与 Sub-Store 联动](#与-sub-store-联动)
- [Web 面板与安全模型](#web-面板与安全模型)
- [测试](#测试)
- [目录结构与运维](#目录结构与运维)
- [资源占用与已知边界](#资源占用与已知边界)
- [项目文档](#项目文档)

---

## 核心特性

- **真实内核实测**：所有测试流量经 mihomo 内核出站（不是 HTTP 库模拟），配置先过内核
  自己的 `mihomo -t` 校验；
- **双视角解析 + 逐 IP 测试**：CN / 海外两套 DoH 各解析一遍（Geo-DNS 两边答案常常完全
  不同），每个解析出的地址都生成临时测试项，结果聚合回域名——任一地址活即判活；
- **出口验证车道**：N 条并行车道同时测不同节点，经内核回读**真实出口 IP 和国家**，
  剔除「探针能应答但出口落地不对」的假活节点；
- **跨轮次收敛**：按连接参数指纹（不是显示名）建账，连续失败 N 轮才判死；
  通过一次立刻恢复；整轮护栏防止一次网络事故清空订阅；
- **入口过滤**：入口全落在受限网络（默认 CN）上的节点整轮跳过、记 `excluded`，
  「测不了」不算「死」——这类节点名字常常自称美国，靠名字根本看不出来；
- **链式代理**：前置 → 落地链路按一等公民管理（面板增删、指纹含前置、导出自带
  未打标签的前置副本、全节点链式模式让普通节点也按过链实测）；
- **ipmap 落地探测**：经每个节点真实请求 IP 回显端点，v4/v6 分族实测落地出口，
  与写明的 `server` 比对识别中转，映射可回填 Sub-Store 节点名；
- **Sub-Store 双向打通**：测活结果作为导出端点被 Sub-Store 拉取（推荐），或测活后
  upsert 回写；另有 Script Operator 脚本让**任意**订阅按测活账本过滤死节点/标注国别；
- **告警与看门狗**：Telegram / Webhook 双通道、每类告警独立冷却期、轮次总预算超时
  中止并释放调度锁；
- **前后端分离**：前端是纯静态文件，同源部署或推 CDN 均可，产物构建时强制校验
  不带任何令牌；
- **零重依赖**：后端唯一第三方包是 PyYAML，其余全是 Python 标准库。

---

## 架构

```
                    ┌─────────────────────────────────────────┐
   上游订阅源        │  VPS                                    │
   (Sub-Store)       │                                         │
        │            │   ┌──────────────┐   REST /proxies/...   │
        │  ① 拉取     │   │ mihomo-probe │◄─────────────────┐   │
        └───────────►│   │ (真实内核)    │                  │   │
                     │   │ :19190 API   │                  │   │
                     │   │ :19194 HTTP  │──── ② 出口验证 ───┤   │
                     │   └──────────────┘   真实流量出站     │   │
                     │                                     │   │
                     │   ┌──────────────────────────────┐  │   │
                     │   │ mihomo-test                  │──┘   │
                     │   │ (测活引擎 + 收敛 + Web 面板)  │      │
                     │   │ :8088                        │      │
                     │   └──────────────────────────────┘      │
                     │            │                            │
                     │            │ ③ 输出 ClashMeta           │
                     │            ▼                            │
                     │   ┌─────────────────┐                   │
                     │   │ cloudflared     │                   │
                     │   └─────────────────┘                   │
                     └────────────┬────────────────────────────┘
                                  │ ④ 拉取 / 浏览器访问
                                  ▼
                    Sub-Store 远程订阅  +  Web 面板
```

三个容器，一个 `docker compose up -d` 全起来：

| 容器 | 作用 |
|---|---|
| `mihomo-test` | 应用本体：Web 面板、测试引擎、调度器、Sub-Store 客户端 |
| `mihomo-probe` | mihomo 内核，所有测试真实经过它出站 |
| `cloudflared-probe` | 把面板和导出端点发布出去（可选） |

**一轮的顺序：**

1. 从 Sub-Store 拉取每个数据源（组合订阅或单条订阅）的节点列表；链式来源则从自身
   配置内联生成。
2. 生成 mihomo 配置（节点 + `__PROBE__` select 组 + `MATCH,__PROBE__` 规则），用内核
   自己的 `mihomo -t` 校验，把内核不接受的节点剔除后重试。
3. **双视角解析 + 逐 IP 测试**：域名先用 CN 解析器（doh.pub + ECS）和海外解析器
   （Cloudflare + ECS）各解析一遍，解析出的**每个地址**都生成临时测试项，经内核并发调
   `GET /proxies/<name>/delay` 真实出站；结果聚合回域名——任一地址活即判活
   （`verify.domain_pass: all` 可改为全部活才算活）。视角解析结果缓存 6 小时。
4. **出口验证（并行车道）**：内核里建 N 个 select 组（`__LANE0__…`）和 N 个 loopback
   入站，用 `IN-NAME` 规则把每个入站钉到自己的组上。N 条车道同时各测各的节点，通过
   内核 HTTP 入站请求 `cloudflare.com/cdn-cgi/trace`，读回**真实出口 IP 和国家**，
   剔除 CN 出口。车道独立性有专门测试：交换两条车道的选中节点，报回的出口 IP 跟着交换。
5. **收敛**：按连续失败计数决定升级/降级，写入 SQLite。
6. **发布**：写 `data/exports/<key>.yaml`，并（可选）把结果 upsert 进 Sub-Store。

延迟测试是并发的（默认 20）。出口验证用 8 条并行车道——实测把 251 节点 / 189 活的
整轮从 288 秒压到约 90 秒。

---

## 快速开始

### 前置条件

- 一台**境外** VPS（测试点本身不能在受限网络里；v6 节点测活要求宿主机有 IPv6 出口）；
- Docker + Docker Compose；
- （可选）一个 Sub-Store 实例作为上游/下游；
- （可选）Cloudflare 账号（Tunnel 公网入口）。

### 部署

```bash
# 1. 把仓库放到 VPS 上（默认约定路径 /srv/mihomo-test，见下方「路径约定」）
git clone <本仓库> /srv/mihomo-test && cd /srv/mihomo-test

# 2. 配置环境变量
cp .env.example .env && $EDITOR .env
#   SUBSTORE_BACKEND   上游 Sub-Store 后端（可含密钥路径，别写进 compose）
#   TUNNEL_TOKEN       已有 Tunnel 就填，没有留给 setup_tunnel.py 生成

# 3. 起栈
docker compose up -d --build
bash install.sh          # 跑容器内测试 + 健康检查 + 停用旧的宿主机 systemd 单元（如有）

# 4. （可选）建 Cloudflare Tunnel 公网入口
python3 setup_tunnel.py  # 需要 Cloudflare 凭据
```

面板入口：`http://127.0.0.1:8088/?token=<TOKEN>`。首次启动会自动生成两个 token 存进
`data/config.json`（见[安全模型](#web-面板与安全模型)），读回来：

```bash
python3 -c "import json;print(json.load(open('data/config.json'))['auth']['token'])"
```

### 路径约定

项目目录在宿主机和容器内必须**同路径**（默认 `/srv/mihomo-test`）：应用要指挥宿主机
Docker 给内核跑 `mihomo -t` 校验，bind-mount 路径是宿主机 daemon 在解析，容器内独有的
路径它看不见。项目放别处时在 `.env` 里设 `MIHOMO_TEST_HOST_ROOT=<宿主机路径>`。

### 迁移到另一台 VPS

状态全在项目目录里（`data/` 存 SQLite 账本、config.json、UI token、内核 secret、
tunnel token），所以：

```bash
./migrate.sh pack                       # 在旧机器上打包
scp mihomo-test-*.tar.gz 新机:/tmp/
mkdir -p /srv/mihomo-test && tar -xzf /tmp/mihomo-test-*.tar.gz -C /srv/mihomo-test
cd /srv/mihomo-test && docker compose up -d --build
```

迁移后面板 token、收敛历史、数据源、告警设置全部保留，不用重建任何东西。

### 作为任意 Sub-Store 的上游

导出端点是：

```
https://<面板域名>/api/export/<key>.yaml?token=<publish.token>
```

任何 Sub-Store 实例把它当远程订阅拉即可（本机实例也可以走
`http://127.0.0.1:8088/api/export/...`，更稳）。反过来，本系统测谁的订阅由
`SUBSTORE_BACKEND` 指定——可以是本机实例，也可以是任何可达的后端（含密钥路径）。
所以在 A 机测活、把结果喂给 B 机的 Sub-Store，只是同一个变量的事。

---

## 为什么这样设计（实测先行的三条结论）

动手前先对旧管线做了实测，有三条结论直接决定了实现方式：

**1. 对一个死节点，多测几次救不活它。** 同一批 47 个节点连测 3 轮，存活是
17 / 15 / 15——第 1 轮反而最乐观，重测只会**减少**节点。失败是确定性的：24 个节点
连续三轮返回同一个 503，6 个连续三轮 504，签名完全可复现。所以轮内重试的收益很小。

**2. 真正需要重试的是「分辨失败类型」，而不是「重复同一件事」。** mihomo 对 503 和
504 给了不同含义，但旧管线把响应体丢掉了，只留 `HTTP Error 503`，两种诊断被压成
一个黑箱。本服务保留响应体并把失败归类成 `timeout` / `kernel_error` /
`bad_request` / `bad_response` / `bad_delay` / `unreachable` / `controller_error` /
`http_<status>` / `verify_failed` / `entry_cn` 等。

**3. 重试真正的价值在跨轮次，而且方向上主要是防「误杀」而不是「漏杀」。**
实测里只有 2 个节点在一轮内翻转（而且它们其实是同一台服务器），说明轮内抖动极小；
反倒是整轮性事故（测试目标挂掉、VPS 网络抖动）会一次性把所有好节点判死。旧管线
无条件覆盖输出、没有任何下限保护，这种情况下客户端订阅会直接空掉。

所以：**轮内只做有意义的少量重试，跨轮次用连续失败计数收敛，再加一道整轮护栏。**

---

## 收敛策略与判活规则

每个节点按**连接参数指纹**（`sha256(type|server|port|凭据…)[:16]`）建账，而不是按
显示名。上游有重名节点，用名字做键会把两个不同节点合并成一条记录，让一个的失败
抵消另一个的成功。链式节点的指纹把前置也折进去——同一个落地挂两个不同前置是两个
不同出口。

状态机：

| 状态 | 含义 |
|---|---|
| `alive` | 本轮通过，连续失败计数为 0 |
| `pending` | 曾经活着，正在连续失败但还没到阈值 |
| `dead` | 连续失败达到阈值 |
| `unknown` | 从未通过，且还没到阈值 |
| `excluded` | 不可测：入口 IP 在受限网络上（见下） |

规则：

- **通过一次立刻恢复为 `alive`**，不需要连续通过——恢复是便宜的，就让它便宜；
- **连续失败 3 轮（可配）才降级为 `dead`**，单轮失败只记为 `pending`；
- **整轮护栏**：本轮存活数低于「上轮存活数 × 50%」且低于绝对下限 3 时，判定本轮可疑，
  **不发布**，保留上一轮输出，并在面板上告警。

**入口过滤（`verify.entry_check`，默认开）**：把每个节点的入口（server）解析成
IP，用 ip-api.com 批量查归属国家并缓存进 SQLite（`ip_geo` 表，一个 IP 只查一次）。
**入口全落在受限网络（默认 CN）上的节点会被整轮跳过**——它们不是「死」，是从境外
测试点根本测不了；记成 `excluded`、原因 `entry_cn`，不参与死亡计数，也不进导出。
实测抓到过自称 `US-Los Angeles` 的节点实际入口在 CN 骨干网上，靠名字根本看不出来。
保守处理：多宿主主机只要还有一条非 CN 路径就照测；归属查询失败时本轮不做任何排除，
宁可多测也不误杀。

**测试时剥离 ECH（`verify.strip_ech`，默认开）**：mihomo 的 ECH 实现对 Cloudflare
前置节点不稳定——同一节点带 `ech-opts` 实测 4 次里 1 次 404、其余延迟翻倍；剥掉后
次次通过且更快。代价是没法证明 ECH 路径本身（客户端若依赖 ECH 抗封锁，那是客户端侧
的另一回事）。

**域名按原有形式推回**：逐 IP 只是测试手段，导出给 Sub-Store 的订阅里 `server` 字段
仍是**原始域名**（SNI/skip-cert-verify 等也原样保留）——客户端拿到的是正常域名订阅，
由客户端自己的解析器去挑地址；测活系统只负责证明「这个域名下的地址是活的」。

轮内重试是非对称的：

| 失败类型 | 处理 |
|---|---|
| 成功 | 立即返回，不再重试 |
| `timeout`（504） | 换测试目标重试，并把超时从 5s 提到 9s |
| `kernel_error`（503） | 换目标重试（换目标有可能改变结果） |
| `bad_request` / `bad_delay` / `unreachable` | 确定性失败，立即停止 |

**判活口径（HTTPS 必过）**：测试目标必须是真实 HTTPS 端点（默认
`https://www.google.com/generate_204` 一类），HTTP 端点的「成功」不算数——链式节点
尤其如此，只过 HTTP 很可能是前置的透明应答而不是整条链路真通了。

并发进程互斥用**文件锁**（`data/round.lock`）：CLI 和常驻服务是两个进程，线程锁拦不住
它们；重叠执行会让同一批节点在一轮内被计两次失败，从而提前降级（开发中真实踩到过：
29 个节点因此提前一轮被判死）。

---

## 链式代理（前置 → 落地）

一条链路 = 一个**前置** + 一个**落地**。落地节点的 proxy 上带 `dialer-proxy: <前置名>`，
内核于是先从前置出去、再由前置连落地。面板的「链式代理」区块管理它们，不用改配置文件。

- **＋ 快捷添加**：填一个前置，再填若干落地（每行一个），一次加一批；
- **前置/落地两种填法**：可以从已知节点里挑（输入框带 `datalist`），也可以直接粘贴
  分享链接，两者可混用；
- **启停 / 删除**：单条链路可停用；删掉最后一条时整个 `chains` 来源一并移除，避免在
  Sub-Store 里留下一个必然 500 的空订阅。

支持粘贴的协议：`vless` `vmess` `trojan` `ss` `hysteria2`(含 `hy2`) `tuic`
`socks`/`socks5`。不支持的协议或残缺的链接会**明确报错**，不会被半解析成一个必然
测不通的节点——那种失败和「落地节点本身是死的」在面板上长得一模一样。

几个设计点，都不是随手写的：

- **链路是一个普通数据源**（`kind: "chain"`，key 默认 `chains`），不是另一套存储。
  轮次、账本、导出文件、Sub-Store 联动、导出清理全都按原路走，没有特殊分支；
- **两端在添加时就解析成完整的 proxy dict**，存的不是「节点名引用」。存名字的话，
  上游一改名或来源一停用，链路会**静默变成直连落地节点**——这正是这个功能要防的事；
- **链路指纹把前置折进去**：同一个落地挂两个不同前置是两个不同出口，共用账本行会让
  一个前置的失败抵消另一个的成功；
- **导出的 YAML 里带上未打标签的前置副本**：`dialer-proxy` 按名字找前置，而导出会给
  节点打上测出来的国别标签（`[JP] CF前置`），标签每轮都可能在变，所以不带标签的副本
  是唯一稳定的名字。前置因此可能出现两次，这是故意的：多一行的代价 vs. 少了前置导致
  整条链路静默直连；
- **前置不可用不会毁掉整轮**：如果内核拒绝了配置、而报错指向的不是任何一个被测节点
  （几乎总是前置），这一轮会丢掉全部链路继续跑，而不是让几百个节点跟着一起失败；
- **`ech=` 里的 DoH 地址保留为 `_dns`**：mihomo 没有「配置列表 URL」字段，但实测能用的
  dict 就是这么写的。它是嵌套键，**会**参与指纹计算——同一个前置换个 DoH 地址会被当成
  新出口，代价是重新验证一次，方向是保守的。

**全节点链式模式（客户端路径）**：上游不带 `dialer-proxy` 的普通节点也可以按「过链」
实测——经前置池出站再连节点，任一前置通即算活。这回答的问题和直连测试不同：
「客户端配了这个前置之后，这个节点还能不能用」。两种模式各占账本一行、分类统计分别
计入，导出形态跟随实测结果（哪个模式测活的就按哪个形态导出）。

**链式真实性校验**：链式判活不止看延迟数值，还会做真实拉流校验——经链路拉一段真实
内容，确认「数据真的走了这条链」，而不是前置代替落地应答了探针。

链路来源和普通来源一样发布：`exports/chains.yaml` → `/api/export/chains.yaml` →
Sub-Store 远程订阅 `probe-chains`。客户端拉这个文件就同时拿到链路和它需要的前置。

---

## IP 回显探测（ipmap）

测活回答「节点活不活」；ipmap 回答「节点到底是谁」：经每个节点真实请求一个 IP 回显
端点，回显正文就是该节点的**实测出口**。v4 回显端点（仅 A 记录）和 v6 回显端点
（仅 AAAA）分开测，得到落地机两个协议族的地址，再用 Cloudflare trace 的 `loc=`/`colo=`
交叉印证国别。出口与节点写明的 `server` 一致 → 直落；不一致 → 入口是中转，落地机才是
配置里没写明的那台。这份映射可以填回 Sub-Store，让每个节点名自己带着实测出口。

```bash
# 在 VPS 上跑（宿主机或应用容器内均可）：
cd /srv/mihomo-test
python3 -m mihomo_test ipmap \
    --url 'https://<订阅地址>' --family v6 \
    --key demo-v6 --push-sub ipmap-demo-v6
```

- **节点来源三选一**：`--url`（裸 Clash YAML 或其 base64；分享链接文本不支持——链接
  解析交给 Sub-Store，录入后用 `--source` 引用）、`--file`、`--source <Sub-Store 资源名>`
  （`--source-kind collection` 选组合订阅）；
- **`--family v4|v6|all`** 按 `server` 的字面量族过滤（域名节点只在 `all` 下保留）；
  `--limit N` 只测前 N 个；`--lanes` 并发车道数（默认 4）；
- **产出**：`data/ipmap/<key>.json`（逐节点全字段）与 `<key>.md`（映射表）；控制台同步
  打印。`--push-sub <名字>` 把节点改名成 `原名 · [国别] 出口v6 出口v4` 后 upsert 成
  Sub-Store 本地订阅。JSON 产物**不含**节点凭据（`orig_proxy` 不落盘），报告标题用
  `--key` 而不是订阅 URL——产物拷出 `data/` 也不会泄露；
- **与测活轮次完全隔离**：复用车道机制（select 组 + `IN-NAME` 钉住的 loopback 入站），
  但用独立容器 `mihomo-ipmap`、独立 API 端口 19191、独立车道端口段 19300 起、内核目录
  `data/ipmap-core/`。跑再久也不会动 `mihomo-probe` 的配置；跑完自动移除容器
  （`--keep-core` 可保留排查）；
- 内核目录放在 `data/` 下是刻意的：compose 只把 `./data`、`./core` bind 进应用容器，
  自建目录在容器内执行时 docker bind-mount 的宿主路径是空的；
- v6 节点要求宿主机有 IPv6 出口（compose 注释记录了为什么必须 host 网络——bridge 没有
  v6 路由，v6-only 节点会全军覆没）；
- 订阅拉取用 clash 系 UA 优先、浏览器 UA 兜底（部分订阅站对浏览器 UA 是 403）；
- 实测样例（2026-09-27，某订阅 12 个 v6 节点，23 秒）：12/12 全部直落，出口 v6 与写明
  入口一致，出口 v4 各不相同且与节点名声称的落地国别一致。

离线单测在 `tests/test_ipmap.py`（无网络无 docker：族过滤、回显解析、标注、报告、
来源解析；起真内核经真节点 curl 的部分由实战覆盖）。

---

## 与 Sub-Store 联动

两种方式，**推荐第一种**。

### 方式一：Sub-Store 反向拉取（默认，推荐）

Sub-Store 把一个远程订阅指向本服务的导出端点：

```
https://<面板域名>/api/export/<key>.yaml?token=<publish.token>
```

用 `link_substore.py` 一键建好：

```bash
sudo python3 /srv/mihomo-test/link_substore.py            # 创建/刷新
sudo python3 /srv/mihomo-test/link_substore.py --remove   # 撤销
```

它会建：
- 远程订阅 `probe-<key>` → 指向上面的导出 URL
- 组合订阅 `probe` = 所有 `probe-<key>`

**为什么推荐这种方式**：没有任何东西需要写进 Sub-Store 里一个「名字可能变过」的对象。
之前的两套旧管线都死在回写这一步——一套的目标订阅被删了（404），另一套的输入集合被
改名了（Sub-Store 对不存在的资源返回的是 **HTTP 500 而不是 404**），两者都静默空转、
还退出 0。

### 方式二：测活后写入 Sub-Store（可选）

在面板「设置」里打开「测活后写入 Sub-Store」，或：

```bash
sudo python3 -m mihomo_test push
```

它会 upsert 一条本地订阅 `probe-<key>-local`（名字带 `-local` 后缀，不会覆盖方式一的
远程订阅）。每次写入都是 upsert（不存在就创建），并且把 Sub-Store 的
「500 + `SUBSCRIPTION_NOT_FOUND`」和「500 + 未捕获 TypeError」都识别为「不存在」，
不会把改名当成失败。

导出文件只在发布开启（`publish.enabled`，默认开）时由每一轮写出；发布一关，面板上的
「推送 Sub-Store」和 `POST /api/push` 会直接拒绝并说明原因，而不是把停用前留在磁盘上的
旧快照当成新结果推过去——那种推送会返回成功，然后在 Sub-Store 里留下一份再也不会更新
的订阅。

### 扩展面：Script Operator 消费测活账本

`substore_bridge/probe_filter.script.js` 是一个官方 Sub-Store Script Operator 脚本：
粘贴进**任意**订阅的「操作」里，它就会调用本服务的只读端点 `GET /api/probe/nodes`
（publish token 作用域、9 字段白名单、零凭据泄露），按测活账本：

| `$arguments` 参数 | 取值 | 语义 |
|---|---|---|
| `mode` | `filter`（默认） | 仅删除账本状态 `dead` 的节点；`pending/unknown/excluded` 与无记录节点一律保留（「不测 ≠ 死」，失败开放不误杀） |
| | `annotate` | 存活节点名尾追加 ` [CC]`（实测国别），dead 追加 ` ·dead`；幂等 |
| | `both` | 先 filter 后 annotate |
| `missing` | `keep`（默认） | probe 不可达 / 节点无记录 → 保留 |
| | `drop` | 无记录即删（严格模式：probe 挂了输出会清空，慎用） |

参数语义、契约与架构细节见 `ARCHITECTURE.md`；接入步骤见 `MIGRATION_GUIDE.md` §3。

### 面板里的数据源管理

- **刷新列表**：从 Sub-Store 读出全部组合订阅和单条订阅（带成员数、来源类型）；
- **勾选 / 取消**：勾上即启用，改动立刻保存，下一轮生效；
- **直连 / 链式**：两个互相独立的测量开关，只对带 `dialer-proxy` 的节点有区别。
  「链式」开 → 链式节点经前置池测；「直连」开 → 额外把它当作自己的服务器测一遍
  （剥掉 dialer）。两个都关会回落到直连——本轮没测到的节点会被账本清理、导出被截断，
  要真正停测请取消「启用」；
- **key**：决定导出文件名和 URL。勾选时按名称自动生成，可手改；重名自动加 `-2` 后缀。
  key 的校验是硬性的：`/ \ : * ? " < > |` 和以点开头的写法会被拒绝并提示原因——key
  同时是文件名，这一步也是路径穿越的防线；
- **同步 Sub-Store 联动**：为每个已启用来源建一条远程订阅 `probe-<key>` 指向本服务的
  导出端点，并维护聚合集合 `probe`；来源被取消勾选时，对应的远程订阅会被移除。
  只有关联字段变化才触发同步，纯测量开关的保存不会去碰 Sub-Store。

两点行为值得知道：

- **零存活就不联动**。Sub-Store 对任何解析出零个节点的订阅都返回 HTTP 500
  （`proxies: []`、空文档、`proxies:` 三种写法都实测过，都是 500），所以没有存活节点
  的来源不会被建成远程订阅；等它有节点了会自动重建。
- **取消勾选会清掉该来源的节点记录**。否则这些节点会以 `unknown` 状态永久留在面板上，
  把总数撑大。被「停用」但仍留在列表里的来源保留记录，重新勾选时不必从零开始累积
  连续失败计数。

---

## Web 面板与安全模型

面板是**纯静态文件**（`mihomo_test/web/`：`index.html` / `app.css` / `app.js` /
`theme.js`），后端只提供 `/api/*`。同源部署时后端顺手把这几个文件发出去；也可以把
前端单独部署到 CDN，让后端只当 API（见下「前后端分离」）。主题跟随系统
（`prefers-color-scheme`），可手动切换并记忆。

- 状态卡：节点总数 / 存活 / 观察中 / 已死 / 本轮存活 / 上轮耗时
- 手动测活：**直连测活**和**链式测活**分开（链式未配置时直接拒绝并提示去设置里开启，
  而不是悄悄跑一轮等同直连的轮次）
- 订阅输出：可直接复制的导出链接（已带 publish token）
- 节点表：源、名称、协议、**实测出口国家**、延迟、连续失败数、状态、近 12 轮趋势、
  最近失败原因
- 链式代理管理、ipmap 入口、数据源管理（见上）
- 设置：间隔、并发、超时、判死阈值、护栏比例、测试目标、出口验证开关、排除国家、
  告警通道、数据源 JSON
- 日志：最近 200 条事件

### Token 模型

Token 存在 `data/config.json`，**分成两个**：

| 键 | 用途 | 传给谁 |
|---|---|---|
| `auth.token` | 管理凭据：面板、`/api/*`、改配置 | 只留在浏览器地址栏 |
| `publish.token` | 只读凭据：**只能**读 `/api/export/*.yaml` 和 `/api/probe/nodes` | 贴进 Sub-Store / 任何客户端 |

两者都由 `config.load()` 在缺失时自动生成，不用手填。导出链接里带的是 `publish.token`，
所以一份订阅链接泄露不会连带交出改配置的权限——而改配置的权限在本部署上还能指挥
宿主机的 docker daemon。

`/healthz` 不需要鉴权，其余全部需要。传法三选一：`?token=`、`X-Auth-Token:`、
`Authorization: Bearer`。**空 token 一律拒绝**（不视为「关闭鉴权」）。

### HTTP 面加固

- 响应统一带 `X-Content-Type-Options: nosniff`、`X-Frame-Options: DENY`、
  `Referrer-Policy: no-referrer` 和一条 CSP。CSP 是 `script-src 'self'`（**没有**
  `'unsafe-inline'`）——bootstrap 块是 `<script type="application/json">`，对浏览器是
  数据不是代码；面板里也没有任何内联 `onclick=`；
- 静态资源（`/app.css`、`/app.js`、`/theme.js`）**免鉴权且只有白名单里的三个文件名会被
  服务**（不是目录静态服务），`/index.html`、`/_headers` 之类一律落到鉴权分支；
- CORS 默认空数组（同源部署不需要）；CDN 部署时把前端 origin 精确加进白名单——
  **不做通配**，通配子域意味着任何一个 `evil.<你的域>` 都能读到带令牌的响应；
  `OPTIONS` 预检免鉴权（预检根本带不了自定义头）；
- `/api/status` 返回的 `config` 里两个面板 token 已打码为 `***`。

### 前后端分离（前端可部署到 CDN）

```bash
python tools/build_web.py --api-base https://<面板域名>   # → dist/
python tools/deploy_pages.py --name <项目名>              # → Cloudflare Pages
python tools/verify_cdn.py https://<项目名>.pages.dev --api-base https://<面板域名>
```

两种模式跑的是**同一份文件**，差别只有 `index.html` 里的一个 bootstrap 块：

| 模式 | `apiBase` | 令牌从哪来 |
|---|---|---|
| 同源（默认） | `""` | 服务端渲染进 bootstrap |
| CDN | 后端 origin | 地址栏 `?token=` / `#token=`，取到后立刻用 `history.replaceState` 从地址栏抹掉，存 localStorage |

⚠️ **CDN 产物里绝不带令牌**（`build_web.py` 会检查并在缺失时拒绝构建）。`dist/` 是公开
的，放令牌等于发布凭据。

### docker.sock 的取舍

应用容器挂载了 `/var/run/docker.sock`（重启内核容器、跑内核自己的配置校验）。这等于
给它宿主机 root 级别的能力——本部署本来就是 root 在跑，实际安全面没有变大，但你应该
知道这一点。代码侧做了三件事把这个能力圈起来：`core.container` **不接受远程修改**
（它会被直接交给 `docker restart`，等于一个能停任意容器的原语；要改就改 `.env` 里的
`MIHOMO_TEST_CORE_CONTAINER` 再重启）、导出端点用独立的只读 token、空 token 一律拒绝。
真正的收紧是换 `docker-socket-proxy` 只放行 `container:start/stop/restart/logs`，还没做。
不挂 socket 也能跑：内核重启、配置校验两项自愈能力会退化，其余全部正常（代码里做了
优雅降级）。

### 告警与看门狗

面板「设置」里配置，两个通道都可选：

- **Telegram**：填 Bot Token 和 Chat ID，勾选启用。`tools/wire_telegram.py` 可以从
  你已有的凭据源（自建 watcher 脚本或 acme.sh 的 Telegram hook 配置）提取并校验后
  写入，凭据全程不落打印；
- **Webhook**：填 URL，告警会 POST 一个 JSON（`key` / `level` / `title` / `body` / `ts`）。

**每类告警有独立冷却期**（默认 240 分钟）。没有冷却的话，「存活过低」这类告警会在
问题持续的每个周期都发一条，告警很快变成被静音的噪音。冷却状态存在
`data/alert-state.json`；面板上的「发送测试告警」按钮会先保存设置再发一条金丝雀。

| 告警 | 触发 |
|---|---|
| `round_failed` | 一轮执行抛出异常 |
| `round_timeout` | 一轮超出预算被中止 |
| `suspect_round` | 护栏触发，本轮结果未发布 |
| `alive_low` | 存活数低于配置下限（`alive_floor`，0 = 关闭） |
| `no_nodes` | 所有来源都拉取失败，输出保持上一轮 |

**看门狗**：每轮有总预算（`watchdog.round_timeout_minutes`，默认 20 分钟），在拉取 /
构建内核 / 延迟测试 / 发布四个阶段之间检查，超时即中止并**释放调度锁**——否则一次
卡住的轮次会让文件锁永远被持有，后续所有轮次被静默跳过。当前轮次在做什么记录在
`data/round.state.json`，轮次结束即删除。

---

## 测试

分两层，回答的是两个不同的问题。

### 第一层：离线单元测试

**幂等、无网络、无部署**。内核被 mock 掉，SQLite 建在临时目录。回答「逻辑对不对」。

```bash
python3 -m unittest discover -s tests          # 全部（含 test_live 自动跳过）
python3 -m unittest tests.test_logic           # 只跑核心逻辑
python3 -m unittest tests.test_logic.ApplyAndPublishTest   # 只跑收敛/发布那一步
```

`tests/_isolation.py` 不是可选的：`config.DATA` 就是 `$MIHOMO_TEST_ROOT/data`，不加
隔离的话，一次 `unittest discover` 会在**生产** `data/` 里留下 `round.state.json`、
`state.db`，甚至被 `POST /api/config` 重写的 `config.json`——而那个残留的 config.json
又会让 `test_live` 误判「存在真实部署」，于是离线跑测试开始对着线上发请求。

### 第二层：实战测试 —— `tests/test_live.py`

**对着真实运行的栈跑**，真实内核、真实上游订阅、真实节点。回答单元测试回不了的
问题：「这东西在外面到底能不能用」。它读的是**当前部署的存活配置**，节点名从不
硬编码。

```bash
python3 tests/test_live.py                # 除 slow 外全部
python3 tests/test_live.py health api     # 只跑指定分组
python3 tests/test_live.py --include-slow # 加上有状态的多轮用例
```

分组，按由便宜到贵的顺序：

| 分组 | 覆盖 |
|---|---|
| `health` | 三个容器在跑、面板应答、鉴权真的拦得住、`/healthz` 免鉴权 |
| `api` | 面板端点返回文档承诺的形状；导出的 yaml 是合法 ClashMeta 且条数与元数据一致；key 不能路径穿越 |
| `kernel` | 内核报版本；只绑 loopback；车道真能出网；delay API 给出真实延迟；死节点被**分类**而非只记失败 |
| `lanes` | 每条车道入站在、车道组是不同 selector、`IN-NAME` 真的钉住了入站、**交换两条车道的选中节点出口 IP 跟着交换**（车道独立性的决定性证据） |
| `data` | 账本不变量：没有永不闭合的轮次、`finished_at >= started_at`、`dead` 必须真越过阈值、`alive` 连续失败必为 0、指纹必须是 16 位十六进制、`excluded` 永不累积失败计数、`ip_geo` 缓存可信、事件日志有上限 |
| `substore` | 后端可达；远程订阅指向本机；联动订阅非空；聚合集合只列我们自己的订阅 |
| `round` | 真跑一轮并盯着收敛：预算内跑完、确实测了东西、可疑轮次必须说明为什么被扣下 |
| `slow` | 跑完整两轮，检查死集合是否稳定（**必须 `--include-slow` 才跑**） |

设计上的两条硬规矩：

- **任何花钱、改状态、要跑几分钟的东西都标 `slow` 且默认关闭。** 一次测试运行绝不该
  意外污染生产账本。
- **`round` / `slow` 等的是账本，不是面板的「上一轮」。** 调度器随时可能自己起一轮，
  一旦有第二个生产者，trigger 字符串就不是可靠的身份。

`round` 分组要重定向输出再跑（`unittest.TextTestRunner` 在 stdout 是管道时会缓冲，
而这一组要跑约 2 分钟）：

```bash
ssh <user@vps> 'cd /srv/mihomo-test && nohup python3 -u tests/test_live.py -v round > /tmp/roundout.txt 2>&1 &'
# 稍后
ssh <user@vps> 'cat /tmp/roundout.txt'
```

开发过程中的实战测试与重构一共抓出并修掉了 4 个产品/测试 bug（双时钟、崩溃残留状态
关错轮次、严格模式失败原因丢失、车道交换用例抖动），完整过程与修法见
[DEVELOPMENT.md](DEVELOPMENT.md) 的「实战抓虫」一节。

---

## 目录结构与运维

```
/srv/mihomo-test/
├── Dockerfile              # 应用镜像（含 docker CLI，用于指挥内核容器）
├── docker-compose.yml      # mihomo-test + mihomo-probe + cloudflared-probe
├── requirements.txt        # 依赖清单（只有 PyYAML）
├── migrate.sh              # 打包 / 迁移
├── core/config.yaml        # 每轮按源重新生成
├── data/
│   ├── config.json         # 设置（Web 面板可改）+ 两个 token
│   ├── state.db            # SQLite：节点账本 / 轮次 / 逐轮结果 / 事件
│   ├── exports/<key>.yaml  # 给 Sub-Store 拉的输出
│   ├── round.lock          # 跨进程互斥
│   ├── core.secret         # 内核 API secret
│   └── tunnel.token        # Cloudflare Tunnel token
├── mihomo_test/            # 服务代码
│   ├── server.py           # HTTP 服务、鉴权、CORS、静态资源
│   ├── engine.py           # 测试引擎：拉取/构建/测活/收敛/发布
│   ├── core.py             # 内核容器管理、配置生成
│   ├── policy.py           # 状态机与收敛规则
│   ├── db.py               # SQLite 账本（全 UTC 时间戳）
│   ├── ipmap.py            # IP 回显探测
│   ├── substore_bridge.py  # 只读账本投影（payload 白名单）
│   ├── store.py            # Sub-Store HTTP 客户端
│   └── web/                # 静态前端（可独立推 CDN）
├── substore_bridge/        # Sub-Store Script Operator 脚本（probe_filter）
├── tests/                  # 离线单元测试 + 实战测试（见上节）
├── tools/                  # 诊断 / 验证 / 部署辅助脚本
├── setup_tunnel.py         # 建 Tunnel + DNS
└── link_substore.py        # 写 Sub-Store 联动对象
```

**依赖** —— 唯一的第三方包是 PyYAML（解析上游订阅、生成内核配置、输出导出文件三处
都要 YAML），其余全是标准库：

```bash
python3 -m venv .venv && . .venv/bin/activate
pip install -r requirements.txt
```

常用命令：

```bash
docker compose ps                     # 三个容器的状态
docker compose restart mihomo-test    # 改完代码后重启应用
docker compose up -d --build          # 代码有改动时重建并滚动
docker logs -f mihomo-test            # 应用日志
docker exec mihomo-test python3 -m unittest discover -s tests   # 跑离线单元测试
docker exec mihomo-test python3 tests/test_live.py health api   # 跑实战测试（子集）
docker exec mihomo-test python3 -m mihomo_test round            # 手动跑一轮
docker exec mihomo-test python3 -m mihomo_test status           # 看统计
docker exec mihomo-test python3 -m mihomo_test push             # 导出写进 Sub-Store
docker compose restart mihomo-probe   # 重建内核容器（重读生成的配置）
docker logs --tail 50 cloudflared-probe                        # Tunnel 日志
./migrate.sh pack                     # 打包整个部署用于迁移
```

端口（全部绑 loopback，公网入口只走 Tunnel）：内核 API `127.0.0.1:19190`，
8 条验证车道 `127.0.0.1:19200-19207`（车道数由 `core.lanes` 定，端口从 `core.base_port`
起顺延），内核 HTTP 入站 `127.0.0.1:19194`，面板 `127.0.0.1:8088`。

---

## 资源占用与已知边界

实测资源占用：

| 组件 | 内存 |
|---|---|
| mihomo-probe | 约 27 MB（上限 384 MB） |
| cloudflared-probe | 约 22 MB |
| mihomo-test | 约 24 MB |

实测吞吐：251 节点 / 189 活含出口验证约 90 秒（8 车道）；ipmap 12 个 v6 节点 23 秒。

已知边界：

- **出口验证默认 8 条车道并行**（`core.lanes`，上限 32）。车道越多越快，代价是占用
  更多 loopback 端口；
- **协议支持取决于内核**：`mieru` 这类 mihomo 扩展协议由 mihomo 接受，但 Sub-Store 转
  sing-box 时会丢掉（实测 18 个节点转出 13 个 outbound），这是 Sub-Store 侧的协议
  覆盖问题；
- **同一台服务器列两遍**会被折叠成一条记录和一次导出（连接参数完全相同，指纹一致）；
- 内核配置校验会剔除内核拒绝的节点；如果一个源整体不可用，会记事件并继续处理其它源，
  而不是让整轮失败；
- v6-only 测活要求宿主机有 IPv6 出口且容器用 host 网络（见 ipmap 一节）；
- `core.mixed_port` 首次部署需在 `data/config.json` 显式给出（`config.DEFAULTS` 无此
  键，详见 `MIGRATION_GUIDE.md` §4 的部署陷阱清单）。

---

## 项目文档

| 文档 | 内容 |
|---|---|
| [DEVELOPMENT.md](DEVELOPMENT.md) | **开发过程**：从旧管线之死到公开发行的完整时间线、决策、实战抓虫与踩坑记录 |
| [ARCHITECTURE.md](ARCHITECTURE.md) | 架构契约：模块边界、Sub-Store 扩展面（N-01~N-03）的接口定义 |
| [FEATURE_INVENTORY.md](FEATURE_INVENTORY.md) | 功能清单：100+ 项功能逐条索引（面板/UI/引擎/运维工具） |
| [MIGRATION_MAPPING.md](MIGRATION_MAPPING.md) | 官方 Sub-Store 移植映射矩阵（reuse/enhance/extension/adapter/deprecate 五类） |
| [MIGRATION_GUIDE.md](MIGRATION_GUIDE.md) | Sub-Store 接入手册：路径 A/B、部署陷阱、外发核对清单 |
| [SECURITY_REVIEW.md](SECURITY_REVIEW.md) | 安全审计报告：S-01~S-19 发现与整改状态 |
| [REVIEW.md](REVIEW.md) | 移植批次的独立审查报告 |
| [TEST_REPORT.md](TEST_REPORT.md) | 测试门禁记录与批次测试证据 |
| [PLAN.md](PLAN.md) / [CHANGELOG_MIGRATION.md](CHANGELOG_MIGRATION.md) | 移植计划与批次台账（B1~B4） |

## 许可证与免责

- 依赖许可：PyYAML（MIT）、mihomo（MIT）、cloudflared（Apache-2.0）、Docker CLI
  （Apache-2.0），全部与自选许可兼容；本项目未嵌入 Sub-Store 源码，仅 HTTP API
  互操作（详见 `SECURITY_REVIEW.md` §二）；
- 本项目仅用于对自己拥有/订阅的节点做可用性监测，与任何「科学上网」服务无关联；
  使用时请遵守所在地法律法规。
