# DNS/ECS

## 目标
交付可供父任务集成的 DNS/DoH 强校验实现，提供 ECS、IDNA、CNAME 链及正负响应 TTL。
本次工作者仅修改 `crates/probe-dns/**` 和本文件，不负责引擎、持久化缓存或部署。

## 输入与依赖
- 已阅读完整规范的 DNS/ECS 第 6 节、安全第 2.3/15 节、测试与质量门禁第 16/17 节。
- 沿用当前 `reqwest`、`base64`、`getrandom`、`tokio`、`thiserror`，测试沿用 `axum`。
- IDNA 使用 `reqwest::Url` 已有的转换能力；本工作者未添加依赖，也未修改任何 Cargo 清单或锁文件。
- 保留原始 8 个二进制 DNS fixture 的字节；未修改 Python 生成器、其他 crate 或部署数据。

## 当前实现审计
- 本轮之前：名称只跳过指针、不解引用；未核对响应 ID/问题；畸形 A/AAAA 长度被跳过；
  CNAME、TTL 被丢弃；NXDOMAIN 被当作普通失败；DoH 未限制 body、MIME 或重定向。
- 历史基础批记录过 22 个 crate 测试。该数字及旧的兼容断言不再代表当前实现；
  本轮实际结果见下方，不沿用此前工作区全量测试数字作为本次证据。
- 旧 `cname-then-a.bin` 实际把 A 放在 CNAME 所有者，而非其目标，违反 CNAME 排他性。
  现在明确断言其失败，另以合法的压缩/乱序 CNAME 链验证成功路径。
- 旧自指针夹具的成功断言改为 `InvalidPointer`；旧坏标签改为保留标签位错误，
  不是为了兼容而把畸形报文继续当成有效空结果。

## 公共接口
`probe_dns::wire` 导出以下契约，crate 根同时重新导出：

```rust
pub struct DnsAnswer {
    pub ips: Vec<String>,
    pub cname_chain: Vec<String>,
    pub ttl_secs: u32,
    pub negative: bool,
}

pub fn parse_response(
    data: &[u8],
    expected_id: u16,
    expected_name: &str,
    expected_qtype: u16,
) -> Result<DnsAnswer, WireError>;
```

- 新增 `DohResolver::query_answer(&self, name, qtype, view, timeout) -> Result<DnsAnswer, String>`。
- `query` 参数及 `Result<Vec<String>, String>` 保持不变，委托给 `query_answer`。
- `resolve_views` 保留逐视角 A/AAAA、视图内去重保序、失败留空及跳过空 resolver 的行为。
- `parse_ips` 保留为严格结构校验的旧入口，但其 ID/问题取自报文本身，
  **不能代替查询与响应的关联验证**；DoH 只使用 `parse_response`。

## 设计决策
- 名称：IDNA、ASCII 大小写归一、一枚绝对尾点及根名称；标签最多 63 字节，
  解压后的完整 wire 名称最多 255 字节，用户输入最多 1020 UTF-8 字节。
  用固定非数字后缀避免 URL 主机解析把 `127.1` 等数字标签改写成 IPv4。
- ECS：IPv4 前缀 0..32、IPv6 前缀 0..128；验证可选 CIDR 后缀；
  显式 `ecs_prefix` 仍为最终前缀，发送恰好 ceil(prefix/8) 字节，并清零无效尾位。
- 压缩：只允许指向此前已解析名称标签边界的后向指针，禁止指进 header、整数字段、
  标签内部及未知类型的 opaque RDATA；有真实环检测及每名称 256 步上限。
- 整包：验证 QR/opcode/TC/保留位、ID、唯一 IN A/AAAA 问题、类型/名称匹配；
  校验全部三个记录区的 class、边界与已知 RDATA 结构，拒绝尾随垃圾。
  总记录数上限 1024，DNS 消息上限 65535 字节。
