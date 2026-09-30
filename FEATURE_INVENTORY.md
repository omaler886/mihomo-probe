# FEATURE_INVENTORY — mihomo_test 现状功能清单（legacy-audit 第一轮）

> 审计对象：`D:\ChatGPT\代理测活`（部署实体在 vps:/srv/mihomo-test）。
> 审计方式：只读通读 README.md、mihomo_test/ 全部 .py（忽略 \*.bak-\*）、tests/、tools/、
> Dockerfile、docker-compose.yml、requirements.txt、.env.example、link_substore.py、
> setup_tunnel.py、mihomo_test/web/。所有行号以当前工作区文件为准。
> 目的：作为「合并移植到官方 Sub-Store 架构」功能映射矩阵的输入。
>
> 约定：`入口` = 功能的代码入口（文件:行号）；`配置` = config.json 键名（含环境变量）；
> `依赖` 分 标准库 / 第三方（仅 PyYAML）/ 外部服务 / mihomo 内核 / docker daemon。

---

## 域 1 订阅获取与解析

### F-01 Sub-Store HTTP 客户端（重试 + 500→「不存在」语义归一化）
- 入口：`mihomo_test/store.py:31`（class Client）、`store.py:37 _request`、`store.py:69 _is_missing`
- 输入：backend URL（`substore.backend`，可带密钥路径）、方法/路径/JSON payload
- 输出：`(status, text)`；GET JSON 封装为 `get_json`（要求 `status=="success"`，取 `data`）
- 配置：`substore.backend`（默认 `http://127.0.0.1:3000`，可被环境变量 `SUBSTORE_BACKEND` 注入，config.py:203）
- 依赖：标准库 urllib；外部服务 Sub-Store 后端
- 副作用：无（只发请求）
- 异常与失败行为：`NotFound`（404，以及 500 但正文含 `SUBSCRIPTION_NOT_FOUND` / `RESOURCE_NOT_FOUND` / `Cannot convert undefined or null to object` / `error.details==404`）、`StoreError`（403 含 "1010" 视为 Cloudflare UA 拦截、其他 4xx 立即抛、5xx/网络错误重试 `retries=2`、退避 0.8s×n）；这是把旧管线「静默空转」根因（Sub-Store 对不存在资源返回 500 而非 404）显式建模
- 数据格式：Sub-Store REST `/api/subs`、`/api/sub/<name>`、`/api/collections`、`/api/collection/<name>`、PATCH/POST/DELETE
- 调用链：engine.collect_entries / manual_fronts / link_substore / push_exports / server 路由 / ipmap.run / __main__ push

### F-02 订阅下载与 proxies 解析（collection/sub 双类型）
- 入口：`store.py:108 download_collection`、`store.py:112 download_sub`、`store.py:138 fetch_proxies`、`store.py:142 fetch_sub_proxies`、`store.py:146 fetch_source`
- 输入：资源名 + `target=ClashMeta`（默认）
- 输出：proxy dict 列表（`yaml.safe_load` 后取 `proxies`，仅保留 dict）
- 依赖：PyYAML；Sub-Store download 路由（远程订阅内容必须走 `/download/...` 而不是读记录的 `content`，见 store.py:115 注释）
- 异常与失败行为：YAML 非法 / 无 proxies 列表 → `StoreError`；engine.collect_entries 逐源捕获后记错误继续其它源（engine.py:528-530）
- 数据格式：ClashMeta YAML `proxies:` 数组
- 调用链：engine.collect_entries（每轮）、collect_fronts、server /api/substore-nodes、ipmap --source

### F-03 Sub-Store 资源清单（数据源选择器）
- 入口：`store.py:152 list_resources`
- 输出：`[{kind: collection|sub, name, members(组合订阅成员数), source_type}]` + errors 列表
- 调用链：server.py:368 `GET /api/substore-resources` → 前端「数据源」面板刷新按钮
- 异常：单个 kind 拉取失败进 errors，不中断另一 kind

### F-04 手动前置粘贴文本物化为 Sub-Store 本地订阅
- 入口：`engine.py:568 manual_fronts`、`engine.py:562 manual_front_sub_name`（名字 `<publish.prefix>-front-manual`）、清理 `engine.py:621 _drop_manual_front_sub`、`engine.py:640 drop_manual_front_sub`
- 输入：`chain.front_text`（分享链接每行一条，或 base64 订阅正文）
- 输出：解析出的前置 proxy 列表
- 配置：`chain.front_text`、`publish.prefix`；上限 `MAX_FRONT_TEXT=262144`（config.py:136）
- 副作用：**写回 Sub-Store**（upsert 本地订阅 `probe-front-manual`，payload `source:"local", content:<粘贴文本>`）；文本为空时 DELETE 该订阅；进程内 digest 缓存 `_MANUAL_FRONT_SYNCED` 避免重复写
- 设计理由：本服务不自带分享链接解析器，解析交给 Sub-Store（engine.py:571-577）
- 异常：upsert 失败记 error 并返回空池（前置全死 → 链式节点 front_dead）；/api/config 保存路径若清空了 front_text 也会触发清理（server.py:496-500）
- 调用链：collect_fronts → manual_fronts；server POST /api/config → drop_manual_front_sub

### F-05 ipmap 独立订阅拉取（UA 门禁兜底 + base64 + 分享链接边界）
- 入口：`ipmap.py:321 fetch_subscription`（clash UA `clash-verge/v1.7.7` 优先、浏览器 UA 兜底，ipmap.py:62-66）、`ipmap.py:334 _maybe_base64`、`ipmap.py:348 extract_proxies`
- 输入：`--url`（裸 Clash YAML 或其 base64）/ `--file` / `--source [–source-kind]`（走 F-02）
- 输出：proxies 列表
- 异常与失败行为：分享链接文本（vless/vmess/ss/trojan/hysteria2/hy2/tuic/ssr 开头）**明确报错**指向 Sub-Store（ipmap.py:364-368），不半解析；拉取两种 UA 都失败 → `ImapError`
- 调用链：ipmap.run（ipmap.py:431）

---

## 域 2 mihomo 内核生命周期

### F-06 内核配置生成 build_config
- 入口：`core.py:219 build_config`
- 输入：proxies（prepare 产物）、core_cfg、secret
- 输出：写 `<out_dir>/config.yaml`（默认 `core/config.yaml`；ipmap 传 `data/ipmap-core/`）
- 配置：`core.api`（端口取 `parse_port`，默认 19190）、`core.mixed_port`（**注意：不在 DEFAULTS 里**，core.py:236 直接下标读取；vps 靠已部署 config.json 里的 `core.mixed_port:19194` 才能跑，新装会 KeyError——移植陷阱）、`core.lanes`、`core.base_port`
- 数据格式（YAML schema 要点）：`mixed-port`、`allow-lan:true + bind-address:127.0.0.1`、`ipv6:true`、`unified-delay`、`tcp-concurrent`、`find-process-mode:off`、`external-controller:127.0.0.1:<port>`、`secret`、`profile.store-selected:false`、`dns`（fake-ip、nameserver 223.5.5.5/1.1.1.1）、`proxies`（每个 proxy 一行 inline JSON）、每车道一个 `__LANE<i>__` select 组 + 一个 `lane<i>` mixed listener（端口 19200+i）、规则 `IN-NAME,lane<i>,__LANE<i>__` + `MATCH,__LANE0__`（core.py:276-290）
- 调用链：make_testable（每轮、ipmap 各一次）
- DROP_FIELDS：`dialer-proxy`（仅当值在 keep_dialer 中才保留）、`interface-name`、`routing-mark`（core.py:25）

### F-07 mihomo -t 配置校验 + docker CLI 探测（优雅降级）
- 入口：`core.py:360 config_test`、`core.py:331 docker_cli`（60s TTL 缓存，`docker version` 探测）
- 输入：生成的 config.yaml；`host_dir`（必须传**宿主机路径**，因为 `docker run -v` 由宿主 daemon 解析，core.py:362-364）
- 输出：`(ok, error_text)`
- 依赖：**docker daemon**（挂载的 /var/run/docker.sock）、镜像 `metacubex/mihomo:latest`
- 副作用：临时起一个 `docker run --rm` 容器做校验
- 异常与失败行为：docker 不可用 → 返回 `(True, "docker unavailable; config validation skipped")`（跳过而不是失败轮次，README「优雅降级」）；`subprocess.SubprocessError`（含 hung daemon 的 TimeoutExpired）→ 捕获记失败（core.py:351-356 修复过逃逸到 run_round 的 bug）
- 调用链：make_testable（每轮最多 6 次）

### F-08 make_testable 剪枝重试
- 入口：`core.py:387 make_testable`、culprit 定位 `core.py:436 _culprit_from`
- 输入：entries（含 fp/orig_proxy/role/front/category 透传字段）、`strip_ech`、`keep_dialer`
- 输出：`(proxies, mapping, dropped)`；mapping 每项含 source/original/mihomo/index/orig_proxy/test_ip/fp/proto/server/role/front/category（core.py:187-215）
- 失败行为：`prepare` 先剔除缺 name/type/server/port 或 port 非法的条目；`-t` 失败时按 culprit（引号名 → 有界词名 ≥4 字符 → server 地址，保守匹配）定位并按 **entry index** 整组剔除，最多 `max_prune=5` 次；无 culprit → `CoreError`（整轮 CoreError 由 run_round 捕获记 aborted）
- 重命名：同名节点 `Name #2` 去重（mihomo 按 name 键会静默合并）
- 调用链：_run_round、ipmap.run

