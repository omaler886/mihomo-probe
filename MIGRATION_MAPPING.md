# MIGRATION_MAPPING — 功能映射矩阵

> 输入：`FEATURE_INVENTORY.md`（F-01~F-100，legacy-audit）× `OFFICIAL_CAPABILITY_MAP.md`（官方 backend@`e08f1b1`，upstream-research）。
> 规则：每项功能在编码前必须在此表中有映射行；行字段与提示词要求的 JSON schema 一一对应
> （feature / legacy_entry / official_equivalent / decision / target_module / compatibility / tests / risk / owner）。
> decision 取值：`reuse`=官方能力直接复用｜`enhance`=官方能力+本服务增强｜`extension`=官方无对应，独立扩展模块｜
> `adapter`=平台/语义差异适配器｜`deprecate`=不进入目标架构（保留为私有运维资产或一次性产物）。
> 决策统计（F-01~F-100）：reuse 3 / enhance 8 / extension 68 / adapter 6 / deprecate 15；另有本批新增 N-01~N-04（extension）。

## 0. 总体架构决策（先于逐条映射）

```json
{
  "feature": "mihomo-probe 并入官方 Sub-Store 的总体形态",
  "legacy_entry": "整个 mihomo_test 包（约 6800 行）",
  "official_equivalent": "无（官方 17 个 operator / 全部路由均无测活；官方生态测活 = Script Operator + 外置 http-meta 内核）",
  "decision": "extension",
  "target_module": "probe 保持独立服务（扮演官方文档中 http-meta 的『本地测试执行器』角色）；经路径 B（远程订阅上游）+ 路径 A（Script Operator）两个官方扩展点接入；拒绝路径 C（fork 官方核心）",
  "compatibility": "不改官方核心一行代码；对官方只暴露 HTTP（导出 YAML / JSON 端点）；官方 500 即『不存在』等语义怪癖由 store.py 显式建模",
  "tests": "tests/ 全量（基线 479 用例）+ 新增 tests/test_substore_bridge.py",
  "risk": "官方 API/文档演进（上游日更）；docker.sock 特权面（S-11，已有 token 分离缓解）",
  "owner": "architecture + backend-impl"
}
```

## 1. 逐条映射矩阵（F-01~F-100）

### 域 1 订阅获取与解析

| # | 功能 | legacy 入口 | 官方对应 | 决策 | 目标模块 | 兼容方式 | 测试 | 风险 | owner |
|---|---|---|---|---|---|---|---|---|---|
| F-01 | Sub-Store REST 客户端（重试+500→不存在语义） | store.py:31 | 官方 REST API 本身 | reuse | store.py（保留薄客户端） | 500 语义已建模（_is_missing） | test_logic StoreTest | 官方 API 变更（路由稳定，低） | 无（保留现状） |
| F-02 | 订阅下载与 proxies 解析 | store.py:108-146 | `/download/:name?target=ClashMeta` | reuse | store.py | 只走 download 路由不读 content | test_logic StoreTest | 同上 | 无 |
| F-03 | 资源清单（数据源选择器） | store.py:152 | `/api/subs`+`/api/collections` | reuse | store.py | 单 kind 失败不中断 | test_logic ResourceListTest | 同上 | 无 |
| F-04 | 手动前置物化为 Sub-Store 本地订阅 | engine.py:568 | 官方 local sub（source:local+content） | enhance | engine.manual_fronts | upsert/DELETE 幂等 + digest 缓存 | test_logic FrontPoolInputsTest | 写回失败→前置池空（已降级处理） | 无 |
| F-05 | ipmap 独立订阅拉取（UA 门禁/base64/链接边界） | ipmap.py:321 | 官方下载器（带 ua 配置） | adapter | ipmap.fetch_subscription | clash UA 优先浏览器 UA 兜底；分享链接明确报错 | test_ipmap SourceTest | 订阅站门禁变化 | 无 |

### 域 2 mihomo 内核生命周期

