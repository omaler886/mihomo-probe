# GLM-5.3-Flash 执行总控：mihomo-probe Rust 全量重构

> 目标：保留 Mihomo 作为独立代理内核，将订阅接入、DNS/ECS、核心测活、真实出口验证、状态收敛、SQLite、发布、Sub-Store 接入、API、调度、可观测性及安全控制全部重构为 Rust。
>
> 原仓库：https://github.com/omaler886/mihomo-probe

## 0. 你的角色与硬性要求

你是该仓库的主开发 Agent，同时也是架构负责人、测试负责人和安全审查负责人。直接开始工作，不要只给建议，不要等待确认。

必须遵循：

1. 先完整读取仓库，不允许凭 README 猜实现。
2. 先运行现有测试并记录真实基线；测试未运行不得宣称通过。
3. 使用多个相互独立的 Markdown 工作台并行推进，主控文件只负责索引、决策、依赖和汇总。
4. 每个工作台都必须持续记录：目标、现状、待办、修改文件、执行命令、测试证据、风险、阻塞、下一步。
5. 所有修改必须落入 `CHANGELOG_RUST.md`，包含影响范围和回滚方法。
6. 不允许一次性删除 Python 实现。先兼容、双跑、比对，达到门禁后再切换默认实现。
7. 不得重新实现 Mihomo 的代理协议栈。节点配置是否合法最终以 `mihomo -t` 结果为准。
8. 不得修改官方 Sub-Store 核心。通过远程订阅和 Script Operator 等扩展点接入。
9. 不得把真实 token、UUID、密码、私钥、short-id、订阅链接、节点配置写入代码、测试、日志或提交历史。
10. 每个阶段都必须执行格式化、静态检查、单元测试、集成测试和回归比对。
11. 任何测试失败都必须使 CI/部署失败，禁止 `|| true`、吞异常或伪造成功。
12. 若仓库文档与实际代码冲突，以实际代码和可复现测试为准，并记录冲突。
13. 优先完成可运行的纵向切片，不要同时铺开大量空壳模块。
14. 代码、注释、API 字段使用清晰一致的英文；开发文档可以中文。

## 1. 开始时立即创建的并行 Markdown 工作台

在仓库根目录创建以下文件，并立即填入实际审计结果。不要只创建空模板。

```text
workstreams/
├── 00_MASTER_STATUS.md
├── 01_REPO_AUDIT.md
├── 02_ARCHITECTURE.md
├── 03_DOMAIN_STATE_MACHINE.md
├── 04_MIHOMO_CONTROLLER.md
├── 05_DNS_ECS.md
├── 06_PROBE_ENGINE.md
├── 07_EXIT_VERIFICATION.md
├── 08_STORAGE_MIGRATION.md
├── 09_SUBSTORE_EXPORT.md
├── 10_API_SCHEDULER.md
├── 11_OBSERVABILITY.md
├── 12_SECURITY_HARDENING.md
├── 13_TEST_COMPATIBILITY.md
├── 14_DEPLOYMENT_RELEASE.md
├── 15_UI_PRODUCT.md
└── 16_FINAL_REVIEW.md
```

### 每个工作台统一格式

```markdown
# 工作流名称

## 目标
## 输入与依赖
## 当前实现审计
## 设计决策
## 待办清单
- [ ] ...
## 修改记录
| 时间 | 文件 | 变更 | 原因 |
## 执行命令与输出摘要
## 测试证据
## 风险与回滚
## 阻塞项
## 下一步
```

`00_MASTER_STATUS.md` 必须维护：

- 当前阶段与总体完成状态
- 各工作流负责人/Agent 标识
- 文件所有权，防止并行冲突
- 工作流依赖图
- 已合并提交
- 当前失败测试
- 待决策 ADR
- 下一批可以并行执行的任务

并行规则：

