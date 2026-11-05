# 测试基线报告（迁移前基线 / test-baseline）

日期：2026-09-28。目的：在功能合并移植到官方 Sub-Store 架构之前，记录当前仓库离线测试套件的运行方式与结果基线，供迁移后回归对照。本次除本报告外未写/改任何文件，未安装任何依赖。

## 一、环境

| 项目 | 值 | 获取命令 |
|---|---|---|
| Python | 3.12.10 | `python --version` |
| pytest | 8.3.4 | `python -m pytest --version` |
| PyYAML | 6.0.3 | `python -c "import yaml; print(yaml.__version__)"` |
| 平台 | win32 10.0.26200 x64，Git Bash | 环境信息 |
| 运行时依赖 | 仅 `PyYAML==6.0.3`（requirements.txt 唯一条目，其余为标准库） | `cat requirements.txt` |

其他事实：

- 仓库无 pytest.ini / setup.cfg / pyproject.toml / conftest.py，测试为纯 unittest 风格，测试配置零依赖。
- README 约定跑法：`python3 -m unittest discover -s tests`（本机 Windows 用 `python -m unittest discover -s tests`）。
- 环境变量 `MIHOMO_TEST_ROOT`、`MIHOMO_TEST_LIVE` 均未设置（`echo ${MIHOMO_TEST_ROOT:-<unset>}` → `<unset>`）。
- 工作区在测试运行前已带未提交改动（`git status --porcelain` 显示 `mihomo_test/*.py`、`tests/test_*.py`、`tools/*.py` 等多个 M 状态；文件时间戳早于本次运行）。本基线针对"当前工作区状态"而非某个干净 commit。

## 二、隔离机制与 test_live 守卫（先读后跑的结论）

**`tests/_isolation.py`**：离线套件的密闭性保障。离线模块（test_logic / test_alerts_lanes / test_hardening / test_ipmap）在 `setUpModule()` 调 `isolate()`，把以下模块级路径全部重定向到 `tempfile.mkdtemp(prefix="mihomo-test-suite-")`，`tearDownModule()` 调 `restore()` 还原：

```
config.DATA, config.CONFIG_PATH, db.DB_PATH, db._conn,
engine.ROUND_STATE, engine.EXPORT_DIR, notifier.STATE_PATH
```

模块 docstring 明确：不隔离的测试会把 `round.state.json`/`config.json`/`state.db` 写进生产数据目录，且残留的 `data/config.json` 会让 `test_live` 误判"存在真实部署"而开始打线上面板。`tests/test_live.py` 刻意不用它——它就是为打真实部署而存在的。

**`tests/test_live.py` 模块级守卫**（本机自动跳过的原因）：

```python
ROOT = Path(os.environ.get("MIHOMO_TEST_ROOT", "/srv/mihomo-test"))
LIVE_DEPLOYED = ((ROOT / "data" / "config.json").exists()
                 and (ROOT / "data" / "state.db").exists())
if not LIVE_DEPLOYED and os.environ.get("MIHOMO_TEST_LIVE") != "1":
    raise unittest.SkipTest(f"no live deployment at {ROOT}; ...")
```

本机无 `/srv`（`ls /srv` → no such directory），守卫生效，模块级 SkipTest。**实测确认：`unittest discover` 与 pytest 收集均不会连任何真实部署。**

## 三、测试结果表

全量命令（仓库根目录）：`python -m unittest discover -s tests -v`
分模块命令：`python -m unittest discover -s tests -p <模块名>.py -v`