| # | 功能 | legacy 入口 | 官方对应 | 决策 | 目标模块 | 兼容方式 | 测试 | 风险 | owner |
|---|---|---|---|---|---|---|---|---|---|
| F-06 | 内核配置生成 | core.py:219 | null（官方内核外置于 http-meta） | extension | core.build_config | 独立模块不动官方 | test_logic BuildConfigTest | `core.mixed_port` 不在 DEFAULTS，新环境 KeyError（见 ARCHITECTURE 陷阱清单） | 无 |
| F-07 | mihomo -t 校验 + docker 探测 | core.py:331,360 | null | extension | core.config_test | docker 不可用优雅降级 | test_logic CoreTest | docker.sock 面（S-11） | 无 |
| F-08 | make_testable 剪枝重试 | core.py:387 | null | extension | core.make_testable | culprit 定位逐组剔除 | test_logic PrepareTest | 误剔（保守匹配已缓解） | 无 |
| F-09 | 内核容器启停/重载 | core.py:467 | null | extension | core.Core | reload 失败回退 restart | test_logic CoreTest | 同 F-07 | 无 |
| F-10 | 逐节点延迟测量与失败归类 | core.py:563,102 | null（官方不做） | extension | core.Core.delay | 保留响应体、controller_error 不计失败 | test_logic RetryTest | 内核版本行为差异 | 无 |
| F-11 | select 组切换 + 车道出口请求 | core.py:596,602 | null | extension | core.select/egress | 独立端口段避让 19090 | test_alerts_lanes LanesTest | 端口占用 | 无 |

### 域 3 测活引擎

| # | 功能 | legacy 入口 | 官方对应 | 决策 | 目标模块 | 兼容方式 | 测试 | 风险 | owner |
|---|---|---|---|---|---|---|---|---|---|
| F-12 | 一轮编排 run_round | engine.py:912,1030 | null | extension | engine._run_round | 阶段 checkpoint + 告警解耦 | test_logic 全套 + test_live round | 整轮异常路径 | 无 |
| F-13 | 来源条目化 + 指纹派生 | engine.py:511,465 | null | extension | engine.collect_entries | 自引用过滤 | test_logic ApplyAndPublishTest | 上游改名/去重 | 无 |
| F-14 | 双视角 DoH 解析 + 逐地址展开 | engine.py:355,435 | 官方 Resolve Domain（仅解析，无 ECS/双视角/缓存语义） | extension | engine.classify_and_expand | 解析失败回落内核自解析 | test_logic DomainViewsTest | GeoDNS 漂移（缓存 6h） | 无 |
| F-15 | 入口 IP 归属与受限 ISP 过滤 | engine.py:290,331 | `/api/utils/node-info`（仅入口单查） | extension | engine.lookup_countries | 查询失败本轮放弃过滤（宁多测不误杀） | test_logic EntryClassificationTest | ip-api 限流 | 无 |
| F-16 | 非对称轮内重试 | engine.py:213 | null | extension | engine.test_one | 终态原因立即停 | test_logic RetryTest | 无 | 无 |
| F-17 | 并发执行 | engine.py:1388 | null | extension | engine._test_all | deadline 有界等待 | test_logic | 无 | 无 |
| F-18 | 指纹身份体系（fp/variant/orig_fp） | core.py:38,57; engine.py:465 | null（官方按 name） | extension | core.fingerprint_proxy 族 | 忽略 dialer/name；变体派生防覆盖 | test_logic FingerprintTest + test_live data | 指纹口径变更=账本重建（有 _backup） | 无 |
| F-19 | 域名聚合记分 | engine.py:1585 | null | extension | engine._score_bucket | any/all 两模式 | test_logic ApplyAndPublishTest | 无 | 无 |
| F-20 | ECH 剥离 | core.py:148 | null | extension | core.prepare | 开关 verify.strip_ech | test_logic StripEchTest | 只证明非 ECH 路径（已文档化） | 无 |
| F-21 | 出口国别过滤 | engine.py:1557 | null（MMDB 静态归属≠实测出口） | extension | engine._resolve_exit | 全失败→verify_failed 不算活 | test_logic ExitFilterTest | trace 端点不可达 | 无 |
| F-22 | 导出代理构建（标签幂等/YAML1.1 引号） | engine.py:2024 | 官方 producers 输出 ClashMeta | extension | engine._export_proxies | 域名原形式回推 | test_logic ExportTest/YamlQuoteTest | 上游 producers 变更（只消费不受影响） | 无 |
| F-23 | 导出文件写入与清理 | engine.py:2110,2316 | null（官方 artifact 走 Gist） | extension | engine._write_export | tmp+rename 原子写 | test_logic ApplyAndPublishTest | 磁盘空间 | 无 |