- 可以并行：仓库审计、DNS、领域状态机、安全审计、测试夹具、部署审计、UI 审计。
- 需要串行：领域模型确定后再固化数据库；API 契约确定后再改 UI；兼容比对通过后再切换默认服务。
- 两个 Agent 不得同时修改同一文件。冲突由主控 Agent 统一整合。
- 每完成一个原子任务就更新对应 Markdown，不要等到最后补文档。

## 2. 第一阶段：建立真实基线

### 2.1 仓库审计

完整读取：

```text
README.md
PLAN.md
ARCHITECTURE.md
FEATURE_INVENTORY.md
MIGRATION_MAPPING.md
MIGRATION_GUIDE.md
REVIEW.md
SECURITY_REVIEW.md
TEST_REPORT.md
CHANGELOG_MIGRATION.md
mihomo_test/**
substore_bridge/**
tests/**
tools/**
Dockerfile
docker-compose.yml
install.sh
migrate.sh
setup_tunnel.py
requirements.txt
.env.example
```

输出到 `workstreams/01_REPO_AUDIT.md`：

- 模块、入口、调用链、线程/并发模型
- 所有外部服务和端口
- SQLite schema 与迁移方式
- 配置键、默认值、环境变量覆盖规则
- API 清单及鉴权方式
- 状态机和失败分类
- 导出与 Sub-Store 接入链路
- Docker 权限与网络模式
- 测试分类、测试数量和真实执行结果
- TODO/FIXME、异常吞噬、未处理边界
- Python 功能到 Rust crate 的逐项映射

### 2.2 基线测试

在不修改代码时运行并保存完整输出：

```bash
python3 -m py_compile mihomo_test/*.py
python3 -m unittest discover -s tests -v
```

如果必须在容器中运行，就构建原版镜像并在容器中运行。记录：

- 精确命令
- commit SHA
- 运行环境
- 通过/失败/跳过数
- 失败详情
- 网络依赖测试是否被隔离

修复 `install.sh` 中任何导致测试失败仍继续部署的逻辑。

### 2.3 安全前置处理

优先完成：

- 检查 `/api/status` 等响应是否泄露 token。
- 检查日志和异常是否泄露节点凭据。
- 搜索 Git 历史及工作区中的真实凭据。
- 创建 `SECURITY_CREDENTIAL_ROTATION.md`，只记录需要轮换的位置和动作，不复制秘密值。
- 增加自动 secret scanning。
- 修复 `core.mixed_port` 等缺省配置问题。
- 将 query token 标为兼容模式，新接口优先 `Authorization: Bearer` 或 `X-Auth-Token`。
- 制定去除主应用 Docker Socket 的方案。

发现已泄露的真实凭据时，不要继续展示其值；立即用 `[REDACTED]` 替代并记录轮换要求。

## 3. 目标架构

最终容器：

```text
mihomo-probe-rs   Rust 控制面与数据面
mihomo            独立 Mihomo 内核
cloudflared       可选公网入口
probe-supervisor  可选最小权限内核管理器
```

Rust Workspace：

```text
Cargo.toml
crates/
├── probe-domain/
├── probe-config/
├── probe-storage/
├── probe-mihomo/
├── probe-dns/
├── probe-substore/
├── probe-engine/
├── probe-api/
├── probe-scheduler/
├── probe-observability/
├── probe-supervisor/
└── probe-cli/
web/
migrations/
fixtures/
integration-tests/
```

依赖方向必须单向：

```text
probe-domain
   ↑
config / dns / mihomo / substore / storage
   ↑
probe-engine
   ↑
api / scheduler / cli
```

`probe-domain` 不得依赖 Web、数据库、Docker 或网络客户端。

建议技术栈，采用前先核实当前稳定版本并锁定：

```text
tokio
axum
reqwest
serde / serde_json / serde_yaml
sqlx + SQLite
tracing / tracing-subscriber
thiserror
sha2
hickory-proto 或 hickory-resolver
prometheus client
uuid
time
tokio-util CancellationToken
```

