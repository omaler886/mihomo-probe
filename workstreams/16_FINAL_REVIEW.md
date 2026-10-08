# 最终审查（持续更新至 §21 门禁）

## 目的
按总控 §21 完成定义逐项留证；未达标项如实标注。

## 当前状态：未完成（进行中）
R0~R3 已交付（R0=6360e31，R1=4e5c1af，R2=87a710d，R3=控制器补全）；R5 已交付
（分层并发闸门 + 轮次编排 + 节点采集/test_one）；R6/R7 已交付（前置池+链式两遍测试、
整轮护栏+节点收敛）；**R4 已交付（补做批，2026-10-08）**：`probe-dns`
（DoH+ECS 手写报文 + 8 个二进制 fixture），但**未接入 engine**——`classify_and_expand`
仍是 TODO，所以轮次仍"一个 server 测一个地址"。R8（发布/PublishDecision）未开始。逐项门禁现状：
| 门禁 | 状态 | 证据 |
|---|---|---|
| Rust 干净环境构建/启动/健康检查 | 部分达成：本机 cargo fmt/clippy/test --locked 243 通过 + CLI 冒烟（含 /healthz /readyz）；容器化待 R12 | TEST_REPORT_RUST.md |
| 真 Mihomo 端到端测活 | 未达成（本机无内核；Rust 切片只到 controller reload，真内核待 vps） | - |
| 旧数据无损迁移 | Python 旧库→Rust 迁移演练通过（备份/保全/Python 复读/回滚守卫）；真部署库迁移待 vps | TEST_REPORT_RUST.md R2 |
| shadow 双跑差异 | 未开始（R10） | - |
| 整轮异常不清空订阅 | Python 侧有护栏与测试锚定；Rust 待 R7 | test_logic |
| Sub-Store 双路径 | Python 侧达成（554 测试含契约）；Rust 待 R8 | test_substore_bridge |
| token 不入 API/日志/指标/历史 | 本机工作区 + 新增内容已由 scanner 验证 | tools/scan_secrets.py |
| Docker Socket 移除/supervisor | 方案 ADR-0002 已定，实施 R12 | workstreams/12 |
| 强制测试与静态检查 | Python 568 通过 / 2 跳；Rust 243 通过 + fmt/clippy -D warnings 干净 | TEST_REPORT_RUST.md |
| 一条命令回滚 | Python 现网回滚=git revert + compose rebuild；Rust 切换后复验 | - |

## 外部方案核对（2026-10-01）
对象：`D:\mihomo-probe-go-migration-review.md`（外部产出的 Go 合并方案）。
结论：**不采纳**（ADR-0004，见 02）。其 P0/P1 清单逐条核对如下——注意该方案
自述"未能通过 `git clone` 拉取仓库并在本地执行完整测试"，判断基于公开页面，故多处已过时。

| 方案条目 | 方案判断 | 仓库实际状态 | 证据 |
|---|---|---|---|
| P0-1 Git 历史凭据泄漏 | 可能含 UUID/REALITY/token | **已清** | `tools/scan_secrets.py --history` 扫 17 个 revision → clean，exit 0（2026-10-01 实测） |
| P0-2 Docker Socket 过大 | 需引入 socket-proxy | 属实但**已有方案** | `docker-compose.yml:76` 仍挂载；ADR-0002 三步退役（12） |
| P0-3 状态 API 泄密 | 需白名单 DTO | **R0 已修** | `redacted_config` 扩展掩码 + Rust `/api/v1/status` 零秘密；StatusRedactionTest×4 |
| P1-4 `mixed_port` 缺省 | 会 KeyError | **R0 已修** | DEFAULTS + 夹取 (1024,65535)；MixedPortDefaultTest |
| P1-5 测试统计口径 | 需机器可读 | **部分**：已并入 13，R10 落地 | ci.yml 现为文本输出 |
| P1-6 模块职责混合 | 禁止巨型 `engine.go` | **已满足** | 6 个 crate 单向依赖（02） |
| §2.2 三级测试管线 | 建议 | **已具备** | `_test_phases` → `test_one` → `_verify_chain_payload` → `_verify_egress`（06） |
| §2.3 分层并发 | 建议 | **已并入**（R5 实现） | 06 |
| §2.4 配置哈希跳过 reload | 建议 | **已评估，不采纳**：每轮仅 1 次 reload，命中率 100% 也只省 1 个 PUT | `engine.py:1250` 单一调用点（不在循环内）；见 04 |
| §2.5 SQLite PRAGMA / 索引 | 建议 | **已满足，无缺口** | `probe-storage/src/lib.rs:21-23`；`0001:31,61`；见 08 |
| §9 整轮护栏 7 条 | 建议 | **7 条中 6 条已具备**，1 条待 ADR | 03 |
| §10 Prometheus 指标 | 建议 | **已并入**（R9 实现） | 11 |
| §11 CI 门禁 | 建议 | **部分已具备**，Go 专用命令已换 Rust 等价物 | 14 |

**已并入的 6 项**：13（Golden 夹具清单 / Shadow 差异报告 / 机器可读汇总）、06（分层并发）、
03（护栏 7 条 + `inconclusive` 候选）、11（指标清单）、14（CI 门禁对照）。
**核对后无可执行缺口**：§2.5（PRAGMA/索引）——含 `foreign_keys=ON` 一项，因 schema 无外键而不适用（08）。
**列 R5 待评估**：无（§2.4 已评估完毕，结论为不采纳，见 04）。
**未并入**：语言与架构部分（Go 重写、Mihomo 源码树合并、单二进制）——ADR-0004 已否决。

## 结论
- 不得宣称"迁移完成"。默认实现仍为 Python；Rust 为 shadow。
