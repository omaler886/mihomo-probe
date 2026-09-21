# mihomo-test — 节点测活中心

在 VPS 上用真实 mihomo 内核周期测活，跨轮次收敛出「确实死」的节点，把活节点
回灌 Sub-Store，并提供一个 Web 面板 + Cloudflare Tunnel 入口。

部署位置：`vps:/srv/mihomo-test`，面板入口 `https://probe.example.com/`。

---

## 部署与迁移（整个项目就是一套 Docker 栈）

三个容器，一个 `docker compose up -d` 全起来：

| 容器 | 作用 |
|---|---|
| `mihomo-test` | 应用本体：Web 面板、测试引擎、调度器、Sub-Store 客户端 |
| `mihomo-probe` | mihomo 内核，所有测试真实经过它出站 |
| `cloudflared-probe` | 把面板和导出端点发布出去 |

```bash
# 全新部署
# 把本目录拷到目标机后：
docker compose up -d --build
bash install.sh        # 跑测试 + 健康检查 + 停用旧的宿主机 systemd 单元
python3 setup_tunnel.py   # 需要 Cloudflare 凭据（本机 acme.sh 里存着）
```

**迁移到另一台 VPS** —— 状态全在项目目录里（`data/` 存 SQLite 账本、config.json、
UI token、内核 secret、tunnel token），所以：

```bash
./migrate.sh pack                       # 在旧机器上打包
scp mihomo-test-*.tar.gz 新机:/tmp/
mkdir -p /srv/mihomo-test && tar -xzf /tmp/mihomo-test-*.tar.gz -C /srv/mihomo-test
cd /srv/mihomo-test && docker compose up -d --build
```

迁移后面板 token、收敛历史、数据源、告警设置全部保留，不用重建任何东西。

**作为任意 Sub-Store 的上游** —— 导出端点是：

    https://<面板域名>/api/export/<key>.yaml?token=<TOKEN>

任何 Sub-Store 实例把它当远程订阅拉即可（本机实例也可以走
`http://127.0.0.1:8088/api/export/...`，更稳）。反过来，本系统测谁的订阅由
`SUBSTORE_BACKEND` 指定——可以是本机实例，也可以是任何可达的后端（含密钥路径）。
所以在 A 机测活、把结果喂给 B 机的 Sub-Store，只是同一个变量的事。

**路径约定** —— 项目目录在宿主机和容器内必须同路径（默认 `/srv/mihomo-test`）。
应用要指挥宿主机 Docker 给内核跑 `mihomo -t` 校验，bind-mount 路径是宿主机 daemon
在解析，容器内独有的路径它看不见。项目放别处时在 `.env` 里设
`MIHOMO_TEST_HOST_ROOT=<宿主机路径>` 即可。

**两个已知取舍**：

- 应用容器挂载了 `/var/run/docker.sock`（重启内核容器、跑内核自己的配置校验）。
  这等于给它宿主机 root 级别的能力。本部署本来就是 root 在跑，实际安全面没有变大，
  但你应该知道这一点。
- 不挂 socket 也能跑：内核重启、配置校验这两项自愈能力会退化，其余全部正常
  （代码里做了优雅降级，`docker` 不可用时跳过校验并记日志）。

---

## 为什么是这样设计的

设计前先做了实测，有三条结论直接决定了实现方式：

**1. 对一个死节点，多测几次救不活它。** 同一批 47 个节点连测 3 轮，存活是
17 / 15 / 15——第 1 轮反而最乐观，重测只会**减少**节点。失败是确定性的：24 个节点
连续三轮返回同一个 503，6 个连续三轮 504，签名完全可复现。所以轮内重试的收益很小。

**2. 真正需要重试的是「分辨失败类型」，而不是「重复同一件事」。** mihomo 对 503 和
504 给了不同含义，但旧管线把响应体丢掉了，只留 `HTTP Error 503`，两种诊断被压成
一个黑箱。本服务保留响应体并把失败归类成 `timeout` / `kernel_error` /
`bad_request` / `bad_response` / `bad_delay` / `unreachable` / `controller_error` /
`http_<status>` / `verify_failed` / `entry_cn` 等。

