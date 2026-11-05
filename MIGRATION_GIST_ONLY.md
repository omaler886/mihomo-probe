# MIGRATION_GIST_ONLY — Gist 托管迁移运维手册（指导 §10 阶段 A–E）

> 面向操作者的分阶段手册。所有命令**以今天仓库里真实存在的入口为准**
> （基线 `13c2889`，B5 接线批次并行进行中——凡该批次尚未合入的能力都已标注）。
> 带有 **【需要用户在场】** 的步骤触碰生产（hk3 部署、真实 Gist、真实 Sub-Store、
> 客户端切换），不得由 Agent 无人值守执行。
>
> 架构背景见 `ARCHITECTURE_GIST_ONLY.md`；逐条验收证据见
> `workstreams/19_ACCEPTANCE_MAP.md`。

## 命令与开关速查

| 入口 | 作用 | 现状 |
|---|---|---|
| `probe-cli --root <dir> db migrate` | 账本 SQLite 迁移（备份→应用→校验；含 0010/0011/0012） | 已有 |
| `probe-cli --root <dir> db backup --out <file>` / `db rollback --from <file> --yes` | 账本备份 / 恢复 | 已有 |
| `probe-cli --root <dir> gist test` | 探测已配置的 Gist 目的地（`configured=`/`ok=`/`error_code=`，token 脱敏） | 已有（29cf9cd） |
| `probe-cli --root <dir> gist init` | 首建那一个专用 Gist；幂等（已配置且 healthcheck 通过 → `already-initialized`，绝不二次 POST）；**不代写 config.json**，打印要粘贴的片段 | 已有（29cf9cd） |
| `probe-cli --root <dir> migrate scan --prefix <P> --report <file>` | Sub-Store 重复对象只读扫描 → plan JSON（永不建、永不删） | B5 接线批次（并行进行中；B5 核心 `4f27413` 已提供 scan/apply/rollback 引擎） |
| `probe-cli --root <dir> migrate apply --plan <file> [--execute] [--quarantine-hours N]` | 按 plan retarget 到稳定 URL + 隔离改名；**不带 `--execute` 即 dry-run**（只出报告零写入） | 同上 |
| `probe-cli --root <dir> migrate rollback --report <file>` | 按 apply 报告还原（幂等） | 同上 |
| `python3 link_substore.py` | **降级目标**：只保留 scan/migrate/rollback（SSH 侧入口，与 Rust 扫描同算法）。HEAD 基线上它仍是旧的 create/`--remove` 面——**降级合入前不要在生产跑它的写模式**，定时任务必须先停（阶段 A） | 降级中 |
| `POST /api/v1/publishers/gist/test` | 同 `gist test` 的 API 面（admin token） | 已有 |
| `POST /api/v1/migrations/substore/{scan,apply,rollback}` | 迁移 API 面 | 基线为 404「未启用」降级；接线批次进行中 |

环境开关：

| 开关 | 默认 | 语义 |
|---|---|---|
| `ALLOW_LEGACY_SUBSTORE_WRITE` | false | 第二道门禁；与 `publish.push_to_substore` 同时为真才允许**更新**既有 canonical 对象（永不新建）。回滚期逃生口，平时保持关 |
| `GITHUB_TOKEN_FILE`（= `publish.gist.token_file`） | 无 | GitHub token 文件路径；文件权限必须 0600（Unix 强校验）；token 不进 config/DB/日志 |
| `publish.provider` | `local` | `local`（默认，无远程腿）\| `gist` \| `composite` |
| `publish.gist.{gist_id, public, timeout_seconds, max_retries}` | — | `public` 默认 false（secret Gist） |

## 阶段 A：止血

1. 确认双门禁默认关：`publish.push_to_substore=false` 且 `allow_legacy_substore_write=false`
   （或 env `ALLOW_LEGACY_SUBSTORE_WRITE` 未设置）。当前代码默认即如此，检查即可，无需改动。
2. **【需要用户在场】** 停止 `link_substore.py` 的定时执行（crontab/计划任务里注释掉该行）。
   在降级版合入前，该脚本的默认模式仍会 create/refresh。
3. 暂停自动清理，先只读盘点重复对象：
   `probe-cli --root <dir> migrate scan --report /tmp/scan-plan.json`（接线合入前用只读 API/人工盘点替代）。
4. **【需要用户在场】** 备份：SQLite、`config.json`、`data/exports`、Sub-Store 配置。
   账本侧命令：`probe-cli --root <dir> db backup --out /backup/state-<date>.db`（服务停或轮间执行）。
5. source_id 分配：无需手工——服务启动/round 前的 `sync_ledger_sources` 会为每个
   配置 key 分配稳定 UUID 并建 artifact（幂等，key 不变则 id 不变）。

**回滚 A**：恢复 crontab 即可（代码侧无变更；开关本来默认关）。

## 阶段 B：引入 artifact 层

1. `probe-cli --root <dir> db migrate`（应用 0010/0011/0012；additive，纯增表/列，幂等）。
2. `data/exports` 现有产物**不需要**导入：artifacts 只登记身份与摘要，内容仍在
   export_snapshots（0010 回填已把既有 key 全部入册）。
3. 验证一源一产物：`probe-cli --root <dir> db check`；SQL 复核
   `SELECT source_id, count(*) FROM artifacts GROUP BY source_id;` → 每行 1。
4. 内容摘要与 NO_CHANGE 跳过默认开启（`publish_round` 内容寻址复用 +
   `publish_round_remote` NoChange 不推进 digest）。
