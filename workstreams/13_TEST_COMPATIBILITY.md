# 测试与兼容

## 目标
共享 fixture、双跑比对、fuzz（总控 §16）。

## 基线（2026-09-30，HEAD=03cb6ea）
- 命令：`python -m py_compile mihomo_test/*.py`（干净）；`python -m unittest discover -s tests -v`。
- 结果：**Ran 554 tests, OK (skipped=2)，38.797s**。
- 跳过项：test_live 中需要 MIHOMO_TEST_LIVE=1 的真内核用例（网络隔离设计，符合"Cloudflare 不作为测试成功必要条件"）。
- 环境注记：本机 Windows 无 Docker——`mihomo -t`/容器路径用例靠 mock；真内核门禁在 vps 执行。

## 测试分类盘点
- test_logic.py（2910 行）：策略/重试/HTTPS 判定/链式真实拉流/失败分类/reload 兜底/prepare/别名折叠/导出/YAML 引号/链式 dialer 组/轮次模式与清理锁/配置 patch/schema/禁用源/孪生键/DNS 缓存/迁移/分类统计/裁剪/源 key/联动/仪表脚本/BuildConfig/自引用/入口分类/出口未验证/轮次预算/链式/前置池/内核拒绝/链失败分类/应用与发布/时间戳/中止轮/剥 ECH/孤儿回收/推送。
- test_hardening.py：token 保障、validate_patch、鉴权矩阵（三通道/publish 域/空 token）、HTTP 面（401/安全头/泄密）、轮锁、watchdog、excluded streak、CDN 构建。
- test_live.py：真内核集成（门控）/账本完整性/车道独立性/Sub-Store 联动/完整轮。
- test_substore_bridge.py：九字段契约/信封/端点/脚本。
- test_alerts_lanes.py、test_ipmap.py：告警/车道配置/ipmap 纯函数。

## 双跑比对设计（R10）
- fixtures/ 语言无关（configs/subscriptions/normalized-nodes/mihomo-responses/dns-packets/exit-responses/rounds/databases/expected）。
- 比对维度：节点总数/fingerprint/配置拒绝集/延迟成功集/出口验证集/失败分类/状态转换/guard 决策/导出集合/API payload；延迟数值允许波动，集合与语义必须一致。
- 已知行为差异候选（须在 R10 前裁决）：config_test 无 docker 时 Python 跳过 vs Rust 显式失败（见 04）。

## Golden 夹具清单（R2 设计夹具，R10 用于双跑）
来源：外部 Go 方案的 Loop 1 清单，按本仓库语义重写。每条须有一个语言无关的输入夹具
与一份"Python 当前版"的期望输出，**Rust 与 Python 用同一份夹具跑**。

| # | 场景 | 期望锚点 |
|---|---|---|
| G-01 | 重名节点（不同 server、同 name） | fp 不同 → 两条独立节点；导出名去重 |
| G-02 | 同一节点不同展示名 | fp 相同 → 折叠为一条；`_orig_fp` 唯一定点 |
| G-03 | 同域名解析出多个 IP | 每 IP 一个变体；变体集合与顺序稳定 |
| G-04 | 两个 DNS 视角结果不一致 | 视图内去重保序；只在"至少一个视角有答案"时缓存 |
| G-05 | 内核返回 503 / 504 | 503 → `kernel_error`、504 → `timeout`（`core.py:_reason_from`，118-121 行）；其余状态码回落 `http_<status>` |
| G-06 | 测试目标本身故障（trace 端点不可达） | 本轮**不更新死亡状态**；不发布 |
| G-07 | 全轮大面积失败（失败率超阈值） | `round_is_suspect` → 不发布，保留上一轮有效结果 |
| G-08 | 空订阅 | 不误删既有节点；零节点不联动 Sub-Store |
| G-09 | 非法节点字段（内核拒绝） | `make_testable` 裁剪定位 culprit；裁剪后仍可测 |
| G-10 | Sub-Store 不可达 | 本轮标失败但不清空账本；重试/退避语义稳定 |
| G-11 | SQLite 重启恢复（WAL 未 checkpoint 时杀进程） | 恢复后计数与轮次完整；`integrity_check = ok` |
| G-12 | 节点从 dead 恢复 | 记 `restore` 转换；`consec_fail` 清零 |
| G-13 | 数据源临时返回空集合 | `demote_disabled_sources` 降 unknown，**不判死** |
| G-14 | 入口在受限 ISP（本仓库补充，外部清单未含） | `excluded` 态；显式清零 streak，绕过 policy.apply |

夹具落地位置（**目录尚未创建**，R2 建）：`fixtures/`（configs / subscriptions / normalized-nodes /
mihomo-responses / dns-packets / exit-responses / rounds / databases / expected）。

## Shadow 差异报告（R10）
Go 方案的差异报告结构适用，字段按本仓库命名。Go 影子**只记录，不发布、不回灌、不告警**。

```json
{
  "round_id": "2026-10-01T10:00:00Z",
  "python": { "alive": 120, "dead": 15, "unknown": 3 },
  "rust":   { "alive": 120, "dead": 14, "unknown": 4 },
  "diff": {
    "state_mismatch": 1,
    "missing_in_rust": 0,
    "missing_in_python": 0
  }
}
```

任何差异必须能归因到下列**七类之一**，归不到即视为未解释差异，门禁不通过：
1. 输入差异（两版拿到的订阅/节点集合不同）
2. fingerprint 差异
3. DNS 结果差异
4. 内核测试结果差异
5. 超时/预算差异
6. 收敛算法差异
7. 数据库状态差异（起始状态不一致）

允许忽略：YAML/JSON 字段顺序、时间戳、无意义格式差异。
**不允许忽略**：节点丢失、节点误杀、节点身份（fp）变化、收敛计数变化、失败分类不同。

## 机器可读测试汇总（R10 起）
测试结果不得只维护在 Markdown 里，也不得靠文本正则判断通过。每次运行输出：

```json
{ "total": 479, "passed": 477, "failed": 0, "skipped": 2 }
```

CI 需归档：JUnit XML、上述 JSON 汇总、覆盖率文件、Benchmark 基线、`-race` 结果、
Golden Test 差异。Rust 侧落地方式（`cargo test --format json` / nextest JUnit 输出）在 R10 定。

## 已知口径差异（双跑对账时按此折算）
- R6（06）：轮行 `total` Python 按 (source, fp) 去重 vs Rust 按变体数；
  `results` 行两侧都按变体写。
- R7（03）：收敛折叠暂不更新 `nodes.proto` / `server` / `country` /
  `ip_alive` / `ip_total` 展示列（等 R4 per-address 变体与 ipmap 切片落地），
  死活状态机列（status/consec_fail/total_*/last_*）已对齐；折叠口径 =
  `_score_bucket` 的 `domain_pass="any"`（任一变体活即活、活者最小延迟、
  category 跟随通过变体），`domain_pass="all"` 到 R4 再接。
- R7（ADR-0005）：`rounds.inconclusive` 列为 Rust 侧新增；Python 读方将其
  当作「ok=0 的完成轮」——suspect 护栏因此保守触发，方向安全，无需改 Python。

## 待办清单
- [x] R0：基线记录
- [x] R0：P0 修复回归测试（14 例）
- [x] R1：Rust 侧切片单测（config/storage/mihomo/api，见 TEST_REPORT_RUST.md）
- [x] R7：收敛口径差异登记（本节，R6/R7 批次）
- [ ] R2：fixtures 目录与首批共享夹具（G-01 ~ G-14）
- [ ] R10：双跑差异报告 + 机器可读汇总

## 下一步
- R2 fixtures。
