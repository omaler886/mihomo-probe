# 主控状态（Master Status）

## 当前阶段与总体状态
- 阶段：R0~R3 已交付（R3=Mihomo 控制器补全）；**R4 已交付（补做批，2026-10-08）**：
  `probe-dns`（DoH+ECS 手写报文 + 8 个二进制 fixture）——但**未接入 engine**
  （`classify_and_expand` 仍是 TODO），见 05；R5 已交付（并发闸门 + 轮次编排 +
  RoundCtx + 节点采集/test_one）；R6 引擎侧已交付（前置池 + 链式展开 +
  两遍测试）；**R7 已交付**：节点收敛（`probe-domain::policy` 五态折叠 +
  `probe-storage::converge_nodes` 单事务落账 + history）+ `GuardDecision`
  三态护栏 + `round_is_suspect` 存活下限 + ADR-0005 `rounds.inconclusive`
  （migration 0003）。R8（发布/PublishDecision）未开始。
  另：内嵌 Sub-Store（probe-substore，rquickjs 跑官方 bundle）已落地并接入 serve，
  字节码缓存实测评估为不建（见 09）。
- 原则：Python 实现保持为默认运行路径；Rust 以 shadow 双跑方式逐步逼近门禁。
- 总控需求文档：`GLM_5.3_Flash_mihomo_probe_Rust_full_rewrite.md`（已入库）。

## 基线事实（2026-09-30）
- commit SHA：03cb6ea（R0 之前）；分支 main。
- 运行环境：Windows 10.0.26200 / Git Bash；Python 3.12.10（默认）与 3.13.14；rustc/cargo 1.98.1；本机无 Docker。
- 基线测试：`python -m py_compile mihomo_test/*.py` 干净；`python -m unittest discover -s tests -v`
  → **Ran 554 tests, OK (skipped=2)，38.8s**。输出存 `.tmp_diag/baseline_unittest.txt`（不入库，.gitignore 覆盖）。
- 限制：容器相关验证（`mihomo -t`、compose 栈）在本机无法执行，需在 vps 侧复核；记录为受限项而非通过项。

## 工作流索引与所有权（防止并行冲突）
| 文件 | 主题 | 状态 |
|---|---|---|
| 01 | 仓库审计 | R0 完成 |
| 02 | 目标架构 | R0 草案定稿 |
| 03 | 领域状态机 | R0 审计完成，设计待 R2 |
| 04 | Mihomo 控制器 | R0 审计完成 |
| 05 | DNS/ECS | R4 交付（transport + fixture）；engine 接入待做 |
| 06 | 探测引擎 | R0 审计完成 |
| 07 | 出口验证 | R0 审计完成 |
| 08 | 存储迁移 | R0 审计完成 |
| 09 | Sub-Store 导出 | R0 审计完成；内嵌 Sub-Store 已落地（probe-substore），R8 导出待做 |
| 10 | API/调度 | R0 审计完成，P0 修复落点 |
| 11 | 可观测性 | R0 审计完成 |
| 12 | 安全加固 | R0 完成 P0 批次（S-12/mixed_port/query token/scanner/轮换台账/Socket 方案） |
| 13 | 测试兼容 | R0 基线记录完成 |
| 14 | 部署发布 | R0 完成 install.sh 门禁修复 |
| 15 | UI 产品 | R0 审计完成 |
| 16 | 最终审查 | 汇总页，持续更新 |

## 依赖图（串行约束）
- 领域模型（03）定稿 → 数据库固化（08）。
- API 契约（10）定稿 → UI 适配（15）。
- 双跑兼容（13）达标 → 默认实现切换（14）。
- 安全高危清零（12）→ 任何发布。

## 已合并提交
- R0：审计工作台 + P0 整改（见 CHANGELOG_RUST.md R0 条目）。
- R1：Rust workspace + 纵向切片。
- R2：SQLite 迁移体系（migrations/ + probe-cli db 五子命令 + Python 兼容保证）。
- R3：Mihomo 控制器补全（lanes/select/egress/fetch/`mihomo -t` 校验器/culprit 定位）。
- R5（部分）：`probe-engine` 分层并发闸门（global/per_source/per_server_ip/diagnose
  + RoundCtx 取消语义）；13 个新测试。
- R5（部分）：轮次编排 `probe-engine::round`——闸门接入 cli/api，轮次行「调用方开、
  runner 必关」，结果**单事务**批量写 `results`；新增 `TestConfig` 与
  `record_results`/`finish_round_with_counts`。Rust 测试 58 → **79**。
