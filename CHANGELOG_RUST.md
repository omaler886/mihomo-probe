# CHANGELOG_RUST

Rust 全量重构的变更台账。每批含影响范围与回滚方法；总控文档见
`GLM_5.3_Flash_mihomo_probe_Rust_full_rewrite.md`，逐工作流状态见 `workstreams/`。

格式：每批一节，`影响范围` / `回滚` 必填。

---

## R0 — 基线审计 + P0 整改（2026-09-30）

基线：HEAD 03cb6ea，Python 3.12.10，`python -m unittest discover -s tests -v`
→ **Ran 554 tests, OK (skipped=2)**（基线输出留档于会话诊断，不入库）。

### 新增
- `workstreams/00..16*.md`：17 个工作流工作台（审计结论、设计决策、待办、证据）。
- `GLM_5.3_Flash_mihomo_probe_Rust_full_rewrite.md` 入库（总控文档）。
- `SECURITY_CREDENTIAL_ROTATION.md`：凭据轮换台账（只记位置与动作，不记值）。
- `tools/scan_secrets.py`：secret 扫描门禁（工作区 + `--history` 全历史；本次运行：工作区与 13 个历史提交均 clean）。
- `tests/test_hardening.py`：StatusRedactionTest（4 例）、MaskablePatchTest（3 例）、
  MixedPortDefaultTest（2 例）、QueryTokenCompatTest（3 例）。

### 修复（P0）
1. **/api/status 泄密（SECURITY_REVIEW S-12 → P0）**：`server.redacted_config` 原先只掩
   `auth.token`/`publish.token`，Telegram bot token、webhook URL、Sub-Store 后端密钥路径
   随 5 秒轮询明文外送。现全部掩码（backend 保留 scheme+host、路径掩码）。
   影响范围：仅 `/api/status` 响应；`exports[].url` 仍带 publish token（面板复制用途，读-only）。
   配套：`config.validate_patch` 对 `config.MASKABLE_PATHS` 丢弃哨兵 `"***"`——
   设置表单回读掩码值再保存**不会**把真实凭据覆盖成掩码。
2. **core.mixed_port 缺省 KeyError（ARCHITECTURE 陷阱清单 / 总控 §2.3）**：
   `DEFAULTS["core"]["mixed_port"]` = `MIHOMO_TEST_MIXED_PORT` 环境变量或 19194
   （部署文档现网值）；`NUMERIC_BOUNDS` 增加 `(1024, 65535)` 夹取。
   新装环境首轮不再死于 `core.build_config` 直接下标。
3. **query token 标记兼容模式（总控 §2.3）**：鉴权三通道保持；新增 `server.auth_kind`
   区分 (query|header)×(admin|publish)。admin 域经 query token 认证 → 每进程一次 warn
   日志引导迁移 `X-Auth-Token`/`Bearer`；publish 域（/api/export/*、/api/probe/nodes）
   的 query token 是 Sub-Store 既定集成面，不受影响。401 hint 改推头部。
4. **install.sh 测试门禁（总控 §2.2）**：原 `docker exec … unittest … | tail -3 || true`
   —— 管道吞退出码 + 硬吞失败，红套件照常部署。改为构建后、替换现网栈之前用
   `docker compose run --rm --no-deps` 一次性容器跑套件，POSIX 安全取 rc，失败 exit 1
   且现网栈不动。
5. **Docker Socket 退役方案（总控 §2.3 / ADR-0002）**：三步走写入 `workstreams/12`；
   本批不改 compose 行为（Python 期保持现网稳定）。

### 文档一致性
- README / MIGRATION_GUIDE / ARCHITECTURE 中「mixed_port 不在 DEFAULTS、新装必踩」的
  迁移陷阱条目标注 R0 已修复（保留原文描述以留档陷阱成因）。

### 影响范围
- Python 默认实现的行为变化仅限：/api/status 掩码扩展、401 hint 文案、admin query token
  一次性 warn 日志、新装环境 mixed_port 有默认值。轮次/导出/联动逻辑零变化。
- 554+12 测试全部通过后提交；Rust 尚未引入。

### 回滚
- `git revert` 本提交即回到 03cb6ea 行为；无数据/格式迁移，无状态兼容问题。

---

## R1 — Rust workspace + 首条纵向切片（2026-09-30）

切片链路：`probe-cli` 读 config.json（兼容 Python 部署的 config 格式）→ 生成内核配置
（与 core.build_config 逐字段一致）→ 调 Mihomo Controller（/version、PUT /configs?force=true）
→ 写入一条 round（SQLite）→ `GET /api/v1/status` 返回。默认 Python 服务不受影响
（Rust 不接流量，shadow 形态）。

### 新增
- `Cargo.toml` + `Cargo.lock`（提交锁定，CI `--locked`）。
- `crates/probe-domain`：RoundId/RoundStatus/RoundSummary 最小领域模型（不依赖 IO）。
- `crates/probe-config`：兼容读 `data/config.json` + DEFAULTS 语义（含 mixed_port 默认 19194、
  core.secret 文件读取）；缺字段回退默认而非报错（对齐 Python `load()` 容错）。
- `crates/probe-mihomo`：ConfigBuilder（lanes/IN-NAME 规则/inline-JSON proxies 逐字段对齐）、
  `ReasonClassifier`（对拍 Python `_reason_from` 的 timeout/kernel_error/bad_request/
  unreachable/http_<status> 表）、Controller 客户端（version/reload，Bearer secret）。
- `crates/probe-storage`：rusqlite(bundled)（ADR-0001：Windows 可复现构建；R2 复评 sqlx），
  rounds 表读写（与 Python schema 同列）。
- `crates/probe-api`：axum `/healthz` `/readyz` `/api/v1/status`（零秘密）`POST /api/v1/rounds`；
  Bearer 鉴权；loopback 默认。
- `crates/probe-cli`：`serve` / `round` / `status` 子命令。
- `.github/workflows/ci.yml`：python（py_compile + unittest + secret scan）与 rust
  （fmt --check / clippy -D warnings / test --locked / secret scan）双 job 门禁。
- `TEST_REPORT_RUST.md`：cargo fmt/clippy/test 真实输出留档。

### 本机环境注记（如实记录）
- 本机默认工具链 x86_64-pc-windows-gnu 缺 C 编译器，rusqlite bundled 无法构建；
  本机构建用 `cargo +stable-x86_64-pc-windows-msvc`（VS 2022 在位）。CI（ubuntu）不受影响。
- serde_json 启用 `preserve_order`：Python `json.dumps` 按文档序输出 proxy 字段，
  生成的内核配置须与 Python 逐字节可比（快照测试锚定）。
- 冒烟：临时根上 `probe-cli status/round/serve` 全链路通过（round 行如实闭合、
  /api/v1/status 零秘密、401/202、WAL、core.secret 自动生成），详见 TEST_REPORT_RUST.md。

### 影响范围
- 全部为新增目录；不修改任何 Python 文件、不改动 compose。Python 服务照常部署运行。

### 回滚
- 删除 `crates/`、`Cargo.toml`、`Cargo.lock`、`TEST_REPORT_RUST.md`（后续批次按同法）。