**3. 重试真正的价值在跨轮次，而且方向上主要是防「误杀」而不是「漏杀」。**
实测里只有 2 个节点在一轮内翻转，而它们其实是同一台服务器（见下），说明轮内抖动
极小；反倒是整轮性事故（测试目标挂掉、VPS 网络抖动）会一次性把所有好节点判死。
旧管线无条件覆盖输出、没有任何下限保护，这种情况下客户端订阅会直接空掉。

所以：**轮内只做有意义的少量重试，跨轮次用连续失败计数收敛，再加一道整轮护栏。**

---

## 架构

```
                    ┌─────────────────────────────────────────┐
   上游订阅源        │  vps                                    │
   (Sub-Store)       │                                         │
        │            │   ┌──────────────┐   REST /proxies/...   │
        │  ① 拉取     │   │ mihomo-probe │◄─────────────────┐   │
        └───────────►│   │ (真实内核)    │                  │   │
                     │   │ :19190 API   │                  │   │
                     │   │ :19194 HTTP  │──── ② 出口验证 ───┤   │
                     │   └──────────────┘   真实流量出站     │   │
                     │                                     │   │
                     │   ┌──────────────────────────────┐  │   │
                     │   │ mihomo-test.service          │──┘   │
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

一轮的顺序：

1. 从 Sub-Store 拉取每个数据源（组合订阅或单条订阅）的节点列表。
2. 生成 mihomo 配置：节点列表 + 一个 `__PROBE__` select 组 + `MATCH,__PROBE__` 规则，
   然后用内核自己的 `mihomo -t` 校验配置，把内核不接受的节点剔除后重试。
3. **双视角解析 + 逐 IP 测试**：域名先用两套 DoH 各解析一遍——CN 解析器
   （doh.pub）带 ECS{114.114.114.0/24}，海外解析器（Cloudflare）带 ECS{8.8.8.8/24}。
   Geo-DNS 给两边的答案往往完全不同（实测 baidu.com 一个视角出百度海外段、另一个
   出南京段），只测内核自己解析到的那一个地址，等于抽样。解析出的**每个地址**都
   生成一个临时测试项，经内核并发调
   `GET /proxies/<name>/delay?timeout=&url=` 真实出站；结果**聚合回域名**——
   任一地址活即判活（`verify.domain_pass: all` 可改为全部活才算活），面板显示
   `IP 活/总数`。视角解析结果缓存 6 小时（`dns.cache_hours`）。
4. **出口验证（并行车道）**：：内核里建 N 个 select 组（`__LANE0__…`）和 N 个 loopback
   入站（`19200…`），用 `IN-NAME` 规则把每个入站钉到自己的组上。这样 N 条车道可以同时
   各测各的节点，通过内核的 HTTP 入站请求 `cloudflare.com/cdn-cgi/trace`，读回
   **真实出口 IP 和国家**，剔除 CN 出口。这一步区分了「节点能应答探针」和「节点真的
   能承载流量、并且出口在它声称的地方」。已验证车道相互独立：交换两条车道的选中节点，
   两条车道报回的出口 IP 也跟着交换。
5. **收敛**：按连续失败计数决定升级/降级，写入 SQLite。
6. **发布**：写 `data/exports/<key>.yaml`，并（可选）把结果 upsert 进 Sub-Store。

延迟测试是并发的（默认 20）。出口验证用 8 条并行车道——实测把 251 节点 / 189 活的
整轮从 288 秒压到约 90 秒。

---

## 收敛策略

每个节点按**连接参数指纹**（`sha256(type|server|port|凭据…)[:16]`）建账，而不是按
显示名。上游有重名节点，用名字做键会把两个不同节点合并成一条记录，让一个的失败
抵消另一个的成功。

状态机：

| 状态 | 含义 |
|---|---|
| `alive` | 本轮通过，连续失败计数为 0 |
| `pending` | 曾经活着，正在连续失败但还没到阈值 |
| `dead` | 连续失败达到阈值 |
| `unknown` | 从未通过，且还没到阈值 |
| `excluded` | 不可测：入口 IP 在受限 ISP 上（见下） |

规则：

- **通过一次立刻恢复为 `alive`**，不需要连续通过——便宜的那一半做成便宜的。
- **连续失败 3 轮（可配）才降级为 `dead`**，单轮失败只记为 `pending`。
- **整轮护栏**：本轮存活数低于「上轮存活数 × 50%」且低于绝对下限 3 时，判定本轮可疑，
  **不发布**，保留上一轮输出，并在面板上告警。

**入口过滤（`verify.entry_check`，默认开）**：把每个节点的入口（server）解析成
IP，用 ip-api.com 批量查归属国家并缓存进 SQLite（`ip_geo` 表，一个 IP 只查一次）。
**入口全落在受限 ISP（默认 CN）上的节点会被整轮跳过**——它们不是「死」，是
从香港这个测试点根本测不了；记成 `excluded`、原因 `entry_cn`，不参与死亡计数，
也不进导出。这类节点名字常常自称美国（实测抓到 `US-Los Angeles` 实际入口在
CN 中国移动），靠名字根本看不出来。保守处理：多宿主主机只要还有一条非 CN
路径就照测；归属查询失败时本轮不做任何排除，宁可多测也不误杀。

**测试时剥离 ECH（`verify.strip_ech`，默认开）**：mihomo 的 ECH 实现对
Cloudflare 前置节点不稳定——同一节点带 `ech-opts` 实测 4 次里 1 次 404、其余
延迟翻倍；剥掉后次次通过且更快。剥掉之后测的是「非 ECH 路径」的可用性，
代价是没法证明 ECH 路径本身（CN 客户端若依赖 ECH 抗封锁，那是客户端侧的
另一回事）。

**域名按原有形式推回**：逐 IP 只是测试手段，导出给 Sub-Store 的订阅里
`server` 字段仍是**原始域名**（SNI/skip-cert-verify 等也原样保留）——客户端拿到
的还是正常域名订阅，由客户端自己的解析器去挑地址；测活系统只负责证明
「这个域名下的地址是活的」。

轮内重试是非对称的：

| 失败类型 | 处理 |
|---|---|
| 成功 | 立即返回，不再重试 |
| `timeout`（504） | 换测试目标重试，并把超时从 5s 提到 9s |
| `kernel_error`（503） | 换目标重试（换目标有可能改变结果） |
| `bad_request` / `bad_delay` / `unreachable` | 确定性失败，立即停止 |

并发进程互斥用**文件锁**（`data/round.lock`），因为 CLI 和 systemd 服务是两个进程，
线程锁拦不住它们；重叠执行会让同一批节点在一轮内被计两次失败，从而提前降级
（开发中真实踩到过：29 个节点因此提前一轮被判死）。

---

## 告警与看门狗

面板「设置」里配置，两个通道都可选：

- **Telegram**：填 Bot Token 和 Chat ID，勾选启用。
- **Webhook**：填 URL，告警会 POST 一个 JSON（`key` / `level` / `title` / `body` / `ts`）。

**本机已接入 Telegram**（2026-09-19）：凭据复用 `selfhost-watcher/watcher.py` 里那对
已验证可用的 bot + chat（`tools/wire_telegram.py` 负责提取、getMe 校验、发金丝雀并
写入配置）。告警从 `@demo-telegram-bot` 发出——想换个专属 bot，在面板里
替换 token 即可。改完配置记得 `docker compose restart mihomo-test` 让运行中的应用
重新读取。

**每类告警有独立冷却期**（默认 240 分钟）。没有冷却的话，「存活过低」这类告警会在
问题持续的每个周期都发一条，告警很快就会变成被静音的噪音。冷却状态存在
`data/alert-state.json`；面板上的「发送测试告警」按钮会先保存设置再发一条金丝雀
（`POST /api/alert-test`）。

触发条件：

| 告警 | 触发 |
|---|---|
| `round_failed` | 一轮执行抛出异常 |
| `round_timeout` | 一轮超出预算被中止 |
| `suspect_round` | 护栏触发，本轮结果未发布 |
| `alive_low` | 存活数低于配置下限（`alive_floor`，0 = 关闭） |
| `no_nodes` | 所有来源都拉取失败，输出保持上一轮 |

**看门狗**：每轮有总预算（`watchdog.round_timeout_minutes`，默认 20 分钟），在拉取 /
构建内核 / 延迟测试 / 发布四个阶段之间检查，超时即中止并**释放调度锁**——这一点很
重要，否则一次卡住的轮次会让文件锁永远被持有，后续所有轮次被静默跳过。出口验证
阶段还会在每条车道内提前收手，所以不会先跑满预算再报错。当前轮次在做什么记录在
`data/round.state.json`，轮次结束即删除。

## 数据源管理

数据源在面板的「数据源」面板里勾选，不需要改配置文件。

- **刷新列表**：从 Sub-Store 读出全部组合订阅和单条订阅（带成员数、来源类型）。
  当前实例上能列出 33 个资源。
- **搜索 / 类型筛选 / 只看已启用**：资源多时用来快速定位。
- **勾选 / 取消**：勾上即启用，取消即停用；改动立刻保存，下一轮生效。
- **key**：决定导出文件名和 URL（`/api/export/<key>.yaml`、`exports/<key>.yaml`）。
  勾选时按名称自动生成，可手改；重名会自动加 `-2` 后缀。
- **＋ 手动添加**：列表里没有的资源（例如刚在别处建好还没刷新）可以按名称直接填。
  非法 key 会被拒绝并给出原因，不会静默改名。
- **同步 Sub-Store 联动**：为每个已启用来源建一条远程订阅 `probe-<key>` 指向本服务的
  导出端点，并维护聚合集合 `probe`；来源被取消勾选时，对应的远程订阅会被移除。

key 的校验是硬性的：`/ \ : * ? " < > |` 和以点开头的写法会被拒绝并提示原因。
key 同时是文件名，所以这一步也是路径穿越的防线（`read_export` / `export_meta` 会再校验一次）。

