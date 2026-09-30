# DNS/ECS

## 目标
移植 doh.py 双视角 DoH+ECS，建立二进制 fixture 与 fuzz。

## 当前实现审计（doh.py，140 行）
- 手写报文：build_query（随机 id、RD、OPT+ECS option、idna 标签编码）；parse_ips（读 QD/AN 计数、_skip_name 支持压缩指针、A=4B/AAAA=16B 边界校验、解析失败跳过）。
- resolve_views：逐视角 A+AAAA、异常→空列表（不拖垮整轮）、视图内去重保序；调用方（engine._resolve_candidates）只缓存"至少一个视角有答案"的结果。
- 缓存：db.domain_views（domain→views json，TTL=cache_hours 6h；未来时间戳视为不可用重解析）。
- 已知边界：_skip_name 对指针环（指向自身）会死循环风险——当前用 offset>=len 保护，环指针会一路 +2 越界终止（内存安全由 Python 界保证）；Rust 解析器必须显式防环+限跳数。
- 未做：响应 ID/RCODE 校验（parse_ips 不校验 ID、不区分 NXDOMAIN 与空 answer）——Rust 侧按总控 §6 补齐并保持兼容（旧结果字段不变）。

## 待办清单
- [ ] R4：hickory-proto 或手写解析器 + 截断包/恶意长度/指针环/坏标签 fixture
- [ ] R4：正/负响应 TTL 缓存；CNAME 链记录
- [ ] R4：any/all 聚合策略已存在（verify.domain_pass），Rust 侧入 engine

## 测试证据
- Python: tests/test_logic.py DomainViewCacheTest；fixture 二进制包 R4 建 fixtures/dns-packets/。

## 风险与回滚
- 未实现，无。

## 下一步
- R4。