- EDNS：OPT 只能位于 additional、必须未压缩根名称且唯一；校验版本/保留位、
  扩展 RCODE、选项长度及 ECS 家族/前缀/尾位，拒绝重复 ECS。
- CNAME：按链接顺序返回目标名称，不包含原查询名称；通常为小写 ASCII/punycode、
  无尾点，根目标为 `.`，二进制标签使用转义文本。所有 answer CNAME 路径均检测环，
  链最多 128 跳；拒绝冲突目标及除许可 DNSSEC 元数据外的同所有者其他数据。
- 正响应：只取最终 CNAME 目标上请求家族的地址，去重保序；
  TTL 为地址 RRset 与经过的所有 CNAME TTL 的最小值。
- 负响应：NXDOMAIN 和 NODATA 返回 `negative=true`；TTL 取最终目标所属区域的
  authority SOA 的 `min(RR TTL, MINIMUM)`，再受 CNAME TTL 限制。
  没有适用 SOA 时返回 TTL 0，不能缓存；NS referral 和带终点地址的 NXDOMAIN 失败。
  其他 RCODE（含 EDNS 扩展码）均失败。
- TTL 0 原样保留；按 RFC 2181，高位置位的 TTL 视作 0，绝不人为抬高缓存时长。
- DoH：结构化 URL 拼装，替换已有 `dns` 参数，保留其他查询参数；
  拒绝非法 scheme、userinfo 和 fragment。维持 HTTP/HTTPS 配置兼容，
  私网/元数据端点允许策略属于父任务的配置安全边界。
- HTTP：只接受 200 和唯一 `application/dns-message` MIME，禁止全部重定向；
  先检查 Content-Length，再按 chunk 累计限长，不能绕过限制。
  调用方 timeout 覆盖建连、响应头和整个 body 读取。错误文本不含 URL、参数或响应体。

## 待办清单
- [x] ECS 位掩码及 v4/v6 前缀范围。
- [x] IDNA、绝对名称、名称长度及标签验证。
- [x] 压缩名称解码、真实边界/环保护及整包校验。
- [x] CNAME 链、正 TTL、SOA 负 TTL 与明确 negative 标记。
- [x] 公共 `DnsAnswer` / `parse_response` / `query_answer` 及旧调用包装。
- [x] 本地 DoH mock、保留二进制夹具及有界确定性变异测试。
- [x] crate 级格式化、检查、Clippy、离线锁定依赖测试及交接记录。
- [ ] 父任务：将这些元数据接入持久化正负缓存、视角/家族键、探测展开及 any/all 聚合。

## 修改记录
- `crates/probe-dns/src/wire.rs`：强校验、边界预算、ECS/IDNA、CNAME 和 TTL。
- `crates/probe-dns/src/resolver.rs`：强 DoH 接口及兼容包装、限长、拒绝跳转、错误脱敏。
- `crates/probe-dns/src/lib.rs`：公共导出与模块职责文档。
- `crates/probe-dns/src/wire/tests.rs`：报文、夹具及确定性性质测试。
- `crates/probe-dns/src/resolver/tests.rs`：仅 loopback 的 HTTP mock 测试，生命周期结束会终止服务。
- `workstreams/05_DNS_ECS.md`：本次事实与父任务交接。

## 执行命令与输出摘要
日期：2026-10-09。使用仓库固定的 Rust 1.99.0；此机器 `.rustup` 的原路径权限受限，
仅在验证命令的 PowerShell 子进程中设置已存在的实际工具链目录，未修改全局配置。

```powershell
$env:RUSTUP_HOME = 'E:\C-relocated\dot-rustup'
cargo fmt -p probe-dns
cargo fmt -p probe-dns -- --check
cargo check -p probe-dns --all-targets --all-features --locked --offline
cargo clippy -p probe-dns --all-targets --all-features --locked --offline -- -D warnings
cargo test -p probe-dns --all-features --locked --offline
git diff --check -- crates/probe-dns workstreams/05_DNS_ECS.md
```