两点行为值得知道：

- **零存活就不联动**。Sub-Store 对任何解析出零个节点的订阅都返回 HTTP 500
  （`proxies: []`、空文档、`proxies:` 三种写法我都实测过，都是 500），所以没有存活节点的
  来源不会被建成远程订阅，避免留下一个必然 500 的地址；等它有节点了会自动重建。
- **取消勾选会清掉该来源的节点记录**。否则这些节点会以 `unknown` 状态永久留在面板上，
  把总数撑大。被「停用」但仍留在列表里的来源（`enabled: false`）保留记录，
  这样重新勾选时不必从零开始累积连续失败计数。

---

## 与 Sub-Store 联动

两种方式，**推荐第一种**。

### 方式一：Sub-Store 反向拉取（默认，推荐）

Sub-Store 把一个远程订阅指向本服务的导出端点：

```
https://probe.example.com/api/export/air.yaml?token=<TOKEN>
```

用 `link_substore.py` 一键建好（已执行过）：

```bash
sudo python3 /srv/mihomo-test/link_substore.py            # 创建/刷新
sudo python3 /srv/mihomo-test/link_substore.py --remove   # 撤销
```

它会建：
- 远程订阅 `probe-air` → 指向上面的导出 URL
- 组合订阅 `probe` = [`probe-air`]

