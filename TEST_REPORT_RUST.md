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

---

## R2 — 2026-09-30

### cargo fmt / clippy
```
cargo +stable-x86_64-pc-windows-msvc fmt --all -- --check   # 通过
cargo +stable-x86_64-pc-windows-msvc clippy --workspace --all-targets -- -D warnings
→ Finished（一处 needless_mut_ref 于测试代码已改）
```

### cargo test --workspace --locked（30 通过 / 0 失败）
```
probe-storage 9: 迁移版本连续性、幂等重放、Python 旧库迁移后数据逐行保全+新表就位+integrity ok、
                 备份快照一致性、open_without_migrating 不变更状态、round/时间戳原有 4 例
probe-mihomo 8 / probe-api 5 / probe-config 6 / probe-domain 2（与 R1 相同，回归通过）
```

### db 子命令端到端演练（Python 形态 legacy 库，数据为占位符）
```
db check   → applied 0, pending 2, integrity ok, required tables missing
db migrate → backup written: state.db.bak-<ts>；2 applied；counts: nodes 1 / rounds 1 / results 1（legacy 数据保全）
db verify  → integrity ok + 全表计数
Python 复读 → nodes/rounds/results 原值不变，export_snapshots 可见
db rollback → 缺文件 rc=1；无 --yes rc=2；--yes 恢复迁移前快照，check 如实报“缺表”（备份早于迁移，语义正确）
```

---

## R3 — 2026-10-01

### fmt / clippy
```
cargo fmt --all -- --check   # 通过
cargo clippy --workspace --all-targets -- -D warnings   # Finished（校验器补 120s 超时+kill）
```

### cargo test --workspace --locked（45 通过 / 0 失败，新增 15）
- lanes(3)：端口区间、lane_count 夹取(0→1/99→32)、组名/监听名与 Python 逐字符一致
- exit(6)：trace 解析（ip/loc/colo、垃圾行、空键）、经车道读出口身份（reqwest+Proxy::all 对 axum mock 代理，
  绝对 URI 代理形态即真实 mihomo 入站所见）、空 trace 判失败、fetch 字节上限（1MiB 体 64KiB 帽，提前停）、
  拨号失败有界错误（≤160 字符）
- config_check(6)：退出码→Passed/Failed、无 MIHOMO_BIN 且无 docker → Degraded（显式降级，绝不读作通过）、
  culprit 定位（引号名优先/短名永不 token 匹配——"jp" 历史 bug/长名边界匹配（BageVM-Tokyo 不匹配
  BageVM-Tokyo-2）/server 兜底）
- controller 新增 select：PUT /proxies/{group}（200/204 ok，非 2xx 带正文报错）
- 回归：R1/R2 全部用例通过