| 模块 | 退出码 | 通过 | 失败 | 跳过 | unittest 计时 | 挂钟耗时 |
|---|---|---|---|---|---|---|
| **全量 discover（含 test_live 模块 skip）** | **0** | **477** | **0** | **2** | 28.463s | 29s |
| tests/test_logic.py | 0 | 337 | 0 | 1 | 3.648s | 4s |
| tests/test_alerts_lanes.py | 0 | 21 | 0 | 0 | 0.801s | 1s |
| tests/test_hardening.py | 0 | 88 | 0 | 0 | 24.101s | 24s |
| tests/test_ipmap.py | 0 | 31 | 0 | 0 | 0.016s | <1s |
| tests/test_live.py（模块级跳过） | 0 | 0 | 0 | 1 | 0.000s | <1s |

数字核对：unittest 全量 `Ran 479 tests` = 478 个普通用例 + 1 个模块级 skip（unittest 把 test_live 的模块级 SkipTest 记 1 个）；pytest 收集 478 个，两套口径一致。

**全量输出尾部原文**：

```
Ran 479 tests in 28.463s

OK (skipped=2)
```

（后台运行记录：`EXIT_CODE=0`，`ELAPSED=29s`。远低于 10 分钟上限，未出现超时。）

**pytest 收集兼容性**：`python -m pytest tests/ --collect-only -q` →

```
478 tests collected in 0.29s
```

分模块收集数：test_logic 338 / test_hardening 88 / test_ipmap 31 / test_alerts_lanes 21；`grep -c test_live` 收集输出 = **0**（模块级 SkipTest 令 pytest 不收集该模块），无收集错误。tests/ 下的 `*.bak-*` 备份文件不被两种收集器匹配。结论：pytest 8.3.4 可完整收集，无兼容性障碍；但基线口径以 unittest discover 为准。

## 四、跳过项明细（共 2 个，均为预期跳过，非失败）

1. **`test_live`（整个模块，unittest.loader.ModuleSkipped）**
   原文：`test_live (unittest.loader.ModuleSkipped.test_live) ... skipped 'no live deployment at \\srv\\mihomo-test; run tests/test_live.py on the host instead'`
   根因分析：设计如此。该套件要在 vps 部署环境（`/srv/mihomo-test/data/` 下有 config.json + state.db）运行，模块级守卫防止开发机误连真实部署。这是期望行为，迁移后仍应保持该守卫。

2. **`test_logic.RoundLockTest.test_second_round_is_refused_while_one_holds_the_lock`**
   原文：`test_second_round_is_refused_while_one_holds_the_lock (test_logic.RoundLockTest.test_second_round_is_refused_while_one_holds_the_lock) ... skipped 'flock is POSIX-only'`
   根因分析：源码 `tests/test_logic.py:977` 为 `@unittest.skipIf(os.name != "posix", "flock is POSIX-only")`——flock 文件锁仅 POSIX 有，Windows 上平台性跳过。注意：迁移后门禁若跑在 Linux 容器内，该用例会**开始执行**，属于"迁移后新增执行"，其结果不影响本基线对照。

## 五、原有失败项清单

**无。0 个失败。** 全量与分模块运行退出码均为 0，无 ERROR、无 FAIL。

## 六、冒烟测试

`python -m mihomo_test --help` → **退出码 0**，argparse 正常输出用法（摘录）：

```
usage: mihomo_test [-h] [--host HOST] [--port PORT] [--source SOURCE]
                   [--mode {direct,chain}] [--trigger TRIGGER] [--no-schedule]
                   ...
                   [{serve,round,push,status,ipmap}]

Entry point: run the dashboard, or execute a single round from the CLI.

positional arguments:
  {serve,round,push,status,ipmap}
```

说明：`python -m mihomo_test` 不带参数未执行——默认动作是启动 dashboard 长驻服务，不适合冒烟；`--help` 已证明入口模块可正常加载与解析。

## 七、tools/ 脚本用途与运行前置条件（只读代码，未执行）

以下 verify/诊断类脚本均**未实际执行**，仅读头部 docstring 与常量记录：