必须提交 `Cargo.lock`，CI 使用 `--locked`。

## 4. 核心领域模型

至少设计以下类型：

```rust
NodeId
SourceId
NodeFingerprint
EndpointFingerprint
RoundId
NormalizedNode
NodeState
Observation
FailureKind
ProbeResult
ExitIdentity
RoundOutcome
GuardDecision
PublishDecision
Policy
```

保留原状态：

```text
unknown
alive
pending
dead
excluded
```

新增状态必须先以 ADR 说明语义：

```text
degraded
quarantined
stale
```

要求：

- 状态转换实现为纯函数。
- 一次成功立即恢复的既有语义默认保持。
- 连续失败达到阈值才判死。
- `excluded` 不得等价于 `dead`。
- 数据源缺失不得立即判死，必须有 stale 策略。
- fingerprint 算法必须与现有 Python 结果逐项比对。
- 敏感字段不得出现在 API 账本、日志和指标标签中。

## 5. Mihomo 控制与配置

Rust 负责：

- 生成临时 Mihomo 配置。
- 创建探测 selector 和并行出口验证车道。
- 调用 Controller API。
- 切换 selector。
- 调用 delay API。
- 读取并分类非 2xx 响应正文。
- 监控内核健康。
- 配置加载、回滚和重试。

Mihomo 负责：

- 节点协议解析。
- 真实代理连接。
- 数据转发。
- delay 测试。

配置合法性必须调用 Mihomo 自身：

```bash
mihomo -t -f <generated-config>
```

不得仅以 Rust YAML 反序列化成功作为合法标准。

Docker Socket 处理优先级：

1. 首选不挂载 Docker Socket，Rust 通过 Mihomo Controller reload 配置。
2. 必须执行容器操作时，使用固定能力的 `probe-supervisor`。
3. supervisor 只允许固定 Mihomo 实例的 validate/reload/restart/status。
4. 禁止任意命令、任意路径、任意容器名。

## 6. DNS/ECS 模块

移植并验证现有双视角 DoH + ECS 行为：

- 支持 A、AAAA。
- 正确构造和解析 EDNS Client Subnet。
- 支持 DNS name compression。
- 校验响应 ID、RCODE、长度和记录边界。
- 记录 CNAME 链。
- 按 TTL 缓存正响应与负响应。
- 不同 DNS 视角分别保存结果。
- 每个解析 IP 生成探测项，最后聚合回域名。
- 支持 `any` 和 `all` 聚合策略。
- IPv4、IPv6 结果独立记录。
- DNS 整体异常不得导致批量误杀。

必须建立二进制 DNS fixture 和属性测试/fuzz 测试，覆盖截断包、恶意长度、压缩指针环和无效标签。

## 7. 核心测活引擎

实现完整异步流水线：

```text
收集数据源
→ 规范化和去重
→ 配置校验
→ DNS 多视角解析
→ delay 快速测试
→ 真实出口验证
→ 失败归类
→ 域名/IP 聚合
→ 状态转换
→ 整轮保护
→ 原子发布
```

并发与取消：

- 全局 semaphore。
- 每数据源 semaphore。
- 每验证目标限速。
- 每节点 timeout。
- 每阶段 timeout。
- 整轮 deadline。
- `CancellationToken` 支持 API 取消和优雅停机。
- 禁止无限重试。
- retry 必须按 FailureKind 决定。
- 慢节点不得拖住整轮。

失败类别至少兼容现有：

```text
timeout
kernel_error
bad_request
bad_response
bad_delay
unreachable
controller_error
http_<status>
verify_failed
entry_cn
```

可以增加细分错误，但旧的 API 和数据库迁移必须有兼容映射。

## 8. 分层检测与自适应调度

实现三级检测：

### L1 快速筛查

- Mihomo delay API。
- 低成本目标。
- 对全部候选节点执行。

### L2 真实出口验证

