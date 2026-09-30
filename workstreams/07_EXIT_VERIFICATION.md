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
