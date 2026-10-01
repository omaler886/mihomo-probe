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

## 待评估：配置内容哈希跳过 reload（外部方案 §2.4）
现状：`core.py:519 reload()` 无条件 `PUT /configs?force=true`，**没有内容比对**；
`build_config` 每轮重写配置文件后即 reload（`core.py:423`）。
外部方案建议：对规范化 YAML 算 SHA-256，与当前运行配置相同则跳过校验与 reload；
并把变化分类为「节点集合 / 内核运行配置 / 测试目标 / Web 配置 / 通知配置」，
**只有影响内核的变化才触发 reload**。

先评估收益再决定实现——本仓库的配置里内嵌整轮待测节点集合，
**节点集合每轮都变 → 哈希大概率不同 → 主要收益只落在两个场景**：
同一轮内的重复 reload、以及仅非内核配置（告警/发布）变化时的空 reload。
若实测这两类场景占比很低，则不值得引入哈希层（多一个失效面）。
R5 用真实轮次数据统计 reload 次数与哈希命中率后再裁决。

## 待办清单
- [x] R1：ConfigBuilder + /version + reload + reason 分类（切片）
- [x] R3：lanes（lane_count/ports/命名）+ select + egress/fetch + `mihomo -t` 校验器
      （MIHOMO_BIN > docker > 显式 Degraded）+ culprit_from 移植（含 jp 误匹配回归）
- [ ] R5：make_testable 裁剪循环移植（随引擎 entry/fp 模型一并落地）
- [ ] R5：评估「配置内容哈希跳过 reload」收益（数据先行，见上）

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
