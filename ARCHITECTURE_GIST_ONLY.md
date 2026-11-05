# ARCHITECTURE_GIST_ONLY — Gist 托管目标架构（B7 交付物）

> 依据：《mihomo-probe 精简重构指导：单一测活产物、Gist 托管、独立GUI》§0–§3、
> 《最终功能清单》F-001/F-002/F-003。本文描述**目标态**与当前实现（HEAD `13c2889`
> + 重构批次 B1/B2/B4/B5/B2接线，批次史见 `CHANGELOG_REFACTOR.md`）的对应关系。
> 运维操作另见 `MIGRATION_GIST_ONLY.md`；逐条验收映射见
> `workstreams/19_ACCEPTANCE_MAP.md`。

## 0. 一句话

Sub-Store 从「测活结果数据库」降级为「只读上游 + 稳定 URL 消费者」；每个逻辑订阅
只保留一个不可变 `source_id`、一个 `artifact`、一个固定 Gist 文件；测活历史只进
SQLite；GitHub Gist 首建一次、之后只 PATCH 固定文件。

## 1. 数据流

```text
上游订阅 / Sub-Store collection          （只读：GET）
          |
          v
probe-source  拉取 -> 规范化 -> 指纹身份 (fingerprint = 连接参数，排除显示名)
          |                                       crates/probe-source/src/{fetch,identity,prepare}.rs
          v
probe-engine  测活（真实 mihomo 内核）-> 跨轮收敛 -> 护栏（suspect alive-floor）
          |                                       crates/probe-engine/src/{collect,round}.rs
          v
发布门禁  guard? -> select_source_exports -> 内容寻址 sha256(canonical yaml)
          |                               crates/probe-engine/src/publish.rs
          |
          +------------------+---------------------------+
          v                  v                           v
   SQLite 历史          本地导出（缓存/回退）        Gist 固定文件
   rounds/results/      data/exports/<sha256(key)>-  PATCH /gists/{gist_id}
   node_state_history/  <sha256(content)>.yaml       （首建一次，之后永不 POST）
   quality_samples/                                  crates/probe-publish/src/gist.rs
   artifacts/source_runs   |                           |
                           +-----------+---------------+
                                       v
                    稳定订阅 URL: GET /artifacts/{source_id}/subscription
                    （本地最后成功快照，ETag/304）      crates/probe-api/src/sources.rs
                                       v
                          Sub-Store / 任何客户端 只订阅这一个 URL

独立 GUI (/gui) <-> /api/v1/* 只调 probe API；不直连 Sub-Store，不碰 GitHub Token
                                       crates/probe-api/src/gui.rs
```

## 2. 一源一产物（F-003）

永久身份在账本，不在配置或名称里：

* `sources`：`source_id` 为随机 v4 UUID 主键，`key` 为唯一自然键。`display_name`、
  上游 URL、Sub-Store key 都可以改，改了也不会换 `source_id`
  （`crates/probe-storage/src/sources.rs::sync_source`：同名不同参=更新属性，
  key 不变则 source_id 不变）。迁移 0010 启动回填既有 key（`INSERT OR IGNORE` 幂等）。
* `artifacts`：`source_id` **UNIQUE**、`gist_file` **UNIQUE**、`last_content_sha256`、
  `last_success_run_id`、`last_published_at`。数据库层一个 source 永远只有一行
  artifact——这是 schema 承诺，`crates/probe-storage/src/sources/tests.rs::
  one_source_never_grows_a_second_artifact` 是绕过者会撞上的测试。
* 一轮只产生一个候选快照：`publish_round`（engine）每源至多一个 `Published`，
  内容寻址复用（同 sha 直接复用快照与不可变文件，不落新文件）。
* 运行簿记：`source_runs`（0011）+ `publish_status`（0012，词表冻结：
  `published|no_change|publish_failed|gist_config_error`），每源最多一个
  claimed/active 槽（部分唯一索引 `ux_source_runs_one_open`）。