- R5（部分）：RoundCtx 接进轮次流水线（a52d058）。
- R5：节点采集 + test_one 重试语义（1d070f2）——`probe-source` 新 crate
  （fetch/identity/prepare/source/subscription），`collect.rs` 单点装配
  fetch→flatten→prepare→jobs，`measure.rs` 移植 test_one；jobs 不再恒空。
  Rust 测试 79 → **135**。
- 内嵌 Sub-Store（213bb6b）——probe-substore：rquickjs 宿主跑官方 bundle 2.42.2，
  独立/集成双入口；详见 09。Rust 测试 135 → **151**。
- Sub-Store cron（b35d753）——Gist 同步 + produce 缓存预热（loon_server.go
  移植，SkipIfStillRunning + no-redirect 自调）。Rust 测试 151 → **159**。
- R6 前置池 + 链式展开 + 两遍测试（本批）——`probe-config::ChainSection`
  （chain 段 + SourceSpec.direct/chain + publish.prefix）、`probe-source`
  `SubAdmin`（upsert/delete，axum stub 测试）+ `Role` 三层透传、
  `probe-engine::collect::collect_fronts/expand_chains`、`round::run_phases`
  两遍（front 先测→链式限活前置→front_dead 未拨判败）；CLI `--mode` +
  API 挂接。Rust 测试 159 → **196**。
- R7 整轮护栏 + 节点收敛（本批）——`probe-domain::policy`（五态折叠纯函数 +
  `round_is_suspect`，无时钟无 IO）；`probe-config::PolicySection`（三阈值）；
  `probe-storage`（migration 0003 `rounds.inconclusive`、`converge_nodes`
  单事务折叠 + `node_state_history`、`previous_alive_count` 跳过 suspect/
  inconclusive、`finish_round_full`）；`probe-engine`（`GuardDecision`
  ApplyConvergence/PreservePreviousState/MarkRoundInconclusive、
  `fold_verdicts` any-alive 口径、suspect 轮 note + 轮级标记）。ADR-0005 裁决
  落稿（03）。Rust 测试 196 → **219**。
- R4 probe-dns（补做批）——`wire`（手写报文：`build_query`/`parse_ips`/`skip_name`，
  三处有意硬化见 05）、`resolver`（DoH GET + `resolve_views`）、`geo`（ip-api 批量
  查询，45s 超时 + 一次重试 + 错误文本 80 字符）、`fixtures/dns-packets/`（8 个二进制包
  + `tools/make_dns_fixtures.py`）。**未接 engine**，无调用方。Rust 测试 219 → 221
  （车道分桶修复 +2）→ **243**。

## 当前失败测试
- 无。Python 568 通过 / 2 跳过；Rust **243 通过**（R3 后 45，R5 并发 +13，
  R5 编排 +17，R5 采集 +56，Sub-Store +16，cron +8，R6 链式 +37，
  R7 护栏 +23，车道分桶修复 +2，R4 probe-dns +22，axum/tokio net feature 为
  probe-source stub 测试引入）。

## ADR 索引
- ADR-0001 存储驱动：切片用 rusqlite(bundled)，R2 迁移落地时复评 sqlx（理由见 08）。
- ADR-0002 Docker Socket 退役路径：三步走（见 12）。
- ADR-0003 / 0003b 迁移框架与表改名时机（R2 定稿，见 08）。
- ADR-0004 语言选型：维持 Rust，否决 Go 重写（2026-10-01，见 02；外部 Go 合并方案已评估）。
- ADR-0005 `rounds.inconclusive` 轮次态：**已裁决**（2026-10-05，见 03——
  轮级标记、非第六节点态；migration 0003；`previous_alive_count` 跳过；
  Python 读方保守兼容）。

## 下一批可并行任务
- R5 收尾：用真机轮次数据复核四层并发阈值与 policy 阈值（现为比例/经验初值）。
- Sub-Store 侧（见 09）：`$notification` 补 Apprise；hk3 部署切换（需用户
  在场，先备份迁移）。
- R4 收尾（见 05）：engine 接入 `classify_and_expand` + `domain_views`/`ip_geo`
  缓存 + any/all 聚合；`cargo-fuzz` 未接。
- R6 收尾（见 07）：出口验证 + ipmap 落地映射。
- R8：发布与 PublishDecision（suspect 轮保留上轮发布的执行面；export_snapshots
  表已就位）。
- 前端适配 /api/v1（依赖 10 契约冻结）。
- 口径差异待对账（见 13「已知口径差异」）：轮行 total 去重口径、收敛折叠
  展示列（proto/server/country/ip）暂缺、inconclusive 列 Python 保守兼容。