- 只对 L1 通过节点执行。
- 验证真正通过节点出站。
- 获取出口 IP、国家/地区、ASN。
- 区分入口位置和出口位置。

### L3 深度质量检测

- 仅对候选优质、结果反复或人工指定节点执行。
- 多次样本。
- 统计延迟分布和成功率。
- 可选小流量吞吐测试，必须设置严格流量上限。

自适应调度：

```text
alive       正常周期
degraded    缩短周期
pending     下一轮优先
dead        指数退避，仍需低频复活检测
excluded    DNS/IP 信息变化后重测
quarantined 人工解除或低频验证
stale       按数据源缺失策略处理
```

不得因为节点长期 dead 就永不复测。

## 9. 质量评分

新增可配置评分，不影响第一阶段 alive/dead 兼容输出：

```text
availability
median latency
p95 latency
jitter
success rate
exit stability
optional throughput
```

存储原始样本或有明确定义的聚合值：

```text
min
median
p90
p95
max
jitter
sample_count
success_rate
```

评分权重配置化，缺测项的处理必须明确。不能把未知数据自动当满分或零分。

## 10. 整轮保护

在现有存活数下降保护基础上实现：

- 相对存活数保护。
- 绝对存活数保护。
- 大量相同错误保护。
- DNS 全局异常保护。
- 验证目标异常保护。
- Mihomo Controller 异常保护。
- Sub-Store 拉取异常保护。
- 单数据源隔离。
- 发布节点差异上限。
- 零节点发布禁止。
- 强制发布必须显式授权并进入审计日志。

一旦触发保护：

- 不更新连续失败计数，或按明确策略冻结。
- 不覆盖上一版有效导出。
- API 和面板展示触发原因。
- 指标和通知记录事件。

## 11. SQLite 与数据迁移

要求：

- 使用 SQL migration，不在业务代码中临时建表。
- 启用 WAL 和合理 busy timeout。
- 约束和索引明确。
- 所有时间统一 UTC。
- 支持读取和迁移现有 Python SQLite。
- 迁移前自动备份。
- 迁移失败不破坏原数据库。
- 提供离线 `probe-cli db check` 和 `probe-cli db migrate`。

建议表：

```text
schema_migrations
sources
nodes
node_endpoints
rounds
observations
node_state_history
dns_cache
ip_geo_cache
exit_history
exports
export_snapshots
config_audit
security_audit
```

不得为了 Rust 重写而删除历史收敛数据。

## 12. Sub-Store 与发布

保持两条接入路径：

1. Rust 导出 ClashMeta YAML，Sub-Store 将其作为远程订阅。
2. `/api/v1/nodes` 提供无凭据的只读账本，供 Script Operator 过滤/标注。

要求：

- 兼容旧 `/api/export/<key>.yaml`。
- 新接口优先 header token。
- 输出内容原子写入。
- 每次发布生成快照和 hash。
- 支持查看 diff 和回滚上一版。
- 零节点不得发布。
- 多格式转换继续交给官方 Sub-Store producer，不在 Rust 重造全部格式。
- 保持 `filter / annotate / both` 和 `missing=keep/drop` 语义。

建立契约测试验证 Script Operator 与 API payload。

## 13. API、调度和 UI

API 至少提供：

```text
GET  /healthz
GET  /readyz
GET  /metrics
GET  /api/v1/status
GET  /api/v1/nodes
GET  /api/v1/nodes/{fingerprint}
GET  /api/v1/rounds
GET  /api/v1/rounds/{id}
POST /api/v1/rounds
POST /api/v1/rounds/{id}/cancel
GET  /api/v1/exports
POST /api/v1/exports/{key}/rollback
```

API 约束：

- OpenAPI 契约或等价 schema。
- 管理 token 与只读发布 token 分离。
- 错误响应稳定、可机器读取。
- 状态接口绝不输出秘密。
- 默认只监听 loopback，公网通过 Cloudflare Tunnel 或明确配置开放。
- 所有写操作有审计记录。
- 防止重复启动同一轮。