### 域 4 出口验证并行车道

| # | 功能 | legacy 入口 | 官方对应 | 决策 | 目标模块 | 兼容方式 | 测试 | 风险 | owner |
|---|---|---|---|---|---|---|---|---|---|
| F-24 | 车道拓扑常量与端口 | core.py:295 | null | extension | core.lane_* | 车道数 1-32 可配 | test_alerts_lanes LanesTest | 端口占用 | 无 |
| F-25 | 出口验证执行 | engine.py:1473 | null | extension | engine._verify_egress | 车道内提前收手、unverified 交发布门 | test_alerts_lanes + test_live lanes | 车道抖动（交换用例已加固） | 无 |

### 域 5 收敛账本与策略

| # | 功能 | legacy 入口 | 官方对应 | 决策 | 目标模块 | 兼容方式 | 测试 | 风险 | owner |
|---|---|---|---|---|---|---|---|---|---|
| F-26 | 状态机 | policy.py:21 | null | extension | policy.apply | 通过即恢复/阈值判死 | test_logic PolicyTest | 无 | 无 |
| F-27 | 整轮护栏 | policy.py:79 | null | extension | policy.round_is_suspect | 扣留时不覆盖旧导出 | test_alerts_lanes SuspectTest | 阈值过敏（可配） | 无 |
| F-28 | 别名聚合 | engine.py:1538 | null | extension | engine.group_by_fingerprint | (source,fp) 折叠 | test_logic AliasFoldTest | 无 | 无 |
| F-29 | 单端点收敛 | engine.py:1610 | null | extension | engine._converge_bucket | 出口语义修正后记账 | test_logic | 无 | 无 |
| F-30 | 非拨号记账三件套 | engine.py:1657,1695,1750 | null | extension | engine._record_* | excluded 永不计失败 | test_hardening ExcludedStreakTest | 无 | 无 |
| F-31 | 账本修剪与对账 | engine.py:1795,998 | null | extension | engine._prune_*/_reconcile | seen 白名单删除 | test_logic PruneTest | 误删（disabled 降级保留） | 无 |
| F-32 | 未验证不发布 + 截断 | engine.py:1910 | null | extension | engine._apply_and_publish | 过滤配置联动 | test_logic UnverifiedTest | 无 | 无 |
| F-33 | 分类统计 | db.py:598 | null | extension | db.stats_by_category | 三桶口径 | test_logic StatsTest | 无 | 无 |
| F-34 | UTC 时间戳约定 | db.py:113 等 6 处 | null | extension | db/engine/policy/server | TimestampTest 锁定 | test_logic TimestampTest | 漏改点被测试挡住 | 无 |

### 域 6 ipmap 节点↔落地IP 映射

| # | 功能 | legacy 入口 | 官方对应 | 决策 | 目标模块 | 兼容方式 | 测试 | 风险 | owner |
|---|---|---|---|---|---|---|---|---|---|
| F-35 | ipmap CLI 入口 | __main__.py:61; ipmap.py:431 | null | extension | ipmap.run | 与测活轮完全隔离（独立容器/端口） | test_ipmap | docker 依赖 | 无 |
| F-36 | 族过滤与回显解析 | ipmap.py:83-103,251 | null | extension | ipmap | 族不匹配即拒 | test_ipmap FamilyTest/EchoParseTest | 回显端点变更（多目标兜底） | 无 |
| F-37 | 并发探测 | ipmap.py:147,182 | null | extension | ipmap.probe_all | 车道分桶复用 | test_ipmap ProbeOneTest | 同 F-25 | 无 |
| F-38 | 标注与写回 Sub-Store | ipmap.py:217,234,411 | 官方 local sub / Rename Operator | enhance | ipmap.push_to_substore | 改名能力官方有，映射数据是增强；产物可喂官方 ip-flag.js | test_ipmap AnnotateTest | S-09 产物含凭据（已记 P2） | 无 |
| F-39 | 报告渲染与落盘 | ipmap.py:282,307 | null | extension | ipmap.render_report | JSON+MD 双产物 | test_ipmap ReportTest | S-09 | 无 |
| F-40 | ipmap 一次性内核容器 | ipmap.py:372,403 | null | extension | ipmap.start_core/stop_core | 跑完即删 | 实战覆盖 | docker.sock 面 | 无 |

### 域 7 通知告警