- 格式检查、`cargo check`、严格 Clippy 均退出 0；没有依赖或锁文件阻塞。
- 收尾期间，共享的 `probe-engine/Cargo.toml` 与根锁文件变更曾使一次 `--locked` Clippy
  提示需要更新锁文件；本工作者没有改锁或撤销他人改动。共享依赖恢复同步后，
  `--locked --offline` 的 check、58 项测试及严格 Clippy 已再次全部通过。
- 测试 **58 通过 / 0 失败 / 0 忽略**：wire 41、resolver 14、未改动的 geo 3；
  doctest 0。两次收尾运行均通过，测试执行约 2 秒（不含编译）。
- 格式化始终限制在 `-p probe-dns`；未运行 workspace 级格式化或测试。
- Git 仅提示原环境 ignore 路径权限及 LF/CRLF 转换，不影响 crate 检查。

## 测试证据
- 保留 A/AAAA 成功夹具，明确拒绝旧 self-pointer、错 CNAME 所有者、坏标签与截断夹具；
  NXDOMAIN 夹具现在验证显式、不可缓存的负响应。
- ECS 遍历两种地址家族的全部合法前缀，包含 /0、非整字节与完整前缀。
- 正/负响应逐字节截断；两个有效种子逐位置进行 9 种字节替换；
  固定 LCG 生成长度 0..511 的报文；固定 Unicode 标量步进检查合法名称往返。
- 三项新回归先用 `cargo test -p probe-dns --locked --offline reject_hidden`
  复现为 3 失败：非名称字段指针、查询路径外 CNAME 环、CNAME/TXT 排他冲突；
  修复后已纳入上述 58 项全绿结果。
- 本地 DoH 覆盖 A/AAAA、负响应、ID/问题 name/type/class 错配、截断、固定/流式超大 body、
  错误/缺失/重复 MIME、非 200、301/302/303/307/308 不跟随、响应头及 body 超时、
  中途断开、URL 参数替换、错误脱敏、视图隔离与真实去重。
- DoH 测试仅访问 `127.0.0.1` 动态端口；不再依赖向 `.invalid` 或真实 DNS 服务发请求失败。

## 兼容变化
- 有效 NXDOMAIN 从错误改成显式负响应；`query` 对它返回空 IP 列表，`resolve_views` 的
  外部形状不变。调用方需区分有效负响应与传输/解析失败。
- 非 ASCII 域名现在支持 IDNA；空标签、重复尾点、URL 语法、越界前缀、伪指针、
  畸形已知记录、CNAME 冲突、非 IN 数据、过长报文/记录数量以及尾随字节现在拒绝。
- 查询入口明确限制 A/AAAA；响应不再收集与问题或 CNAME 终点无关的地址。
- 未识别 RR 的 RDATA 保持 opaque，只检查声明长度与外层边界，不能作为压缩指针目标。
- HTTP 非 200、错误 MIME、重定向以及带 userinfo/fragment 的 endpoint 不再接受。

## 风险与回滚
- 不回滚共享工作树中其他人的改动，不改根 Cargo 文件，不提交、推送或部署。
- 如需撤销本工作者实现，由父任务逐文件审查上述所有权内 diff；
  不应删除整个 crate 或使用工作区级恢复命令。
- 未做真实公网 DoH、全工作区集成、长时间随机 fuzz、`cargo audit`/`cargo deny`；
  本次离线确定性测试不代表这些额外验证已完成。

## 阻塞项
本工作者契约内无未完成项或依赖阻塞。

## 下一步
父任务使用 `query_answer` 接入缓存与引擎；按视角和 A/AAAA 分别保存结果，
`ttl_secs == 0` 不写可复用缓存，解析/传输错误不能当成可缓存负回答。
保留现有 DNS 故障保护与事件记录职责，并完成其他工作者负责的整轮集成验证。
