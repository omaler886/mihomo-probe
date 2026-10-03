# HANDOFF — probe-substore 内嵌 Sub-Store（路线 C，两阶段已完成）

> 写给下一个接手本任务的 agent。读完本文 + `workstreams/09_SUBSTORE_EXPORT.md` 即可开工；
> 历史决策链在会话记忆 `subs-check-pro-evaluation`（C:\Users\ckai7\.zcode\cli\memories\...\memory\）。
> 状态：**已提交**（两批分开：`1d070f2` = R5 节点采集（本任务开始前工作区里的遗留批，
> 已验证 135 测后先提）；`213bb6b` = probe-substore 两阶段，151 测全绿。§6 的拆分
> 拍板已按「两批各自整理后分开提交」执行完毕）。

---

## 0. 一句话现状

用户的 Rust 探针项目（`D:\ChatGPT\代理测活`，mihomo-test 的 Rust 重写，R5 已提交）现在自带一个
**进程内嵌的 Sub-Store**：`crates/probe-substore/` 用 rquickjs 0.14（QuickJS）跑官方
`sub-store.min.js`（脚本引擎版 bundle，vendor 2.42.2），复刻的是
`sinspired/subs-check-pro`（Go）的 `substore/` 目录架构。两种入口：

```bash
# 独立入口（只跑 Sub-Store）
probe-cli --root <dir> substore --port 8299

# 集成入口（config.json 里 "substore": {"embedded": true} 时，serve 一个进程带起 探针API + 内嵌Sub-Store）
probe-cli --root <dir> serve --host 127.0.0.1 --port 8088
```

embedded 开启时，探针的 collection fetcher（`AppState.backend`）自动指向
`http://<listen><backend_path>`；`SUBSTORE_BACKEND` 环境变量仍最高优先（指向独立实例不需改配置）。

## 1. 背景与决策链（为什么是这个形态）

- 用户评估过 sinspired/subs-check-pro：**不能替代自建探针**（缺链式测活/收敛账本/护栏/ipmap），
  但其「内嵌 Sub-Store 无 Node」的架构被用户拍板复刻（原话「先写了再说 后续再修」）。
- 总控文档（`GLM_5.3_Flash_mihomo_probe_Rust_full_rewrite.md`）两条硬约束仍然有效：
  ①不碰官方 Sub-Store 核心，经远程订阅/Script Operator 接入；②多格式转换交给官方 producer。
  本 crate 满足：跑的就是官方 bundle，只是宿主换成了 Rust。
- 分层约束（根 Cargo.toml 注释）：`probe-domain <- config/dns/mihomo/substore/storage <- engine <- api/scheduler/cli`。
  probe-substore 位于 substore 层，**不得**依赖 probe-engine/probe-api。

## 2. 交付物清单

**probe-substore crate（本阶段新建，15 测全绿）**

| 文件 | 内容 |
|---|---|
| `src/engine.rs` | QuickJS 引擎：共享 AsyncContext + 请求级 dispatch Mutex；`$httpClient`→reqwest 桥（spawn 任务重入 `ctx.async_with` 投递回调）；`task_local CURRENT` 携带每请求 Scope（body 寄存器+$done oneshot+closed 标志）；170s 中断处理器兜底 |
| `src/init_js.rs` | Loon 六件套 shim 模板（`$loon/$script/$request/$argument/$persistentStore/$notification/console/$httpClient/$done`）+ 自写 URL/URLSearchParams + TextEncoder/Decoder + atob/btoa + crypto.getRandomValues/randomUUID + setTimeout（host 定时器重入）；`build_init_script(bundle)` 把 bundle 内联进 `__run_sub_store_script()`（函数作用域=每请求状态重置） |
| `src/kv.rs` | `KvStore` trait + `JsonKvStore`：`sub-store.json` 主键每读直读磁盘、写走 tmp+rename pretty（preserve_order 保序）；其余键内存 + 2s 防抖落盘 `sub-store-cache.json`。忠实移植 Go loon_store.go |
| `src/assets.rs` | vendor 兜底（`include_str!` 1.37MB bundle）+ 磁盘副本优先 + GitHub releases 更新（ghproxy 只包下载 URL 不包 API）+ 前端 dist.zip 暂存目录扁平化安装（官方 zip 嵌套目录）+ 秘密路径生成持久化 |
| `src/server.rs` | axum 独立端口：CORS 全放行（预检反射请求头 + PNA）、秘密路径前缀路由（或 host=="sub.store"）、`/download/*` 脱钩合并（key=method|url|UA|cors，watch channel 合流）、`/api/utils/env` 注入（**显式创建 meta.node.env 链**，见 §4 坑3）、前端静态 + SPA 回退 + 缺失非 html 404 |
| `assets/sub-store.min.js` + `assets/BACKEND_VERSION` | vendor 的官方 2.42.2，勿手改；更新走 `assets::update_backend` |

**集成改动（第二阶段）**

