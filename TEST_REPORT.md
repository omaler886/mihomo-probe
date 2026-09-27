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