| 脚本 | 用途 | 运行前置条件 |
|---|---|---|
| tools/lanes_verify.py | 证明 N 个入站 listener 各自路由到自己的 select 组（并行车道的前提）：两个车道选不同节点、确认出口不同，再互换确认出口互换 | 硬编码 `BASE=/srv/mihomo-test`、Docker（隔离内核容器 `lanetest-core`）、`/tmp/lanetest`，须在 vps VPS 上跑 |
| tools/v6_fix_verify.py | 验证探针容器具备 IPv6 后，IPv6-only 节点能通过测试 | `/srv/mihomo-test`、Docker IPv6 bridge `healthcheck-v6`（`V6_NETWORK` 可覆盖）、端口 19293、宿主机有 IPv6 |
| tools/chain_verify.py | 在真实 mihomo 上验证链式路径三件事：内核接受带 `dialer-proxy`/`__FRONT<n>__` 的配置文本（而非静默忽略走直连）、链式确实经 front、两相拆分在无可用 front 时的裁决 | vps、`/srv/mihomo-test`、隔离容器，生产不受影响 |
| tools/verify_cdn.py | 从 Cloudflare 外部验证部署好的 CDN 前端：页面可服务、资产 content-type、`_headers` 安全头落地、bootstrap 指名后端；自带防本地 HTTPS_PROXY 误判处理 | 仅需网络可达目标 URL（用法 `python tools/verify_cdn.py https://<pages> --api-base https://<api>`），无需 VPS 凭据 |
| tools/chain_diagnose.py / chain_test2.py / chain_ab.py | 链式诊断：直连 vs 经 front 链式的失败定位、用原始节点字典复测、direct/chained A/B 对照 | vps、`/srv/mihomo-test`（chain_ab 用私有根 `MIHOMO_TEST_ROOT=/tmp/chainab`）、隔离容器端口 19296/19298 |
| tools/v6_ab_test.py / v6_host_verify.py / v6_debug.py / v6_chain_cause.py / v6_front_scan.py | IPv6 系列诊断：dns.ipv6 开关 A/B、host 网络验证 v6 出口、抓内核 debug 日志、定位链式 v6 失败根因、扫描可做 v6 front 的节点 | vps、Docker、隔离容器端口 19295-19301，部分需 `--network host` |
| tools/front_ablation.py / front_debug.py | edgetunnel front 字段消融（ech-opts / x-padding-* 是否是关键）与 front 单独可达性诊断 | `/srv/mihomo-test`、隔离容器端口 19294/19299 |
| tools/verify_measure_switch.py | 线上验收「直连/链式」测量开关：真浏览器渲染检查、checkbox ref 用来源唯一 key、点击后抓 `POST /api/config` 请求体验证写入并复原 | 线上面板 URL + 浏览器自动化环境 |
| 其余 tools/ 脚本 | build_web.py（构建 web 静态资源）、deploy_files.py / deploy_pages.py（部署）、fix_round_times.py（修历史数据）、loop_query / loop_scan / loop_snapshot（循环观测）、pull_file.py / push_run.py（文件传输）、wire_telegram.py（告警接线） | 多数依赖 vps 部署或线上凭据，均为运维工具非测试 |

小结：除 `verify_cdn.py` 只需目标 URL 外，verify/诊断类脚本全部要求在 vps VPS（`/srv/mihomo-test` 存在）+ Docker 环境运行；它们与生产栈隔离（私有目录 + 独立容器 + 独立端口），但依赖 VPS，不进入本机基线。

## 八、未执行项及原因