## 3. 发布器层（F-002）

`crates/probe-publish`（commit `67317b0`，接线 `29cf9cd`）：

```text
Publisher trait (publish(&ArtifactRef, &str) -> NoChange | Published | Failed)
├── LocalPublisher   原子写导出根（本地是权威副本，Gist 是对外镜像）
├── GistPublisher    GitHub Gist 固定文件发布
└── CompositePublisher  local 成功后 gist；gist 失败随行不失败整体
```

`GistPublisher` 的硬规则（`crates/probe-publish/src/gist.rs`，全部有测试钉住）：

1. Token 只从 `publish.gist.token_file` 指向的文件读，构造期快速失败；Unix 上校验
   0600；token 不进 config.json/SQLite/日志/API（错误文本经 `redact` 脱敏）。
2. sha 相同 → 零请求返回 `NoChange`（无 revision、无 PATCH）。
3. 同 Gist 进程内写锁；锁内 GET 重读，PATCH 只触目标文件 + `manifest.json`，
   永不发送 `content: null`。
4. 401/403/404 终态不重试（token 轮换 / `gist init` 才能解）；429 尊重
   Retry-After 有界；5xx/网络退避 1s/2s/4s 有界；409/422 重读合并一次。
5. `Failed` 时上一成功产物（远端文件、账本 digest、稳定 URL）原样保留。

引擎接线 `publish_round_remote`（`crates/probe-engine/src/publish_remote.rs`）：

* `publish.provider = local`（默认）→ 零构造、零请求、零写入——行为与接线前一致。
* 每个本地 `Published` 的源：`NoChange → source_runs.publish_status=no_change`
  且**不写 artifact**（digest 只在真发布后推进）；`Published → mark_artifact_published`
  （digest/run id/gist id）；`Failed → publish_failed + error_code`。
* 发布器构造失败 → 每个合格源 `gist_config_error`，轮次结果不受影响。

## 4. 单飞（F-021/F-022）

* 每源：`claim_source_run`（幂等键 > 活动槽 > 最小间隔 > 新 claim），
  DB 部分唯一索引是最终互斥；进程崩溃留下的槽由调度 tick 的
  `reap_orphan_source_runs` 收尸（先护：round 仍 open 的槽不收）。
* 全局：`in_flight` CAS + 跨进程 `RoundLock`；调度器 Busy 直接拒绝。
* 幂等：`Idempotency-Key` 头（或 body 字段）在 24h 内重放返回同一 run_id
  （`crates/probe-storage/src/runs.rs`，API 解析在 `crates/probe-api/src/sources.rs`）。

## 5. 稳定订阅 URL（§3.2）

`GET /artifacts/{source_id}/subscription`（`crates/probe-api/src/sources.rs`）：

* 按永久 `source_id` 寻址（改名不影响 URL）；服务账本当前 active 快照的**逐字节**
  内容，空/失败轮永远不会让它变空。
* `ETag = "<sha256>"`；`If-None-Match` → 304 空体；凭据是只读 publish token
  （或 admin token），与 admin 面分离。
* 缓存策略目标 `public, max-age=300, stale-if-error=86400`。
  **已知缺陷 B6-1**：该头目前被 `security_headers` 中间件覆盖为 `no-store`
  （见 `workstreams/19_ACCEPTANCE_MAP.md` §7；`crates/probe-api/tests/
  subscription_endpoint.rs` 的 ignored 测试钉住期望行为，修复后取消 ignore）。
* Gist raw URL **不**写进 Sub-Store（含 revision 的 URL 会把消费者钉死在旧版本）；
  `stable_url` 列留给部署自己的对外地址，接线批明确不写 revision URL。

## 6. manifest.json

私密 Gist 的固定文件布局（`gist init` 建一次，commit `67317b0`）：

```text
mihomo-probe-exports (secret gist)
├── <key>-<source_id 前 8 位>.yaml   ← 每源一个固定文件
└── manifest.json                    ← 当前状态，不是历史
```

