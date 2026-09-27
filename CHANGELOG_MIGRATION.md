# CHANGELOG_MIGRATION — 移植批次台账

> 格式：每批记录 提交 SHA / 目的 / 文件 / 功能影响 / 兼容影响 / 测试结果 / 审查 Agent / 回滚方法。
> 批次划分见 PLAN.md（B1 基线 → B2 文档 → B3 实现 → B4 定稿）。

---

## B1 `B1` — 基线入库 + 安全整改（2026-09-28）

- **目的**：把上一阶段全部未提交工作快照入库作为移植基线；按 SECURITY_REVIEW S-04~S-08 完成提交前整改。
- **文件**：mihomo_test/ 全部模块改动 + ipmap.py（新）+ web/（新前端）；tests/test_hardening.py、
  test_ipmap.py（新）、_isolation.py（新）、diag_round_suite.py（新）；tools/ 10 个脚本（新）+ 2 个改动；
  Dockerfile/README/.gitignore；夹具脱敏：tests/test_ipmap.py、tests/test_logic.py、tools/chain_verify.py。
- **功能影响**：无新功能（快照既有工作）；ipmap/前端/加固测试首次入库。
- **兼容影响**：.gitignore 新增 `.tmp_diag/ reports/ dom_*.html console_*.txt chain-alive-*.md`
  （S-04~S-07/S-10，敏感产物永久不入库）；测试夹具真实 UUID/IP/域名 → 文档地址（S-08，行为不变）。
- **测试结果**：脱敏后 `python -m unittest tests.test_ipmap tests.test_logic` → 369 用例 OK（skipped=1，
  Windows flock 预期跳过）。
- **审查 Agent**：security-license（第一轮）、reviewer（第二轮 §五 复核确认）。
- **回滚方法**：`git revert B1`（无数据迁移；.gitignore 行恢复后注意勿再提交敏感文件）。
- **遗留**：S-01~S-03 已入 git 历史的真实凭据轮换 + filter-repo 清洗 = 外发硬前置（MIGRATION_GUIDE §1）。

## B2 `B2` — 第一轮报告 + 计划 + 功能映射矩阵（2026-09-28）

- **目的**：固化第一轮四个并行子 Agent（upstream-research / legacy-audit / test-baseline / security-license）
  的调研证据；锁定官方基线 commit；建立 100 项功能的逐条映射（MIGRATION_MAPPING.md，编码前置门禁）。
- **文件**：FEATURE_INVENTORY.md、OFFICIAL_CAPABILITY_MAP.md、TEST_REPORT.md、SECURITY_REVIEW.md、
  PLAN.md、MIGRATION_MAPPING.md（全部新增）。
- **功能影响**：无代码。
- **兼容影响**：无。
- **测试结果**：TEST_REPORT 基线记录——`python -m unittest discover -s tests` 479 用例 / 477 过 / 0 失败 /
  2 预期跳过（28.5s），作为回归门禁基线。
- **审查 Agent**：四份报告即第一轮产物；reviewer 第二轮复核映射完备性（通过，R-01 统计数字除外）。
- **回滚方法**：`git revert B2`（纯文档）。

## B3 `B3` — 扩展面实现 N-01~N-03 + 架构契约 + 独立测试（2026-09-28）

- **目的**：按 ARCHITECTURE.md 契约实现移植的唯一编码范围：probe 账本只读端点 + payload 适配层 +
  Sub-Store Script Operator 脚本。
- **文件**：mihomo_test/substore_bridge.py（新，N-03）、mihomo_test/server.py（+37/-2，N-01：`_publish_scoped`
  谓词 + `GET /api/probe/nodes` 路由）、substore_bridge/probe_filter.script.js（新，N-02）、
  tests/test_substore_bridge.py（新，52 用例）、ARCHITECTURE.md（新）、MIGRATION_MAPPING.md（2 处修正）、
  TEST_REPORT.md（§10）。
- **功能影响**：新增 N-01/N-02/N-03 三个扩展点（见 REVIEW.md §四：状态全部「完成」，N-02 附真机验收保留意见）。
- **兼容影响**：publish.token 作用域从 `/api/export/`（前缀）扩到 `∪ /api/probe/nodes`（精确匹配）——reviewer
  逐行核对 `auth_ok`/`_matches`/`_presented_tokens`：admin 全通、空 token 拒绝、尾随路径/大小写/POST 语义
  零回归；新端点 9 字段白名单零凭据泄露（敌意键有测试钉死）。
- **测试结果**：全量 `python -m unittest discover -s tests` → 531 用例（479 基线 + 52 新增）/ 529 过 /
  0 失败 / 2 预期跳过；`node --check` 通过；JS harness 20 行为用例 + 端点 16 用例全绿（reviewer 独立复跑复现）。
- **审查 Agent**：tests（独立编写 52 用例，4 处裁量逐条核对「接受」）→ reviewer（有条件通过，无 P0/P1，
  条件为文档侧 R-01~R-06，已于 B4 全部落实）。
- **回滚方法**：`git revert B3`（三处新增整体移除；无数据迁移；`/api/export/*` 老行为不受影响）。

## B4 本提交 — 审查修正 + N-04 接入手册 + 台账（2026-09-28）

- **目的**：落实 reviewer 全部条件：R-01（决策统计程序化重算更正：reuse 3 / enhance 9 / extension 66 /
  adapter 6 / deprecate 16）、R-02/R-05（测试落点与 publish 作用域描述按实际实现更正）、R-04（§4.6 骨架补
  空入站早退行）、R-06（聚合行标注）；交付 N-04 MIGRATION_GUIDE.md（含外发硬前置与真机验收清单）与本台账。
- **文件**：PLAN.md、MIGRATION_MAPPING.md、ARCHITECTURE.md、SECURITY_REVIEW.md（§四整改状态回填）、
  MIGRATION_GUIDE.md（新）、CHANGELOG_MIGRATION.md（本文件）、REVIEW.md（新，审查报告入库）。
- **功能影响**：无代码。
- **兼容影响**：无。
- **测试结果**：无代码改动；批次结束时全量套件维持 531/529/2（reviewer 复跑口径）。
- **审查 Agent**：reviewer（本轮审查产物即 B4 输入；其建议后续动作 3/4——真机端到端验收与 CI Node 断言——
  记入 MIGRATION_GUIDE §3.4/§5，待部署环境执行）。
- **回滚方法**：`git revert <本提交 SHA>`（纯文档）。
