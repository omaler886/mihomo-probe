# 目标架构

## 目标
按总控文档 §3 落地 Rust workspace，依赖方向单向，Mihomo 保持独立内核。

## 输入与依赖
01 仓库审计；总控文档 §3。

## 设计决策
- Workspace 布局（R1 已建立，见 crates/）：probe-domain / probe-config / probe-storage / probe-mihomo / probe-dns / probe-substore / probe-engine / probe-api / probe-scheduler / probe-observability / probe-supervisor / probe-cli。
- 依赖方向：domain ← config/dns/mihomo/substore/storage ← engine ← api/scheduler/cli。domain 不依赖 web/db/网络客户端。
- 技术栈（R1 锁定于 Cargo.lock）：tokio / axum / reqwest / serde+serde_json+serde_yaml / rusqlite(bundled) / tracing+tracing-subscriber / thiserror / sha2 / uuid / time / tokio-util。
  - 偏离说明：切片阶段用 rusqlite(bundled) 而非 sqlx——Windows 无系统 libsqlite，bundled 可复现构建；R2 迁移落地时复评（ADR-0001）。
  - hickory/prometheus client 在对应批次（R4/R9）引入，避免空壳依赖。
- 容器最终形态：mihomo-probe-rs（控制面）+ mihomo（独立内核）+ cloudflared（可选）+ probe-supervisor（可选，最小权限容器操作）。
- 配置合法性一律以 `mihomo -t` 为准；Rust YAML 反序列化成功不算数（R3 落地）。

## 待办清单
- [x] R1：workspace 骨架 + 首条纵向切片（config→controller→round→API）
- [ ] R2：SQL migration 体系（migrations/ 目录，替代 Python 的代码内迁移）
- [ ] R3：probe-mihomo 补 delay/lanes/egress
- [ ] R4：probe-dns（DoH+ECS+fixture fuzz）
- [ ] R5/R6：引擎与出口验证
- [ ] R7：状态机/整轮保护/原子发布
- [ ] R8：Sub-Store 兼容 API
- [ ] R9：调度/取消/指标
- [ ] R12：probe-supervisor

## 修改记录
| 时间 | 文件 | 变更 | 原因 |
|---|---|---|---|
| 2026-09-30 | Cargo.toml, crates/* | R1 workspace | 纵向切片 |

## 本机工具链注记
- 默认工具链 x86_64-pc-windows-gnu 缺 gcc/dlltool（rusqlite bundled、windows-sys 构建失败）；
  本机构建用 `cargo +stable-x86_64-pc-windows-msvc`（VS 2022 在位）。CI（ubuntu）无此问题。
- serde_json 启用 preserve_order：Python json.dumps 按文档序输出 proxy 字段，
  内核配置需与 Python 逐字节可比（快照测试锚定）。

## 执行命令与输出摘要
- `cargo test --workspace --locked`：见 R1 提交与 TEST_REPORT_RUST.md。

## 测试证据
见 13。

## 风险与回滚
- Rust 侧全部为新增目录，Python 默认路径不动；回滚=删除 crates/ 与 Cargo.toml。

## 阻塞项
- 无。

## 下一步
- R2 迁移体系 + probe-cli db migrate。