- `crates/probe-config/src/lib.rs`：新增 `SubStoreSection`（embedded/listen/backend_path/gh_proxy/auto_update/push_service），
  `default_tree()` 加 `"substore"` 节，`Config.substore` 字段。**默认 embedded=false 整节 no-op**；
  Python 对未知键宽容，共享 config 双侧合法。
- `crates/probe-api/src/lib.rs`：`ServeConfig`/`AppState` 增 `substore: Option<probe_substore::SubStoreConfig>`；
  `serve()` 内 `tokio::spawn(probe_substore::serve(...))`，**失败只 error 日志降级**（探针 API 存活）。
- `crates/probe-cli/src/main.rs`：`substore` 子命令（独立入口）；`cmd_serve` 解析 embedded →
  `resolve_backend_path` 预解析 → backend 自指向 → 传入 ServeConfig。
- `crates/probe-substore/src/lib.rs`：`resolve_backend_path(data_dir, explicit)` 公开接口（调用方与 serve 共享同一条持久化路径）。
- 根 `Cargo.toml`：workspace members 加 probe-substore；tokio features 加 `"fs"`（勿删）。
- `workstreams/09_SUBSTORE_EXPORT.md`：本阶段完整记录。

## 3. 关键语义与运行时契约（改代码前必读）

1. **请求串行**：一个 AsyncContext，`execute()` 全程持 dispatch Mutex。并发下载*在一个请求内部*
   照常扇出（$httpClient spawn reqwest 任务）。要真并行 → 换 per-request runtime（Go 的做法），
   `execute` 签名不变。
2. **$done 双通道**：`__done` 绑定发 oneshot 给 Rust；`$done` 的 JS 侧同时调 `__resolve_done()`
   解析 runner promise。execute 的 match：外层 `tokio::time::timeout(190s)`，内层
   `Ok(())` → 等 rx；`Err` → js_error（message+stack 都取）。
3. **中断处理器**：`deadline_override` 非 0 时替代默认 170s（仅测试用）；中断异常会被 runner 的
   try/catch 接住转成 promise reject。
4. **KV 双文件契约**：`"sub-store"` 键 = Sub-Store 全部配置（读必走磁盘）；其他键 = 缓存。
   这是 Sub-Store 在 Loon 上的存储模型，勿改语义。
5. **`/download/*` 合并**：UA 参与合并 key（不同客户端生成不同订阅，上游语义）；leader 脱离客户端
   连接跑完，订阅者用 watch 收结果；leader 完成后按 Arc::ptr_eq 清理 map 项。

## 4. 已踩的坑（修过，别再踩；改 engine/init_js 前重读）

1. **rquickjs 0.14 HRTB 闭包**：`ctx.async_with(...)` 必须用**原生 async 闭包**
   `async move |ctx| {...}`。写成 `|ctx| async move {...}` 或显式标注 `|ctx: rquickjs::Ctx|`
   都会报 "lifetime may not live long enough"（Ctx<'js> 对 'js 不变）。闭包参数是借用 `Ctx<'js>`，
   **不能逃出闭包**——spawn 出去的任务一律持有 `engine.ctx.clone()`（AsyncContext，Clone+Send）。
2. **rquickjs 0.14 其他 API 形态**：函数注册用 `Function::new(ctx.clone(), closure)`（没有 `Func::wrap`）；
   参数助手 `Opt<T>`/`Rest<T>`；`ctx.eval(code.as_str())`（要 `&str`）；`Promise::into_future::<()>()`
   在 async_with 内直接 await；错误文本用 `Err::<(),_>(e).catch(ctx)` + `CaughtError::Exception`
   的 `.message()`/`.stack()`（QuickJS 的 stack 不含 message 行，**两个都要取**）。
3. **npm url-polyfill 在 QuickJS 完全不可用**：它整个解析器靠 `document.createElement("a")` 让浏览器
   解析 href。已弃用 vendor，`init_js.rs` 里自写了无 DOM 的 URL/URLSearchParams
   （RFC3986 + 特殊 scheme 默认端口 + IPv6 方括号 + 相对解析四形态 + dot-segment + opaque path）。
   扩展时跑 `url_and_btoa_available` 测试（17 项断言）。
4. **meta 必须是 JS 字符串字面量**：Go 版 `strconv.Quote(JSON)`，init.js 里 `JSON.parse(resMetaJson)`。
   直接传对象字面量 → `JSON.parse("[object Object]")` → SyntaxError → 回调永不触发 →
   请求挂到 179s 被 in-JS guard reject（症状：看似死锁）。见 `deliver()` 的 `meta_literal`。
5. **env 注入路径**：Loon 版 bundle 的 `/api/utils/env` 只回 `data.meta.loon`（Node 版才有
   `meta.node`）。注入器必须**显式创建** `data.meta.node.env`（Go 版同款行为），否则前端拿不到
   `SUB_STORE_FRONTEND_BACKEND_PATH`。
6. **MSVC 工具链**：本机构建必须 `cargo +stable-x86_64-pc-windows-msvc`（默认 gnu 缺 gcc；
   rquickjs 0.14 = quickjs-ng，MSVC 直接编译通过，无需额外配置）。
