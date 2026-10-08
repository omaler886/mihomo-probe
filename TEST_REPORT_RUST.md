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

---

## R5（并发部分）— 2026-10-01

新 crate `probe-engine`，本批只含分层并发闸门（`src/limits.rs`）。
新增依赖：`tokio` 补 `sync`/`time` 特性、`tokio-util 0.7.19`（CancellationToken）。
`tokio-util` 的 `sync` 模块在 0.7 中**不受 feature 门控**——写 `features = ["sync"]`
会报 `tokio-util does not have that feature` 并中止解析（实测踩过）。

### fmt / clippy
```
cargo fmt --all -- --check                                  # 通过
cargo clippy --workspace --all-targets -- -D warnings       # Finished（零 lint；仅 Windows
                                                            # 增量目录 os error 5 噪声）
```

### cargo test --workspace --locked（**58 通过 / 0 失败**，新增 13）
```
probe-engine 13:
  global_limit_is_enforced                      global=2 / 8 个独立源与地址，全局峰值恰为 2
  per_source_limit_serialises_a_single_source   同源 4 任务峰值 1；另有 3 个对照组任务
                                                （独立源+独立 IP）证明 global 确实放行了 >1
  per_server_ip_limit_serialises_one_address    同 IP 4 任务峰值 1；同样带对照组
  all_three_layers_hold_at_once                 3 源 × 3 IP，三层同时生效，峰值均不越界
  permits_return_to_the_pool                    12 任务后 available_permits 复原
  cancel_fails_a_queued_job_instead_of_waiting  取消唤醒阻塞中的 acquire，返回 Cancelled
  check_refuses_the_ledger_write_after_cancel   check() 在取消后拒绝写账本
  mixed_load_finishes_within_budget             60 任务 / 4 源 / 3 IP，5s 内排空（死锁哨兵；
                                                理论下限 ~0.5s）
  diagnose_lane_runs_while_the_fast_lane_is_saturated  快车道打满时诊断车道仍可获取
  idle_registry_entries_are_pruned              2098 个不同 IP 后注册表 < 256（跨多轮剪枝窗口）
  zero_limits_degrade_to_serial_instead_of_hanging  全 0 上限夹取为 1，不死锁
  effective_limits_clamp_at_the_ceiling         usize::MAX / 0 / 99999 的夹取
  from_concurrency_keeps_the_deployed_ceiling   concurrency=20 → global 仍为 20
其余 crate（probe-api 5 / config 6 / domain 2 / mihomo 23 / storage 9）回归通过
```

### 时序用例稳定性
```
for i in 1..10: cargo test -p probe-engine   → 10/10 轮 13 passed，无抖动
```

### 独立审查（只读 subagent）发现并修正的 7 项
1. 模块文档把 `diagnose` 画成 `global` 的子层，与实现矛盾——`acquire_diagnose` 不取
   global，诊断车道在 global 打满时仍可并发。**已改为平级并写明峰值是 `global + diagnose`**。
2. `Permit` 注释称"逆序归还"，实际 `Vec` 正序 drop（global 先放）。已改注释。
3. 剪枝测试断言 `ips <= 1024` 过弱（只要剪枝跑过一次就必过）。改为 2098 个 IP + `< 256`。
4. 死锁哨兵 20s 过松（理论下限 ~0.5s）。降到 5s。
5. per_source / per_server_ip 两测缺正向断言，可能因上层顺带串行而假阳性。
   **加对照组后暴露了原断言设计错误**（4 个任务同源时 global 峰值必然是 1），已修正。
6. `REGISTRY_PRUNE_AT` 的理由写成"长生命周期进程累积"——`Gate` 随轮次建销，不成立。
   已改为"限制单轮内的分配"。
7. §2.4 的"节点集合每轮都变"是未实测的前提。已改写为不依赖该前提的论证。

### 已知限制（如实记录）
- 本批只交付**闸门本身**，尚无调用方：未接入实际轮次流水线，因此
  "取消后不写账本"目前由 API 约定保证，不是由类型系统强制。
  **→ 已由下一节（R5 轮次编排）解决：闸门已接进 `run_round`，并有对应单测。**
- **`diagnose` 不在 `global` 之下**（有意为之：否则会排在它所绕开的拥塞后面）。
  因此单轮在飞峰值为 `global + diagnose`，配置两层时要一起算。