5. **暂时只发布到本地缓存，跑 3 个周期**：保持 `publish.provider=local`（默认），
   `probe-cli --root <dir> round` 跑 3 轮，观察 `source_runs.publish_status`
   （全为 published/no_change，无 publish_failed）。

**回滚 B**：`git revert` 对应迁移提交即可；彻底回退 schema：
删除 `sources`/`artifacts`/`source_runs` 三表并
`DELETE FROM schema_migrations WHERE version IN (10,11,12);`（旧二进制读新表会忽略，
一般不需要）。

## 阶段 C：Gist

1. **【需要用户在场】** 准备 token 文件（root-only，0600，Docker secret 亦可）：
   `install -m 0600 /dev/null /run/secrets/github_gist_token && chmod 600 ... && vi /run/secrets/github_gist_token`
   （fine-grained PAT，仅 gist 写权限）。配置 `publish.gist.token_file` 或 env `GITHUB_TOKEN_FILE`。
2. `probe-cli --root <dir> gist test` → 期望 `configured=false error_code=config`
   （未建 Gist 时这是正常起点）。
3. **【需要用户在场】** `probe-cli --root <dir> gist init` → 建一次专用 secret Gist；
   把打印出的 `"publish": {"gist": {"gist_id": "..."}}` 片段粘贴进 config.json。
   幂等：重跑且 healthcheck 通过 → `already-initialized`。
4. 把 `publish.provider` 改为 `gist`（或 `composite`），重启 serve。
5. 首次 PATCH 固定文件：跑一轮 `probe-cli --root <dir> round`，然后
   `probe-cli --root <dir> gist test` → `ok=true`。
6. **【需要用户在场】** 对比本地与 Gist raw 内容 SHA256：
   `curl -s <gist_raw_url> | sha256sum` 对比 `probe-cli --root <dir> db check` /
   artifacts.last_content_sha256。
7. **【需要用户在场】** 故障演练（只对测试源/测试期）：临时改错 token 文件内容
   （401）→ 期望 run 记 `publish_failed`+`gist_auth`、本地与稳定 URL 不变；
   恢复 token 后下一轮自愈。429/503 由发布器有界退避处理，不建议对真实 GitHub
   主动打；单测已钉（probe-publish stub 组）。

**回滚 C**：`publish.provider` 改回 `local`（env/config 均可），重启 serve。Gist 与
token 文件保留（无害）；如需彻底，删除 Gist（**【需要用户在场】**）并清空
`publish.gist.*`。稳定 URL 全程只读账本，不受影响。

## 阶段 D：Sub-Store 清理

1. dry-run 扫描：`probe-cli --root <dir> migrate scan --prefix "probe-" --report /tmp/plan.json`
   （分类 keep/duplicate/uncertain，节点摘要取回有上限，永不写）。
2. **【需要用户在场】** 人工确认 plan：每个逻辑源选一个 canonical 对象；
   `uncertain` 组默认不动。
3. dry-run apply：`probe-cli --root <dir> migrate apply --plan /tmp/plan.json`
   （**不带 `--execute` = 零写入报告**，逐条检查将发生的 retarget/隔离动作）。
4. **【需要用户在场】** 真正 apply：加 `--execute`。隔离改名 `deprecated-*` 依赖
   后端 rename 能力——真实后端默认保守禁用（`rename_supported=false`），因此
   旧对象会保留原名但 URL 已 retarget；确认报告后人工删除。
5. **【需要用户在场】** 从客户端确认订阅正常（走稳定 URL 的 collection 能拉通、
   节点数符合预期）。
6. **【需要用户在场】** 隔离 24 小时（`--quarantine-hours 24` 默认）确认无误后，
   删除重复对象并保存回滚报告（apply 报告 JSON 即回滚依据）。
7. 万一要回退：`probe-cli --root <dir> migrate rollback --report <apply-report>.json`
   （幂等还原 URL；改名回滚同样幂等）。

**回滚 D**：上一步的 rollback 命令；客户端 URL 切回旧值（apply 报告里有原文）。

## 阶段 E：独立 GUI

1. 先上线只读总览：`/gui` 的总览/详情页本来只读（B4 已交付）。
2. 再放开写操作（立即测活：admin token + CSRF 双提交 + 审计；B4 已交付，
   迁移 apply/回滚按钮在迁移 API 接线后自动生效）。
3. **【需要用户在场】** 旧 Python 面板切只读：停止面板容器/进程的写路径
   （部署层操作），保留只读查看。
4. **稳定 7 天后再删除旧写路径**——前置条件是验收红线全绿
   （`workstreams/19_ACCEPTANCE_MAP.md`；尤其「Sub-Store 写请求 = 0」与
   24h 对象数不增）。删除动作属未来批次，届时先跑
   `tests/test_substore_write_zero.py` 与全部门禁。

**回滚 E**：恢复旧面板进程/写路径（部署层），代码侧 revert 对应提交。

## 全局原则

* 每一步先 dry-run / 只读 / 备份，再写；每个写动作都有报告文件，每个报告文件
  对应一条幂等回滚命令。
* Agent 不自动操作生产 Sub-Store、生产 Gist、Cloudflare DNS、真实 Tunnel 或用户
  订阅（《最终功能清单》§12）；标了 **【需要用户在场】** 的步骤由人执行或当面授权。
* Token 生命周期：只住 0600 文件；泄漏时只轮换 token 文件内容——source_id、
  artifact、Gist id 全都不用动。
