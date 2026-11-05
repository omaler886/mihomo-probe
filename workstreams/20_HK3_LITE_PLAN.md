# HK3 轻量化改造 · 实施计划（workstreams/20）

> 依据：《HK3 轻量化改造_多探测点_Gist_Web_SubStore人工推送_实施指导》（D:\HK3_轻量化改造_多探测点_Gist_Web_SubStore人工推送_实施指导.md，641 行）
> 基线：commit 2c22db6（B8）+ 工作区未提交的 B6 测试/B7 文档；cargo 709 / python 605 全绿（2026-11-05 实跑复录于 baseline-*.log）。
> 修改历史记录在 `HK3_LIGHTWEIGHT_CHANGELOG.md`（P0-1 已建）。

## 1. 关键裁决（文档与代码冲突，按指导 §17.8 记录）

1. **实现主线 = Python 侧**。指导 §11 的改造清单（probe_dispatch/aggregation/gist_store/canonical_export/manual_publish + config/engine/db/server/web）全部指向 `mihomo_test/`；Python 侧是成熟测活引擎与死亡逻辑所在地（605 测试锁死），Gist/GUI/调度在 Rust 侧已存在但文件模型不同（per-source `<key>.yaml`+manifest.json）。HK3 改造落在 Python 侧，Rust 侧保持绿且不改（其 chain 默认关、front_dead 不计 streak、自动推送双门禁默认关已满足指导硬边界；Gist 布局差异记为 Rust 线后续项）。
2. **front_dead 语义改造**：Python `_record_chain_failures`(engine.py:2084) 当前把前置全死记入目标节点 consec_fail（3 连败判死），与指导 §5.4「前置失败不得推进目标节点死亡计数」相反——按指导改造，旧用例同步更新并在 CHANGELOG 记录。Rust 侧本就符合（round.rs:1198-1214）。
3. **本地 per-source 导出保留为诊断产物**，新增唯一 canonical 订阅产物 `data/exports/mihomo-alive.yaml`：Gist 发布、Sub-Store 人工推送、订阅 API 只读 canonical（指导 §8 去重保证）；per-key 文件不再作为订阅入口。这样既满足 §18「不产生按探测点/轮次扩散的订阅」，又不破坏 605 个既有回归（指导 §14：单探测点模式结论不变）。
4. **B6/B7 未提交产物不抢先提交**，HK3 各 Phase 在其之上继续；HK3 提交前缀 `hk3-lite:`（指导 §17.3）。
5. **多探测点 = 同一代码库两种角色**：Controller（现有 server/engine）+ Probe Agent 新 CLI 模式（`python -m mihomo_test agent`），复用 core.py 内核客户端；不引入新依赖（纯标准库 + 已有 PyYAML）。

## 2. 任务拆分与 Phase 映射

| Phase | 内容 | 产物 | 依赖 |
|---|---|---|---|
| P0 | 基线冻结 | HK3_LIGHTWEIGHT_CHANGELOG.md、baseline 日志 | 已完成 |
| P1 | 死亡逻辑 characterization tests（指导 §7 八条语义锁死） | tests/test_death_logic_contract.py（仅新文件） | 无 |
| P2 | config schema v3 + DB 三表 + 2→3 迁移 | mihomo_test/config.py、db.py、tests/test_config_v3.py | 无 |
| P3 | 观察+聚合层：aggregation.py、probe_dispatch.py、engine 接线（收集观察→聚合→原状态机）、front_dead 修复、probe API（heartbeat/result）、agent CLI 模式 | 新模块 + engine.py + server.py + tests/test_aggregation.py | P2 配置键（契约先行指定） |
| P4 | canonical export + Gist 固定四文件 | canonical_export.py、gist_store.py、engine 发布区、POST /api/v1/gist/publish、tests | P2 |
| P5 | 人工 Sub-Store 推送 + 删除自动 push 链 | manual_publish.py、/api/v1/substore/{preview,confirm-token,push}、publish_audit、tests | P4 |
| P6 | Web 控制台重构（总览/节点/探测点/发布中心）+ dashboard/nodes/probes API + docker-compose 角色 + .env.example | web/、ui.py、server.py、compose、env | P3/P5 API |
| P7 | 清理交付：文档、全量门禁、灰度/回滚文档、最终报告 | README/ARCHITECTURE 章节、HK3_LIGHTWEIGHT_CHANGELOG 收尾 | 全部 |