| # | 功能 | legacy 入口 | 官方对应 | 决策 | 目标模块 | 兼容方式 | 测试 | 风险 | owner |
|---|---|---|---|---|---|---|---|---|---|
| F-41 | Telegram/Webhook 通道 | notifier.py:49,70 | 官方 $.notify（shoutrrr/URL 模板，TG 支持） | enhance | notifier | 官方通知仅绑定 cron 任务完成/失败；本服务告警语义更全（护栏/存活下限/超时），保留自有通道 | test_alerts_lanes NotifierTest | TG API 可达性 | 无 |
| F-42 | 告警聚合与冷却 | notifier.py:87 | null（官方无冷却） | enhance | notifier.send | 投递成功才记冷却 | test_alerts_lanes CooldownTest | 无 | 无 |
| F-43 | 金丝雀与冷却重置 | notifier.py:159,180 | null | enhance | notifier.test_channels | /api/alert-test | test_alerts_lanes | 无 | 无 |
| F-44 | 告警触发点汇总 | engine.py 6 处 | null | enhance | engine._maybe_alert | 告警失败不影响轮次结论 | test_alerts_lanes | 无 | 无 |

### 域 8 DoH 模块

| # | 功能 | legacy 入口 | 官方对应 | 决策 | 目标模块 | 兼容方式 | 测试 | 风险 | owner |
|---|---|---|---|---|---|---|---|---|---|
| F-45 | DNS 线格式构建与解析 | doh.py:25-73 | 官方 doh 工具（ProxyUtils.doh） | extension | doh.py | ECS wire 手写（JSON API 不支持） | test_logic DohTest | 无 | 无 |
| F-46 | DoH 查询与双视角解析 | doh.py:102,114 | null（官方单视角） | extension | doh.resolve_views | 单视角失败不抛 | test_logic | DoH 可达性 | 无 |
| F-47 | 域名视图缓存 | db.py:399,425 | null | extension | db.domain_views_* | 未来时间戳不可信 | test_logic DomainViewsTest | 无 | 无 |

### 域 9 数据库 / 存储层

| # | 功能 | legacy 入口 | 官方对应 | 决策 | 目标模块 | 兼容方式 | 测试 | 风险 | owner |
|---|---|---|---|---|---|---|---|---|---|
| F-48 | 连接管理与 WAL | db.py:137 | 官方单 JSON 文件存储 | extension | db.connect | 账本需要 SQL（官方 JSON 不适合）；与官方存储互不侵入 | test_logic | 无 | 无 |
| F-49 | Schema 与迁移 | db.py:24,183 | null | extension | db._migrate | 迁移前 _backup | test_logic MigrationTest | 无 | 无 |
| F-50 | 节点账本读写 | db.py:309-472 | null | extension | db | 分块删除 | test_logic | 无 | 无 |
| F-51 | 轮次/结果/事件表 | db.py:271-570 | null | extension | db | results 500 轮修剪、events 2000 上限 | test_logic TrimTest | 无 | 无 |
| F-52 | 仪表盘查询面 | db.py:494-598 | null | extension | db.stats 等 | recent_trends 疑似死代码（附 C.1，不移植新用法） | test_logic TrendsTest | 无 | 无 |

### 域 10 HTTP server 与 web UI

| # | 功能 | legacy 入口 | 官方对应 | 决策 | 目标模块 | 兼容方式 | 测试 | 风险 | owner |
|---|---|---|---|---|---|---|---|---|---|
| F-53 | 双 token 鉴权模型 | server.py:93-135 | 官方无内建鉴权（秘密路径+CORS 模型） | enhance | server.auth_ok | 强于官方：auth/publish 分离、空 token 拒绝；对官方仅暴露 publish 作用域 | test_hardening AuthTest | 无 | 无 |
| F-54 | 安全头与 CSP | server.py:43 | null | extension | server.SECURITY_HEADERS | 每响应携带 | test_hardening | 无 | 无 |
| F-55 | CORS | server.py:62,288 | 官方 SUB_STORE_CORS_ALLOWED_ORIGINS | extension | server.cors_origin | 精确回表白名单 | test_hardening CorsTest | 无 | 无 |
| F-56 | 静态资源白名单与面板渲染 | ui.py:38-90 | null | extension | ui.py | 白名单 3 文件 | test_hardening UiTest | 无 | 无 |
| F-57 | GET 路由集 | server.py:310 | —（本服务自有 API；本批新增 /api/probe/nodes → N-01） | extension | server.do_GET | — | test_hardening HttpFaceTest | 无 | 无 |
| F-58 | POST 路由集 | server.py:410 | — | extension | server.do_POST | mode 白名单 | test_hardening | 无 | 无 |
| F-59 | 配置补丁校验 | config.py:558,628 | null | extension | config.validate_patch | 白名单重建/数值夹取/自引用拒绝 | test_logic ConfigTest | 无 | 无 |
| F-60 | 配置保存副作用链 | server.py:477-521 | null | extension | server | 联动签名变化才碰 Sub-Store | test_logic LinkTest | 无 | 无 |
| F-61 | 前端面板 | mihomo_test/web/ | 官方 Vue 前端是订阅管理端，不承载测活面板 | extension | web/（保留现状） | 本迁移无新 UI 需求 → frontend-impl 免建 | test_hardening UiTest + verify_cdn | 无 | 无 |

