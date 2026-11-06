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
| G-1 | canonical 唯一订阅产物（`data/exports/mihomo-alive.yaml` + meta） | P4 已落地（canonical_export.py + engine 接线） | **已落地（G1-R1，workstreams/24）**：`probe-engine::canonical` 纯函数模型 + 15 条不变量测试，digest 跨实现同字节串；发布链路接线属 G-2 |
| G-2 | Gist 固定四文件 + canonical 幂等门禁（digest 未变零 PATCH） | P4 已落地（gist_store.py） | **已落地（G2-R1，workstreams/25）**：`FilesPublisher` + `canonical_publish` 四文件装配与场景测试；round 流程接线与 G-3 同批 |
| G-3 | 多探测点聚合（probes[]/aggregation/any_alive + probe agent CLI） | P3 已落地 | 无（Rust scheduler 是单进程本地模型） |
| G-4 | 人工 Sub-Store 推送三件套（preview/confirm-token/push） | **P5 已落地**（manual_publish.py 三步 + 防重放 + 删自动回灌；P6 发布中心 UI） | 无（Rust 有自动 push 双门禁，语义不同） |
| G-5 | 死亡逻辑 HK3 语义（front_dead 不推 streak） | P3 已修复并契约锁死 | 本就符合（round.rs:1198-1214，20_PLAN 裁决 2） |

### 2.2 既有平价缺口（19_ACCEPTANCE_MAP 实测记录，非新发现）

| # | 条目 | 状态 | 证据 |
|---|---|---|---|
| G-6 | 相同节点不同显示名只拨号一次 | partial：账本/发布折叠已实现，「一次拨号」未实现 | 19_ACCEPTANCE_MAP §11.1-4 |
| G-7 | Idempotency-Key HTTP 级重放 | partial：claim 语义已钉，无端点级重放测试 | §11.1-8 |
| G-8 | 100 节点规模发布场景 | partial：测试用 1-2 节点 | §11.2-1 |
| G-9 | pending→dead 三轮跨轮集成 | partial：两段各自 covered，未串 | §11.2-3 |
| G-10 | 双进程/双端口并发触发 | partial：DB 互斥 + 单进程测试传递 | §11.2-5 |
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
