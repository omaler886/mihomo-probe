# 测试与兼容

## 目标
共享 fixture、双跑比对、fuzz（总控 §16）。

## 基线（2026-09-30，HEAD=03cb6ea）
- 命令：`python -m py_compile mihomo_test/*.py`（干净）；`python -m unittest discover -s tests -v`。
- 结果：**Ran 554 tests, OK (skipped=2)，38.797s**。
- 跳过项：test_live 中需要 MIHOMO_TEST_LIVE=1 的真内核用例（网络隔离设计，符合"Cloudflare 不作为测试成功必要条件"）。
- 环境注记：本机 Windows 无 Docker——`mihomo -t`/容器路径用例靠 mock；真内核门禁在 vps 执行。

## 测试分类盘点
- test_logic.py（2910 行）：策略/重试/HTTPS 判定/链式真实拉流/失败分类/reload 兜底/prepare/别名折叠/导出/YAML 引号/链式 dialer 组/轮次模式与清理锁/配置 patch/schema/禁用源/孪生键/DNS 缓存/迁移/分类统计/裁剪/源 key/联动/仪表脚本/BuildConfig/自引用/入口分类/出口未验证/轮次预算/链式/前置池/内核拒绝/链失败分类/应用与发布/时间戳/中止轮/剥 ECH/孤儿回收/推送。
- test_hardening.py：token 保障、validate_patch、鉴权矩阵（三通道/publish 域/空 token）、HTTP 面（401/安全头/泄密）、轮锁、watchdog、excluded streak、CDN 构建。
- test_live.py：真内核集成（门控）/账本完整性/车道独立性/Sub-Store 联动/完整轮。
- test_substore_bridge.py：九字段契约/信封/端点/脚本。
- test_alerts_lanes.py、test_ipmap.py：告警/车道配置/ipmap 纯函数。

## 双跑比对设计（R10）
- fixtures/ 语言无关（configs/subscriptions/normalized-nodes/mihomo-responses/dns-packets/exit-responses/rounds/databases/expected）。
- 比对维度：节点总数/fingerprint/配置拒绝集/延迟成功集/出口验证集/失败分类/状态转换/guard 决策/导出集合/API payload；延迟数值允许波动，集合与语义必须一致。
- 已知行为差异候选（须在 R10 前裁决）：config_test 无 docker 时 Python 跳过 vs Rust 显式失败（见 04）。

## 待办清单
- [x] R0：基线记录
- [x] R0：P0 修复回归测试（14 例）
- [x] R1：Rust 侧切片单测（config/storage/mihomo/api，见 TEST_REPORT_RUST.md）
- [ ] R2：fixtures 目录与首批共享夹具
- [ ] R10：双跑差异报告

## 下一步
- R2 fixtures。