### 域 11 Sub-Store 联动

| # | 功能 | legacy 入口 | 官方对应 | 决策 | 目标模块 | 兼容方式 | 测试 | 风险 | owner |
|---|---|---|---|---|---|---|---|---|---|
| F-62 | 导出端点 /api/export/<key>.yaml | server.py:399; engine.py:2542 | 官方远程订阅（路径 B 接入点） | enhance | engine.read_export | publish token 只读作用域；路径穿越双重校验 | test_hardening ExportTokenTest | S-16 query token（已缓解） | 无 |
| F-63 | 推送模式（-local upsert） | engine.py:2166 | 官方 local sub | enhance | engine.push_exports | publish.enabled 关闭即拒推旧快照 | test_logic PushTest | 无 | 无 |
| F-64 | 拉取联动 link_substore | engine.py:2394 | 官方 remote sub/collection 对象 | adapter | engine.link_substore | 500=不存在、零节点跳过、expected 对账防误删 | test_logic LinkSubstoreTest | 官方对象语义变化 | 无 |
| F-65 | CLI link_substore.py | link_substore.py:22 | 同上 | adapter | link_substore.py | engine 薄壳 | test_logic | 无 | 无 |

### 域 12 链式代理（前置 → 落地）

| # | 功能 | legacy 入口 | 官方对应 | 决策 | 目标模块 | 兼容方式 | 测试 | 风险 | owner |
|---|---|---|---|---|---|---|---|---|---|
| F-66 | 链式配置块与生效判定 | engine.py:541 | null | extension | engine.chain_block | 半配置回落直连并 warn | test_logic ChainTest | 无 | 无 |
| F-67 | 前置池收集 | engine.py:651 | null | extension | engine.collect_fronts | 内核保留名 __FRONTn__ | test_logic FrontPool*Test | 无 | 无 |
| F-68 | 链式展开（双测量开关/变体指纹） | engine.py:751,774 | null | extension | engine.expand_chains | 直连孪生 variant_fingerprint | test_logic ChainTest | 双测语义回退（已建模） | 无 |
| F-69 | 两阶段测试 | engine.py:1409 | null | extension | engine._test_phases | role/category 分离 | test_logic ChainRoundTest | 无 | 无 |
| F-70 | 手动直连/链式轮模式 | engine.py:892; server.py:425 | null | extension | server/api/run | mode 白名单+未配置 400 | test_hardening | 无 | 无 |
| F-71 | 导出补组 + 前置账本 | engine.py:1966 | null | extension | engine.derived_dialer_groups | 悬空 dialer 改写+select 组 | test_logic DerivedDialerTest | 无 | 无 |

### 域 13 看门狗与调度

| # | 功能 | legacy 入口 | 官方对应 | 决策 | 目标模块 | 兼容方式 | 测试 | 风险 | owner |
|---|---|---|---|---|---|---|---|---|---|
| F-72 | 预算与阶段检查 | engine.py:74,81 | null（官方 cron 无预算概念） | extension | engine._checkpoint | RoundTimeout 释放锁 | test_alerts_lanes WatchdogTest | 无 | 无 |
| F-73 | 三层轮次互斥 | engine.py:179,961; server.py:24 | null | extension | engine/server 锁 | flock 非 POSIX 降级 | test_hardening RoundLockTest | Windows 无 flock（测试已标注） | 无 |
| F-74 | 调度器 | server.py:538 | 官方 SUB_STORE_PRODUCE_CRON（预热缓存，语义不同） | extension | server.scheduler_loop | 调度器永不死 | test_logic | 无 | 无 |
| F-75 | 崩溃恢复 | engine.py:31,1285,1321,1363 | null | extension | engine.reap_orphan_rounds | 残留状态文件只信开着的轮 | test_logic AbandonTest | 无 | 无 |

