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

## Kernel Worker Update (2026-10-09)

This section supersedes earlier validation/test-status claims for this worker's
changes. Specification sections 5, 7 and 15 were read. No engine wiring, live
kernel test, daemon operation, deployment change, credential read, commit or
other-agent work was performed.

### Scope and Changes

| File | Change |
|---|---|
| `crates/probe-mihomo/src/controller.rs` | Parsed URL construction, base-path preservation, encoded path/query data, redacted errors, bounded responses/deadlines, conservative infrastructure classification, local mock tests |
| `crates/probe-mihomo/src/config_check.rs` | Explicit native validation, concurrent bounded drains, bounded process-tree cleanup, pinned and bounded Docker fallback, unambiguous Unicode-safe culprit lookup |
| `crates/probe-mihomo/src/config_check/tests.rs` | Verdict, truncation, ambiguity, Unicode, missing-path and symlink tests |
| `crates/probe-mihomo/src/config_check/tests/native.rs` | Isolated native helper compilation and lifecycle tests |
| `crates/probe-mihomo/src/config_check/tests/helper.rs` | Standalone std-only synthetic subprocess fixture, not a Mihomo replacement |
| `workstreams/04_MIHOMO_CONTROLLER.md` | This API and verification handoff |

Existing controller changes to `host_port()`, reload recovery documentation and
transport redaction were retained and extended. Manifests, lockfiles, `exit.rs`,
`lanes.rs`, engine code and deployment files were not edited.

### Exact Public APIs

Existing signatures remain:

```rust
Controller::new(api: &str, secret: Option<&str>) -> Controller
Controller::url(&self) -> &str
Controller::host_port(&self) -> Option<String>
Controller::version(&self) -> Result<serde_json::Value, KernelError> // async
Controller::reload(&self, config_path: &str) -> Result<bool, KernelError> // async
Controller::select(&self, group: &str, name: &str) -> Result<(), KernelError> // async
Controller::delay(
    &self, proxy: &str, url: &str, timeout_ms: u64, expected: &str
) -> Result<DelayOutcome, KernelError> // async

config_check::validate(host_dir: &Path) -> ConfigCheck
config_check::culprit_from(output: &str, proxies: &[ProxyRef]) -> Option<usize>
ConfigCheck::is_ok(&self) -> bool
```

New helpers, available without any re-export or manifest changes:

```rust
Controller::try_new(api: &str, secret: Option<&str>) -> Result<Controller, KernelError>

probe_mihomo::config_check::validate_with_binary(
    binary: &std::path::Path,
    config_path: &std::path::Path,
    timeout: std::time::Duration,
) -> ConfigCheck
```

`try_new` rejects invalid controller bases immediately. The compatible `new`
constructor instead creates an inert client for an invalid base: operations
return redacted infrastructure errors without sending requests.

`validate_with_binary` uses only the explicitly supplied executable and config,
canonicalizes both regular files, and executes exactly `-t -f <config-path>`.
It does not consult `MIHOMO_BIN`, search for validators, or fall back to Docker.
Windows requires a native `.exe`, avoiding implicit batch-file shell execution.
It is synchronous; async round preparation must use a bounded blocking task.

### Controller Contracts

- Endpoint paths are appended to the parsed base path, including bases without
  a trailing slash. Names are path segments; delay `url`, `timeout` and `expected`
  are query pairs. Empty/dot-only proxy path segments are rejected.
- HTTP(S) only. Userinfo (including empty userinfo), fragments, control
  characters and backslashes are rejected. Controller base queries are rejected;
  test-target queries are allowed. `url()` remains a compatibility accessor for
  the configured string and must not be logged. `host_port()` omits path/secret.
  Caller-owned controller/target allowlists remain necessary; this primitive
  permits private addresses because a loopback/private controller is expected.
- Redirect policy is `none`; ambient HTTP(S)/ALL_PROXY settings are disabled.
- Connect timeout: 5 seconds. Default/version/select timeout: 20 seconds.
  Reload timeout: 40 seconds. Node timeout clamps to 1..=120,000 milliseconds;
  delay client timeout adds 8,000 milliseconds, so even `u64::MAX` is bounded.
- Every response, including reload/select and non-success responses, has a
  65,536-byte retained-body limit. Both advertised and streamed/chunked lengths
  are checked; stalled or truncated bodies are infrastructure errors.
- Transport, auth, redirects, missing/unknown proxies, malformed JSON, invalid
  delay values, unexpected statuses and unknown error shapes return
  `KernelError::Controller`, not evidence of a dead node.
- Delay failure recognition is intentionally narrow: HTTP 504 with JSON
  sole `message: "Timeout"` field (case insensitive) produces the existing `timeout` kind;
  HTTP 503 with `message: "An error occurred in the delay test"` produces
  `kernel_error`. Returned messages are fixed diagnostics, not copied bodies.
  Broader real-kernel failure shapes require parent evidence before expansion.
- Reload returns `true` only for 200/204, `false` for 400/422 config refusals,
  and infrastructure errors otherwise. Neither refusal nor infrastructure
  failure proves the requested aliases were installed.

### Validation and Cleanup Contracts

| Result | Meaning | Caller action |
|---|---|---|
| `Passed(output)` | Native validator completed normally with exit 0 | May continue preparation |
| `Failed(output)` | Native validator completed with a normal nonzero exit | May attempt conservative culprit lookup |
| `Degraded(reason)` | Missing files/binary/daemon, spawn/containment/read failure, timeout, abnormal termination, reserved Docker launch failure or unconfirmed cleanup | No validation verdict; never treat as a pass or prune nodes |