### F-09 内核容器启停/重载
- 入口：`core.py:467 class Core`；`up`(core.py:491 docker restart/start)、`logs`(509)、`reload`(513 `PUT /configs?force=true` 失败则 restart)、`wait_ready`(524 轮询 `/version` 45s)、`start_and_load`(536 三态 started/reloaded/restarted)、`_alive`(556)
- 依赖：docker daemon（restart/inspect/start/logs）、mihomo REST
- 异常与失败行为：`CoreError("mihomo did not become ready")` → run_round 记 aborted + round_failed 告警
- 调用链：_run_round（每轮加载新配置）

### F-10 单节点延迟测量与失败归类
- 入口：`core.py:563 Core.delay`（`GET /proxies/<name>/delay?timeout&url&expected`，HTTP 超时 = timeout_ms/1000+8）；归类 `core.py:102 _reason_from`
- 输出：`(delay_ms, reason, detail)`；reason ∈ `timeout(504)/kernel_error(503)/bad_request(400)/unreachable/bad_response/bad_delay/controller_error/http_<status>`
- 关键语义：`controller_error` = 内核 API 本身不可达，**不属于节点失败**、故意不在 TERMINAL_REASONS（core.py:576-583），避免控制器抖动推进所有节点的失败 streak；保留响应体是本轮设计初衷（README「为什么是这样设计的」第 2 条）
- 调用链：engine.test_one

### F-11 select 组切换 + 车道出口请求
- 入口：`core.py:596 Core.select`（`PUT /proxies/<group>` body `{name}`）、`core.py:602 Core.egress`（经 `127.0.0.1:<lane port>` 的 HTTP 代理拉 trace URL，解析 `key=value` 行）
- 输出：`fields` dict（`ip/loc/colo` 等）
- 异常：select 失败抛 CoreError（车道记录 error）；egress 网络异常返回 `(None, error)`
- 调用链：_verify_egress（每车道）、ipmap.probe_one

---

## 域 3 测活引擎

### F-12 一轮编排 run_round / _run_round
- 入口：`engine.py:912 run_round`、`engine.py:1030 _run_round`
- 输入：cfg、trigger（schedule/manual/cli）、`only_source`（CLI --source / API body.source）、`mode`（None/direct/chain）
- 输出：summary dict `{round_id,total,alive,dropped,restored,new_alive,suspect,note,duration_s,...}`
- 流程（阶段顺序 = checkpoint 点）：fetch → （链式前置收集）→ classify_and_expand → make_testable → build/start 内核 → delay-test（两阶段）→ egress 验证 → publish（_apply_and_publish）→ _reconcile_ledger → cleanup_exports → finish_round → _maybe_alert → trim_results
- 副作用：写 SQLite（rounds/results/nodes/events/ip_geo/domain_views）、写 `data/exports/<key>.yaml(+meta.json)`、写 `data/round.state.json`（结束删除）、可选写回 Sub-Store、发告警
- 异常：`Busy`（已有轮次）；`RoundTimeout` → `_abandon_round` + round_timeout 告警；其它异常 → `_abandon_round` + round_failed 告警后 re-raise；告警/trim 失败不影响轮次结论（engine.py:1263-1270）
- 调用链：server.run_in_background（线程）、`python -m mihomo_test round`

### F-13 来源条目化 collect_entries
- 入口：`engine.py:511 collect_entries`；自引用过滤 `engine.py:261 _drop_self_references`（配置侧 `config.py:489 reject_self_reference` 在保存时拒绝）
- 输入：启用的 sources（`kind: collection|sub`，key 唯一）
- 输出：entries `[{source,name,proxy,index,fp,category}]` + errors
- 关键：fp 在这里由 `_orig_fp`（engine.py:465，**唯一**的账本身份派生点，剔除 DROP_FIELDS 后哈希原始域形式 proxy）算出；category 由 `classify_category`（engine.py:492：relay > chain(有 dialer-proxy，兼容 `dialer_proxy` 拼写) > direct）就地判定
- 配置：`sources[]`（key/kind/name/label/enabled/export/relay/direct/chain，`normalize_sources` config.py:62 白名单重建）

### F-14 双视角 DoH 解析 + 逐地址展开 classify_and_expand
- 入口：`engine.py:355 classify_and_expand`、`engine.py:435 _resolve_candidates`
- 输入：entries + fronts；`dns.views`（cn: doh.pub + ECS 114.114.114.0/24；overseas: cloudflare-dns.com + ECS 8.8.8.8/24，config.py:242-247）、`dns.cache_hours`（默认 6h）、`dns.timeout_s`
- 行为：字面量 IP 直接是候选；域名先查 `domain_views` 缓存（过期/未来时间戳不信任，db.py:399-422），未命中则双视角 A+AAAA 解析并**只在至少一个视角有答案时**写缓存；每个（域名的每个）可测地址生成独立 test entry（`proxy.server=ip`，`orig_proxy` 保留域形式，fp 用原始 fp）；`no_expand`（前置池）整组透传；两视角全失败 → 让内核自己解析
- 异常：解析异常 warn 后 views={}（当轮不过滤）

### F-15 入口 IP 归属查询与受限 ISP 过滤（entry_cn）
- 入口：`engine.py:290 lookup_countries`（批量，SQLite `ip_geo` 缓存，一个 IP 只查一次）、`engine.py:331 _fetch_country_batch`（`http://ip-api.com/batch?fields=query,countryCode,isp`，90 IP/批，1 次重试，pause 2s）
- 输出：candidates 中落在 `verify.exclude_entry_countries`（默认 `["CN"]`，`verify.entry_check` 开关默认开）的 IP 被剔除；某域全部地址受限 → 整组进 `excluded`
- 异常与失败行为：ip-api 查询失败 → **本轮放弃入口过滤**（返回已缓存部分，宁可多测不误杀），并 warn 未定性地址数
- 副作用：写 `ip_geo` 表；excluded 节点由 F-30 登记为 `status=excluded, last_reason=entry_cn, consec_fail=0`
- 调用链：_run_round → classify_and_expand 内部

### F-16 逐节点非对称重试 test_one
- 入口：`engine.py:213 test_one`
- 输入：`test.targets`（默认 hicloud/gstatic/cloudflare 三个 generate_204）、`test.expected_status`("204")、`test.max_attempts`(3)、`test.timeout_ms`(5000)、`test.timeout_ms_retry`(9000)、`test.retry_pause_s`(0.3)
- 规则：成功立即返回；`timeout` → 升超时到 retry 值再试；`kernel_error` → 换目标再试；TERMINAL_REASONS（`bad_request/bad_delay/bad_response/unreachable`，engine.py:119）→ 立即停；`controller_error` 可重试；每 attempt 前检查 deadline（RoundTimeout）
- targets 为空抛 ValueError（config 校验层也拒绝空 targets，config.py:603-606）

### F-17 并发执行 _test_all
- 入口：`engine.py:1388 _test_all`（ThreadPoolExecutor，`test.concurrency` 默认 20，上限由 validate_patch 夹取 1-200）
- 输出：`{mihomo名: outcome{delay_ms,reason,detail,attempts,url}}`
- 异常：pool.map 传播第一个异常；其余任务因 deadline 检查快速返回（等待有界）

### F-18 指纹身份体系
- 入口：`core.py:38 fingerprint_proxy`（sha256(连接参数 JSON, 排序, 剔除 `name` 与 `_` 前缀)[:16]）、`core.py:57 variant_fingerprint`（sha256(f"{fp}#{variant}")[:16]）、`engine.py:465 _orig_fp`
- 数据格式：账本断言指纹是**恰好 16 位十六进制**（test_live 以长度识别旧「名字做键」残留）
- 关键约束：`fingerprint_proxy` 忽略 `dialer-proxy` → 链式/直连双测时直连孪生必须用 variant_fingerprint 派生身份，否则两测量互相覆盖（engine.py:859-886 长注释）；逐地址变体全部携带**原始域形式**的 fp，防止 DNS 抖动重置 consec_fail（engine.py:467-489 注释记录的事故）

### F-19 域名聚合记分 _score_bucket
- 入口：`engine.py:1585 _score_bucket`
- 规则：`verify.domain_pass`（`any` 默认 / `all`）；ok=任一地址活（all 模式要求全部活且 ip_total>1）；失败原因取**第一个真正失败的地址**（不是 outcomes[0]，修过 last_reason=None bug）；返回 `(ok,delay=min,detail,ip_alive,ip_total)`

### F-20 ECH 剥离 strip_ech
- 入口：`core.py:148-149`（prepare 中 pop `ech-opts`）；开关 `verify.strip_ech` 默认 True（config.py:260）
- 理由：mihomo ECH 实现对 CF 前置不稳定（实测 4 轮 1 次 404、延迟翻倍）；代价是只证明「非 ECH 路径」

### F-21 出口国别过滤 _resolve_exit
- 入口：`engine.py:1557 _resolve_exit`、排除集 `engine.py:1946 verify_cfg_excludes`
- 规则：出口验证全失败 → `verify_failed`（不算 alive）；出口国别在 `verify.exclude_countries`（默认 `["CN"]`）→ reason=`exit_<CC>` 且本轮不活
- 配置：`verify.enabled`、`verify.trace_url`（默认 cloudflare /cdn-cgi/trace）、`verify.timeout_s`(15)、`verify.max_nodes`(0)