## 3. Agent 分工（每波：实现 → 测试 → 审查，三方独立）

- **实现 Agent**：按 brief 拥有明确文件所有权，写代码+自测，报告变更清单。
- **测试 Agent**：独立跑 `python -m unittest discover -s tests`（+相关新建测试 -v），记录通过/失败/跳过数字与失败用例，不修代码。
- **审查 Agent**：独立读 git diff，对照指导对应章节逐条核验（含 §13 安全要求：token 脱敏、无凭据入日志/前端、confirm token 防重放），输出通过/问题清单。

主 Agent 负责：波次调度、冲突消解、CHANGELOG 记账、门禁数字汇总、最终一致性检查。

## 4. 技术方案要点

### 4.1 config v3（config.py，唯一事实来源 data/config.json）

新增键（迁移后）：`schema_version:3`、`probe_policy`（interval/timeout/concurrency/dead_after_consecutive_failures/round_guard_*，由现有 test/policy 值派生，engine 仍读 policy 键，v4 再收口）、`probes:[]`（空=本地单探测点兼容模式）、`aggregation{strategy:any_alive,minimum_completed_probes:1,late_result_seconds:90}`、`rename{enabled,template:"{country} | {display_name}",collision_suffix:" #{n}",preserve_original_name:true}`、`gist{enabled,gist_id,token_env:"GITHUB_GIST_TOKEN",visibility:secret,canonical_file:"mihomo-alive.yaml",status_file:"probe-status.json",history_file:"probe-history.jsonl"}`、`substore{...,target_name:"mihomo-probe-alive",manual_push_only:true}`。
校验（启动/保存/测试前同一套）：`substore.manual_push_only` 必须 true 否则拒绝启动；`probes[].id` 唯一且匹配 `^[a-z0-9][a-z0-9_-]{1,31}$`；`chain.enabled` 缺失强制 false；token 只允许环境变量名，不明文返回前端/入日志；`dead_after_consecutive_failures` 沿用现值不清 streak。2→3 一次性迁移：先备份 config.json（`config.json.bak-v3-<UTC>`）再写。

### 4.2 DB（db.py，追加式）

按指导 §12 SQL 建 `probes`/`probe_observations`/`publish_audit` 三表；CREATE TABLE IF NOT EXISTS 幂等；首次建表前备份 sqlite 文件；不清空/不重建 nodes。Python 现有代码内迁移机制（db.py `_migrate`）延用。

### 4.3 多探测点数据流

Controller 每轮：收集+规范化上游一次 → 为每个 enabled probe 生成轻量 Job（round_id/probe_id/config_digest/deadline/chain_enabled/nodes[fingerprint,mihomo_name,proxy]）→ Probe Agent 执行（复用 core.py）→ 回传 ProbeResult → Controller 校验（round 存在、probe 已知、digest 一致、未过 deadline；旧轮/未知点/digest 不符只入审计事件不推进死亡计数）→ `probe_observations` 幂等落库（PK round+probe+fingerprint，重复上报覆盖）→ `aggregation.aggregate_round`（any_alive；探测点离线/Controller 错误不计节点失败；不足 minimum_completed_probes → inconclusive 不推 streak）→ 一次性调 `policy.apply`（原状态机不动）。本地兼容模式（probes=[]）= 单一隐含探测点 "local"，结论与改造前一致。

### 4.4 canonical export + 重命名

