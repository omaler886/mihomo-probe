# Mihomo 控制器

## 目标
Rust 侧重建 core.py 的内核控制面：配置生成、`mihomo -t` 校验、Controller API、reload 兜底。

## 当前实现审计（core.py）
- build_config：loopback 绑定（allow-lan:true + bind-address:127.0.0.1，缺一不可）、ipv6、fake-ip DNS、每车道 select 组 `__LANE<i>__` + mixed listener `lane<i>`（19200+i）+ IN-NAME 规则；proxies 每行 inline JSON（避开 YAML 1.1 浮点陷阱的写入侧）；`mixed-port` 读 `core_cfg["mixed_port"]`（R0 前不在 DEFAULTS，KeyError 陷阱，已修）。
- config_test：`docker run --rm -v <host_core>:/root/.config/mihomo metacubex/mihomo:latest -t …`；docker 不可用时**跳过并记 skip**（风险：降低校验精度，Rust 侧必须改为硬失败或显式降级记录）。
- make_testable：最多 6 轮裁剪-重试，按内核报错文本定位 culprit（引号名 > 有界 token > server），按 entry index 删除。
- Core API：Bearer secret；GET /version（wait_ready 45s）；PUT /configs?force=true（40s，失败→restart 兜底，2026-09-29 修复点）；GET /proxies/{name}/delay?timeout&url&expected（失败正文分类 timeout/kernel_error/bad_request/unreachable/http_<status>）；PUT /proxies/{group} 切换；车道 egress/fetch 走 127.0.0.1:port HTTP 代理。
- 失败语义：controller_error ≠ node unreachable（不算入 TERMINAL_REASONS）。

## 设计决策
- probe-mihomo：ConfigBuilder（与 Python 逐字段一致，快照测试对拍）、Controller 客户端（reqwest，固定超时）、ReasonClassifier（对拍 Python _reason_from）。
- `mihomo -t` 校验：默认经 supervisor；无 docker 时返回 Err 而非跳过（与 Python 行为差异，记入 13 兼容清单由双跑门禁裁决）。

## 评估结论：配置内容哈希跳过 reload（外部方案 §2.4）— **不采纳**
外部方案建议：对规范化 YAML 算 SHA-256，与当前运行配置相同则跳过校验与 reload，
并把配置变化分类为「节点集合 / 内核运行配置 / 测试目标 / Web 配置 / 通知配置」，
只有影响内核的变化才触发 reload。

**实测调用链（2026-10-01）：**

| 事实 | 证据 |
|---|---|
| 每轮 `start_and_load` 只调一次，且**不在任何循环里** | `engine.py:1250`（唯一调用点，其外层无 `for`/`while`） |
| 因此每轮 reload = **1 次** | `core.py:555 start_and_load` → `core.py:566 self.reload()` |
| 每轮 `build_config` 最多写 6 次文件 | `core.py:423`，在 `make_testable` 的 `max_prune=5` 裁剪循环内 |
| `reload()` 无条件 PUT，无内容比对 | `core.py:519-534` |

**为什么不采纳：** 关键不是命中率，而是**每轮只有 1 次 reload**。
即使哈希 100% 命中（即上一轮配置与本轮逐字节相同），每 30 分钟也只省下
**1 个 HTTP PUT** —— 为这点收益引入一层哈希（多一个失效面、多一份
"什么时候该跳过"的判断）不划算。方案自己列的"同轮内重复 reload"场景在本仓库
**不存在**（`start_and_load` 每轮只调一次）。

（附注：配置里内嵌整轮待测节点集合，实际命中率应该远低于 100%；
这个前提**未实测统计**，但结论不依赖它。）

**真正的开销在别处：** `make_testable` 每次裁剪尝试都跑一次
`config_test`（`core.py:424`，`docker run --rm metacubex/mihomo:latest -t`），
最多 6 次容器启动/轮。哈希方案解决不了它。若 R5 要优化内核侧开销，
**应该盯这里**：例如把校验改成常驻 `probe-supervisor` 的一次 HTTP 调用
（ADR-0002 第二步已经规划），而不是加哈希。记入 R12 supervisor 批次。

## 待办清单
- [x] R1：ConfigBuilder + /version + reload + reason 分类（切片）
- [x] R3：lanes（lane_count/ports/命名）+ select + egress/fetch + `mihomo -t` 校验器
      （MIHOMO_BIN > docker > 显式 Degraded）+ culprit_from 移植（含 jp 误匹配回归）
- [x] R5：评估「配置内容哈希跳过 reload」→ **不采纳**（结论与证据见上）
- [ ] R5：make_testable 裁剪循环移植（随引擎 entry/fp 模型一并落地）
- [ ] R12：把 `config_test` 从「每次 docker run」改为常驻 supervisor 调用（真正的开销所在）

## 测试证据
- Rust 单测：配置快照、reason 分类表、mock controller（version/reload/delay/select/鉴权中间件）、
  车道出口身份读取（axum 兼任代理 mock）、fetch 字节上限、culprit 定位表（见 crates/probe-mihomo）。
- 本机无 docker/内核：`mihomo -t` 真校验用例为 #[ignore] 形态待 vps；Degraded 分支已测。

## 风险与回滚
- 新增 crate，不影响 Python；回滚删目录。

## 阻塞项
- 本机无 mihomo 内核/docker，真内核联调在 vps（R3 门禁）。

## 下一步
- R3 按 06 的失败类别清单补全。
