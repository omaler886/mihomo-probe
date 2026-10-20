# 出口验证

## 目标
真实出口验证（L2）与深度质量检测（L3）的 Rust 移植设计。

## 当前实现审计
- L2（engine._verify_egress + core.egress）：存活节点经 lanes（各 select 组 + loopback mixed 入站，IN-NAME 钉扎）拉 Cloudflare trace，取 loc/ip/colo；出口国别 ∈ exclude_countries（默认 CN）→ exit_<CC> 判败；未完成验证 + 配了国别过滤 → 本轮 suspect 不发布（宁可少发布不放过误杀）。
- 真拉流校验（_verify_chain_payload）：链式/前置在延迟 204 后各拉一次真实页面，任何完整 HTTP 响应算过；拨号错/超时/TLS reset → payload_fail（front → front_dead 连坐链式变体）。
- ipmap（ipmap.py）：独立一次性内核做节点↔出口 IP 映射（v4/v6 回显端点分组、族匹配防 CDN 污染、trace loc/colo 交叉印证、直落/中转判定、名称标注写回 Sub-Store）。
- 入口定位：classify_and_expand 用 ip-api batch（缓存 ip_geo 表）判入口国别；入口 CN 排除（entry_cn）。

## 设计决策
- Rust：lane 机制进 probe-mihomo；ExitIdentity {ip, country, asn, colo} 存储 exit_history 表（R2 schema）。
- L3（多次采样/延迟分布/限流小流量吞吐）为新增，ADR+流量硬上限先行，R9 批次。

## 待办清单
- [ ] R6：egress/trace/fetch 移植 + unverified/over_limit 语义对拍
- [ ] R9：L3 与质量评分（总控 §9，缺测项不当满分/零分）

## 测试证据
- Python 锚点：EgressUnverifiedTest/LaneIndependenceTest/ChainPayloadVerifyTest。

## 下一步
- R6。

## 纯质量统计与评分（Quality Worker，2026-10-09）
- 范围：新增 `crates/probe-domain/src/quality.rs`、`quality/tests.rs`，仅在 `lib.rs` 增加 `pub mod quality;`；保留已有 `DomainPass` 和其他并行修改。未修改 manifest、lockfile、引擎、DNS、Python 或部署配置。
- API：`summarize_samples(&[QualitySample])` 返回可序列化统计；`evaluate_quality(samples, current_availability: Option<bool>, &QualityConfig)` 返回统计、归一化分项、权重覆盖率和可选总分。所有行为为纯计算，无 IO、时钟或新增依赖。
- 数值约定：延迟使用毫秒，分位数为 type-7 线性插值（零基下标 `(n-1)*p`）；抖动为按输入顺序过滤后的成功延迟相邻绝对差均值，至少两个有效延迟；出口稳定性为众数身份占比，至少两个成功且有身份的样本。失败计入成功率，其延迟/出口不进入成功统计；缺失值保持 `None`。
- 吞吐约定：完成的有界传输必须属于成功样本，实际字节数与有限耗时均须大于零；聚合 Mbps = `sum(bytes) * 0.008 / sum(elapsed_ms)`，不是各次速率的算术平均。缺测吞吐不生成零值，不能表示的聚合值返回显式错误。引擎仍负责实际流量和时间预算。
- 评分约定：六项默认等权，吞吐默认权重零；所有权重必须有限且非负。延迟/抖动按 `reference/(reference+value)`，吞吐按 `value/(reference+value)` 映射，参考值必须有限且为正。默认中点为 median 200 ms、p95 500 ms、jitter 50 ms、throughput 100 Mbps，可配置。
- 缺测处理：覆盖率为已观测权重/配置总权重，默认最低 0.5（含边界）；仅在覆盖率达标且存在正权重观测时输出 `[0,100]` 总分。全缺测、全零权重均无总分；当前可用性单独传入，不从历史成功率推断。极小覆盖率可能按 f64 精度下溢到零，但显式零门槛下仍保留已观测分项的加权均值。
- 测试源码：26 个确定性 `#[test]`，涵盖空/单/奇偶样本、分位数、输入顺序抖动、失败与缺测、出口稳定性、吞吐单位、非法数值/NaN/Infinity、单调性、参考值、覆盖率、极端权重、分数边界、Serde 往返。
- 实际命令与结果：`rg -c '^#\[test\]' crates/probe-domain/src/quality/tests.rs` 返回 `26`；限定拥有路径的 `git diff --check` 返回 0（仅已有 `lib.rs` 的 LF/CRLF 提示）。`cargo --version` 与 `cargo test --offline --locked -p probe-domain` 均返回 1：rustup 无法创建 `C:\Users\ckai7\.rustup`，`os error 183`，没有进入编译或运行测试。未搜索机器工具链、变更全局设置或尝试绕过沙箱。
- 验证限制：上述 26 项是已编写测试，不是通过计数；编译、测试与 rustfmt 检查待父任务在可用工具链环境执行。建议先运行 `rustfmt --edition 2021 crates/probe-domain/src/quality.rs crates/probe-domain/src/quality/tests.rs`，再运行 `cargo test --offline --locked -p probe-domain` 与 `cargo clippy --offline --locked -p probe-domain --all-targets -- -D warnings`。
- 剩余集成：引擎需采集真实 L3 样本、提供独立当前可用性并执行传输预算；配置层需接入权重/参考值，存储/API 需接入统计与分项。此纯模块不改变 alive/dead、整轮保护或发布策略，也不代表 R9 或全量交付已完成。