UI 第一阶段复用现有静态页面并适配新 API，之后再增强：

- 当前进度。
- 各状态分布。
- 每轮趋势。
- 失败类型。
- 单节点历史。
- 延迟 P50/P95。
- 出口变化。
- 发布 diff 与回滚。
- 整轮保护原因。
- 调度和数据源健康。

不要在核心兼容未完成前重写前端框架。

## 14. 可观测性

结构化 tracing 字段：

```text
round_id
source_id
node_fingerprint
stage
failure_kind
duration_ms
```

不得记录节点凭据和完整订阅 URL。

Prometheus 指标至少包括：

```text
probe_round_total
probe_round_duration_seconds
probe_round_guard_total
probe_node_observation_total
probe_node_state_total
probe_mihomo_request_total
probe_dns_request_total
probe_substore_request_total
probe_export_nodes
probe_export_age_seconds
```

禁止把节点名、完整 IP 或高基数字段直接放进 label。

## 15. 安全要求

必须执行威胁建模，重点覆盖：

- Docker Socket/root 权限。
- SSRF。
- 不可信订阅和 YAML。
- 恶意 DNS 响应。
- 路径穿越。
- 命令注入。
- token 泄露。
- 日志泄密。
- 错误配置导致公网暴露。
- 资源耗尽和压缩炸弹。
- 任意重定向。
- 云元数据地址访问。

具体控制：

- 限制订阅响应大小、节点数和字段长度。
- 限制重定向次数。
- 对管理端配置的 URL 做允许策略和私网/元数据保护。
- 所有文件路径做 canonicalize 与根目录约束。
- 不拼接 shell 命令。
- `mihomo -t` 使用固定 argv。
- secrets 走环境变量、Docker Secret 或权限受控文件。
- API 返回统一脱敏。
- 支持 token 轮换和短期双 token 过渡。
- Rust 依赖运行 `cargo audit`、`cargo deny`。
- 容器非 root、只读根文件系统、最小 capabilities。

## 16. 测试策略

### 16.1 共享 fixture

创建语言无关 fixture：

```text
fixtures/
├── configs/
├── subscriptions/
├── normalized-nodes/
├── mihomo-responses/
├── dns-packets/
├── exit-responses/
├── rounds/
├── databases/
└── expected/
```

Python 和 Rust 对同一输入输出兼容结果。

### 16.2 单元测试

覆盖：

- fingerprint。
- 状态转换。
- guard 决策。
- 失败分类。
- DNS 报文。
- 配置默认值。
- token 脱敏。
- YAML 导出。
- Sub-Store 异常语义。

### 16.3 集成测试

- Mock Mihomo Controller。
- 真 Mihomo 容器配置验证。
- 临时 SQLite 迁移。
- API 权限矩阵。
- 原子发布和回滚。
- 整轮取消。
- Cloudflare 不作为测试成功的必要条件。

### 16.4 双跑兼容测试

相同输入同时运行 Python 和 Rust，比较：

```text
节点总数
fingerprint
配置拒绝集合
延迟成功集合
出口验证集合
失败分类
状态转换
guard 决策
导出节点集合
API payload
```

延迟数值允许合理波动，但 fingerprint、状态语义、发布决策及导出集合必须达到设计门禁。

### 16.5 Fuzz 与性质测试

重点 fuzz：

- DNS parser。
- YAML/订阅规范化。
- API 请求。
- 配置迁移。
- 节点名称和 Unicode。
- 导出序列化。

## 17. CI 与质量门禁