### F-22 导出代理构建 _export_proxies
- 入口：`engine.py:2024 _export_proxies`、标签幂等正则 `_LEADING_TAG`（engine.py:106，吃掉任意 `[XX]`/旗emoji 前缀再重打，防止自供集合轮次叠加标签）、YAML 1.1 保留字串引号 `_YAML_11_SCALAR`/`_ExportDumper`（engine.py:2080-2107，修 REALITY short-id 被 Go yaml.v3 读成浮点的实测 bug）
- 规则：同 display 名取**带 dialer 的（链式）变体**优先；再按 fingerprint 去重（同服务器两行列一遍）；`publish.add_region_tag`（默认 True）→ 名字前缀 `[实测国别]`
- 输出：`(proxies, groups)`

### F-23 导出文件写入与清理
- 入口：`engine.py:2110 _write_export`（tmp+rename 原子写 `<key>.yaml` + `<key>.meta.json` `{count,updated_at}`）、`engine.py:2316 cleanup_exports`（删除已停用/静音来源的旧导出）
- 副作用：写 `data/exports/`
- 数据格式：ClashMeta YAML `proxies:`（+`proxy-groups:` 当有 derived_dialer_groups）
- 调用链：_publish_sources（engine.py:1832，护栏通过时逐 key 写，`publish.push_to_substore` 开时接 F-63）

---

## 域 4 出口验证并行车道

### F-24 车道拓扑常量与端口
- 入口：`core.py:295 LANE_PREFIX("__LANE")`、`lane_group/lane_name/lane_count`(1..32,默认8)/`lane_ports`（base_port 19200 起）
- 配置：`core.lanes`、`core.base_port`；容器内 inbound `mixed-port`（README：HTTP 入站 19194）
- 端口约定：**故意不用 19090**（旧 mihomo-health 栈占用）；全部绑 loopback（config.py:206-209）

### F-25 出口验证执行 _verify_egress
- 入口：`engine.py:1473 _verify_egress`
- 输入：survivors（延迟测试存活者）、verify_cfg、deadline；每车道 select 组 `__LANE<i>__` + loopback 入站 + `IN-NAME` 钉扎（由 F-06 建好）
- 流程：survivors 按 `i::lanes` 分桶 → 每车道线程 `select(group, name)` → `core.egress(port, trace_url)` 读**真实出口 IP + loc + colo**；进度每 25 个打点
- 输出：`{name: {country, ip, colo, error}}`、`unverified`（deadline 前没测到的）、`over_limit`（被 `verify.max_nodes` 截断的）
- 异常与失败行为：车道内超时即收手不跑满预算；unverified 集合交 F-32 决定是否拒发布
- 独立性证据：test_live LaneIndependenceTest「交换两条车道选中节点→出口 IP 跟着交换」

---

## 域 5 收敛账本与策略

### F-26 状态机 policy.apply
- 入口：`policy.py:21 apply`
- 状态：`alive/pending/dead/unknown/excluded`（policy.py:11-18）；通过一次立即 alive（restore 仅限原 dead、新活记 `new`）；失败 streak+1，达 `policy.drop_after_consecutive_fails`(3) → dead；未达阈值保持 pending/unknown
- 配置：`policy.drop_after_consecutive_fails`(1-20)
- 调用链：_converge_bucket、_record_rejected_nodes、_record_chain_failures（非拨号失败同样推进 streak）

### F-27 整轮护栏 round_is_suspect
- 入口：`policy.py:79 round_is_suspect`；上轮基数 `engine.py:1950 _previous_alive_count`（跳过 suspect 轮取上一正常轮的 ok）
- 规则：alive < max(`policy.suspect_floor_absolute`(3), 上轮存活×`policy.suspect_floor_ratio`(0.5)) → 本轮可疑：**不发布、保留上一轮导出**，rounds.suspect=1，告警 suspect_round
- 调用链：_apply_and_publish（发布门）

### F-28 别名聚合 group_by_fingerprint
- 入口：`engine.py:1538 group_by_fingerprint`
- 规则：按 `(source, fp)` 折叠同端点全部别名/变体（重名双列、每地址变体、每前置变体），避免一轮内同端点计两次失败

### F-29 单端点收敛 _converge_bucket
- 入口：`engine.py:1610 _converge_bucket`
- 行为：score → 出口语义修正（F-21）→ policy.apply → upsert nodes（含 country/ip_alive/ip_total/proto/server/category 每轮重打）→ record_result（results 行，attempts 取最大）→ 返回 (transition, alive primary names)

### F-30 非拨号记账三件套
- 入口：`engine.py:1657 _record_excluded_nodes`（入口 CN，consec_fail 强制 0）、`engine.py:1695 _record_rejected_nodes`（内核拒绝，reason=`kernel_rejected`，真实 fail 走正常收敛）、`engine.py:1750 _record_chain_failures`（前置全死，reason=`front_dead`，category 强制 chain）
- 共同点：都返回 `(by_source, fps)` 供 F-31 视为「本轮已见」，否则会被 complement-prune 删行、刚推进的 streak 被清

### F-31 账本修剪与来源对账
- 入口：`engine.py:1795 _prune_removed_nodes`（按本轮 seen 白名单 `db.delete_nodes_not_in`）、`engine.py:998 _reconcile_ledger`（`db.delete_sources_not_in` 清掉已取消勾选来源的节点记录；`db.demote_disabled_sources` 把停用来源的行降级 unknown 但保留历史与计数；前置池 `__front__` 在 chain 配置存在时保留）
- 副作用：SQLite nodes 删/降级

### F-32 未验证不发布 + max_nodes 截断
- 入口：`engine.py:1910-1932`（_apply_and_publish 内）
- 规则：有 unverified 且配置了出口国别过滤 → 本轮 suspect 不发布（否则可能把用户流量发到被拒国别）；未配置过滤 → 按未验证发布；over_limit 记 warn

### F-33 分类统计 stats_by_category
- 入口：`db.py:598 stats_by_category`、桶 `db.py:595 CATEGORY_BUCKETS=(direct,relay,chain)`
- 输出：每桶 `nodes{}`（账本口径）+ `tested{total,ok,fail,skipped}`（行口径，excluded 单列）+ `delay_ms{median,avg,min,max}`（仅 alive 行）+ `reasons`/`top_countries`（各 6）
- 调用链：`GET /api/stats?round_id=`（前端分类统计卡）

### F-34 UTC 时间戳约定
- 入口：`db.py:113 now`（`time.gmtime()`，秒精度无后缀）、`db.py:129 to_epoch`、`policy.py:71 _stamp`、`server.py:566 _parse`、`engine.py:1275 _stored_epoch`、`_write_state.ts`（engine.py:57）
- 约定：所有落库时间列为 UTC（容器 TZ=Asia/Shanghai vs 宿主 UTC 曾经差 8 小时，README「时间戳约定」）；`TimestampTest`（test_logic.py:4389）锁住

---

## 域 6 ipmap 节点↔落地IP 映射

### F-35 ipmap CLI 入口与参数
- 入口：`__main__.py:61-67`（命令 `ipmap`）、编排 `ipmap.py:431 run`
- 参数：`--url/--file/--source(--source-kind)` 三选一、`--family all|v4|v6`、`--limit`、`--lanes`(4)、`--timeout-s`(12)、`--api-port`(19191)、`--base-port`(19300)、`--mixed-port`(19494)、`--key/--out`、`--push-sub`、`--keep-core`
- 依赖：docker daemon + mihomo 镜像；与测活轮次完全隔离（独立容器 `mihomo-ipmap`、独立内核目录 `data/ipmap-core/`——刻意放 data/ 下使 bind-mount 路径在容器内外都成立）

### F-36 族过滤与回显解析
- 入口：`ipmap.py:83 server_family`（按 server 字面量判 v4/v6/domain）、`ipmap.py:92 filter_family`（域名节点只在 all 保留）、`ipmap.py:103 parse_echo_ip`（正文首行必须解析为 IP 且族匹配，`family=None` 接受任意）、`ipmap.py:251 _norm_ip`（compressed+lower，防 v6 前导零误判中转）
- 回显端点：`ipmap.py:71 ECHO_TARGETS`（v4: api.ipify.org / ipv4.icanhazip.com；v6: api6.ipify.org / ipv6.icanhazip.com，明文 HTTP，组内按序取第一个成功）

### F-37 并发探测 probe_all / probe_one
- 入口：`ipmap.py:182 probe_all`（车道分桶同 F-25 `i::lanes`）、`ipmap.py:147 probe_one`（select → 两族回显 → trace 交叉印证 loc/colo/trace_ip）、`ipmap.py:124 fetch_via`（经车道入站拉正文）
- 复用：F-08 make_testable（同一套配置生成/校验/剪枝）、F-24 车道机制

### F-38 标注与写回 Sub-Store
- 入口：`ipmap.py:217 annotate_name`（`原名 · [国别] 出口v6 出口v4`，失败附原因）、`ipmap.py:234 annotated_proxies`（用 original 名，不动原对象）、`ipmap.py:411 push_to_substore`（upsert 本地订阅 `--push-sub`）
- 副作用：**写回 Sub-Store** 本地订阅

### F-39 报告渲染与落盘
- 入口：`ipmap.py:282 render_report`（Markdown 映射表，备注列 `landing_note` 判「直落/疑似中转/入口是域名/实测失败」）、`ipmap.py:307 summarize`
- 输出：`data/ipmap/<key>.json`（全字段）+ `<key>.md` + 控制台打印；留档副本见 `reports/ipmap-demo-v6.{json,md}`

### F-40 ipmap 一次性内核容器
- 入口：`ipmap.py:372 start_core`（`docker run -d --network host --cap-add NET_ADMIN,NET_RAW --memory 384m`，先 `rm -f` 清同名残留）、`ipmap.py:403 stop_core`（默认跑完即删，`--keep-core` 保留排查）
- 失败行为：启动失败/未就绪 → ImapError；探测异常 finally 里删容器

