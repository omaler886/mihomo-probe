# DNS/ECS

## 目标
移植 doh.py 双视角 DoH+ECS，建立二进制 fixture 与 fuzz。

## 当前实现审计（doh.py，140 行）
- 手写报文：build_query（随机 id、RD、OPT+ECS option、idna 标签编码）；parse_ips（读 QD/AN 计数、_skip_name 支持压缩指针、A=4B/AAAA=16B 边界校验、解析失败跳过）。
- resolve_views：逐视角 A+AAAA、异常→空列表（不拖垮整轮）、视图内去重保序；调用方（engine._resolve_candidates）只缓存"至少一个视角有答案"的结果。
- 缓存：db.domain_views（domain→views json，TTL=cache_hours 6h；未来时间戳视为不可用重解析）。
- 国别批量查询：`lookup_countries` / `_fetch_country_batch`（`ip-api.com/batch`，90 个一批、
  `urlopen(timeout=45)`、失败重试一次、错误文本截 80 字符），缓存 `db.ip_geo`。
- 已知边界：`_skip_name` 只有一个 `while True`，靠 `offset >= len(data)` 退出——
  **指针环不会死循环**（环指针每轮 +2，越界即返回），是"靠数据边界兜底"而非显式防环。
  Rust 侧改为具名的 `MAX_NAME_JUMPS` 上限，并在 doc 里写明该上限约束的是畸形 label 链
  （指针只被跨过、从不被解引用）。
- 未做：响应 ID/RCODE 校验。Rust 侧**补了 RCODE**（非零即 `WireError::Rcode`，NXDOMAIN
  不再与"空 answer"混同），**ID 仍不校验**——与 Python 一致，随机 id 只用于发问、
  响应从不与之比对（`resolver.rs` 的 `rand_id` 注明）。调用方可见行为不变：
  `resolve_views` 对"查询失败"与"无记录"同样产出空视图。

## 待办清单
- [x] R4：手写解析器（不引 hickory-proto——报文构造/解析是本模块的全部价值，引库反而
      要绕开它的 API 才能塞 ECS）——落点 `crates/probe-dns/src/wire.rs`：
      `build_query` / `parse_ips` / `skip_name`。
- [x] R4：截断包/恶意长度/指针环/坏标签 fixture —— 落点
      `crates/probe-dns/fixtures/dns-packets/`（8 个二进制包，`tools/make_dns_fixtures.py`
      可复现生成、每次写出同样字节），断言在 `wire.rs` 的 `mod fixtures`。
      其中「恶意长度」「坏标签」是内联测试原本没覆盖的两类。
- [ ] R4：正/负响应 TTL 缓存；CNAME 链记录 —— **未做**。Python 侧也没有：`parse_ips`
      丢弃 TTL 与 CNAME，缓存（`db.domain_views`，6h）归调用方。Rust 侧同理，缓存留给 engine。
- [ ] R4：any/all 聚合策略接入 engine（Python 的 `_score_bucket(bucket, domain_pass)`）
      —— **未做**。`probe-engine` 尚未依赖 `probe-dns`，`classify_and_expand` 仍只是
      `probe-source` crate doc 里的一个 TODO。这是 R4 剩下的主要部分。

## 测试证据
- Python: tests/test_logic.py DomainViewCacheTest。
- Rust（本批）：`cargo test -p probe-dns` → **22 通过 / 0 失败**（`wire` 18、`geo` 3、
  `resolver` 1）。全量 `cargo test --workspace --locked` → **243 通过 / 0 失败**。
  详见 TEST_REPORT_RUST.md 的 R4 节。

## 风险与回滚
- 纯新增 crate，**无调用方**（`probe-engine` 未依赖它）→ 回滚 = 删 `crates/probe-dns/`、
  `Cargo.toml` 的成员行、`Cargo.lock` 条目；Python 默认路径零影响。

## 已知限制（本批如实记录）
- **未接入任何调用方**：全仓库 grep 只有 `probe-dns` 提到自己，`probe-engine` 没依赖它。
  这批只交付 transport，一轮仍然「一个 server 测一个地址」。
- **无日志接缝**：Python 的 `query`/`resolve_views` 不记日志、`_fetch_country_batch` 把
  错误文本交给调用方打印。Rust 同样返回 `Result<_, String>` 而不自己 `tracing::warn`
  ——照抄 Python 的分工，接入 engine 时要记得把字符串落到 `events` 表。
- **非 ASCII 域名是真实分歧**：Python 的 `label.encode("idna")` 会成功（中文域名能解析），
  Rust 无 `idna` 依赖、直接拒绝 → 该域名视图为空。订阅里实际都是 ASCII
  （见 `wire.rs` 的 `encode_label`）。

## 下一步
- R4 收尾：`probe-engine` 依赖 `probe-dns`，移植 `classify_and_expand` +
  `_resolve_candidates`（含 `domain_views` 6h 缓存）与 `lookup_countries` 的 `ip_geo` 缓存。