客户端或配置里引用：

```
/download/collection/probe?target=ClashMeta
/download/probe-air?target=ClashMeta
```

**为什么推荐这种方式**：没有任何东西需要写进 Sub-Store 里一个「名字可能变过」的对象。
之前两套管线都死在回写这一步——一套的目标订阅被删了（404），另一套的输入集合被改名了
（Sub-Store 对不存在的资源返回的是 **HTTP 500 而不是 404**），两者都静默空转、还退出 0。

### 方式二：测活后写入 Sub-Store（可选）

在面板「设置」里打开「测活后写入 Sub-Store」，或：

```bash
sudo python3 -m mihomo_test push
```

它会 upsert 一条本地订阅 `probe-<key>-local`（名字带 `-local` 后缀，不会覆盖方式一的
远程订阅）。每次写入都是 upsert（不存在就创建），并且把 Sub-Store 的
「500 + `SUBSCRIPTION_NOT_FOUND`」和「500 + 未捕获 TypeError」都识别为「不存在」，
不会再把改名当成失败。

---

## Web 面板

`https://probe.example.com/?token=<TOKEN>`

- 状态卡：节点总数 / 存活 / 观察中 / 已死 / 本轮存活 / 上轮耗时
- 订阅输出：可直接复制的导出链接（已带 token）
- 节点表：源、名称、协议、**实测出口国家**、延迟、连续失败数、状态、近 12 轮趋势、最近失败原因
- 设置：间隔、并发、超时、尝试次数、判死阈值、护栏比例、测试目标、出口验证开关、
  排除国家、数据源 JSON