| 项 | 原因 |
|---|---|
| tests/test_live.py（作为实战测试执行） | 设计上不对本机跑：模块级守卫在本机触发 SkipTest（已确认生效，未连任何真实部署）。真实运行方式为 vps 上 `python3 tests/test_live.py [组名]`（health/api/kernel/lanes/data/substore/round，slow 组需 `--include-slow`）或容器内 `docker exec mihomo-test python3 -m unittest discover -s tests` |
| tests/diag_round_suite.py | 容器内诊断脚本：首行 `sys.path.insert(0, "/srv/mihomo-test")`，`import test_live` 后构建 round 组套件。本机无 `/srv/mihomo-test`，导入 test_live 即触发模块级 SkipTest 退出，无本机运行价值；未执行 |
| `python -m mihomo_test`（无参数） | 默认启动长驻 dashboard 服务；冒烟以 `--help`（退出码 0）为准 |
| tools/ 全部脚本 | 见第七节：依赖 VPS/真实部署/浏览器环境，任务约定只读不执行 |

## 九、基线结论

1. **回归门禁**：`python -m unittest discover -s tests`（Windows 本机）即迁移前回归门禁——全量 479 个用例 28.5 秒跑完，退出码 0、0 失败、2 个预期跳过，可作为迁移后每轮回归的对照基线。四个离线模块均可独立运行（`python -m unittest discover -s tests -p <模块>.py` 或 `python -m unittest tests.test_logic`），便于迁移期间分模块对照。
2. **模块定位**：test_logic（338，测活决策/导出/订阅逻辑）、test_hardening（88，鉴权/token/指纹/并发/静态资源/CORS）、test_alerts_lanes（21，告警/车道/看门狗）、test_ipmap（31，ipmap 离线逻辑）四者密闭（经 `_isolation.py` 隔离数据目录），迁移后应原样保留为门禁；test_live（7 个分组）依赖 vps 真实部署，不进本机门禁，迁移后在部署环境单独跑。
3. **已知平台差异（非 flaky）**：Windows 上仅 2 个预期跳过——test_live 模块守卫、RoundLockTest 的 flock POSIX-only skipIf。无任何观察到的 flaky 用例：全量与分模块两次口径结果一致，hardening 模块虽含大量真实 HTTP server/线程（88 个用例 24 秒，为最慢模块）也稳定通过。
4. **迁移后注意**：(a) 门禁若改跑 Linux 容器，RoundLockTest 会开始执行，属预期新增，勿记为回归；(b) `_isolation.py` 的隔离守卫与 test_live 的部署检测守卫是两道安全栏，移植测试时必须一并带走；(c) pytest 8.3.4 可完整收集 478 个用例，可作辅助跑法，但基线口径以 unittest discover 为准。

## 十、批次 B3：substore_bridge 实现后（2026-09-28）

### 10.1 范围与环境

- 本节由第二轮 tests 子 Agent 追加：为 backend-impl 实现的 N-01~N-03（`mihomo_test/substore_bridge.py` 新增、`mihomo_test/server.py` 增 `GET /api/probe/nodes` 与 `_publish_scoped` 作用域谓词、`substore_bridge/probe_filter.script.js` 新增）做独立契约测试。本轮**只新增** `tests/test_substore_bridge.py` 与本节，未改任何实现文件（`git status --short` 核对：实现文件仍是 backend-impl 的既有改动集，测试新增仅 `?? tests/test_substore_bridge.py`）。
- 环境与基线批次完全一致：Python 3.12.10、win32 10.0.26200 x64 + Git Bash、运行时依赖仍仅 PyYAML 6.0.3；本机另有 Node **v24.18.0**（`node --version`）。所有测试经 `_isolation.isolate()/restore()` 隔离，HTTP 全部打 `127.0.0.1` 回环，JS 用例的 fetch 由 harness 内 `globalThis.fetch` 替换应答，**无任何外网依赖**。

### 10.2 新模块用例数与构成

`tests/test_substore_bridge.py` 共 **52** 个用例（`python -m unittest tests.test_substore_bridge -v` 计数核对）：