### 域 14 tools/ 运维诊断脚本

| # | 功能 | legacy 入口 | 官方对应 | 决策 | 目标模块 | 兼容方式 | 测试 | 风险 | owner |
|---|---|---|---|---|---|---|---|---|---|
| F-76~F-85 | 诊断/修复/部署脚本族（loop_snapshot、loop_query、loop_scan、fix_round_times、wire_telegram、deploy_files/pull_file/push_run、chain_*、v6_*、lanes_verify、verify_measure_switch） | tools/ 各文件 | null | deprecate | 不进入目标架构 | 原样保留为私有运维资产（绑定 vps 路径/凭据/部署拓扑，见 FEATURE_INVENTORY 域 14）；其中 S-01~S-03 涉及的 4 个已跟踪脚本需凭据轮换+历史清洗后方可外发 | 不适用（运维工具） | 凭据泄露（SECURITY_REVIEW A 节） | 无 |

### 域 15 部署形态

| # | 功能 | legacy 入口 | 官方对应 | 决策 | 目标模块 | 兼容方式 | 测试 | 风险 | owner |
|---|---|---|---|---|---|---|---|---|---|
| F-86 | Dockerfile | Dockerfile | 官方无 Dockerfile（推荐 xream/sub-store 镜像） | adapter | Dockerfile | 自有镜像，与官方镜像并存（不同职责） | test_hardening CdnBuildGuardTest（tools 进镜像的约束） | 无 | 无 |
| F-87 | 三容器 compose | docker-compose.yml | null | adapter | docker-compose.yml | host 网络/路径同位约定已文档化 | 实战 | docker.sock 面 | 无 |
| F-88 | .env.example / requirements.txt | .env.example | null | adapter | .env.example | 凭据全走 env | — | 无 | 无 |
| F-89 | migrate.sh | migrate.sh | null | deprecate | 保留私有 | 一次性运维 | — | — | 无 |
| F-90 | install.sh | install.sh | null | deprecate | 保留私有 | 同上 | — | — | 无 |
| F-91 | setup_tunnel.py | setup_tunnel.py | null | deprecate | 保留私有 | 绑定 CF 凭据/域名 | — | S-13 域名硬编码 | 无 |
| F-92 | ban_legacy.sh | ban_legacy.sh | null | deprecate | 保留私有 | 已执行过的一次性清理 | — | — | 无 |
| F-93 | CDN 前端构建/部署/验收 | tools/build_web.py 等 | null | deprecate | 保留私有 | 私有 CDN 部署链；build_web 的凭据护栏被 test_hardening 引用故保留 | CdnBuildGuardTest | — | 无 |

### 域 16 测试体系

| # | 功能 | legacy 入口 | 官方对应 | 决策 | 目标模块 | 兼容方式 | 测试 | 风险 | owner |
|---|---|---|---|---|---|---|---|---|---|
| F-94 | 测试密封 _isolation | tests/_isolation.py | — | extension | tests/ | 防止写生产 data/ | 自身 | 无 | 无 |
| F-95 | test_logic 338 用例 | tests/test_logic.py | — | extension | tests/ | 迁移回归门禁 | 自身 | 无 | 无 |
| F-96 | test_alerts_lanes 21 用例 | tests/test_alerts_lanes.py | — | extension | tests/ | 同上 | 自身 | 无 | 无 |
| F-97 | test_hardening 88 用例 | tests/test_hardening.py | — | extension | tests/ | 同上 + N-01 新用例落此处风格 | 自身 | 无 | 无 |
| F-98 | test_ipmap 31 用例 | tests/test_ipmap.py | — | extension | tests/ | 夹具已脱敏（S-08） | 自身 | 无 | 无 |
| F-99 | test_live 实战 41 用例 | tests/test_live.py | — | extension | tests/ | 需真实部署，CI 外 | 自身 | 环境依赖 | 无 |
| F-100 | diag_round_suite | tests/diag_round_suite.py | — | deprecate | 保留 | 一次性诊断脚本，非常规测试 | — | — | 无 |