---

## 域 7 通知告警

### F-41 Telegram / Webhook 通道
- 入口：`notifier.py:49 telegram_send`（api.telegram.org sendMessage）、`notifier.py:70 webhook_send`（POST JSON `{key,level,title,body,source,ts}`）
- 配置：`alert.telegram.{enabled,token,chat_id}`、`alert.webhook.{enabled,url}`、总开关 `alert.enabled`
- 依赖：外部服务 Telegram Bot API / 任意 webhook 端点
- 异常：返回 (ok, detail)，不抛

### F-42 告警聚合发送与冷却
- 入口：`notifier.py:87 send`
- 冷却：`alert.cooldown_minutes`(240,1-10080)；状态存 `data/alert-state.json`（原子写）；**冷却只在至少一个通道投递成功后记录**（投递失败不消耗窗口，notifier.py:92-98）；全部失败按 error 级记事件
- 时间戳：UTC 明示（notifier.py:118-121）
- 调用链：engine._maybe_alert / _abandon_round / _run_round(no_nodes/front_dead) / server /api/alert-test

### F-43 测试告警（金丝雀）与冷却重置
- 入口：`notifier.py:159 test_channels`、`notifier.py:180 reset_cooldown`（仅测试引用）；面板按钮 → `POST /api/alert-test`（server.py:471）
- 配置凭据来源：`tools/wire_telegram.py` 从 acme.sh/内部告警脚本 提取写入（部署侧一次性动作）

### F-44 告警触发点汇总
- `round_failed`：run_round 兜底异常（engine.py:934-939）
- `round_timeout`：RoundTimeout（engine.py:929-933）
- `suspect_round`：_maybe_alert（engine.py:1374）
- `alive_low`：存活 < `alert.alive_floor`（0=关闭，engine.py:1378-1385）
- `no_nodes`：全部来源拉取失败（engine.py:1140-1143）
- `front_dead`：前置全死链式停摆（engine.py:1210-1216）

---

## 域 8 DoH 模块

### F-45 DNS 线格式构建与解析
- 入口：`doh.py:46 build_query`、`doh.py:37 ecs_option`（EDNS Client Subnet，wire 格式——JSON DoH API 不支持 ECS 所以手写报文）、`doh.py:25 _encode_name`（IDNA）、`doh.py:61 _skip_name`、`doh.py:73 parse_ips`（A/AAAA，保序）
- 依赖：纯标准库（struct/base64/ipaddress/os.urandom）

### F-46 DoH 查询与双视角解析
- 入口：`doh.py:102 query`（GET `resolver?dns=<base64url>`，`Accept: application/dns-message`）、`doh.py:114 resolve_views`（每视角 A+AAAA，失败视角返回空列表，去重保序）
- 配置：`dns.views.{cn,overseas}.{resolver,ecs,ecs_prefix}`、`dns.timeout_s`
- 外部服务：doh.pub、cloudflare-dns.com
- 失败行为：单视角失败不抛（engine._resolve_candidates 层捕获），两视角全空 → 内核自行解析

### F-47 域名视图缓存
- 入口：`db.py:399 domain_views_get`（年龄按 UTC 算；未来时间戳=不可信立即失效，防「一次性解析永久排除节点」）、`db.py:425 domain_views_put`
- 表：`domain_views(domain PK, views JSON, checked_at)`
- 全空结果不缓存（engine.py:454-455）

---

## 域 9 数据库 / 存储层

### F-48 连接管理与 WAL
- 入口：`db.py:137 connect`（进程级单连接 + RLock，`check_same_thread=False`、`timeout=30`、WAL、busy_timeout 30000、synchronous NORMAL；WAL 失败静默回退）
- 文件：`data/state.db`（路径 `db.py:20`）

### F-49 Schema 与迁移
- 入口：`db.py:24 SCHEMA`、`db.py:183 _migrate`（`_add_round_mode` db.py:236、`_add_category_columns` db.py:208、指纹化重建前先 `_backup` db.py:168 复制 state.db→`.bak-<stamp>`）
- 表结构：
  - `nodes(source, fingerprint, display, proto, server, first_seen, last_seen, last_ok, last_delay_ms, country, status, consec_fail, total_ok, total_fail, last_reason, ip_alive, ip_total, category, PK(source,fingerprint))`
  - `rounds(id, started_at, finished_at, trigger, total, ok, failed, dropped, restored, suspect, note, duration_s, mode)`
  - `results(round_id, source, fingerprint, display, verdict, delay_ms, reason, country, attempts, detail, category)` + 4 个索引（含 `idx_results_source_round`、`idx_results_round_category`，为 /api/nodes 5 秒轮询与分类统计建）
  - `ip_geo(ip PK, country, isp, checked_at)`
  - `domain_views(domain PK, views, checked_at)`
  - `events(id, ts, level, message)`（保留 2000 条，db.log 内裁剪）

### F-50 节点账本读写
- 入口：`db.py:309 get_node`、`db.py:313 upsert_node`（**SQL 拼接列名**，仅内部调用方使用）、`db.py:334 delete_nodes_not_in`（分块 400，SQL_VAR_CHUNK db.py:364）、`db.py:432 delete_sources_not_in`、`db.py:446 demote_disabled_sources`、`db.py:472 list_nodes`

### F-51 轮次/结果/事件表与修剪
- 入口：`db.py:279 start_round`、`db.py:287 finish_round`、`db.py:293 record_result`、`db.py:541 trim_results`（KEEP_RESULT_ROUNDS=500 轮，results 是唯一会膨胀的表）、`db.py:271 log`（events 上限 2000）、`db.py:556 open_rounds`、`db.py:562-570 last_rounds/last_round/recent_events`

### F-52 仪表盘查询面
- 入口：`db.py:574 stats`（总数/alive/dead/pending/unknown/excluded）、`db.py:598 stats_by_category`（F-33）、`db.py:494 recent_trends`（**疑似死代码**，见附 C）、`db.py:509 recent_trends_all`（窗口函数单遍，/api/nodes 用）、`db.py:372 ip_geo_get` / `db.py:388 ip_geo_put`

---

## 域 10 HTTP server 与 web UI