7. **前端 dist.zip 嵌套**：官方 zip 内容在版本子目录下，`ensure_frontend` 解到 staging 再
   `single_root` 扁平化；直接解到 frontend/ 会得到「index.html 不在根」的假失败。

## 5. 验证方式（全部通过后再交）

```bash
# 构建 + 全部测试（当前 17 个套件全绿，其中 probe-substore 15 测含真 bundle 验收）
cargo +stable-x86_64-pc-windows-msvc test --workspace

# 脱敏门禁（提交前必跑，用户惯例）
python tools/scan_secrets.py

# 实机冒烟（单进程形态）
#   临时目录写 data/config.json: {"substore":{"embedded":true,"listen":"127.0.0.1:18301"},"auth":{"token":"..."}}
#   probe-cli --root <dir> serve --port 18088
#   curl http://127.0.0.1:18088/healthz                                   -> {"ok":true}
#   curl http://127.0.0.1:18301/<data/substore/backend-path.txt 内容，加前导/>/api/utils/env
#                                                                         -> version 2.42.2, backend Loon
#   独立入口同理：probe-cli --root <dir> substore --port 8299
```

## 6. Git 状态（⚠️ 工作区混着两批未提交改动）

最后提交 = `a52d058`（R5 RoundCtx）。当前未提交分两批：

- **本任务（probe-substore 两阶段）**：`crates/probe-substore/`（新）、`probe-api/{Cargo.toml,src/lib.rs}`、
  `probe-cli/{Cargo.toml,src/main.rs}`、`probe-config/src/lib.rs`、根 `Cargo.toml`/`Cargo.lock`、
  `workstreams/09_SUBSTORE_EXPORT.md`。
- **先前遗留（R5/R6 会话的未收尾工作，不是本任务的，勿混提）**：
  `crates/probe-engine/src/{lib.rs,round.rs,collect.rs,measure.rs}`、`crates/probe-source/`（整个目录未跟踪）、
  `smoke-root/`。
- 重叠点：根 `Cargo.toml`/`Cargo.lock` 同时含两批改动（members/deps 混在一起），hunk 级拆分可行但费劲。
  **建议交用户拍板**：要么两批各自整理后分开提交，要么确认 R5 尾巴已稳定后一把提。提交前跑 scan_secrets。

## 7. 下一阶段任务（按建议优先级）

1. **hk3 部署拍板与迁移（动生产，需用户在场）**：备份导出现有独立 Sub-Store
   （订阅/collection/air 七步 process 脚本，用 Sub-Store 自带 backup）→ 导入 embedded 实例 →
   compose 开 `substore.embedded` → 验证手机 air 链路 → 留回滚（独立实例先别删）。
   详见记忆 `subs-check-pro-evaluation` 的替代风险清单。
2. **字节码缓存**：现在每请求重跑 bundle（函数作用域重置），无编译缓存。Go 版用 quickjs-go 的
   EvalBytecode。rquickjs 侧先评估 `Context::compile`/module bytecode 是否覆盖 script 形态；
   不行再考虑「N 个预 warm 的 runtime 池」。动手前先在真实负载下测一次每请求耗时再决定值不值。
3. **cron 移植**：`SUB_STORE_PRODUCE_CRON`（订阅缓存预热）与 gist 同步（`/api/sync/artifacts`），
   蓝本 = Go `loon_server.go` 的 `StartSubStoreCronJobs`（本地 HTTP 自调 + SkipIfStillRunning 语义，
   no-redirect client + 浏览器 UA）。
4. **`$notification` 补 Apprise 渠道**：现在只有 Bark 式 GET-replace（`[推送标题]/[推送内容]`），
   Go 版还有 appprise 模式可移植。
5. **可选：probe-api 同端口嵌入**：当前 Sub-Store 是独立端口（与 subs-check-pro 一致，订阅 URL 干净）；
   若部署侧（cloudflared/反代）只想开一个口，再加 `server::router` 嵌进 probe-api 路由的选项。
6. **R 系主线**：本 crate 不阻塞 R8（Rust 导出/rollback/snapshot）；继续按总控推进，R8 的
   Script Operator 契约测试可直接拿 probe-substore 当 Sub-Store 实例用。

## 8. 红线

- 不碰 `sub-store.min.js` vendor 文件；升级只走 `assets::update_backend`（semver 比较）。
- `serde_json` 的 `preserve_order` 是 shadow-run 字节级对齐的前提，勿删。
- workspace tokio 的 `"fs"` feature 是 probe-substore 依赖，勿删。
- probe-substore 不得反向依赖 engine/api（分层约束，根 Cargo.toml 注释）。
- 提交前：`scan_secrets.py` 必跑 + 留意 §6 的两批改动不要混提。
- 涉及 hk3 生产切换的操作必须先有备份与回滚路径，并让用户确认（参考仓库脱敏历史重写的教训）。