## 2. 本批新增扩展面（迁移的实际编码范围）

```json
{
  "feature": "N-01 probe 节点账本只读 JSON 端点",
  "legacy_entry": "新增：server.py do_GET + db.list_nodes",
  "official_equivalent": "官方 /api/utils/node-info（仅入口归属，无账本语义）——本端点是 Sub-Store 侧脚本的数据源",
  "decision": "extension",
  "target_module": "mihomo_test/server.py（新增 GET /api/probe/nodes）+ mihomo_test/substore_bridge.py（payload 纯函数）",
  "compatibility": "鉴权沿用现有模型：publish.token 或 auth.token 皆可（与 /api/export 同一作用域）；只读；响应不含任何凭据/token 字段；新增路由不影响现有路由",
  "tests": ["test_hardening 风格：无 token 401 / publish token 200 / auth token 200 / 响应形状 / 不含敏感字段"],
  "risk": "账本行数大时响应体大（?source=/status 过滤；不做 count 截断——截断制造静默差异，且 nodes 表行数有界）",
  "owner": "backend-impl → tests"
}
```

```json
{
  "feature": "N-02 Sub-Store Script Operator 接入脚本",
  "legacy_entry": "新增：substore_bridge/probe_filter.script.js",
  "official_equivalent": "官方 Script Operator（processors/index.js ScriptOperator；签名 operator(proxies, targetPlatform, context)，$arguments 传参）",
  "decision": "extension",
  "target_module": "substore_bridge/probe_filter.script.js（独立脚本，粘贴进 Sub-Store 订阅 process 使用）",
  "compatibility": "不 fork 官方核心；$arguments 约定：probe_url / probe_token / mode(filter|annotate|both) / missing(keep|drop)；节点匹配按 name 优先、server 兜底（nodes 表无 port 列；同 host 多记录状态互相矛盾时按无记录处理，防误杀）；probe API 不可达时按 missing 策略失败开放（默认 keep 不误杀）",
  "tests": ["node --check 语法门禁", "Node 24 行为 harness：mock fetch + 官方 operator 签名调用，断言 filter/annotate/missing 三路径"],
  "risk": "官方脚本沙箱/ProxyUtils 能力差异（脚本只用 $server/$arguments/fetch 最小面，降低耦合）",
  "owner": "backend-impl → tests"
}
```

```json
{
  "feature": "N-03 substore_bridge 适配层（payload 构建）",
  "legacy_entry": "新增：mihomo_test/substore_bridge.py",
  "official_equivalent": "null（桥接既有账本与官方脚本生态）",
  "decision": "extension",
  "target_module": "mihomo_test/substore_bridge.py（probe_nodes_payload(rows) 纯函数 + 文档常量）",
  "compatibility": "纯函数无副作用；server 端点与测试共用同一构建器，形状单一事实来源",
  "tests": ["tests/test_substore_bridge.py：行→payload 映射、字段白名单（不泄露凭据）、空账本"],
  "risk": "低",
  "owner": "backend-impl → tests"
}
```

```json
{
  "feature": "N-04 MIGRATION_GUIDE 接入手册",
  "legacy_entry": "新增文档",
  "official_equivalent": "官方文档 sub-store-org.github.io/doc（remote sub / script 各节）",
  "decision": "extension",
  "target_module": "MIGRATION_GUIDE.md：路径 B（建远程订阅）与路径 A（配 Script Operator）的分步操作、回滚、凭据轮换与历史清洗前置项",
  "compatibility": "全操作可逆（删订阅/删 process 项即回滚）",
  "tests": ["reviewer 核对步骤与官方文档 URL 一致"],
  "risk": "低",
  "owner": "main"
}
```

## 3. 禁止事项（映射层面的硬约束）

- 官方核心（backend/src/**）零修改；路径 C 已否决（理由见 §0 与 OFFICIAL_CAPABILITY_MAP §5）。
- 不把 auth.token 写进任何 Sub-Store 对象或脚本默认值；脚本 token 由使用者以 $arguments 传入。
- 不为本迁移改 `core.container` 不可远程修改等安全圈（SECURITY_REVIEW 正面确认清单）。
- deprecate 项（域 14/15）不得为"看起来完整"而重写进目标架构。
