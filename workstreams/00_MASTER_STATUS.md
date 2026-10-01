# 主控状态（Master Status）

## 当前阶段与总体状态
- 阶段：R0~R3 已交付（R3=Mihomo 控制器补全）；**R5 的并发闸门 + 轮次编排已交付**
  （`probe-engine`：`limits` + `round`，已接进 cli/api）；R4、R5 的节点采集未开始。
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
| 05 | DNS/ECS | R0 审计完成 |
| 06 | 探测引擎 | R0 审计完成 |
| 07 | 出口验证 | R0 审计完成 |
| 08 | 存储迁移 | R0 审计完成 |
| 09 | Sub-Store 导出 | R0 审计完成 |
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

## 当前失败测试
- 无。Python 568 通过 / 2 跳过；Rust **79 通过**（R3 后 45，R5 并发 +13，R5 编排 +17）。

## ADR 索引
- ADR-0001 存储驱动：切片用 rusqlite(bundled)，R2 迁移落地时复评 sqlx（理由见 08）。
- ADR-0002 Docker Socket 退役路径：三步走（见 12）。
- ADR-0003 / 0003b 迁移框架与表改名时机（R2 定稿，见 08）。
- ADR-0004 语言选型：维持 Rust，否决 Go 重写（2026-10-01，见 02；外部 Go 合并方案已评估）。
- ADR-0005（待写）轮次 `inconclusive` 态：引入前先落 ADR（见 03）。

## 下一批可并行任务
- R5：节点采集（Sub-Store → fingerprint → 变体）—— 没有它，闸门限不到任何东西。
- R5：`test_one` 语义移植（HTTPS 优先 / max_attempts / 超时升级 / TERMINAL_REASONS）。
- R5：用真机轮次数据复核四层并发阈值（现为比例初值）。
- R4：probe-dns（DoH + ECS + 二进制 fixture + fuzz，依赖 05）。
- R7：整轮保护 GuardDecision/PublishDecision（依赖 03）。
- 前端适配 /api/v1（依赖 10 契约冻结）。