- 日志：最近 200 条事件

Token 存在 `data/config.json` 的 `auth.token`，同一个值用于面板和导出端点。
`/healthz` 不需要鉴权，其余全部需要。

---

## 目录与运维

```
/srv/mihomo-test/
├── Dockerfile              # 应用镜像（含 docker CLI，用于指挥内核容器）
├── docker-compose.yml      # mihomo-test + mihomo-probe + cloudflared-probe
├── requirements.txt        # 依赖清单（只有 PyYAML）
├── migrate.sh              # 打包 / 迁移
├── core/config.yaml        # 每轮按源重新生成
├── data/
│   ├── config.json         # 设置（Web 面板可改）+ UI token
│   ├── state.db            # SQLite：节点账本 / 轮次 / 逐轮结果 / 事件
│   ├── exports/<key>.yaml  # 给 Sub-Store 拉的输出
│   ├── round.lock          # 跨进程互斥
│   ├── core.secret         # 内核 API secret
│   └── tunnel.token        # Cloudflare Tunnel token
├── mihomo_test/            # 服务代码
├── tests/                  # 测试（见下节）
│   ├── test_logic.py       # 147 个离线单元测试，无需部署
│   ├── test_alerts_lanes.py# 告警与车道的离线用例
│   └── test_live.py        # 对着真实部署跑的实战测试
├── tools/                  # 诊断 / 数据修复脚本
├── setup_tunnel.py         # 建 Tunnel + DNS（用 acme.sh 里的 CF 凭据）
└── link_substore.py        # 写 Sub-Store 联动对象
```

**依赖** —— 唯一的第三方包是 PyYAML（解析上游订阅、生成内核配置、输出导出文件三处都要
YAML），其余全是标准库。清单在 `requirements.txt`，Dockerfile 用
`pip install -r requirements.txt` 装，所以清单是唯一事实来源而不是注释：

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
**故意不用 19090**：旧的 `/srv/mihomo-health` 栈在它自己的 30 分钟 cron 里绑定 19090，
占用会导致它的容器起不来、并且让它误测我们的内核。

---

## 测试

分两层，回答的是两个不同的问题。

### 第一层：离线单元测试 —— `tests/test_logic.py`

**幂等、无网络、无部署**。内核被 mock 掉，SQLite 建在临时目录。回答「逻辑对不对」。

```bash
python3 -m unittest discover -s tests          # 全部（含 test_live 自动跳过）
python3 -m unittest tests.test_logic           # 只跑离线单元测试
python3 -m unittest tests.test_logic.ApplyAndPublishTest   # 只跑收敛/发布那一步
```

`unittest discover` 会导入 `tests/test_live.py`，但它有模块级守卫：找不到部署就
`raise unittest.SkipTest`，所以在这台开发机上跑 discover 不会去连一个不存在的面板、
也不会把一堆连接错误报成失败。

### 第二层：实战测试 —— `tests/test_live.py`

**对着 vps 上真实运行的栈跑**，真实内核、真实上游订阅、真实节点。回答单元测试回不了的
问题：「这东西在外面到底能不能用」。它读的是**当前部署的存活配置**，节点名从不硬编码。

在宿主机或应用容器里直接跑（两边都可以）：

```bash
python3 tests/test_live.py                # 除 slow 外全部
python3 tests/test_live.py -v             # 打印每个用例名
python3 tests/test_live.py health api     # 只跑指定分组
python3 tests/test_live.py --include-slow # 加上有状态的多轮用例
```