### F-53 双 token 鉴权模型
- 入口：`server.py:117 auth_ok`、`server.py:93 _matches`（hmac.compare_digest、UTF-8 字节比较）、`server.py:108 _presented_tokens`（`?token=` / `X-Auth-Token` / `Bearer` 三传法）
- 模型：`auth.token` 管理（面板+/api/*+改配置）；`publish.token` 只读（**仅** `/api/export/*` 接受，auth_ok path 前缀判断）；`/healthz`、`/api/health` 免鉴权；**空/过短 token 一律拒绝**（MIN_TOKEN_LEN=16，config.py:538；`_ensure_tokens` config.py:414 每次 load 保证有 token，防 config.json 损坏→裸奔）
- token 生成：首次 load `secrets.token_hex(16)`，存 `data/config.json`
- 打码：`server.py:172 redacted_config`（/api/status 里两个 token 都是 `***`）
- 不可远程修改：`IMMUTABLE_PATHS=("core.container",)`（config.py:366，防 token 泄露→停任意宿主容器）

### F-54 安全头与 CSP
- 入口：`server.py:43 SECURITY_HEADERS`（nosniff/DENY/no-referrer/CSP：`default-src 'none'; script-src 'self'（无 unsafe-inline）; style-src 'self' 'unsafe-inline'（有意保留，行内 style 属性不可执行）; connect-src 'self'; frame-ancestors 'none'…`）
- 每个响应（含 OPTIONS）都带；`Vary: Origin` 防共享缓存串响应（server.py:265）

### F-55 CORS
- 入口：`server.py:62 cors_origin`（精确 origin 白名单回显，不做通配）、预检 `do_OPTIONS`（server.py:288，**免鉴权**、Max-Age 600、允许头 `X-Auth-Token, Content-Type, Authorization`）
- 配置：`server.cors_origins`（默认 `[]`；CDN 前端必须显式填）
- 无 Allow-Credentials（token 走头不走 cookie）

### F-56 静态资源白名单与面板渲染
- 入口：`ui.py:38 ASSET_TYPES`（**只有** app.css/app.js/theme.js 三个，免鉴权、`Cache-Control: no-cache`）、`ui.py:64 bootstrap_json`（`<`→`\u003c`、U+2028/2029 转义，`<script type="application/json">` 数据块不受 script-src 约束）、`ui.py:90 render`（同源模式把 token 渲进 bootstrap；CDN 模式 token=null）
- 前端文件：`mihomo_test/web/{index.html,app.css,app.js,theme.js}`；theme.js 同步预置 data-theme 防闪

### F-57 GET 路由集
- 入口：`server.py:310 do_GET`；路由表：
  - `/healthz`、`/api/health`（免鉴权 200 {ok:true}）
  - `/app.css`、`/app.js`、`/theme.js`（免鉴权静态）
  - `/`、`/ui`（面板 HTML，需 token）
  - `/api/status`（stats+last_round+busy+busy_mode+next_run+exports+redacted config）
  - `/api/stats?round_id=`（分类统计；非法 round_id 回落最新）
  - `/api/nodes`（全部节点+trend，单遍 recent_trends_all）
  - `/api/logs`（200 条事件）、`/api/rounds`（30 条轮次）
  - `/api/substore-resources`（F-03；StoreError→502）
  - `/api/substore-nodes?kind&name`（前置池选择器节点名列表）
  - `/api/export/<key>.yaml`（F-62；404 返回 YAML 注释体）
  - 兜底 404 / 异常 500（JSON）

### F-58 POST 路由集
- 入口：`server.py:410 do_POST`；路由表：
  - `/api/run`（body `{mode:direct|chain 白名单, source, trigger}`；mode=chain 但 chain_block 为 None → 400 并说明修法（server.py:429-442）；Busy→409；后台线程跑）
  - `/api/push`（publish.enabled=false → 400 拒绝推旧快照，server.py:454-463；调 push_exports+push_summary）
  - `/api/alert-test`（金丝雀）
  - `/api/link`（手动同步联动 link_substore）
  - `/api/config`（validate_patch→update；返回 notes；副作用链见 F-60）
  - `/api/reload`（重读 config.json）
- 后台执行：`server.py:213 run_in_background`（server.BUSY 锁 + 线程；Busy 只记 info「已有一轮在运行」）

### F-59 配置补丁校验 validate_patch / update
- 入口：`config.py:558 validate_patch`、`config.py:628 update`、`config.py:386 prune_dead`、`DEAD_KEYS=("core.probe_group","core.config_path")`（config.py:354，**故意不做 DEFAULTS 白名单**——部署特有键如 core.mixed_port 合法）、`NUMERIC_BOUNDS`（config.py:515，11 个数值键夹取）、token 长度、`test.targets` 非空、`chain.front_text` 超长拒绝
- 保存：`config.py:481 save`（tmp+rename 原子写 data/config.json）；来源字段白名单重建 `normalize_sources`（config.py:62）；链块 `normalize_chain`（config.py:141）；自引用拒绝 `reject_self_reference`（config.py:489）

### F-60 配置保存副作用链（/api/config 内）
- 入口：`server.py:477-521`
- 行为：写库事件、清空的手动前置订阅删除（server.py:496-500）、`sources` 变更且 `publish.enabled` 时若 `engine.link_signature`（engine.py:2359，只投影联动相关字段）有变化才重跑 link_substore（纯测量开关的保存不碰 Sub-Store）
- 配置键全集见附录 B「配置项清单」级 README「设置」

### F-61 前端面板（app.js 单页）
- 入口：`mihomo_test/web/app.js`（~1400 行）+ index.html
- 功能块（app.js 区段注释）：
  - 引导与令牌（bootstrap 读取、URL ?token=/#token= → localStorage、`history.replaceState` 抹 URL、401 令牌闸门 gate）
  - 主题（跟随系统/浅/深，localStorage `mihomo-theme`）
  - 状态卡（总数/存活/观察/死/本轮/耗时、进度条、busy_mode 显示「直连测活/链式测活」）
  - 手动测活两按钮（runMode → /api/run mode=direct/chain）
  - 订阅输出（exports_summary 渲染可复制 URL）
  - 数据源面板（刷新/搜索/类型筛选/只看已启用/勾选 enabled/export/relay/direct/chain、key 编辑与校验 badKey、手动添加、批量列开关 bulkSet、key 自动 -2 后缀 nextKey/slug）
  - 节点表（源/名称/协议/实测国别/延迟/连续失败/状态/近 12 轮趋势/最近原因）
  - 分类统计（renderCategories，三卡 直连/中转/链式）
  - 设置表单（间隔/并发/超时/尝试/判死阈值/护栏/测试目标/出口验证/排除国家/链式块/前置来源/前置池上限/手动前置粘贴/front_pick 选择器/告警/CORS/数据源 JSON）→ /api/config
  - 日志（200 条）
  - 推送按钮（/api/push）、同步联动（/api/link）、发送测试告警（/api/alert-test）

---

## 域 11 Sub-Store 联动

### F-62 导出端点 /api/export/<key>.yaml
- 入口：server.py:399（GET）；读取 `engine.py:2542 read_export`、`engine.py:2557 export_meta`（key 先过 `validate_key`（config.py:47，路径穿越防线）且强制父目录=EXPORT_DIR）；URL 构造 `engine.py:2296 export_url`（key quote）；凭据 `engine.py:2285 export_token`（publish.token 优先，兼容 auth.token）
- 鉴权：publish.token 或 auth.token 任一
- 消费方：Sub-Store 远程订阅 / 任意客户端

### F-63 推送模式 push_exports（方式二，可选）
- 入口：`engine.py:2166 push_exports`（每 key 读导出文件 → upsert 本地订阅 `<prefix>-<key>-local`，payload `source:"local", content:<YAML>`；记录 `{key,name,ok,level,text,count}`）；过期清理 `engine.py:2220 _prune_local_subs`（只删 `source=="local"` 且带我们 prefix 的，keep=本次 keys ∪ publish_keys（engine.py:2131，enabled+export））；`engine.py:2268 push_report` / `engine.py:2273 push_summary`
- 入口点：`POST /api/push`（publish.enabled=false 直接 400）、`python -m mihomo_test push`（__main__.py:79-87）、轮内 `publish.push_to_substore=true` 时 _publish_sources 自动推
- 副作用：写回 Sub-Store；异常逐 key 记录不中断

### F-64 拉取联动 link_substore（方式一，默认推荐）
- 入口：`engine.py:2394 link_substore`
- 行为：对每个 enabled+export 且导出 count>0 的来源，upsert 远程订阅 `probe-<key>`（`source:"remote", url:https://<publish.hostname>/api/export/<key>.yaml?token=<publish.token>`, process=[QUICK_SETTING]）+ 维护聚合集合 `probe`（`_collection_payload` engine.py:2345）；**零存活来源跳过联动**（Sub-Store 对零节点订阅一律 500，engine.py:2440-2444）；按「应存在集 expected」清理 retired 远程订阅（键在 expected 不在 members，防 5xx 误删健康对象）；集合成员对齐实际存在对象；全部写失败时保留现有集合
- 触发：`POST /api/link`、/api/config 保存联动字段变化、CLI link_substore.py
- 配置：`publish.prefix`(probe)、`publish.hostname`（默认 probe.example.com，环境变量 MIHOMO_TEST_HOSTNAME）

### F-65 CLI link_substore.py
- 入口：`link_substore.py:22 main`；`--remove` 删除各来源 `probe-<key>` 远程订阅与聚合集合（走 client._request DELETE）；否则调 engine.link_substore 并打印客户端引用 `/download/collection/<prefix>?target=ClashMeta`
- 前置：能读到 data/config.json、Sub-Store 后端可达

---

## 域 12 链式代理（前置 → 落地）

### F-66 链式配置块与生效判定 chain_block
- 入口：`engine.py:541 chain_block`
- 规则：`chain.enabled` 为 True 且（`chain.front_source.name` 非空 **或** `chain.front_text` 非空）才生效；半配置返回 None（此时链式节点按直连测并 warn，而不是全判 front_dead）
- 配置：`chain.{enabled,front_source{kind,name},front_pick[],front_text,max_fronts(8,1-64)}`

### F-67 前置池收集 collect_fronts
- 入口：`engine.py:651 collect_fronts`
- 三输入按序：front_text（手动，走 F-04）+ front_source 资源（`front_pick` 名单收窄，缺员报告）→ `max_fronts` 截断（超出 warn）
- 内核保留名：`__FRONT<i>__`（FRONT_NAME_PREFIX engine.py:140）防与用户节点重名；`no_expand:True`（前置不按地址展开，engine.py:355 docstring 理由）；`category=CAT_RELAY`
- 失败：池空 → error 日志；有 dialer 节点而池空 → 本轮全部 front_dead（不拨号）

### F-68 链式展开 expand_chains
- 入口：`engine.py:774 expand_chains`、测量开关 `engine.py:751 _measure_flags`（双关掉回落直连，防账本被清理）
- 规则：带 `dialer-proxy` 的节点 → 每个前置一条链式变体（fp 不变，账本折一行，category=chain）；`direct` 开 → 额外剥 dialer 直连变体，**双测时**直连变体用 `variant_fingerprint(fp,"direct")` 派生身份（F-18）；relay 来源节点不再套 dialer（它本身就是前置）；链式轮池空 + `fail_without_front` → `role=chain,front=None` 占位条目交 F-69 判 front_dead

### F-69 两阶段测试 _test_phases
- 入口：`engine.py:1409 _test_phases`
- 规则：第一遍测全部非 chain-role（含前置）→ 活前置集合 `live` → 第二遍只测 `front ∈ live` 的链式变体；无任何活前置剩下的链式节点（按 (source,fp) 去重）返回为 chain_failed → F-30 记 `front_dead`（不拨号、streak 正常推进）
- 语义：`role` 决定测试顺序、`category` 决定统计归属，二者有意分离（engine.py:1421-1426）

### F-70 手动直连/链式轮模式
- 入口：`engine.py:892 round_uses_chains`（mode=direct → 恒 False；否则看 chain_block）；`/api/run` mode 白名单（server.py:425-428）与链式未配置 400（server.py:429-442）；CLI `--mode`（__main__.py:28）；`rounds.mode` 列（db.py:279）与 `engine.current_mode`（engine.py:200）→ 面板 header 显示
- 语义：direct=忽略每来源开关全直连（前置池不测）；chain=按每来源 direct/chain 开关测；调度轮 mode=None 全量链式感知

### F-71 导出补组 derived_dialer_groups + 前置账本
- 入口：`engine.py:1966 derived_dialer_groups`
- 规则：导出 proxies 里有悬空 `dialer-proxy`（不指向本导出任何 proxy）→ 改写为 `chain.front_source.name` 并追加一个同名 select 组（含全部 proxies）；已可解析则不动
- 前置账本：`FRONT_SOURCE_KEY="__front__"`（engine.py:129）让前置自身健康可见、streak 连续；但无导出文件
- 导出还带**未打标签的前置副本**（README「链式代理」节；dialer 按名字找前置，而名字每轮被打标签，未打标签副本是唯一稳定名）

---

## 域 13 看门狗与调度

### F-72 预算与阶段检查
- 入口：`engine.py:74 _budget`（`watchdog.round_timeout_minutes` 默认 20，夹取 1-180）、`engine.py:81 _checkpoint`（fetch/build/delay-test/publish 四阶段间 + test_one 每 attempt 前）
- 超时：抛 `RoundTimeout`（engine.py:27）→ 中止 + 释放锁 + round_timeout 告警

### F-73 轮次互斥（三层）
- 进程内：`engine.py:179 _round_lock`（run_round 非阻塞 acquire，engine.py:923）
- 跨进程：`engine.py:961 _acquire_file_lock`（fcntl flock `data/round.lock`，非 POSIX 平台返回 None 降级）；释放 `engine.py:976 _release_file_lock`（绝不抛，finally 嵌套保证进程内锁不被搁浅）
- server 层：`server.py:24 BUSY`（/api/run 与调度器共用）

### F-74 调度器 scheduler_loop
- 入口：`server.py:538 scheduler_loop`（20s 轮询 stop_event；先 `reap_orphan_rounds`；`schedule.enabled` + `schedule.interval_minutes`(默认 30) + `db.last_round().started_at`(UTC 解析 `_parse`) 计算 due；到点且 BUSY 空闲 → run_in_background(trigger="schedule")）
- 状态：`_next_run`（server.py:25）→ /api/status.next_run
- 异常：整体 try/except 记事件，调度器永不死

### F-75 崩溃恢复
- 入口：`engine.py:31 _write_state`（`data/round.state.json` {phase,round_id,pid,ts,epoch,mode}，原子写；写失败记 warn——静默失败会产生永久 open 的 ghost 轮）、`engine.py:1285 reap_orphan_rounds`（启动时与每调度 tick：close 超预算仍未闭合轮，note="orphaned: 进程在轮次中途退出"）、`engine.py:1321 _abandon_round` + `engine.py:1363 _state_belongs_to`（残留状态文件只在该轮确实还开着时才信，防把已结束轮再关一遍）

---

## 域 14 tools/ 运维诊断脚本（一句话用途 + 前置条件）

| 编号 | 脚本 | 用途 | 前置条件 |
|---|---|---|---|
| F-76 | `tools/loop_snapshot.py` | 只读三段式账本快照（stats/rounds/dead/pending/excluded/事件），每轮迭代第一步 | `MIHOMO_TEST_ROOT` 指向含 data/ 的目录，或 `ssh vps docker exec -i` 管道 |
| F-77 | `tools/loop_query.py` | 只读证据查询 CLI（stats/rounds/node/history/round/reasons/dupes/rows 等子命令） | 同上 |
| F-78 | `tools/loop_scan.py` | 自治循环工作生成器：扫仓库+活账本输出排名 backlog（LIVE/VERDICT/CONFIG/SILENT/TESTS/DOC/SIZE） | 本地即可；--live 需 ssh vps |
| F-79 | `tools/fix_round_times.py` | 修复 TZ bug 造成的 finished_at<started_at 倒挂行；默认 dry-run，`--apply` 才写并先备份 | 生产 state.db；一次性（已用过） |
| F-80 | `tools/wire_telegram.py` | 从 /root/.acme.sh 或 TG_WATCHER_FILE 指定的自有通知脚本提取 Telegram bot/chat，getMe 校验后写入应用配置并发金丝雀 | vps root、acme.sh 凭据 |
| F-81 | `tools/deploy_files.py` / `pull_file.py` / `push_run.py` | 本地→vps base64+stdin 上传指定文件（可 --rebuild 重建容器）/ 远端小文件拉回 / 推脚本到 /tmp nohup 后台跑 | ssh vps 免密 |
| F-82 | `tools/chain_ab.py`、`chain_verify.py`、`chain_diagnose.py`、`chain_test2.py`、`front_ablation.py`、`front_debug.py` | 链式诊断族：直连 vs 链式 A/B、真内核验证 dialer-proxy 钉扎与 front_dead 落账、内核拨号错误捕获、字段消融（ech/x-padding）定位、单个前置可用性 | vps、docker、/srv/mihomo-test 代码；各自用独立 MIHOMO_TEST_ROOT/容器/端口，不动生产 |
| F-83 | `tools/v6_debug.py`、`v6_ab_test.py`、`v6_fix_verify.py`、`v6_host_verify.py`、`v6_front_scan.py`、`v6_chain_cause.py` | IPv6 诊断族：内核 debug 日志取因、dns.ipv6 A/B、bridge vs host 网络验证、扫描可作为 v6 前置的节点、链式 v6 失败归因 | vps（有 v6 出口）、docker |
| F-84 | `tools/lanes_verify.py` | 证明 N 个入站可各钉各的 select 组（两车道换节点→出口互换） | vps、docker |
| F-85 | `tools/verify_measure_switch.py` | 用真浏览器在线验收「直连/链式」开关渲染/ref 定位（key 而非 kind\|name）/写入目标 | 面板可达（默认 probe.example.com） |
| F-93 | `tools/build_web.py`、`deploy_pages.py`、`verify_cdn.py` | 见域 15（F-93/F-94） | 见域 15 |

---

## 域 15 部署形态

### F-86 Dockerfile（`Dockerfile`）
- `python:3.11-slim` + tzdata + `pip install -r requirements.txt`（仅 PyYAML）+ 从 `docker:cli` 拷贝 docker CLI（应用指挥内核容器用）；COPY mihomo_test/tests/tools；ENV `MIHOMO_TEST_ROOT=HOST_ROOT=/srv/mihomo-test, TZ=Asia/Shanghai`；CMD `python3 -m mihomo_test serve --host 127.0.0.1 --port 8088`（仅 loopback）
- tools/ 必须进镜像：test_hardening.CdnBuildGuardTest 用 importlib 加载 tools/build_web.py（Dockerfile:26-29）

### F-87 docker-compose.yml 三容器（全 host 网络）
- `mihomo-probe`：`metacubex/mihomo:latest`，bind `./core:/root/.config/mihomo`，cap NET_ADMIN/NET_RAW，mem 384m；**host 网络是硬要求**（bridge 无 IPv6 路由，v6-only 节点全死，compose 注释实测 0/6 vs 6/6）
- `mihomo-test`：build 本目录；bind `./data`、`./core`、`/var/run/docker.sock`（宿主 root 级能力，README 已知取舍）；env SUBSTORE_BACKEND/MIHOMO_TEST_TOKEN/MIHOMO_TEST_CORE_CONTAINER/MIHOMO_TEST_HOST_ROOT/TZ；mem 256m
- `cloudflared-probe`：`cloudflare/cloudflared:latest tunnel run`，TUNNEL_TOKEN
- 日志轮转 json-file 限制

### F-88 .env.example / requirements.txt
- `.env`：`SUBSTORE_BACKEND`（可带密钥路径）、`TUNNEL_TOKEN`、`MIHOMO_TEST_TOKEN`、`MIHOMO_TEST_HOST_ROOT`、`TZ`
- `requirements.txt`：**只有 `PyYAML==6.0.3`**（订阅解析/内核配置/导出三处）；无测试专用依赖

### F-89 migrate.sh（pack/unpack）
- pack：tar 全项目（含 data/ core/ .env）排除 __pycache__/journal → `mihomo-test-<ts>.tar.gz`；unpack 解到 $PWD 并 chmod 600 密钥文件；随后 `docker compose up -d --build`；token/账本/告警设置全保留

### F-90 install.sh
- 部署/重部署：py_compile 检查 → **停用旧宿主机 systemd 单元 mihomo-test.service**（防 8088 与轮锁之争）→ compose build/up → 容器内跑 unittest → 起 cloudflared → healthz 探测 → 打印下一步

### F-91 setup_tunnel.py
- 用宿主 acme.sh（/root/.acme.sh/account.conf）的 CF 凭据：列出/创建 cfd_tunnel `mihomo-test` → 写 ingress（hostname→http://127.0.0.1:8088 + 404 兜底）→ CNAME `<host>→<tunnel>.cfargotunnel.com`（proxied）→ 取 token 写 `data/tunnel.token`（0600）
- 默认 `--hostname probe.example.com --zone example.com`（硬编码默认值，见附 C）
- 依赖：Cloudflare API v4；幂等可重跑

### F-92 ban_legacy.sh
- 一次性停用旧测活管线：删 root cron `/srv/mihomo-health` 行、disable `mihomo-healthcheck.timer`、stop mihomo-air 容器、注释 nginx `include mihomo-health.conf`（nginx -t 失败自动回滚）、删旧产物集合；全程备份到 /srv/legacy-ban-backup/<ts>/
- 已执行过（README「已禁用的旧管线」节），保留作迁移时的同类清理参考

### F-93 CDN 前端：tools/build_web.py + deploy_pages.py + verify_cdn.py
- build_web：渲染 index.html（bootstrap `token:null, apiBase:<后端>`）+ 原样拷 3 静态文件 + 生成 `_headers`（CSP connect-src 指定后端 origin、no-cache）+ manifest.json（sha256）；**凭据护栏**：产物含 `"token": null` 校验 + 32 位十六进制串扫描，命中即拒绝构建并删旧 index.html（tools/build_web.py:62-79,144-151）；api-base 必须 https 且无 ?/#/token
- deploy_pages：CF API 建项目 + `npx wrangler@4 pages deploy`；凭据 env（CF_ACCOUNT_ID/CF_API_TOKEN 或 GLOBAL KEY），默认凭证文件路径硬编码在 Windows 本机（tools/deploy_pages.py:39-40，见附 C）；随机项目名词库
- verify_cdn：外网验收四件事（页面/资产 Content-Type/_headers 安全头/bootstrap apiBase），处理本地代理/brotli/头大小写三个假失败陷阱
- 前端令牌：URL/`#` → localStorage `mihomo-token` → `history.replaceState` 抹除（app.js:37-79）

---

## 域 16 测试体系

### F-94 tests/_isolation.py — 离线测试密封
- 把 config.DATA/CONFIG_PATH、db.DB_PATH/_conn、engine.ROUND_STATE/EXPORT_DIR、notifier.STATE_PATH 重定向到临时目录；否则一次 discover 会写生产 data/ 甚至触发 test_live 对线上发请求
- 调用链：test_logic / test_hardening / test_alerts_lanes / test_ipmap 的 setUpModule

### F-95 tests/test_logic.py — 338 个离线单测（57 类）
- 覆盖：policy/retry/reason 分类、prepare、store 缺失语义、别名折叠、导出、YAML 引号、derived_dialer_groups、round mode、round 锁、config patch/schema、禁用与静音来源、孪生来源键、domain 视图缓存、迁移、分类统计、取消勾选、key 校验、normalize_sources、导出路径、资源列表、link_substore、面板脚本断言、build_config、自引用、入口分类、出口未验证、轮预算、链式全家（ChainTest/FrontPoolInputsTest/FrontPoolUiTest/ChainFailureCategoryTest/ChainRoundTest）、ApplyAndPublish、时间戳、abandon、strip_ech、孤儿回收、push/prune-local/push 端点、国别标签幂等
- 全 mock 内核 + 临时 SQLite，无网络

### F-96 tests/test_alerts_lanes.py — 21 个：告警、车道配置、护栏陈旧导出、自引用、入口查询
### F-97 tests/test_hardening.py — 88 个：token/validate_patch/auth/HTTP 面（401 矩阵、静态资产、CORS、CDN 构建守卫）、轮锁、看门狗、excluded streak、通知冷却、make_testable、记账、trim、分块查询、trends、ui render、export token
### F-98 tests/test_ipmap.py — 30 个：族过滤/回显解析/标注/报告/来源解析（无网络无 docker）
### F-99 tests/test_live.py — 41 个实战用例，分组 health/api/kernel/lanes/data/substore/round(+slow)；模块级守卫：无部署即 SkipTest；花钱/改状态的标 slow 默认关；`FullRoundTest.MAX_WAIT_S=480` 封顶等待
### F-100 tests/diag_round_suite.py — 一次性诊断脚本（构建 round 子套件带 240s 上限跑），不是常规测试

---

## 附 A 模块 → 功能映射表

| 模块/文件 | 行数 | 承载功能 |
|---|---|---|
| `mihomo_test/__init__.py` | 2 | 版本号 |
| `mihomo_test/__main__.py` | 122 | F-35 CLI 入口（serve/round/push/status/ipmap）、SIGTERM 优雅停、启动时孤儿轮回收 |
| `mihomo_test/config.py` | 655 | 配置加载/保存/校验（F-53/59）、token 生成（F-53）、key 校验（F-62）、来源/链块 normalize（F-13/66）、死键清理、数值夹取、内核 secret、tunnel token 读取（死代码） |
| `mihomo_test/core.py` | 626 | 指纹（F-18）、prepare/build_config（F-06/08）、mihomo -t（F-07）、容器管理（F-09）、delay/select/egress（F-10/11）、车道常量（F-24） |
| `mihomo_test/db.py` | 732 | 全部 SQLite 功能（F-47/48/49/50/51/52/33） |
| `mihomo_test/doh.py` | 141 | DoH 线格式与双视角（F-45/46） |
| `mihomo_test/engine.py` | 2568 | 轮编排（F-12~F-23、F-25~F-32、F-64/63/62 的引擎侧、F-66~F-71、F-72/73/75） |
| `mihomo_test/ipmap.py` | 525 | ipmap 全套（F-05/35~F-40） |
| `mihomo_test/notifier.py` | 186 | 告警通道/冷却（F-41~F-43） |
| `mihomo_test/policy.py` | 91 | 状态机与护栏（F-26/27） |
| `mihomo_test/server.py` | 578 | HTTP API 全路由（F-53~F-58、F-62、F-74） |
| `mihomo_test/store.py` | 198 | Sub-Store 客户端（F-01~F-03） |
| `mihomo_test/ui.py` | 102 | 静态外壳/bootstrap（F-56） |
| `mihomo_test/web/` | — | 面板前端（F-61） |
| `link_substore.py` | 55 | F-65 |
| `setup_tunnel.py` | 149 | F-91 |
| `migrate.sh` / `install.sh` / `ban_legacy.sh` | — | F-89/90/92 |
| `Dockerfile` / `docker-compose.yml` / `.env.example` / `requirements.txt` | — | F-86/87/88 |
| `tools/`（24 个脚本） | — | F-76~F-85、F-93 |
| `tests/` | — | F-94~F-100 |
| 根目录杂项：`chain-alive-r365.md`、`console_live.txt`、`console_offline.txt`、`dom_live.html`、`dom_offline.html`、`reports/`、`.local/`、`.tmp_diag/`、`.pi/`、`.workbuddy-ai/`、`.wrangler/`、`dist/` | — | 审计/会话产物与留档，非运行时功能（dom_*.html 为 500KB 级面板 DOM 快照） |

## 附 B 外部集成点清单

| 集成点 | 方向 | 位置 | 协议/格式 |
|---|---|---|---|
| Sub-Store 后端（`substore.backend`，默认 127.0.0.1:3000，可带密钥路径） | 拉（订阅/资源/节点名） | store.py 全部、engine.collect_entries/collect_fronts/manual_fronts | REST `/api/{subs,collections,sub,collection}`、`/download[/collection]/<name>?target=ClashMeta`；500→「不存在」语义 |
| Sub-Store 后端 | 写（upsert/DELETE 本地订阅、远程订阅、聚合集合、手动前置、ipmap 标注订阅） | engine.push_exports/_prune_local_subs/link_substore/manual_fronts、ipmap.push_to_substore、link_substore.py | PATCH/POST/DELETE，payload `{name,displayName,source:url|local,url,content,process:[QUICK_SETTING]}` |
| 本服务导出端点（被 Sub-Store 拉） | 出 | server.py:399 `/api/export/<key>.yaml?token=`（publish.token） | ClashMeta YAML（含 YAML1.1 引号修正、国别标签、derived_dialer_groups） |
| 订阅源（ipmap --url 直拉） | 拉 | ipmap.fetch_subscription | UA 门禁：clash UA 优先/浏览器 UA 兜底；Clash YAML 或 base64 |
| mihomo 内核 REST API | 测 | core.Core（/version、/configs?force=true、/proxies/<name>/delay、PUT /proxies/<group>） | loopback:19190（Bearer core.secret，`data/core.secret` 0600） |
| mihomo 内核 HTTP 入站（车道） | 测 | core.egress、ipmap.fetch_via | loopback:19194(mixed) + 车道 19200..（ipmap 19300..） |
| docker daemon（挂载 docker.sock） | 指挥 | core.docker_cli/config_test/Core._docker、ipmap.start_core/stop_core | docker CLI：version/inspect/start/restart/logs/run --rm(-t 校验)/rm -f |
| ip-api.com 批量归属 | 查 | engine._fetch_country_batch（`http://ip-api.com/batch`，**明文 HTTP**） | JSON，90 IP/批 |
| DoH：doh.pub + cloudflare-dns.com | 查 | doh.py（GET ?dns=base64url，application/dns-message，ECS wire 格式） | A/AAAA 双视角 |
| Cloudflare /cdn-cgi/trace | 查 | core.egress、ipmap.probe_one、test_live | key=value 文本（ip/loc/colo） |
| ipmap 回显端点 | 查 | ipmap.ECHO_TARGETS | api(.6).ipify.org、ipv4(.6).icanhazip.com，明文 HTTP |
| Telegram Bot API | 通知 | notifier.telegram_send | https://api.telegram.org/bot<token>/sendMessage |
| 通用 Webhook | 通知 | notifier.webhook_send | POST JSON {key,level,title,body,source,ts} |
| Cloudflare API + Tunnel | 部署 | setup_tunnel.py（cfd_tunnel/ingress/DNS/token）、tools/deploy_pages.py（Pages 项目+wrangler） | REST v4；凭据来自 acme.sh / env / 本机凭证文件 |
| 调度/锁（本机文件系统） | 状态 | data/round.lock(fcntl)、data/round.state.json、data/alert-state.json、data/exports/*.yaml+meta、data/config.json、data/ipmap/* | — |

### 配置项键名全集（config.json；括号内为环境变量注入点）
`substore.backend(SUBSTORE_BACKEND)`、`core.{api,lanes,base_port,container(MIHOMO_TEST_CORE_CONTAINER),container_config_path}`、**`core.mixed_port`（部署特有，DEFAULTS 没有！core.py:236 直接下标读，README:19194）**、`sources[].{key,kind,name,label,enabled,export,relay,direct,chain}`、`test.{targets,expected_status,timeout_ms,timeout_ms_retry,concurrency,max_attempts,retry_pause_s}`、`dns.{views.{cn,overseas}.{resolver,ecs,ecs_prefix},timeout_s,cache_hours}`、`verify.{enabled,entry_check,exclude_entry_countries,domain_pass,strip_ech,trace_url,exclude_countries,max_nodes,timeout_s}`、`chain.{enabled,front_source{kind,name},front_pick,front_text,max_fronts}`、`policy.{drop_after_consecutive_fails,suspect_floor_ratio,suspect_floor_absolute}`、`schedule.{interval_minutes,enabled}`、`publish.{enabled,push_to_substore,prefix,hostname(MIHOMO_TEST_HOSTNAME),add_region_tag,token}`、`watchdog.round_timeout_minutes`、`alert.{enabled,telegram{enabled,token,chat_id},webhook{enabled,url},cooldown_minutes,alive_floor}`、`auth.token(MIHOMO_TEST_TOKEN)`、`server.cors_origins`、`ui.title`、`DEAD_KEYS: core.probe_group, core.config_path`；另有 `TUNNEL_TOKEN`、`MIHOMO_TEST_ROOT`、`MIHOMO_TEST_HOST_ROOT`、`MIHOMO_TEST_LIVE`（test_live 开关）。

## 附 C 疑似死代码 / 重复实现 / 硬编码痕迹 / .bak 残留

### C.1 疑似死代码（含只读被测试引用）
1. `mihomo_test/config.py:649 tunnel_token()` — 全仓无调用方（cloudflared 容器直接吃 `TUNNEL_TOKEN` env）。
2. `mihomo_test/db.py:494 recent_trends()` — 已被 `recent_trends_all`(509) 取代，仅注释提及。
3. `mihomo_test/notifier.py:180 reset_cooldown()` — 仅 tests/test_alerts_lanes.py:125 引用（测试辅助，非产品路径）。
4. `mihomo_test/engine.py:31 _write_state` 的 `phase/pid/ts/epoch` 字段 — 代码只读 `round_id` 与 `mode`，其余四个字段纯人读（docstring 自述）。
5. `mihomo_test/db.py:541 trim_results(cfg=None,...)` — 形参 `cfg` 未使用。
6. `mihomo_test/ui.py:50 asset_text` — 产品路径只剩 `index_template` 一处，测试大量引用（保留合理）。
7. `mihomo_test/engine.py:2307 QUICK_SETTING` 的 `"useless":"DISABLED"` 等 Sub-Store process 参数 — 有效但属外部系统方言，移植时需确认 Sub-Store 官方 schema。

### C.2 重复实现
1. **UTC 时间戳解析三胞胎**：`db.to_epoch`(db.py:129) / `server._parse`(server.py:566) / `engine._stored_epoch`(engine.py:1275) —— 同语义（UTC strptime→epoch），移植时应合一。
2. **浏览器 UA 字符串两份**：`store.py:17 BROWSER_UA` 与 `ipmap.py:63 BROWSER_UA`（同值）。
3. **trace key=value 解析两份**：`core.egress`(core.py:619-624) 与 `ipmap._parse_trace`(ipmap.py:138)。
4. **经车道 HTTP 代理拉 URL 两份**：`core.egress`(core.py:609-615) 与 `ipmap.fetch_via`(ipmap.py:124-135)（一个解析字段、一个返回正文）。
5. **YAML→proxies 解析两份**：`store._parse_proxies`(store.py:127) 与 `ipmap.extract_proxies`(ipmap.py:348)（后者多 base64/链接边界处理）。
6. **车道并发分桶 `i::lanes`**：`engine._verify_egress`(engine.py:1494) 与 `ipmap.probe_all`(ipmap.py:192)。
7. **锁双层**：`engine._round_lock` 与 `server.BUSY` 叠加（有意：跨进程 vs API 层），移植时注意不要合并丢失语义。
8. `link_substore.py` 与 `POST /api/link` 都是 `engine.link_substore` 的薄壳（合理复用，非重复实现）。

### C.3 硬编码地址 / 凭据痕迹（只列位置）
- 本部署主机名/域名默认值：`mihomo_test/config.py:300`（publish.hostname）、`setup_tunnel.py:7,109,112`、`tools/build_web.py:38`、`tools/verify_measure_switch.py:27`。
- **真实节点凭据（uuid/server/x-padding key）硬编码**在诊断脚本：`tools/chain_diagnose.py:36-41`、`tools/chain_test2.py:26-39`、`tools/front_ablation.py:22-33`、`tools/front_debug.py:22-25`、`tools/chain_ab.py`（SOURCES 列表）、`tools/lanes_verify.py`。
- 本机（Windows）凭证文件路径：`tools/deploy_pages.py:39-40`。
- 宿主 root 凭据文件路径：`setup_tunnel.py:21`（/root/.acme.sh/account.conf）、`tools/wire_telegram.py`（acme.sh 或 TG_WATCHER_FILE 指定的自有通知脚本）。
- 固定 UA：`ipmap.py:62`（clash-verge/v1.7.7）、`store.py:17`。
- 端口约定：`config.py:206-209`（19190/19200，避让 19090 的注释）、ipmap.py:56-58（19191/19300/19494）。
- **运行时数据文件含敏感信息且留在工作区**（.gitignore 已挡但磁盘上有）：`reports/ipmap-demo-v6.{json,md}`（实测节点+出口 IP）、`chain-alive-r365.md`、`console_live.txt`、`dom_live.html`/`dom_offline.html`（面板 DOM 快照，可能含 token 渲染）、`.local/`（诊断快照含面板 token，.gitignore 注释明言）、`.tmp_diag/`（含 deployed.tgz/vpscode.tgz 打包件）。
- `data/config.json`、`data/core.secret`、`data/tunnel.token`：运行时密钥（不在 git，Dockerfile/compose/bind-mount 体系管理）。

### C.4 .bak 残留文件清单（全部应清理，不计入移植）
包内（11 个）：`mihomo_test/db.py.bak-20260922-004821`、`mihomo_test/engine.py.bak-20260921-224540`、`mihomo_test/engine.py.bak-20260922-004821`、`mihomo_test/server.py.bak-20260921-224540`、`mihomo_test/server.py.bak-20260922-004821`、`mihomo_test/ui.py.bak-20260921-184840`、`mihomo_test/ui.py.bak-20260921-224540`、`mihomo_test/__main__.py.bak-20260921-224540`、`mihomo_test/__main__.py.bak-20260922-004821`、`tests/test_logic.py.bak-20260921-224540`、`tests/test_logic.py.bak-20260922-004821`。
镜像内旧拷贝（7 个）：`.tmp_diag/deployed/deployed/mihomo_test/{db,engine×2,server×2,ui×2,__main__×2}.bak-*`、`.tmp_diag/deployed/deployed/tests/test_logic.py.bak-*`。
诊断目录（2 个）：`.local/diag/snap_substore-resources.json.bak`、`.local/diag/verify_page.py.bak`。
运行时自动备份（db.py:168 `_backup`）：迁移时会生成 `data/state.db.bak-<stamp>`（机制，非残留）。

---

## 移植决策最重要的 5 条架构事实

1. **一切「活/死」结论来自真实 mihomo 内核 REST API，且应用通过挂载的 docker.sock 指挥内核容器**（`mihomo -t` 校验、restart、ipmap 一次性容器）。Sub-Store 官方架构没有等价物——移植时要么保留「内核容器 + docker.sock」这半边，要么放弃「真实出站测量」这一核心卖点；`core.container` 不可远程修改、导出独立 token、空 token 拒绝这三条是围绕该能力的安全圈。
2. **集成主轴是「拉」不是「写」**：Sub-Store 把远程订阅指向 `/api/export/<key>.yaml?token=<publish.token>`；写回（`-local` upsert）只是可选方式二。Sub-Store 的「500 即不存在」「零节点订阅必 500」两个怪癖已被 store.py/_is_missing 和零存活跳过联动显式建模，是历史上两套旧管线静默死亡的根因，移植必须保留这两个语义。
3. **收敛账本身份 = (source, fingerprint)**，指纹是连接参数 sha256[:16]（忽略 name 与 dialer-proxy），变体指纹 = sha256(fp#variant)[:16]；状态机 alive/pending/dead/unknown/excluded + 跨轮连续失败计数 + 整轮护栏（存活 < max(3, 上轮×50%) 不发布）+ excluded 永不计失败。所有落库时间一律 UTC。这是账本正确性的全部不变量，tests/test_live 的 data 分组逐条锁死。
4. **链式（dialer-proxy）是「一种数据源形状」而不是旁路系统**：前置池是普通中转来源（内核保留名 `__FRONTn__`）、账本 `__front__`、两阶段测试（前置先测→链式只经活前置，否则 front_dead）、导出带未打标签前置副本 + derived_dialer_groups 补组；直连/链式是每来源双测量开关，round mode（direct/chain）只是手动覆盖。
5. **部署三容器全 host 网络 + 路径同位约定**：mihomo-probe 必须 host 网络（bridge 无 IPv6）；应用容器 bind-mount docker.sock；`MIHOMO_TEST_ROOT` 与宿主路径必须同位（否则 `mihomo -t` 的 bind-mount 失效），项目内 `core/config.yaml`、`data/` 是唯一状态；**`core.mixed_port` 只存在于已部署的 config.json 而不在 DEFAULTS**（core.py:236 直接下标读）——新环境照 DEFAULTS 生成配置会在 build_config KeyError，这是移植时第一个会踩的坑。