- `Limits::from_concurrency(20)` 的 per_source/per_server_ip/diagnose 是**比例初值**，
  未用真机数据调优（06 已列为待办）。
- 同一任务对同一 key 取两次会自锁（信号量不可重入）；链式节点必须传前置地址。
  这是调用方契约，**代码层无法强制**，只能靠文档与 code review。
- 未做 `-race` 等价检查（Rust 无 `-race`；Miri/loom 选型在 14 的 R12 批次）。
- 单元测试全部用 `multi_thread` 运行时，但仍在单机 4 线程下跑；真机高并发未验证。

---

## R5（RoundCtx 接入流水线）— 2026-10-01

把分层并发闸门接进真实轮次路径。此前轮次逻辑**存在两份**（`probe-cli::cmd_round`
与 `probe-api::start_round`），都是同一段占位代码，且都没走 `RoundCtx` —— 闸门没有调用方。

### 新增/修改
- `crates/probe-engine/src/round.rs`（新）：`run_round` / `run_round_with_ctx`，
  trait `NodeTester` / `Ledger` / `KernelPrep`，`KernelDelayTester` / `ControllerPrep` /
  `NotPrepared`，`RoundSettings`（把「哪些配置决定一轮怎么跑」收在一处）。
- `crates/probe-storage`：`ResultRow`、`record_results`（**单事务**批量写入）、
  `finish_round_with_counts`、`results_for_round`，常量 `VERDICT_OK/FAIL/EXCLUDED`。
- `crates/probe-config`：新增 `TestConfig`（`targets` / `expected_status` / `timeout_ms` /
  `concurrency`），默认值与 Python `config.DEFAULTS["test"]` 一致。
- `crates/probe-cli`、`crates/probe-api`：轮次路径改为调用 `run_round`，删掉两份重复占位。

### fmt / clippy
```
cargo fmt --all -- --check                                # 通过
cargo clippy --workspace --all-targets -- -D warnings     # 0 errors
```

### cargo test --workspace --locked（**79 通过 / 0 失败**，新增 17）
```
probe-engine 25（+12）:
  a_round_records_every_verdict_and_closes_once        一次开行/一次关行/一批写入
  failures_are_counted_and_carry_their_class           失败计数与 FailureKind 落 row
  the_gate_actually_bounds_the_node_phase              per_server_ip=1 真的串行（闸门确实生效）
  distinct_addresses_still_run_concurrently            对照组：不同地址仍并发
  an_unreachable_kernel_skips_the_node_phase_and_still_closes  内核没了就不测，行照样关
  a_refused_reload_still_tests_nodes                   reload 被拒仍测（内核可能在跑旧配置）
  cancellation_drops_results_but_still_closes_the_row  取消：0 结果写入，行仍关闭
  a_round_already_cancelled_writes_nothing_but_closes  开始前已取消
  the_note_never_carries_a_controller_message           note 不含错误正文/URL
  detail_truncation_is_character_safe                  200 字符截断不 panic（CJK 安全）
probe-config 11（+5）：test 段默认值对齐 Python、空 targets 回落、全 http 无首选目标、覆盖
probe-storage 13（+4）：results 往返、空批次不写、finish_with_counts 写 total/ok
probe-api 5 / probe-mihomo 23 / probe-domain 2：回归通过
```

### CLI 端到端冒烟（临时根 `.tmp_diag/smoke-r5`）
```
probe-cli --root <tmp> round
  → round 1: kernel unreachable; 0 node(s) not tested
probe-cli --root <tmp> status
  → last round: #1 trigger=cli finished_at=Some("2026-10-01T14:17:20")
                note=Some("kernel unreachable; 0 node(s) not tested")
账本直查：rounds 1 行（total/ok/failed 均为 0，finished_at 有值）；
          results 0 行；SELECT ... WHERE finished_at IS NULL → 空
```

### 时序用例稳定性
```
cargo test -p probe-engine  × 10 → 10/10 轮 23 passed
cargo test -p probe-api     × 10 → 10/10 轮  5 passed
```

### 独立审查（只读 subagent）发现并修正的 6 项
1. **[高] `in_flight` 非全路径清理**：清理写在 spawned task 尾部，task panic 即跳过
   → `POST /api/v1/rounds` 永久 409。改为 RAII drop guard `InFlight`（unwind 也会释放），
   并把 handler 里 `storage.lock().expect(...)` 的 panic 面收成显式 500。
