# Sub-Store 与发布

## 目标
保持双接入路径 + 原子发布 + 快照回滚（总控 §12）。

## 当前实现审计
- 拉模式：link_substore 为每个 enabled+export 源建 remote sub（URL 带 publish token，**绝不带 admin token**），聚合 collection（零存活源跳过——Sub-Store 对零节点订阅回 500）；prune 只删自己创建且指向本机 host 的对象，keyed on expected 而非成功写入（防 5xx 误删）。
- 推模式：push_exports 每源 upsert `<prefix>-<key>-local`；_prune_local_subs 清理退役源（scoped：-local 后缀 + 本前缀 + source==local）。
- 账本路径：/api/probe/nodes 九字段白名单（substore_bridge.py 单一事实源）+ probe_filter.script.js Script Operator。
- 导出：YAML SafeDumper + YAML1.1 lookalike 强制引号（short-id 123456e2 浮点陷阱，实测真内核 v1.19.29）；链式节点 dialer-proxy 必须在文件内可解析（[前置] 节点直接发布；单前置直指，多前置组；无可解析 dialer 时宁可不发布链式节点）；区域标签 [CC] 以实测出口覆写（幂等正则去旧标签）；tmp+replace 原子写 + meta.json count/updated_at。
- 快照/回滚：现状仅 meta（count/时间），无版本快照与 diff/rollback——总控要求的 export_snapshots 落 R2/R8。

## 待办清单
- [ ] R2：export_snapshots 表
- [ ] R8：Rust 导出 + POST /api/v1/exports/{key}/rollback + diff
- [ ] R8：Script Operator 契约测试（fixtures/expected/）

## 内嵌 Sub-Store（probe-substore crate，2026-10-02，路线 C 先行落地）
- 形态：rquickjs 0.14（quickjs-ng，MSVC 可编译）跑官方 releases 的 `sub-store.min.js`（脚本引擎版，vendor 2.42.2 兜底 + 可更新），复刻 sinspired/subs-check-pro 的 `substore/` 架构（QuickJS + Loon 六件套宿主原语），不碰官方核心（总控约束 8）。
- 宿主面：$httpClient→reqwest 桥（回调重入 AsyncContext）、$persistentStore→双 JSON 文件（sub-store.json 主键直读 + 缓存键 2s 防抖，loon_store.go 移植）、自写无 DOM 的 URL/URLSearchParams（npm url-polyfill 全靠 document.createElement 解析，不可用）、TextEncoder/Decoder、atob/btoa、crypto.getRandomValues/randomUUID、setTimeout（host 定时器重入）。
- server：axum 独立端口（默认 127.0.0.1:8299）+ 秘密路径路由 + CORS 全放行（预检反射头/PNA）+ /download/* 脱钩合并（method|url|UA|cors）+ /api/utils/env 注入（显式创建 meta.node.env，兼容 Loon 版只回 meta.loon）+ 前端 dist.zip 暂存扁平化安装。
- 与 Go 版偏差：共享单 AsyncContext + 请求级 dispatch Mutex 串行（Go 每请求新 runtime；并发下载不受影响）；170s 中断处理器兜底（Go 只能泄漏 goroutine）；$done 的 promise resolve 在 JS 侧完成。
- 已知代价：每请求整段 bundle 重跑（函数作用域重置状态），无字节码缓存——与 Go 的 EvalBytecode 相比有解析开销，慢了再优化（用户拍板「先写了再说」）。
- 验证：15/15 单测含真 bundle /api/utils/env 验收；实机冒烟 env 注入/CORS 预检/前端静态+SPA 回退/缺失 .js 404 全过；scan_secrets 干净。
- **接入（同日第二阶段）**：probe-config 新增 `substore` 节（Rust-only，默认 embedded=false 全节 no-op，Python 对未知键宽容故共享 config 双侧合法）→ `probe-cli serve` 单进程拉起 探针 API + 内嵌 Sub-Store，collection fetcher 的 backend 自指向 `http://<listen><backend_path>`（`SUBSTORE_BACKEND` 环境变量仍最高优先，指向独立实例可不改配置）；probe-api `ServeConfig/AppState` 增 `substore: Option<SubStoreConfig>`，serve 内 tokio::spawn，启动失败仅降级（探针 API 存活，与独立 Sub-Store 挂掉同语义）；`probe_substore::resolve_backend_path` 保证调用方与 serve 落到同一条持久化路径。单进程冒烟：/healthz + env 2.42.2 + 前端安装同日志齐全。

## 字节码缓存评估（2026-10-03，实测后结案：不建）
release 二进制单进程实测（embedded 形态，30 次串行请求/路由）：
- `/api/subs`（真 bundle 全程：解析+初始化+路由，含 env——env 的"注入"是对
  bundle 响应的改写，同样先跑完整 bundle）：**p50 21.4ms / p95 23.7ms / max 220ms**。
- 对照同进程纯 axum 路由 `/healthz`：p50 1.1ms ⇒ bundle 每请求成本 ≈ **20ms**。
- 结论：字节码缓存最多省 20ms/请求；本系统全部请求类别（面板 UI、/download/
  订阅生成动辄数秒、每轮采集、低频 cron 自调）对 20ms 均不敏感，不值得引入
  （quickjs-go EvalBytecode 或 runtime 池皆然）。真正会逼人重访的信号是
  吞吐：dispatch Mutex 在 20ms/请求下上限 ≈50 req/s，单用户面板远够。

## 测试证据
- Python 锚点：ExportTest/YamlScalarQuotingTest/DerivedDialerGroupTest/LinkSubstoreTest/PushExportsTest/PruneLocalSubsTest/test_substore_bridge 全文件。
- Rust 新增：probe-substore 15 测（引擎回显/异常文本/KV 往返/$httpClient 本地桥/URL 全形态/runaway 中断/真 bundle env）。

## 下一步
- R2 快照表；R8 导出引擎；produce/gist cron 移植（蓝本 loon_server.go 的 StartSubStoreCronJobs）；`$notification` 推送补 Apprise 渠道。
- 部署形态待拍板：hk3 compose 把 `substore.embedded` 打开并停独立 Sub-Store（需先备份迁移现有 subs/collections/air 脚本），或双轨并存。