| 类 | 用例数 | 覆盖 |
|---|---|---|
| ProbeNodesPayloadTest | 9 | N-03 投影：9 字段精确集合（不多不少）、`last_delay_ms→delay_ms`、None 容忍、consec_fail None→0、fingerprint/last_reason/first_seen/last_seen/last_ok/total_ok/total_fail/ip_alive/ip_total 及 uuid/password/token 类敌意键不泄漏、空行列表/None、真实 `db.list_nodes` 行投影、NULL category→direct、原始行 display 回退 |
| ProbeNodesSnapshotTest | 6 | N-03 envelope：恰四键 ok/generated_at/count/nodes、count 与 nodes 一致、generated_at 为 UTC 无时区后缀（正则 `^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}$`）且来自 `db.now()`（patch 验证）、显式 generated_at 透传、空账本 count:0 |
| ProbeNodesEndpointTest | 16 | N-01 回环起服（沿用 test_hardening `_LiveServer` 模式）：无 token 401、publish token 200（query 与 X-Auth-Token 头两传法）、auth token 200、publish 作用域未被放大（/api/status 与 /api/config 仍 401）、/api/export 老行为未回退、source/status 过滤、未知 source 空 200、五个合法 status 全 200、status=bogus 400（错误信息列出白名单）、安全头+Content-Type、尾随路径 `/api/probe/nodes/extra`（含尾斜杠）publish token 仍 401、POST→404、响应体无 token/指纹/凭据字段、空账本 200 count:0 |
| ProbeFilterScriptTest | 21 | N-02 Node 子进程 harness：1 个 `node --check` 语法门禁 + 20 个行为用例（mock fetch 驱动官方签名 `operator(proxies,"ClashMeta",{source:"test"})`） |

JS 行为用例覆盖：filter 仅删 dead（默认 mode/missing）、missing=drop 删无记录、未知 mode/missing 回退 filter/keep、HTTP 500 / 401 / 非 JSON / `ok:false` envelope / 空账本均按 missing fail-open、annotate 追加 ` [HK]`（小写归一大写）与 ` ·dead`、country 非 null 且非两字母不加缀、annotate 幂等（二次调用不叠加、旧尾缀剥后重加）、both 先删后改且无 `·dead` 残留、`[CC] ` 前缀名经 server 兜底命中（host 归一化：大小写+尾点）、同 host 多记录状态一致→命中、矛盾→按无记录、无 server 节点按无记录、空订阅+缺 probe_url 仍抛错、空订阅+合法配置不 fetch、fetch 抛错 fail-open、token 只在 `X-Auth-Token` 头且不进 URL。

harness 以临时文件方式在 setUp 生成（`probe_filter_harness.mjs`，tempfile.mkdtemp）、addCleanup 删除；**仓库无 .mjs 残留**（`ls tests/ | grep -i mjs` → 空；`ls substore_bridge/` → 仅 `probe_filter.script.js`）。Node 缺失时 `skipTest("node is not installed")`——本机 Node 24 为真跑。

### 10.3 运行结果（真实输出摘录）

新模块单独运行：

```
$ python -m unittest tests.test_substore_bridge -v 2>&1 | tail -3
Ran 52 tests in 10.724s

OK
```

全量回归（对比基线 479/477/2）：

```
$ python -m unittest discover -s tests -v 2>&1 | tail -6
...
Ran 531 tests in 38.327s

OK (skipped=2)
```

- **531 = 基线 479 + 新增 52**；通过 529、失败 0、错误 0、跳过 2。两个跳过与基线完全相同（原文核对）：`test_live (unittest.loader.ModuleSkipped.test_live) ... skipped 'no live deployment at \\srv\\mihomo-test; ...'` 与 `test_second_round_is_refused_while_one_holds_the_lock ... skipped 'flock is POSIX-only'`。既有 479 个用例无一回退。
- **JS 门禁**：`node --check substore_bridge/probe_filter.script.js` → 退出码 0（无语法错误输出）；语法门禁同时作为 unittest 用例（test_the_script_passes_the_node_syntax_check）在套件内执行，OK。

### 10.4 四处契约裁量的独立核对结论

