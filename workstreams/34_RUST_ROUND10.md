# 34 — 「继续迭代 10 轮」（R0..R9，用户点名批次）

> 用户指令「继续迭代 10 轮」的执行台账。R0 审计把剩余真实缺口列成清单，
> R1..R8 逐批落地，R9 收尾审计 + 推送。每批独立提交、门禁全绿。

## 轮次清单与状态

| 轮 | 内容 | 提交 | 门禁 |
|---|---|---|---|
| R0 | 审计：FEATURE_INVENTORY + Python-only 面 → 清单（alert 缺失、ipmap 决策、CI/Docker、面板摘要） | 本文件 | — |
| R1 | alert 通知核：`AlertSection` + `probe-engine::notifier`（冷却只记送达、全失败按 error、双通道 stub 测试 4 条） | `feat(alert)` | 813 测全绿 |
| R2 | 轮末接线：CLI/API 在行关闭后、发布前调 `maybe_alert_after_round`（suspect→error、alive_floor→warn，与 `_maybe_alert` 同位） | 同上 | 同上 |
| R3 | GUI 探测点面板：总览页「探测点」卡（completed/missing/offline + 最少完成数，缺/离线高亮） | `feat(gui/api)` | 815 测全绿 |
| R4 | 聚合摘要持久化：migration **0015** `rounds.probe_summary` + `set/latest_round_probe_summary` + CLI/API 写入 + `/api/v1/status` `probes` 键 | 同上 | 同上 |
| R5 | 轮级聚合集成测试：远程活拉回本地死节点（FakeTester 死 + 远程 alive → converge ok）+ 兼容模式 no-op 契约 | `test(round)` | 817 测全绿 |
| R6 | ipmap 移植决策：21 号 §4——extension 独立运维工具保持 Python，移植列 P2 待办（范围草图在案） | docs | — |
| R7 | Dockerfile Rust 构建段：probe-cli 进运行镜像；默认 CMD 不变（切换留 compose override） | `build(docker)` | — |
| R8 | 21 号全面刷新（本文件 + §4/§5） | docs | — |
| R9 | 收尾审计 + socks5 推送 | 见 CHANGELOG | 817 测全绿 |

## 验收映射（每条 = 树内指名测试）

| # | 行为 | 测试 | 状态 |
|---|---|---|---|
| 1 | 双通道送达 + 冷却落盘 + payload 形状 | notifier `both_channels_deliver_and_the_cooldown_is_recorded` | covered |
| 2 | 冷却窗内跳过；投递失败不消耗窗口 | `cooldown_skips_within_the_window_and_failures_do_not_consume_it` | covered |
| 3 | alert.enabled=false 永不发送 | `a_disabled_alert_section_never_sends` | covered |
| 4 | suspect → error + alive_floor → warn，健康零告警 | `the_round_end_verdict_fires_the_right_levels` | covered |
| 5 | 摘要落列 + 最近轮读取 | probe-storage `the_round_summary_round_trips` | covered |
| 6 | 兼容模式摘要读 None | `a_missing_summary_reads_as_none` | covered |
| 7 | 远程活拉回本地死节点（轮级集成） | round `a_remote_pass_aggregates_into_the_convergence` | covered |
| 8 | 兼容模式轮内聚合 no-op | round `the_compat_round_skips_the_aggregation_summary` | covered |

## 门禁（R9 收口实跑）

cargo test 34 目标 **817 passed / 0 failed**（809 + 8）；clippy `-D warnings`
零告警；fmt --check；scan_secrets clean。

## 诚实边界

- 轮末告警的 Telegram 真实投递属 NOT_RUN（网络/凭据；协议行为由 127.0.0.1
  stub 钉住）。
- Dockerfile 的 Rust 构建段未在本机构建（无 docker）；CI ubuntu 构建覆盖。
- ipmap 移植（P2）与生产双跑切换（≥2 周）需部署环境与用户在场。