GitHub Actions 或等价 CI：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features --locked
cargo audit
cargo deny check
```

同时保留 Python 基线测试直到完全迁移。

门禁：

- 测试失败禁止构建发布镜像。
- 安全高危禁止发布。
- 数据库迁移测试失败禁止发布。
- 兼容测试失败禁止切换默认实现。
- Docker 镜像不得使用未经记录的漂移版本。
- 生成 SBOM。
- 发布镜像签名。

## 18. 发布与迁移

迁移模式：

```text
legacy    仅 Python
shadow    Python 发布，Rust 双跑不发布
audit     比较 Python/Rust 结果并报告
rust      Rust 发布，Python 保留回滚
rust-only 移除 Python 运行依赖
```

必须提供：

```text
probe-cli migrate check
probe-cli migrate backup
probe-cli migrate apply
probe-cli migrate verify
probe-cli migrate rollback
```

切换到 `rust` 前必须满足：

- 旧数据库成功迁移及回滚演练。
- 多轮 shadow 结果已记录并达到门禁。
- API 契约通过。
- Sub-Store 实机联调通过。
- Mihomo 真内核测试通过。
- 发布/回滚快照通过。
- 安全审计无未处理高危。
- 文档与实际命令一致。

## 19. 建议提交批次

每批原子提交，不混入无关格式化：

```text
R0  基线审计、测试、凭据整改
R1  Rust workspace、领域模型、配置
R2  SQLite schema 与兼容迁移
R3  Mihomo Controller 和配置校验
R4  DNS/ECS
R5  delay 测活和失败归类
R6  出口验证和 IP/ASN 缓存
R7  状态机、整轮保护、原子发布
R8  Sub-Store 与兼容 API
R9  调度、取消、可观测性
R10 shadow 双跑与差异报告
R11 默认切换 Rust，保留回滚
R12 安全加固、发布供应链、文档定稿
```

每批更新：

```text
CHANGELOG_RUST.md
TEST_REPORT_RUST.md
相关 workstream Markdown
必要的 ADR
```

## 20. 交付物

最终必须交付：

- 可编译的 Rust workspace。
- 可运行的 Docker Compose。
- 与原 SQLite 数据兼容的迁移工具。
- 核心测活、DNS/ECS、出口验证、状态机、guard、发布系统。
- Sub-Store 兼容接口与 Script Operator 契约测试。
- API、调度、健康检查、Prometheus 指标。
- 静态 Web 面板适配。
- 单元、集成、兼容、fuzz 测试。
- CI、SBOM、依赖审计。
- 部署、升级、回滚、故障排查文档。
- 完整变更历史和真实测试证据。

## 21. 完成定义

只有同时满足以下条件才允许声明完成：

- Rust 服务在干净环境可构建、启动和健康检查。
- 真 Mihomo 内核端到端测活成功。
- 旧数据无损迁移并验证。
- shadow 双跑差异在门禁范围内。
- 整轮异常不会清空或错误覆盖订阅。
- Sub-Store 拉取和 Script Operator 路径通过。
- token 不出现在 API、日志、指标和仓库历史新增内容中。
- Docker Socket 已移除或由严格 supervisor 隔离。
- 全部强制测试和静态检查通过。
- 可一条命令回滚至上一稳定版本。
- `16_FINAL_REVIEW.md` 给出逐项证据，而不是口头结论。

## 22. 现在立即执行

按以下顺序直接开工：

1. 获取当前 commit SHA 和环境信息。
2. 创建并填充全部 `workstreams/*.md`。
3. 并行完成仓库审计、安全审计、测试基线、架构草案和 fixture 设计。
4. 修复 P0：凭据泄露、状态接口泄密、缺省配置、测试失败仍部署、Docker Socket 方案。
5. 提交 R0。
6. 创建 Rust workspace 和第一条可运行纵向切片：读取配置 → 调用 Mihomo health/controller → 写入一条 round → API 返回状态。
7. 为该切片补齐测试与文档后再扩展下一功能。

每次汇报必须使用以下格式：

```markdown
## 本轮完成
## 修改文件
## 测试命令与真实结果
## 发现的问题
## 风险与回滚
## 下一批并行任务
```

不要只输出规划。立即创建工作台、执行基线测试、提交首批可验证改动。