逐条对照 ARCHITECTURE §3/§4 原文与实现现状，四条均**符合架构意图，按现状断言并接受**：

1. **rows 的 category 为 NULL 时归一为 "direct" —— 接受。** 依据：§3.4 将 `category` 类型钉为 string（示例值 `"direct"`），而 `db.py list_nodes` 的注释明示全仓消费者都以 `n.category || "direct"` 读该列（迁移新增列、仅轮次盖章）；NULL 直传会给发布面制造第四种节点类别。测试按现状断言（`test_a_null_category_reads_as_direct`：NULL→"direct" 且显式 "relay" 不被覆盖；`test_the_body_carries_no_credentials` 端到端复核）。
2. **rows 的 name 为 None 时回退 display —— 接受。** 依据：§3.4 的映射语义即 "`display→name`"——payload 的 name 唯一语义就是 display 名。对 `list_nodes` 行（SQL 已 `display AS name`）零差异：name 为 None 时 display 本身为 NULL，回退后仍为 None；该回退只让未经别名的 nodes 原始行也可投影（`test_a_raw_row_falls_back_to_display_for_name`）。
3. **JS 空入站早退置于 probe_url 校验之后 —— 接受。** 依据：§4.4 将 probe_url 缺失钉为**配置级错误 fail-loud**（理由：本仓吃过「静默死亡管线」的亏，FEATURE_INVENTORY 移植事实 2）——若早退在校验之前，空订阅会静默吞掉配置手误，与 fail-loud 矛盾；§4.5「空入站数组直接原样返回」在 probe_url 合法时依然成立（`test_a_missing_probe_url_throws_even_on_an_empty_subscription` + `test_an_empty_subscription_short_circuits_without_fetching` 双向锁定）。
4. **status 过滤白名单来自 PROBE_NODE_STATUSES、非法值 400 而非空结果 —— 接受。** 依据：§3.3 明文「不在白名单内 → **400**（沿用 /api/run mode 白名单的边缘拒绝风格）」，§3.5 的错误形状（中文 400、枚举五值）与实现一致；白名单常量取自 `substore_bridge.PROBE_NODE_STATUSES`（= policy.py 五常量），端点与测试共用单一事实来源，无漂移（`test_all_five_statuses_are_accepted` + `test_an_unknown_status_is_refused_400`，后者还断言错误信息列出全部五个合法值）。

### 10.5 缺陷清单

**无。** 52 个用例首跑即全绿，未发现实现与 ARCHITECTURE §3/§4 的任何偏差；全量回归 531 用例 OK (skipped=2)，退出码 0。

---

## 11. 2026-11-04 Gist-only 重构验收（B6）

> 验收对象：重构批次 B1（一源一产物/单飞/稳定端点/双门禁）、B2 核心+接线
>（probe-publish Gist 发布器 + engine 远程腿）、B4（/gui + CSRF）、B5 核心
>（Sub-Store 迁移 scan/apply/rollback），基线提交 `13c2889`。逐条门禁映射、
> 证据（file::test）与缺口说明见 **`workstreams/19_ACCEPTANCE_MAP.md`**（本节是其
> 数字摘要）；目标架构见 `ARCHITECTURE_GIST_ONLY.md`，运维手册见
> `MIGRATION_GIST_ONLY.md`。

### 11.1 门禁运行结果（2026-11-04 实跑）

| 门禁 | 结果 |
|---|---|
| `cargo fmt --all -- --check` | exit 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 |
| `cargo test --locked --workspace` | **685 passed / 0 failed / 2 ignored**（28 个测试目标；基线 29cf9cd 为 664/0，增量 = B6 新增 11 例 + 并行 B5 接线批次用例） |
| `python -m unittest discover -s tests` | **605 tests OK (skipped=2)**（基线 598，+7 = B6 新增 3 + 并行批次 4；两个 skip 与基线相同：无 live 部署、flock 仅 POSIX） |
| `python -I tools/scan_secrets.py` | secret scan: clean (tracked files) |
| `python -m compileall -q mihomo_test tests` | exit 0 |
| `node --check substore_bridge/probe_filter.script.js` | exit 0 |