- `is_ok()` is true only for `Passed`.
- `Passed`/`Failed` output remains bounded **sensitive kernel diagnostics** for
  in-memory culprit matching, preserving the public enum payloads. It is not
  safe for logs/API responses. `ConfigCheck` Debug now redacts all payloads;
  unavailable-validation diagnostics are fixed strings without OS error text.
- stdout/stderr drain concurrently during execution, retaining at most 64 KiB
  per pipe. Final combined UTF-8 output is at most 128 KiB including truncation
  marker. Excess bytes are discarded while draining, not left to block a pipe.
- The supplied deadline clamps to 1 ms..=120 s and covers execution and pipe
  completion. Polling is 10 ms; idle pipe readers poll at 1 ms. Cleanup reaping
  has its own 1-second budget and one best-effort Drop retry. OS calls and thread
  scheduling are not hard real-time guarantees; failed cleanup is unavailable.
- Unix validators use an owned process group. Windows uses a hidden, initially
  suspended child, an owned kill-on-close job, then `NtResumeProcess` after job
  assignment. This avoids the pre-assignment descendant-spawn race. Failed
  containment degrades rather than running untracked. Reader cancellation and
  readiness checks avoid indefinite joins on inherited descendant pipes.
- `validate(dir)` checks only `dir/config.yaml` and rejects symlink escapes.
  An explicitly configured but unusable `MIHOMO_BIN` degrades; it is not silently
  replaced by a different validator.
- Docker availability uses the same bounded runner with a 3-second budget and
  60-second cache. Cache contention degrades immediately instead of queuing.
- Only the Rust **validator fallback** image changes to
  `metacubex/mihomo:v1.19.32`, the upstream release verified by the parent.
  The actual default deployment is unchanged. `--pull=never` requires the image
  to be prepared externally; unavailable images/launch exits 125..127 degrade.
- Docker validation mounts only the canonical config read-only, disables
  network and capabilities, uses a read-only root with temporary home/tmp, and
  assigns a process/time/counter-qualified owned container name. Interrupted
  runs attempt `docker rm --force <that-owned-name>` with a separate 3-second
  budget. A daemon that cannot confirm cleanup yields explicit `Degraded`;
  this fallback cannot guarantee remote cleanup while the daemon is unavailable.
- Culprit lookup first requires an unambiguous quoted name. Otherwise it
  considers bounded names and servers together, rejecting duplicate names,
  shared servers and conflicting name/server evidence. UTF-8 byte advancement
  is eliminated; address-prefix and IPv6-prefix matches are rejected.
  Truncated diagnostics never authorize pruning.

### Test Inventory and Actual Results

Implemented tests cover:

- All controller operations under 301/302/303/307/308 redirects, with a second
  local server asserting that redirects never reach it.
- Base paths with/without trailing slash, Unicode/slash/percent/query characters,
  expected-status encoding, bearer auth, zero and maximal timeout inputs.
- 401/403/404, unknown proxy shapes, config refusals, malformed JSON, invalid
  delay types/ranges, advertised/chunked oversized bodies, truncated bodies,
  non-UTF-8 responses, stalled reads and diagnostic redaction.
- Ambient proxy environment overrides in an isolated child test process;
  the parent test runner environment is never mutated.
- Native fixed argv/null stdin, explicit Unicode config paths, successful exit,
  actual rejection, two simultaneous oversized output pipes, invalid UTF-8,
  absent/non-executable files, reserved Docker exit codes and Unix signals.
- Native timeouts with a direct child, a live descendant and an exited parent
  whose descendant retains pipes. Local listener release checks that the owned
  hanging helper stops within a bounded post-termination observation window.
- Unicode matching, case expansion, ambiguous names/shared servers/cross-tier
  evidence, address boundaries, truncation and config-directory symlink escape.

Actual commands/results in this worker session:

| Command | Actual result |
|---|---|
| `cargo --version` | Failed before compilation: rustup could not create/access `C:\Users\ckai7\.rustup` (OS error 183) |
| `git diff --check -- crates/probe-mihomo/src/config_check.rs crates/probe-mihomo/src/controller.rs workstreams/04_MIHOMO_CONTROLLER.md` | Passed at the recorded static check; Git emitted only LF/CRLF conversion notices |
| Cargo unit tests / clippy / rustfmt | **NOT RUN**: the single toolchain attempt failed; no alternate toolchain search or global configuration change was made |
| Real Mihomo validation | **NOT RUN**, neither passed nor rejected |
| Docker daemon/container tests | **NOT RUN** and deliberately excluded from the tests |

The native tests compile their std-only fixture with the test toolchain's
`RUSTC` (or `rustc` on PATH); missing compilation capability makes the test fail,
not skip or pass. No current-workspace test pass count is claimed here.

### Handoff Checklist

- [x] Implement the scoped primitives and isolated regression tests.
- [x] Preserve pre-existing work and keep deployment/engine/manifests untouched.
- [x] Separate unavailable validation from passed/rejected validation.
- [ ] Parent: format and compile on the available pinned toolchain.
- [ ] Parent: run `cargo test -p probe-mihomo --locked` and
  `cargo clippy -p probe-mihomo --all-targets --locked -- -D warnings`.
- [ ] Parent: verify native lifecycle tests on Linux and Windows, particularly
  the Windows job/NT API path and inherited pipe cleanup.
- [ ] Parent: wire validation into round preparation without logging raw kernel
  diagnostics; freeze node failure streaks on infrastructure/degraded results.
- [ ] Parent: run real Mihomo v1.19.32 validation/controller tests and record
  native error-shape evidence, required local assets and reload recovery.

Rollback is limited to this worker's hunks and newly added test files. Do not
delete the crate or revert the pre-existing dirty worktree.
