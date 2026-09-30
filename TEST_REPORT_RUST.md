# TEST_REPORT_RUST

Rust 侧的真实测试证据。每次提交批次追加一节；只记录实际执行过的命令与输出。
Python 基线见 `workstreams/13_TEST_COMPATIBILITY.md`（R0: 568 通过 / 2 跳过）。

---

## R1 — 2026-09-30（HEAD：R1 提交）

### 环境
- rustc/cargo 1.98.1，工具链 `stable-x86_64-pc-windows-msvc`
  （本机默认 `x86_64-pc-windows-gnu` 缺 C 编译器，rusqlite bundled / windows-sys 无法构建；
  MSVC 侧有 VS 2022，构建正常。CI 在 ubuntu 上不受影响。）
- 命令一律 `cargo +stable-x86_64-pc-windows-msvc …`；CI 使用 `cargo test --workspace --locked`。

### 1. cargo fmt
```
cargo +stable-x86_64-pc-windows-msvc fmt --all          # 执行
cargo +stable-x86_64-pc-windows-msvc fmt --all -- --check   # 通过（无输出）
```

### 2. clippy（-D warnings，CI 同参）
```
cargo +stable-x86_64-pc-windows-msvc clippy --workspace --all-targets -- -D warnings
→ Finished `dev` profile … in 1.45s   （零告警；一处 clamp-like 模式已改为 .clamp(1, 32)）
```

### 3. cargo test --workspace --locked
```
probe-api     5 passed   (鉴权矩阵/零秘密状态/round 起一次且必闭合/ct 比较/healthz-readyz)
probe-config  6 passed   (缺文件/坏 JSON→默认值、覆盖合并、空 token 拒绝、core.secret 一次生成)
probe-domain  2 passed   (RoundSummary JSON 往返、错误展示)
probe-mihomo  8 passed   (内核配置逐字节快照、空组 DIRECT、失败分类对拍 _reason_from、
                          mock 内核：Bearer 必带/reload 204/delay 成功与 504 timeout、控制器失联归类)
probe-storage 4 passed   (round 行往返、UTC 时间戳与 Python db.now() 同形、open_rounds、空账本)
probe-cli     0 tests (bin) — 由下方冒烟覆盖
合计 25 passed / 0 failed（含 doc-tests 0）
```

### 4. CLI 端到端冒烟（临时根 .tmp_diag/smoke-root，测毕删除）
```
probe-cli --root <tmp> status   → "no rounds recorded yet"
probe-cli --root <tmp> round    → round 1: slice round: config at <tmp>\core\config.yaml;
                                  slice: controller unreachable (http://127.0.0.1:19190)
probe-cli --root <tmp> status   → last round: #1 trigger=cli finished_at=2026-09-30T14:28:25(note 如实)
probe-cli --root <tmp> serve    → /healthz {"ok":true}（无鉴权）
                                  /api/v1/status（带 token）→ {"service":"mihomo-probe-rs","rounds":…,
                                  "kernel":{"api_host_port_only":"127.0.0.1:19190"}}   # 零秘密
                                  /api/v1/status（无 token）→ 401
                                  POST /api/v1/rounds（带 token）→ 202，任务闭合 round 2（note=unreachable）
                                  WARN 日志: controller unreachable during slice round round_id=2
生成的 core/config.yaml 首 11 行与 Python build_config 逐字段一致
（mixed-port: 19194 / allow-lan: true / bind-address: 127.0.0.1 / external-controller: 127.0.0.1:19190 / secret 自动生成）
data/ 产出 state.db(+WAL) / core.secret(0600 语义) / config.json 未被改写
```

### 已知限制（如实记录）
- 本机无 Docker / 无真 mihomo 内核：`mihomo -t` 校验与真内核 reload 在 vps 复核（R3 门禁）。
- Controller 对 127.0.0.1:1（无监听）的连接失败归类为 Controller 错误——由单测锚定。
- 快照测试发现的 serde_json key 顺序差异已用 `preserve_order` 对齐 Python `json.dumps` 文档序。