### 11.2 B6 新增测试（全部新文件，未改任何既有测试/源码）

| 文件 | 数量 | 钉住的行为 |
|---|---|---|
| `crates/probe-engine/tests/publish_scenarios.rs` | 3 | §11.2 场景 2/4/6 的组合级版本：同输入 10 轮重跑=1 次目的地写入+9×no_change；护栏两种形态拦截且快照/文件/digest 原地；目的地不可用=publish_failed+error_code、本地快照前进、digest 停在远端已收内容、下一轮按 digest 重试 |
| `crates/probe-storage/tests/artifact_invariants.rs` | 2 | 发布与 run claim/close 交织 10 轮仍 1 artifact、每轮 1 行；崩溃槽先受保护（round 未关不收尸）后收尸，artifact 不分裂 |
| `crates/probe-api/tests/subscription_endpoint.rs` | 6（1 ignored） | 稳定订阅端点 HTTP 面：凭据边界、逐字节服务 active 快照、ETag/304、未知/恶意 id 404、无快照 404 不出空体；缓存头一腿发现缺陷 B6-1（见 11.4） |
| `tests/test_substore_write_zero.py` | 3 | F-001 整轮零写：`_run_round` 全程记录 store 所有方法调用，默认配置与「push 标志开/legacy 关」两种形态都零写、本地导出仍落地；变异探针验证（双门禁全开时记录器立即抓到 `update_only`） |

新增用例共 **14 例**（Rust 11 + Python 3）。设计原则：不重复 B1/B2 既有覆盖
（100 次发布一 artifact、改名不分裂、digest 只进不退、401/429/5xx 语义等均已在
树内被指名测试钉住，见 19_ACCEPTANCE_MAP 各表「测试/证据」列）；B6 只补组合、
交织与 HTTP 面。

### 11.3 NOT_RUN 登记（如实标记，禁止伪造）

| 项 | 原因 |
|---|---|
| `docker compose config` | 本机无 docker 命令（exit 127） |
| 无人值守持续运行 ≥ 8 小时 / 24h Sub-Store 对象数观测 | 无 soak 环境；证据为结构禁令+全轮零写测试，非时间窗观测 |
| 真实 GitHub / 真实 Sub-Store 的全部 E2E（§11.2 场景 1/3 全链路、场景 5 真实双进程） | 生产资源，属 `MIGRATION_GIST_ONLY.md` 的「需要用户在场」步骤；代码级以 stub/假发布器覆盖 |
| §11.3-9 的 API 层迁移审计 | 迁移 API 端点在基线 `13c2889` 为 404 降级（gui_tests 钉住），接线批次进行中 |
| SQLite locked / 磁盘满 / 只读目录故障注入 | 无故障注入框架 |

### 11.4 B6 发现的缺陷

| 编号 | 内容 | 状态 |
|---|---|---|
| B6-1 | `GET /artifacts/{source_id}/subscription` 的 `Cache-Control: public, max-age=300, stale-if-error=86400` 被 `security_headers` 中间件覆盖为 `no-store`（`build_router` 层序：订阅缓存中间件在全局安全头内侧，外层后写）。功能/正确性不受影响（ETag/304/正文均正确），损失的是客户端缓存。`subscription_endpoint.rs::the_cache_policy_survives_the_security_headers` 以 `#[ignore]` 钉住期望行为，修复后取消 ignore | **open**（修复属 probe-api 所有者；B6 不改 src） |
| B6-2 | §7.2 字面语义「相同节点不同显示名只测一次」未实现：同指纹不同显示名各拨一次，仅在账本折叠为一行、发布折叠为一份。既有行为决策，非回归 | accepted（记录偏差） |