分组，按由便宜到贵的顺序：

| 分组 | 覆盖 |
|---|---|
| `health` | 三个容器在跑、面板应答、鉴权真的拦得住、`/healthz` 免鉴权 |
| `api` | 面板端点返回文档承诺的形状；导出的 yaml 是合法 ClashMeta 且条数与元数据一致；key 不能路径穿越 |
| `kernel` | 内核报版本；只绑 loopback（不看 peer 列）；车道真能出网；delay API 给出真实延迟；死节点被**分类**而非只记失败 |
| `lanes` | 每条车道的入站都在；车道组是不同的 selector；**交换两条车道的选中节点，出口 IP 跟着交换**（独立性的决定性证据）；`IN-NAME` 规则真的钉住了入站 |
| `data` | 账本不变量：没有永远不闭合的轮次、`finished_at >= started_at`、`dead` 必须真的越过阈值、`alive` 的连续失败必须为 0、指纹必须是 16 位十六进制（名字做键说明旧方案残留）、结果不悬空引用、**`excluded` 节点永不累积失败计数**、`ip_geo` 缓存可信、域名视图缓存里真的是 IP、事件日志有上限 |
| `substore` | 后端可达；远程订阅指向本机；联动订阅非空；聚合集合只列我们自己的订阅 |
| `round` | 真跑一轮并盯着收敛：在预算内跑完、确实测了东西、可疑轮次必须说明为什么被扣下；连续三轮不会把存活集静默清空 |
| `slow` | 跑完整两轮，检查死集合是否稳定（**必须 `--include-slow` 才跑**） |

设计上的两条硬规矩：

- **任何花钱、改状态、要跑几分钟的东西都标 `slow` 且默认关闭。** 一次测试运行绝不该
  意外污染生产账本。
- **`round` / `slow` 等的是账本，不是面板的「上一轮」。** 调度器随时可能自己起一轮，
  一旦有第二个生产者，trigger 字符串就不是可靠的身份。

**入口 CN ISP 直接跳过**这条要求由 `data` 分组守住：`test_excluded_nodes_never_accumulate_a_failure_streak`
断言 `status='excluded'` 的节点连续失败计数永远为 0——「测不了」不该被当「死」。
配套的 `test_ip_geo_cache_is_populated_and_plausible` 保证判定所依赖的归属缓存可信。
离线侧有 `EntryClassificationTest` 的 8 个用例覆盖分类逻辑本身。vps 上实测确认：
`US-02 · TROJAN`（server `node.example.net`，自称美国）被判 `excluded` / `entry_cn`，
`consec_fail=0`，且全库没有任何 excluded 节点带着失败计数。

**注意：`round` 分组要重定向输出再跑。** `unittest.TextTestRunner` 在 stdout 是管道时
会缓冲，而这一组要跑约 2 分钟——直接 `ssh vps 'python3 tests/test_live.py round'` 会看到
「卡住」并且可能被 SSH 超时掐掉，但代码是好的：

```bash
ssh vps 'cd /srv/mihomo-test && nohup python3 -u tests/test_live.py -v round > /tmp/roundout.txt 2>&1 &'
# 稍后
ssh vps 'cat /tmp/roundout.txt'
```

等待上限单独封顶在 480 秒（`FullRoundTest.MAX_WAIT_S`）——看门狗预算是 20 分钟，
但为了 20 分钟阻塞会拖死大多数 shell 和 CI 包装；撞到上限时报 **skip 并附证据**，而不是静默挂住。

vps 上的实测结果（容器内一次 `discover` 会同时收集两层，共 **187 个通过 / 3 跳过**）：
离线单元测试 **147 个通过**（约 5.6 分钟——容器里跑比本机慢，本机约 3 秒）；实战测试
`health` / `api` / `data` / `substore` / `kernel` / `lanes` / `round` 共 **40 个通过**
（1 跳过：mihomo 没有 `/listeners`），其中 `round` 约 2 分钟。
`lanes` 分组连跑 5 次全绿。

### 实战测试与重构中抓到并修掉的 4 个 bug