`canonical_export.py`：聚合后、状态机收敛后生成唯一 `data/exports/mihomo-alive.yaml`；重命名模板变量 {display_name}/{country}/{probe}/{delay}/{protocol}/{index}，冲突追加 ` #2/#3`，同 fingerprint 多源只留 canonical 并记 source_aliases，排序固定 country→protocol→normalized name→fingerprint；身份始终用连接参数指纹。

### 4.5 Gist 固定四文件（gist_store.py，纯标准库 urllib）

`mihomo-alive.yaml`/`probe-status.json`/`probe-history.jsonl`（最近 100 轮滚动截断）/`README.md`；读最新 revision→ETag/重读后写；内容 digest 不变不提交；失败不影响账本结算（run 记 publish_failed，UI 显示"本轮完成，发布失败"）；token 仅从 `token_env` 指定环境变量读，日志全脱敏；默认 secret Gist 且在 UI 警告凭据可经 URL 暴露。

### 4.6 人工 Sub-Store 推送（manual_publish.py）

preview（新增/删除/改名计数+前 20 样例）→ `POST /api/v1/substore/confirm-token` 签发 60s 一次性 token（绑定 target+revision+digest）→ `POST /api/v1/substore/push` 校验消费（过期/ revision 变/digest 变/重复用 → 拒绝，错误码 PUSH_CONFIRM_REQUIRED 等）→ update-only 推送 → `publish_audit` 落审计（audit_id/kind/target/revision/digest/actor/status/summary）。同步删除：`_publish_sources` 自动 push_exports 调用点（engine.py:2195-2196）、scheduler 轮次末尾的 Sub-Store 腿、`/api/push` 一键推送改为必须 confirm token。Gist publish 与 scheduler 不得间接调用 push。

### 4.7 Web（web/ 原生无构建）

四页：总览（状态卡+探测点表+近 20 轮）、节点（搜索/筛选/分页/诊断 JSON 复制，默认不显示凭据）、探测点详情（心跳/版本/任务/错误分布/禁用）、发布中心（Gist 区+Sub-Store 区：预览差异、"确认推送到 Sub-Store"主按钮+二次确认弹窗展示 target/节点数/revision/digest）。≥360px、深浅色、状态有色标+文字、sticky 表头、POST 防重复提交。

## 5. 测试门禁（每波执行，数字记入 CHANGELOG）

- `python -m unittest discover -s tests`（基线 605 OK/skip2，只增不减）；
- 新建测试文件（指导 §14 清单）：test_death_logic_contract / test_config_v3 / test_aggregation / test_gist_store / test_manual_publish（+test_canonical_export）；
- Rust 侧不碰：波次间跑 `cargo test --workspace` 确认不回退（709）；
- `python tools/scan_secrets.py`（若存在）、py_compile 全量；
- 无 docker：compose 集成测试按 19_ACCEPTANCE_MAP 先例记 NOT_RUN 并说明。

## 6. 验收标准（指导 §18 逐条）

chain 默认关且开启行为正确 / 多探测点共用一份配置一个账本一个 canonical export / 节点不因探测点增多加速死亡 / 重命名稳定不改连接参数 / 死亡收敛与整轮护栏回归全过 / Gist 只固定四文件 / Web 可查状态明细原因 / Sub-Store 只能人工预览+二次确认后推送 / 自动推送路径已删或测试证明不可达 / 敏感信息不进日志与前端 / 有迁移+灰度+回滚文档 / 全量测试有真实执行记录。

## 7. 风险与回滚

- engine.py/server.py 为共享热区：波次串行修改，每波独立可 revert（git 提交粒度 `hk3-lite: <phase>`）。
- 行为变更仅两处且为指导明文要求：front_dead 不计 streak、per-source 导出降级为诊断产物。
- 回滚：`git revert` 对应提交；DB 三表为追加式旧版可忽略；config 迁移有 .bak-v3 备份可手工恢复。