## 12. 2026-11-06 HK3 轻量化验收（P4–P7）

> 批次：P4 `a58aa74`、P5 `f9ec735`、P6 `1210999`、P7 本批。每批增量明细见
> `HK3_LIGHTWEIGHT_CHANGELOG.md` 对应条目；本节只录 P7 终跑数字与 NOT_RUN。

### 12.1 P7 终跑门禁（2026-11-06 实跑）

| 门禁 | 结果 |
|---|---|
| `python -m unittest discover -s tests` | **832 OK / 0 failed / 2 skipped**（605 基线 → +35 P1 → +21 P2 → +109 P3 → +6 P4 → +17 P5 → 净增至 832；2 skipped 为既有预期跳过） |
| `cargo test --locked --workspace` | 28 个测试目标全 ok / 0 failed（P4 起 Rust 树零改动，每波例行复跑） |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 通过 |
| `cargo fmt --all -- --check` | 通过 |
| `node --check mihomo_test/web/app.js` | 通过（P6 前端语法门禁） |
| `python tools/scan_secrets.py --include-untracked` | clean |
| `yaml.safe_load(docker-compose.yml)` | 通过（容器级 NOT_RUN，无 docker） |

### 12.2 HK3 批次测试增量（真实用例数）

| 批次 | 新增测试 | 钉住的行为 |
|---|---|---|
| P1 | test_death_logic_contract.py 35 | §7 八条死亡收敛语义（身份=指纹/单次成功恢复/阈值判死/单轮 pending/基础设施错误不计 streak/整轮护栏/无来源不批量/迁移不清账） |
| P2 | test_config_v3.py 22（含 Wave1 白名单 1） | config v3 校验（manual_push_only 强制、probes[] 键白名单、token 只许环境变量名）、2→3 幂等迁移、双 .bak-v3 备份 |
| P3 | test_aggregation.py 57 + test_probe_dispatch.py 53（+2 digest 对齐） | 聚合三规则、观测缺失不计完成数、Job 签发/验签/防重放、agent 生命周期、/api/v1 鉴权矩阵与稳定错误码、front_dead 不推 streak |
| P4 | test_canonical_export.py 21 + test_gist_store.py 20（+engine 接线 3） | canonical 去重/重命名/排序稳定性、Gist 四文件/幂等门禁钉 canonical sha/退避重试/token 脱敏、发布失败不影响账本 |
| P5 | test_manual_publish.py 17 + 端点 4（重写 PushEndpointTest 2） | token 生命周期五态（消费/重放/过期/绑定不符/重签作废）、diff 预览（改名不成对、样例无凭据）、update-only 拒绝新建、自动回灌不可达 |
| P6 | node --check + 渲染壳/dashboard 断言既有 | 五视图路由、发布中心流程（前端）；后端 dashboard 投影纯新增键 |

### 12.3 NOT_RUN 登记（HK3 增量）

| 项 | 原因 |
|---|---|
| compose 容器级集成（Controller + 2 agent + 内核，指导 §14 集成清单） | 开发机无 docker（exit 127）；agent 代码路径由 test_probe_dispatch 覆盖 |
| Playwright UI 验收（§14 UI 清单） | 无 UI 测试环境；覆盖 = node --check + 渲染壳测试 + 端点级测试；人工核对清单在 workstreams/22_ROLLOUT_ROLLBACK.md G1 |
| agent ↔ Controller 真机联调 | 需第二台可达探测点主机 |
| 真实 GitHub / 真实 Sub-Store E2E | stub/transport 注入覆盖 HTTP 语义；真机步骤属 MIGRATION_GIST_ONLY.md |

结论：HK3 P0–P7 代码与文档交付完成，§18 十二条验收 11 PASS + 1 PARTIAL
（缺集成/UI 真机环节），可按 workstreams/22 灰度上生产；数字逐批可追溯。