**1（产品 bug）同一张表里两个时钟（`finished_at` 早于 `started_at` 8 小时）。**
第 55/56 轮的 `started_at` 是 `2026-09-19T20:3x`，`finished_at` 却是 `12:38:09`。
根因是宿主是 UTC，而应用容器设了 `TZ=Asia/Shanghai`：`db.now()` / `policy._stamp()` /
`_write_state` 的 `ts` 写的是容器本地时间（CST），另一些路径写的是 UTC 派生的字符串，
于是同一次运行里两个进程往同一列写了差 8 小时的值。`round.state.json` 自己就露了馅——
`"ts":"2026-09-19T21:55:47"` 和 `"epoch":1789826147`（=13:55:47 UTC）在同一个文件里差 8 小时。

修法：**全链路统一 UTC**（`db.now` / `db.to_epoch` / `policy._stamp` / `_write_state.ts` /
`notifier` / `server._parse` 与 `domain_views_get`），并在 `tests/test_logic.py` 里加了
一整个 `TimestampTest` 类锁住这个约定（含「在非 UTC 时区下 `db.now()` 仍是 UTC」）。

**2（产品 bug）崩溃残留的 `round.state.json` 会关错轮次。**
`_abandon_round` 无条件信任残留状态文件里记的 `round_id`，于是一次崩溃后，它把
**一个已经结束的轮次**当成受害者又关了一遍。修法是加 `_state_belongs_to()`：只有当该轮
确实还开着（`finished_at IS NULL`）才据此关闭，否则记一条 warn 并忽略。

**3（产品 bug）`domain_pass: all` 失败时记不下失败原因。**
严格模式下某个多地址域名只要有**一个**地址不通就判死，但聚合原因取的是 `outcomes[0]`——
而列表首项完全可能是个**健康**地址，于是账本里出现 `last_reason: None`：面板上显示一个
「已死但没有任何原因」的节点。改成取**第一个真正失败的地址**。这个 bug 是拆分
`_apply_and_publish` 时为写测试才发现的——原代码里这条分支从来没有被直接测过。

**4（测试 bug）车道交换用例会抖动，而且失败信息会误导人。**
`test_swapping_two_lanes_swaps_their_exits` 只探测 `lane0` 来挑两个候选节点，然后却对
**从未验证过能用的 `lane1`** 做断言。于是某个节点只是「恰好过不了 lane1」时，会被读成
「交换没生效」——也就是被当成车道独立性被破坏的铁证，而实际上两者都不是。vps 上实测到过一次：
`before=[192.0.2.40, 192.0.2.111]`、`after=[192.0.2.111, None]`，lane1 掉线了。
修法是**交换前把两条车道都验证一遍**，任何一条走不通就计入候选淘汰（最终报 skip 而不是 fail）。
用独立探针确认过车道本身没问题：两条车道在任何探测顺序、带不带 `Connection: close` 下
都稳定报出各自的出口 IP。

历史脏数据用 `tools/fix_round_times.py` 修（**默认 dry-run，`--apply` 才写**，写前自动
打时间戳备份）。当时正好 2 行倒挂，修复后 `remaining inverted rows: 0`；备份在
`/srv/mihomo-test/data/state.db.pre-tzfix-20260919-142309`。

### 时间戳约定

**所有落库的时间列都是 UTC**，不是容器本地时间。这是刻意的：容器 `TZ` 和宿主时区可能
不一致，本地时间会让同一列出现两个时钟。改这条约定时要连着改 `db.now` 的读取方
（`db.to_epoch`、`server._parse`、`policy._stamp`、`notifier`），`TimestampTest` 会挡住漏改。

### 收敛与发布那一步的结构

`_apply_and_publish` 曾经是约 115 行、6 项职责的单体函数。现在它是一个约 49 行的
编排者，每一步都是可单独测试的具名函数：