2. **[中] 全 http 目标与 Python 分歧**：我原把"没有 https 目标"当作 blocked 跳过节点阶段。
   实读 `engine.py:247` 后确认：`https_required = urls[0].startswith("https://")`（分区之后），
   **全 http 列表时 Python 会把 http 通过判为存活**。已对齐，并新增
   `TestConfig::https_required()` 把这个规则写进代码。
3. **[中] 单事务"失败不留半截"没有证据**：新增用例用 `BEFORE INSERT` 触发器让**第二行**中止，
   断言 `results` 一行不剩，且后续批次仍可用 —— 这是唯一能证明回滚的写法。
4. **[中] `unchecked_transaction` 的隐含依赖未写明**：它取 `&self`、绕过借用检查，
   只在"每连接一个 `Mutex`"下安全。已在 doc 里写明"不要把 `Storage` 交给两个线程/两个并发 future"。
5. **[低] 取消后 permit 是否复原无直接断言**：新增 `Gate::available_permits()`，
   在取消用例里断言归位（`(limits.global, limits.diagnose)`）。
6. **[低] 文档陈旧/措辞过强**：`16_FINAL_REVIEW` 仍写"未接流水线"；`06` 的"每条路径都关"
   未标注 `finish_round` 自身失败的例外。均已修。

审查同时确认成立的：`finish_round` 是开行后唯一的 `?`（其余失败都被 match 吞下再关行）；
取消用例非退化（`per_server_ip=1` 串行保证取消落在轮次中）；两条闸门用例互为对照
（纯串行实现过不了对照组，无限流实现过不了主用例）；`TestConfig` 四项默认值与
`config.py:239-247` 逐字一致；API 响应形状未变。

### 已知限制（如实记录）
- **轮次目前没有节点**：`jobs` 恒为空（节点采集 Sub-Store→fingerprint→变体尚未移植）。
  所以闸门虽然接上了，实际还没限到任何东西 —— 单测用假 tester 证明它能限。
- `KernelDelayTester` 只做**一次**尝试、只打分区后的**首个**目标。
  Python `test_one` 的 `max_attempts=3`、超时升级（`timeout_ms_retry`）、
  `TERMINAL_REASONS` 短路、以及失败后轮换目标均未移植；`attempts` 列固定写 1。
- 没有重试、没有整轮护栏（`GuardDecision` 是 R7）、没有出口验证（R6）。
  取消路径已具备且已测，但**没有对外触发点**（`POST /api/v1/rounds/{id}/cancel` 属 R9）。
- `RoundSummary` 未增加 `failed` 字段，`last_round()` 读不到 `failed`；
  `finish_round_with_counts` 写进去了但读侧暂不可见。
- `POST /api/v1/rounds` 的轮次行由 handler 同步开启（为在响应里返回 `round_id`），
  再交给 spawned task 关闭。若 `finish_round` 失败，行会留在打开态 —— 依赖
  `open_round_ids()` / 孤儿回收，日志会明确报出。**这是 `run_round` 唯一不关行的路径。**

---

## R4 — 2026-10-08（补做批；HEAD：R4 提交）

> R4 在 R0–R3 后被跳过、直接进了 R5–R7，`crates/probe-dns/` 一直以未提交的工作区状态
> 存在（未 `git add`，`Cargo.toml` 成员行与 `Cargo.lock` 条目也未提交）。本批整理、补齐、入账。

### fmt / clippy
```
cargo +stable-x86_64-pc-windows-msvc fmt --all
cargo +stable-x86_64-pc-windows-msvc fmt --all -- --check      # 无输出
cargo +stable-x86_64-pc-windows-msvc clippy --workspace --all-targets -- -D warnings
→ 0 error（判据 `grep -cE "^error"`；输出里的 os error 5 是 Windows 增量目录噪声，非 lint）
```
- 上一批（lane 分桶修复）遗留一处 `needless_lifetimes`（`crates/probe-mihomo/src/lanes.rs:47`），
  说明那批没跑过 clippy；本批一并修掉。

