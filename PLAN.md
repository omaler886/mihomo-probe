# PLAN — mihomo-probe → 官方 Sub-Store 架构移植（2026-09-28）

- 官方基线：`sub-store-org/Sub-Store` 后端 @ `e08f1b1`（前端 `4bd7b0a`、文档 `8d3c62b`），本项目基线 commit `B1`（安全整改后），测试基线 479 用例 / 0 失败 / 2 预期跳过。
- 主线决策：官方**无任何测活/落地检测能力** → 采用官方路径 B（probe 作为远程订阅上游，零改动复用）+ 路径 A（Script Operator 消费 probe API 做过滤/改名/标注）；**拒绝路径 C**（fork 官方核心，operator 注册表硬编码 + AGPL + 上游日更，维护成本不可接受）。
- 新增扩展面（最小侵入、全部独立模块，不碰官方核心一行代码）：只读端点 `GET /api/probe/nodes`（publish token 作用域）+ 适配层 `mihomo_test/substore_bridge.py` + Sub-Store 脚本 `substore_bridge/probe_filter.script.js`（`operator(proxies, targetPlatform, context)` 签名，兼容官方 `$arguments` 传参约定）。
- 既有 100 项功能逐条映射见 `MIGRATION_MAPPING.md`（reuse 3 / enhance 8 / extension 68 / adapter 6 / deprecate 15）；核心测量逻辑与 UI、网络、存储解耦现状保持，不绑定单一订阅源/机场/域名。
- 不改官方前端：本迁移无新 UI 需求（自有面板已存在；官方 Vue 前端是订阅管理端，不承载测活面板），frontend-impl 免建。
- 批次：B1 基线（`B1`，已含 S-04~S-08 安全整改）→ B2 第一轮成果+映射+计划 → B3 扩展面实现+独立测试 → B4 架构文档+迁移指南+变更台账定稿。
- 每批原子提交；`CHANGELOG_MIGRATION.md` 记 SHA/目的/影响/回滚；`TEST_REPORT.md` 跟踪门禁。
- 测试门禁：离线全量 `python -m unittest discover -s tests` 不回退（基线 477 过/2 跳）；新增模块独立单测；JS 经本机 Node 24 做语法 + 行为校验。
- 已知风险与未决：S-12（/api/status 携带 bot token）不在本批修，记录于 SECURITY_REVIEW；`core.mixed_port` 缺省陷阱（DEFAULTS 无此键，新环境 KeyError）记入 ARCHITECTURE；凭据轮换与 git 历史清洗是外发前置动作，记入 MIGRATION_GUIDE。
