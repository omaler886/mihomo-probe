# 21 — Rust 替代 Python 的完整性评估（持续更新）

> 目标语义（用户目标「C/Rust 完全替代」在本仓库的操作化定义）：本仓库**没有第一方 C 代码**
> （全库 `.c/.h` 均为 `target/` 内 rquickjs 依赖自带的 QuickJS 源，属构建产物）。系统语言
> 轨道 = `crates/probe-*`（Rust 完全重写，11 crates），替代对象 = `mihomo_test/`（Python
> 遗留实现）。「完全替代」= Rust 侧功能覆盖 ≥ Python 侧（以 FEATURE_INVENTORY 100 项 +
> HK3 指导新增项为准），且生产流量切到 Rust 二进制后 Python 侧可退役。
> 评估基线：commit `f8fadae` + P4 工作区（2026-11-06）。**每条缺口都给证据，不凭印象。**

## 1. 现状快照

| 维度 | Python 侧 | Rust 侧 |
|---|---|---|
| 测试 | 815 passed / 2 skipped（P4 后实跑） | 709 passed（`f8fadae` 基线实跑，P4 未触 Rust） |
| 部署 | vps:/srv/mihomo-test（生产） | ct186 probe-cli（round 44 全绿，CHANGELOG_RUST） |
| 角色 | HK3 改造主线（20_HK3_LITE_PLAN 裁决 1） | 保持绿 + 生产行使职责 |
| 覆盖 | FEATURE_INVENTORY 全量（F-01..） | MIGRATION_MAPPING 100 项映射 + 引擎/发布/存储核心 |

## 2. 缺口清单（Rust 尚未覆盖 Python/HK3 的部分）

### 2.1 HK3 轻量化新增面（本轮 P4 之后形成的新差距）

| # | 能力 | Python 侧状态 | Rust 侧状态 |
|---|---|---|---|
| G-1 | canonical 唯一订阅产物（`data/exports/mihomo-alive.yaml` + meta） | P4 已落地（canonical_export.py + engine 接线） | **已落地（G1-R1，workstreams/24；round 接线 + 本地 parity 落盘 = workstreams/27）** |
| G-2 | Gist 固定四文件 + canonical 幂等门禁（digest 未变零 PATCH） | P4 已落地（gist_store.py） | **已落地（G2-R1，workstreams/25；round 流程接线 = workstreams/27，CLI/API + WebDAV 通道 = workstreams/26）** |
| G-3 | 多探测点聚合（probes[]/aggregation/any_alive + probe agent CLI） | P3 已落地 | **语义闭环（workstreams/28 + 31 + 32）**：聚合纯函数 + probes[]/aggregation config + migration 0013/0014 + HMAC 签名/五类校验 + agent API 端点；dispatch 签名发送 + `probe-cli agent` 模式；轮末聚合消费（远程观测→aggregate_round→folded 改写，inconclusive 不推进状态机）+ deadline 登记表 RESULT_LATE 生效。API 侧 dispatch 发送已接线（workstreams/33，与 CLI 同构）；剩余：真实跨进程时序 NOT_RUN |
| G-4 | 人工 Sub-Store 推送三件套（preview/confirm-token/push） | **P5 已落地**（manual_publish.py 三步 + 防重放 + 删自动回灌；P6 发布中心 UI） | **已落地（workstreams/29）**：核心（token 注册表/diff/update-only push）+ API 三端点 + publish_audit；GUI 按钮接线属 30 批 |
| G-5 | 死亡逻辑 HK3 语义（front_dead 不推 streak） | P3 已修复并契约锁死 | 本就符合（round.rs:1198-1214，20_PLAN 裁决 2） |

### 2.2 既有平价缺口（19_ACCEPTANCE_MAP 实测记录，非新发现）

| # | 条目 | 状态 | 证据 |
|---|---|---|---|
| G-6 | 相同节点不同显示名只拨号一次 | accepted 偏差（19_ACCEPTANCE_MAP §7 B6-2）：账本/发布折叠已实现，「一次拨号」是既有行为决策，不改 | §11.1-4 |
| G-7 | Idempotency-Key HTTP 级重放 | **covered（workstreams/31）**：`parity_gaps.rs` HTTP 级重放同 run_id | §11.1-8 |
| G-8 | 100 节点规模发布场景 | **covered（workstreams/31）**：100 节点 1 artifact + 重跑零写（parity_scale_and_cross_round.rs） | §11.2-1 |
| G-9 | pending→dead 三轮跨轮集成 | **covered（workstreams/31）**：三败跨轮（pending→dead 状态机 × 发布腿串测，真实账本） | §11.2-3 |
| G-10 | 双进程/双端口并发触发 | **covered（workstreams/31）**：两条独立 SQLite 连接（= 双进程）并发 POST，恰一起一拒（parity_gaps.rs） | §11.2-5 |
| G-11 | 24 小时 soak | NOT_RUN：结构禁令 + 零写测试替代 | §11.3-3 |

## 3. 结论与路线（后续迭代执行）

1. **短期（P4-P7）**：Python 主线完成 HK3（P5 人工 Sub-Store → P6 Web → P7 清理）。
   Rust 侧保持 `cargo test/clippy/fmt` 全绿不动 —— 每轮迭代都复跑。
2. **中期（P7 后启动 Rust 收口）**：按 G-1→G-2→G-3 顺序把 canonical/Gist/聚合模型移植到
   Rust（新 workstream 逐项立项，验收复用 19_ACCEPTANCE_MAP 的门禁写法），然后 G-4。
3. **长期（切换条件）**：G-1..G-4 关闭 + G-6..G-10 收口 + 生产双跑 ≥2 周无回归后，
   Python 侧进入只读维护并宣布「Rust 完全替代」达成；G-11 soak 在切换前执行。
4. **诚实声明**：在 2/3 完成前，任何「已完全替代」的说法都不成立 —— 当前真实状态是
   「双轨并存，Rust 覆盖核心测活/发布/存储，Python 覆盖 HK3 全部新增面」。

## 4. 独立运维工具的移植决策（workstreams/34 R6）

| 工具 | 类别 | 决策 | 理由 |
|---|---|---|---|
| ipmap（F-05/F-35..F-40） | extension（独立 Docker 工具，与轮次完全隔离） | **保持 Python 运行，移植列 P2 待办**：订阅拉取（UA 门禁）+ 族过滤 + 一次性内核容器 + 回显探测 + 报告渲染 + 写回 Sub-Store，是完整子系统（依赖 docker.sock 的独立容器编排）；移植不含入轮次路径，收益是运维工具单语言化，风险是 docker 编排重写 | 21 号目标「Rust 完全替代」的操作化定义覆盖轮次/发布/存储主链；运维旁路工具单列 |
| link_substore.py | adapter（F-001 收口后降级） | 保持（只读代理面） | 已降级为最小面 |
| setup_tunnel.py | 部署辅助 | 保持 | 一次性部署动作 |

## 5. 轮末告警（workstreams/34 R1+R2）

Python notifier.py 已在 Rust 闭环：`AlertSection`（telegram/webhook/cooldown/
alive_floor）+ `probe-engine::notifier`（冷却**只记送达**——投递失败不消耗
窗口；全失败按 error 记录）+ CLI/API 轮末接线（suspect → error、
alive_floor → warn，与 `_maybe_alert` 同位）。token/chat_id 沿用 config 内
存储（config.json 不入库，Python 既有 schema 的诚实平移）。