| 函数 | 职责 |
|---|---|
| `_index_original_proxies` | 把内核的逐地址变体映射回**原始域名**形式（导出用） |
| `_score_bucket` | 把一个指纹下所有地址的结果归约成 `(ok, delay, reason, detail)` |
| `_converge_bucket` | 单个端点的状态机推进 + 写本轮结果行 |
| `_record_excluded_nodes` | 被入口过滤跳过的节点的账本登记 |
| `_prune_removed_nodes` | 清掉上游已移除的节点记录 |
| `_publish_sources` | 写各来源的导出文件（可选推 Sub-Store） |

护栏判定（`round_is_suspect`）留在编排者里，因为它是**发布的门**，且只依赖编排者手上的总数。
拆分时补了一整个 `ApplyAndPublishTest`（14 个用例）覆盖原先只靠实战测试间接覆盖的分支：
`domain_pass` 两种模式、出口国家拒绝、`verify_failed`、导出落盘、护栏保留旧输出、
别名只推进一步连续失败计数。其中「护栏必须保住旧导出」这条做过反证——把修复还原后
测试确实失败。

---

## 已禁用的旧管线（2026-09-19）

两套旧的测活管线已停用：它们与本服务功能重复，且长期静默失败、输出无人消费。

| 停用项 | 原因 |
|---|---|
| root cron `*/30 /srv/mihomo-health/refresh.sh` | 回写目标订阅已被删除，每轮 `PATCH` 404，日志写 `sub-store push FAILED` 但退出 0 |
| root cron `5,35 substore_add_alive.py --content` | 只做内容覆盖、从不创建，前面那个 404 的同一问题 |
| systemd `mihomo-healthcheck.timer` | 输入集合 `机场` 已改名 `air`，第一处 API 调用就 500，每天 19:30 失败一次（0.1 秒退出） |
| 容器 `mihomo-air` | 上述 cron 的探测容器，同样占用 19090 端口 |
| nginx `/<秘密路径>…/health/*` | 公开暴露 `alive.yaml`（含 server/port/uuid/password），只靠「秘密路径」保护，停用后会永久发一份过时快照 |
| Sub-Store 集合 `legacy-alive` | 上述管线的产物，0 成员、无人引用 |

**未改动**：`/srv/mihomo-health`、`/srv/healthcheck` 目录仍在磁盘上；`devcloud-healthcheck.timer`、
`node-engine` 容器、`air` / `legacy-sub-c` / `legacy-sub-d` 等上游订阅、以及 Sub-Store 的
`SUB_STORE_PRODUCE_CRON`（仍在每小时预生成 `air`，本服务的输入依赖它）全部保持原样。

备份与回滚说明在 vps 的 `/srv/legacy-ban-backup/<时间戳>/`（含 `crontab.before`、
nginx vhost 原件、被删集合的完整 JSON、`RESTORE.md`）。执行脚本：`ban_legacy.sh`。

`legacy-sub-d` 集合上还留着一套 Sub-Store 原生的 http-meta 测活脚本（可用、但与本服务重复）。
没有停它，因为它能正常工作、不算「没用」，且停掉会改变消费它的配置的行为。

---

## 资源占用

| 组件 | 内存 |
|---|---|
| mihomo-probe | 约 27 MB（上限 384 MB） |
| cloudflared-probe | 约 22 MB |
| mihomo-test.service | 约 24 MB |

实测：251 节点 / 189 活含出口验证约 90 秒（8 车道）。

---

## 已知边界

- **出口验证默认 8 条车道并行**（`core.lanes`，上限 32；端口从 `core.base_port` 起，
  默认 19200）。车道越多越快，代价是占用更多 loopback 端口。
- **协议支持取决于内核**：`mieru` 这类 mihomo 扩展协议由 mihomo 接受，但 Sub-Store 转
  sing-box 时会丢掉（实测 18 个节点转出 13 个 outbound），这是 Sub-Store 侧的协议覆盖问题。
- **同一台服务器列两遍**会被折叠成一条记录和一次导出（连接参数完全相同，指纹一致）。
  实测 `CH BRN Buyvm` 与 `CH BRN Buyvm IPv6` 就是这种情况——这也解释了它们为什么总是
  一起失败：它们本来就是同一台机器。
- 内核配置校验会剔除内核拒绝的节点；如果一个源整体不可用，会记事件并继续处理其它源，
  而不是让整轮失败。
