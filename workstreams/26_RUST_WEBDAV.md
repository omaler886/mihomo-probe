# 26 — Rust WebDAV 发布通道（用户目标「支持 github gist / webdav 等」的 WebDAV 半边）

> 目标：在 Rust 侧新增第三发布目的地 WebDAV，与既有 local/Gist/composite 完全
> 同构（同一幂等契约、同一错误码口径、同一脱敏与重试纪律），并接满 config /
> engine / CLI / API 四层。Gist 半边已在 B2/25 落地；本批补齐用户点名的
> WebDAV。验收沿用 19 号写法，每条 = 树内指名测试。

## 1. 交付物

| 文件 | 内容 |
|---|---|
| `crates/probe-config/src/lib.rs` | `WebdavSection`（url/username/password_file/directory/timeout_seconds/max_retries，clamp 与 Gist 同表）+ `PublishProvider::Webdav`（`"webdav"`）+ `publish.webdav` 解析 |
| `crates/probe-publish/src/webdav.rs` | `WebDavPublisher`：`Publisher`（逐 artifact PUT + manifest.json 合并，外部条目保真）+ `FilesPublisher`（canonical 四文件，gate 最后落）；`create_collection`（`webdav init`，MKCOL 链，405=已存在幂等）；`webdav_auth`/`webdav_missing` 两个新错误码 |
| `crates/probe-engine/src/publish_remote.rs` | `PublishProvider::Webdav` 分支：WebDAV 作为远程腿目的地（local 管线照旧先行提交） |
| `crates/probe-cli/src/main.rs` | `webdav test|init` 子命令（masked 输出：configured/ok/error_code，永不打印密码/URL） |
| `crates/probe-api/src/publishers.rs` | `POST /api/v1/publishers/webdav/test`（与 Gist 探针同一 mask 契约） |

## 2. 设计裁决

1. **manifest.json 与 Gist 同构**（F-031）：`source_id -> entry` 映射，外部条目
   逐字保真；非 JSON object 的 manifest 拒写不覆盖（`manifest_invalid`）。
2. **WebDAV 无批量写 → 顺序即 revision 语义**：artifact 文件先 PUT，
   `manifest.json` 最后 PUT（提交标记）；崩溃在两者之间 → 下一轮「内容已活 +
   manifest 漂移」→ 仅补 PUT manifest，收敛不重复。`FilesPublisher` 同理：
   **gate 文件（canonical）最后落**，gate 即完整集标记。
3. **凭据纪律与 Gist 相同**：密码只存在于 `password_file`（0600 检查，Unix），
   构造期读取；URL 禁止内嵌 `user:pass@`（config.json/日志防泄漏）；全部错误
   消息过密码脱敏。无 password_file = 匿名请求（局域网 WebDAV/反代认证场景）。
4. **错误码**：401/403 → `webdav_auth`（终态）；404（healthcheck）与 409（PUT
   父集合缺失）→ `webdav_missing`（终态，`webdav init` 是修复）；429 →
   `rate_limited`（Retry-After 有界）；5xx/网络 → 重试退避（1s→2s→4s，
   `max_retries` 总次数）。文件级 GET 404 不是错误——是「文件不存在」的正常
   首发状态，降级为 `Ok(None)`。
5. **healthcheck 用 PROPFIND (Depth:0)**（RFC 4918 存在性检查，真实 WebDAV
   服务器全支持；GET on collection 是可选行为不可依赖）。
6. **真 WebDAV 服务器属 NOT_RUN**（沿用 §11 口径）：协议行为由 127.0.0.1 假
   服务器（axum stub，含 PROPFIND/MKCOL/PUT/GET 与故障注入）钉住。

## 3. 验收映射（每条 = 树内指名测试）

| # | 行为 | 测试（webdav.rs tests，除注明外） | 状态 |
|---|---|---|---|
| 1 | 首发 PUT artifact→manifest（提交标记顺序） | `a_first_publish_puts_the_artifact_then_the_manifest` | covered |
| 2 | previous_sha 门禁：零请求（F-032 gate 1） | `a_previous_sha_match_sends_zero_requests` | covered |
| 3 | 远端已同内容+manifest 同步 → GET-only 零写（gate 2） | `live_content_with_a_current_manifest_is_get_only` | covered |
| 4 | 崩溃收敛：manifest 漂移 → 仅补 manifest | `a_manifest_drift_repairs_with_a_manifest_only_put` | covered |
| 5 | 外部 manifest 条目保真；corrupt manifest 拒写 | `foreign_manifest_entries_survive_and_corrupt_ones_are_refused` | covered |
| 6 | 401 终态 `webdav_auth` + 密码脱敏 + 零重试 | `auth_failures_are_terminal_and_redacted` | covered |
| 7 | PUT 409 → 终态 `webdav_missing` | `a_conflict_put_maps_to_webdav_missing` | covered |
| 8 | 5xx 退避重试后成功 | `server_errors_retry_until_success` | covered |
| 9 | gate 未变 → NoChange 零写 | `the_gate_file_unchanged_publishes_nothing` | covered |
| 10 | gate 最后落 + 只发变化文件 | `the_gate_file_put_is_last_in_the_wire_order` | covered |
| 11 | init MKCOL 链 + 405 幂等 | `init_walks_the_directory_chain_idempotently` | covered |
| 12 | healthcheck 冻结类映射（200/207/404/401） | `the_healthcheck_maps_the_frozen_classes` | covered |
| 13 | URL/目录构造期校验（凭据/协议/traversal/query） | `construction_validates_the_destination_before_any_request` | covered |
| 14 | 密码只来自文件（空/缺失拒构造） | `the_password_comes_only_from_its_file` | covered |
| 15 | config 解析/clamp/默认（`WebdavSection`） | probe-config `the_webdav_section_parses_with_the_same_contract` | covered |
| 16 | provider 解析新增 `webdav` | probe-config `the_publish_provider_parses_with_a_conservative_fallback` | covered |
| 17 | 引擎远程腿 Webdav 构造失败 → config_error 且本地结果站住 | publish_remote `an_unconfigured_webdav_marks_config_error_and_keeps_local_results` | covered |
| 18 | API 探针 mask 契约 + stub 200/404 映射 | publishers `webdav_test_shares_the_mask_contract` / `the_webdav_probe_maps_destination_answers` | covered |

## 4. 门禁（2026-11-07 实跑）

| 门禁 | 结果 |
|---|---|
| `cargo test --workspace` | 30 个测试目标全 ok、**748 passed / 0 failed**（730 基线 + 18 新增） |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过（0 警告） |
| `cargo fmt --all -- --check` | 通过 |
| `tools/scan_secrets.py` | clean |

## 5. 集成边界（后续批次）

- GUI 发布中心加 WebDAV 测试按钮/通道选择（与 27 批 GUI 收口同批）。
- canonical 四文件 round 接线（workstream 27）完成后，`provider: webdav` 即
  获得与 Gist 相同的 canonical 幂等四文件链路。
