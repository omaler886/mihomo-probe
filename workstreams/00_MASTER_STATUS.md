# 主控状态（Master Status）

## 当前阶段与总体状态
- 阶段：R0（基线审计 + P0 整改）与 R1（Rust workspace + 纵向切片）已交付；R2（迁移体系）未开始。
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

## 当前失败测试
- 无。Python 568 通过 / 2 跳过（R0 新增 14 例）；Rust cargo test 见 TEST_REPORT_RUST.md。

## 待决策 ADR
- ADR-0001 存储驱动：切片用 rusqlite(bundled)，R2 迁移落地时复评 sqlx（理由见 08）。
- ADR-0002 Docker Socket 退役路径：三步走（见 12）。

## 下一批可并行任务
- R2：SQLite schema/迁移（依赖 03 定稿）。
- R3：Mihomo Controller Rust 侧补全 delay/lanes（切片已含 version/reload）。
- 前端适配 /api/v1（依赖 10 契约冻结）。