### cargo test --workspace --locked（**243 通过 / 0 失败**，本批新增 22）
```
probe-api      5 passed
probe-cli     24 passed
probe-config  20 passed
probe-dns     22 passed   ← 本批新增（wire 18 / geo 3 / resolver 1）
probe-domain  12 passed
probe-engine  67 passed
probe-mihomo  25 passed
probe-source  46 passed
probe-storage 19 passed
probe-substore 23 passed（耗时 190s，rquickjs 跑官方 bundle）
```
（243 = R7 后 219 + lane 修复 2 + 本批 22）

### probe-dns 的 22 个用例
```
wire（18）：报文头/ECS option 逐字节断言、无 ECS 则无 additional、非 ASCII label 拒绝、
           A/AAAA 顺序、跳过其它类型、非零 RCODE、短包、自指针 question、
           answer owner name 走指针；+ 8 个 fixture 用例（见下）
geo（3）：批量 POST 语义（本地 axum stub 回显）、429 一次重试后报错、畸形 body 也重试
resolver（1）：视图内去重保序、无 resolver 的视图被跳过、失败视图产空列表
```

### 二进制 fixture（`crates/probe-dns/fixtures/dns-packets/`，8 个）
由 `tools/make_dns_fixtures.py` 生成（无随机、每次写出同样字节）。逐包断言：
```
a-answer-compressed.bin    43B  owner name 指针→offset 12 → ["1.2.3.4"]
aaaa-answer.bin            56B  16B rdata → ["2001:db8::1"]
cname-then-a.bin           66B  ANCOUNT=2，跳过 CNAME → ["1.2.3.4"]
nxdomain.bin               30B  rcode 3 → Err(Rcode(3))
truncated-answer.bin       41B  rdlength 声称 4 实给 2 → Err(Truncated)
pointer-loop-question.bin  18B  question 自指针 → Ok([])
bad-label-overrun.bin      22B  label 长度 0x41(65) → Err(Truncated)
short-header.bin            6B  < 12 → Err(Truncated)
```
其中「恶意长度」「坏标签」是内联测试原本没覆盖的两类。

### 独立审查（只读 subagent，两轮）发现并修正的 11 项
（完整版见 CHANGELOG_RUST.md 的 R4 节；此处只列摘要）
- **第一轮 6 项**：`fetch_batch` 缺 45s 超时；畸形 JSON body 未重试；`lib.rs`/`Cargo.toml`
  现在时假陈述；「坏标签」fixture 实际缺失；`parse_ips`/`MAX_NAME_JUMPS` doc 不准确；
  `skip_name`/`read_name` 逐字重复 + 3 个未使用依赖。
- **第二轮 5 项**（含第一轮的漏网与我自己的新错误）：`encode_name` doc 仍是假陈述；
  **顶注把「自指针」与「label 链越界」混为一谈（第一轮修复时我自己引入的）**；
  非数组 JSON 被重试（有意选择，已记 doc）；`query` 超时与 `ecs_prefix` 缺 Python 默认值
  （已加 `DEFAULT_TIMEOUT_S`/`DEFAULT_ECS_PREFIX`）；`MAX_NAME_JUMPS` off-by-one 等措辞。

审查者明确**无法验证**的一项：`skip_name`/`read_name` 合并前「逐字节相同」——这批代码
此前未入库，无基线可 diff；依据是整理时读到的原文。

审查确认成立：8 个 fixture 字节逐一手工解析与断言相符；超时/重试/80 截断与 Python 逐项
一致；删依赖无遗漏且 `serde_json` 够用；`rand_id` 对齐 `os.urandom(2)`；`build_query`
显式 qid 不影响对账；`resolve_views` 失败 query 的最终视图与 Python 一致。

### 已知限制（如实记录）
- **未接入任何调用方**：`grep -rn "probe-dns" crates/*/Cargo.toml` 只命中它自己，
  `probe-engine` 未依赖它；`classify_and_expand` 未实现。轮次仍"一个 server 测一个地址"。
- **缓存未做**：`domain_views`(6h) / `ip_geo` 归调用方，本 crate 不持有。
- **`cargo-fuzz` 未接**：md 目标是"fixture 与 fuzz"，本批只交付固定 fixture。
- **非 ASCII 域名是真实分歧**：Python 的 idna 编码会成功、Rust 拒绝（无 `idna` 依赖）。
- 本机无真内核/容器，与既有批次相同。

