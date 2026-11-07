# 30 — Rust Web 控制台发布中心收口 + 浏览器级自动化验证

> 用户目标「最终目标是实现 web 界面自动化」的收口批：Rust 独立 GUI（/gui）
> 的发布设置页接上本会话新增的全部发布面——Gist/WebDAV 双通道测试、
> 通道/就绪状态徽章、人工推送三件套——并用浏览器自动化真实点击验证。
> 验收 = 浏览器级证据（DOM 快照 + 截图 + 账本审计行）。

## 1. 交付物

| 文件 | 内容 |
|---|---|
| `crates/probe-api/src/gui.rs` | 发布设置页：①「测试连接 (WebDAV)」按钮；②「人工推送到 Sub-Store」三按钮（预览差异/签发确认单/推送，推送带 window.confirm 二次确认，一次点击完成 confirm-token→push 链）；③通道徽章（通道 provider、Gist/WebDAV 就绪态）；④`publisherAction` 改读探针端点的 body mask（configured/ok/error_code）——恒 200 的响应不再误报「成功」；⑤修复 `renderPublishConfig` 读 `summary.publish`（端点嵌套形状），此前整页 mask 落空对象默认值 |
| `crates/probe-api/src/config_check.rs` | `/api/v1/config/validate` 的 publish 面补 `provider`/`gist_ready`/`webdav_ready`/`substore_target_name`（布尔化，永不回显凭据） |

## 2. 浏览器级自动化验证（2026-11-08 实跑，throwaway root）

环境：`probe-cli serve`（.tmpcheck/gui-root，auth token + publish.enabled，
schedule 关），ZCode 内置浏览器（IAB）驱动。

| # | 步骤 | 结果 |
|---|---|---|
| 1 | 打开 /gui → 输入管理令牌 → 进入 | gate 隐藏、app 显示、会话建立（CSRF 签发） |
| 2 | 总览页 | 运行状态/上次轮次/Sources 卡片/立即测活按钮齐全 |
| 3 | 发布设置页 | 发布功能=已启用、前缀/主机名/通道=local、徽章四枚（发布已启用/通道 local/Gist 未就绪/WebDAV 未就绪）、推送目标=mihomo-probe-alive |
| 4 | 测试连接 (Gist) | 「未启用（未配置凭据或 URL，error_code config）」——mask 如实呈现 |
| 5 | 测试连接 (WebDAV) | 同上（workstreams/26 端点接通） |
| 6 | 预览差异（无产物） | 409 NO_CANONICAL_ARTIFACT，消息如实显示 |
| 7 | 签发确认单（无产物） | 409 同上 |
| 8 | 落盘 canonical parity 文件后签发确认单 | 成功，60s 有效、绑定 digest sha256:c80d1… |
| 9 | 推送到 Sub-Store（confirm 对话框接受） | 对话框弹出→接受→推送链执行；backend 不可达 → SUBSTORE_ERROR 如实呈现（update-only 的 sub_exists 重试链） |
| 10 | 账本审计 | publish_audit 逐行落：no_artifact×2 / confirm_issued / substore_error（preview）——每个动作含拒绝都可审计 |
| 11 | Source 详情 / 清理中心 | 两页渲染正常；清理扫描 dry-run 对不可达 backend 如实报「扫描清理失败（502 backend）。未做任何更改。」 |
| 12 | 截图 | 发布设置页整页截图（交付于会话工件） |

## 3. 裁决与边界

1. **GUI 只做 mask 呈现**：探针端点恒答 200，真实结论在 body 的
   configured/ok/error_code——UI 据此分流「未启用/应答异常/可达」三种话术，
   绝不把 HTTP 200 当成功。
2. **推送按钮 = 一次点击、两段请求**：confirm-token 仍由服务端签发与消费，
   三重门禁（60s/一次性/绑定）不因 UI 合并而松动；`window.confirm` 是第二
   人工确认。
3. **验证环境的 backend 指向不可达地址**是有意的：失败路径全部如实呈现并
   落审计；成功路径由 manual_push/subadmin 的 stub 测试覆盖（workstreams/29）。
4. **遗留**：推送腿对不可达 backend 的重试链耗时数十秒（SubStoreClient 的
   有界退避），面板在其间停留在「请求中…」——可接受，不做前端超时绕过。

## 4. 门禁（2026-11-08 实跑）

| 门禁 | 结果 |
|---|---|
| `cargo test --workspace` | 31 目标全 ok、**785 passed / 0 failed** |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过 |
| `cargo fmt --all -- --check` | 通过 |
| `tools/scan_secrets.py` | clean |