`manifest.json` 只记录 `schema` 与每源 `{file, sha256, node_count, published_at}`；
发布器在锁内 GET 重读→合并本项目条目→PATCH 回写，**外部条目（其他程序写的）
原样保留**，损坏的 manifest 拒写不重建（`merged_manifest` + 测试）。历史不进
Gist——历史在 SQLite。

## 7. Sub-Store 现在允许做什么 / 禁止做什么（F-001）

允许（只读角色）：

1. **作为上游输入**：`GET` collection/sub 的内容喂给测活
   （`crates/probe-source/src/fetch.rs::SubStoreClient`）。
2. **作为消费者**：客户端侧把一个 collection 的节点 URL 指到
   `/artifacts/{source_id}/subscription`——Sub-Store 只需要拉这一个稳定 URL。
3. 一次性迁移期间的**有限写**：仅 `probe-source::migrate` 的 update-only
   retarget / 隔离改名（默认禁用）/ 回滚，全程无 delete 方法（结构级禁删，
   `crates/probe-source/src/migrate.rs::MigrationStore`）。

禁止（代码里已关死，测试钉住）：

* 每轮 upsert Sub-Store：引擎 push 路径**双门禁**
  `publish.push_to_substore && publish.allow_legacy_substore_write`，两个默认
  false（`crates/probe-engine/src/publish.rs::push_round_exports`；Python 侧
  `mihomo_test/engine.py::push_exports` 同构双门禁）。
* 新建对象：即使双门禁打开也只能 update 已绑定 canonical 对象，缺失即报错不建
  （Rust `update_sub` 与 Python `store.update_only` 同语义）。
* 按轮次/时间戳/节点数拼订阅名；GUI 保存 GitHub token；测活失败时覆盖上一份
  可用产物。

## 8. 独立 GUI（B4，commit `b50a306`）

* `/gui` 内嵌 SPA（`crates/probe-api/src/gui.rs`），只调 `/api/v1/*` 与
  `/artifacts/*`；写操作（立即测活）走 `GET /api/v1/session` 换 CSRF 双提交 +
  admin token，全程 `record_event(security)` 审计。
* 已知弱点（B6 复核，详见 SECURITY_REVIEW.md 新增节）：CSRF token 是
  splitmix64（无 rand 依赖的权衡，记录在代码注释）；会话表内存 TTL 8h 上限 256。
* 旧面板 `/` 保持只读。

## 9. 模块与提交索引

| 关注点 | 模块 | 提交 |
|---|---|---|
| sources/artifacts/source_runs 表 + 回填 | `migrations/0010_source_artifact.sql`、`0011_source_runs.sql`、`0012_source_run_publish.sql`；`crates/probe-storage/src/{sources,runs}.rs` | 4eb3d20 (B1/B3) |
| Sub-Store 双门禁 + update-only | `crates/probe-engine/src/publish.rs`、`crates/probe-source/src/fetch.rs`、`mihomo_test/{config,store,engine}.py` | 4eb3d20 (B1) |
| 发布器 crate | `crates/probe-publish/src/{gist,local,composite,types}.rs` | 67317b0 (B2 核心) |
| 引擎远程腿 + CLI gist + API test 端点 | `crates/probe-engine/src/publish_remote.rs`、`crates/probe-cli/src/main.rs`（gist test/init）、`crates/probe-api/src/publishers.rs` | 29cf9cd (B2 接线) |
| 稳定订阅端点 + /api/v1/sources/runs | `crates/probe-api/src/sources.rs` | 4eb3d20 (B1/B3) |
| 独立 GUI + CSRF | `crates/probe-api/src/gui.rs` | b50a306 (B4) |
| Sub-Store 重复对象迁移 | `crates/probe-source/src/migrate.rs` | 4f27413 (B5 核心) |
| 验收映射 | `workstreams/19_ACCEPTANCE_MAP.md` | B6（本批） |
