# 22 — 灰度与回滚手册（HK3 轻量化，指导 §16）

> 适用范围：把 HK3 改造（P0–P7，HEAD `1210999` 及之后）落到生产 VPS。
> 原则：每一步都有"如何确认它没坏"与"如何退回去"；回滚动作永远先停调度。

## 1. 灰度步骤（指导 §16 逐条落地）

| 步 | 动作 | 确认信号 | 回退动作 |
|---|---|---|---|
| G1 | 部署新版本，`probes=[]`（单探测点兼容模式）、`gist.enabled=false`、不碰 Sub-Store，跑 **3 轮** | `tests/test_aggregation` 灰度用例同款路径：节点结论与改造前逐字段一致；`data/exports/mihomo-alive.yaml` 生成、per-key 文件仍在（诊断产物） | `git revert` 对应批次；旧镜像 tag 仍在（见 §2） |
| G2 | 对比旧版/新版节点结论与 canonical YAML digest | 同一固定输入下 alive/dead 结论一致（P1 契约 35 用例锁死状态机）；digest 用 `mihomo-alive.meta.json` 比对 | 同 G1 |
| G3 | 开启第二探测点（config `probes[]` + agent 部署，compose `--profile probe-agent`），再跑 **≥6 轮，仅观察** | `/api/v1/rounds` 各轮 `insufficient_probes`/inconclusive 语义正确；探测点离线不推进节点死亡（指导 §4.3，test_aggregation 钉住）；agent 心跳出现在 `/api/v1/probes` | 把该 probe 的 `enabled` 置 false（控制台探测点页或直接 config），不需回滚代码 |
| G4 | 开启 Gist（`gist.enabled=true` + `gist.gist_id` + token_env），观察多轮 | Gist 固定四文件、**无重复 revision**（内容未变零 PATCH）、无按探测点/轮次扩散的订阅 | `gist.enabled=false` 即停；远端 Gist 内容保留为最后稳定版，无本地状态需清理 |
| G5 | 开放人工 Sub-Store 推送（发布中心） | 预览 → 60s token → push 全链路；publish_audit 有 preview/confirm_issued/pushed 行；无 token 直接 push 得 409 | 无需回滚：不点按钮即不发生；自动回灌路径已删（tests/test_logic PushEndpointTest + test_hardening 钉住不可达） |

**灰度期间不需要停机**：每步只动配置或只加观察，代码一路保持 `hk3-lite:` 主线。

## 2. 回滚手册（指导 §16）

### 2.1 回滚前置（每次都要）

1. **停 scheduler**（防止新旧两版同时推进 streak）：`docker compose stop mihomo-test` 或把 `schedule.enabled=false` 保存。
2. 确认备份在位：
   - 迁移前 SQLite：`data/state.db.bak-v3-<UTC>`（P2 首次建三表前自动快照）
   - 迁移前 config：`data/config.json.bak-v3-<UTC>`（2→3 迁移前自动备份）
   - 旧镜像 tag：`mihomo-test:local` 被覆盖前的 tag（建议部署前 `docker tag mihomo-test:local mihomo-test:pre-hk3`）

### 2.2 按批回滚

| 要回滚的内容 | 动作 | 数据后果 |
|---|---|---|
| P6（Web 四页） | `git revert 1210999` → 重建镜像 | 无数据交互，纯前端/dashboard 投影 |
| P5（人工推送 + 自动回灌删除） | `git revert f9ec735` | 回滚后轮末自动回灌恢复，但受 `push_to_substore`+`allow_legacy_substore_write` 双开关（默认关）——**无静默写风险**；publish_audit 表保留（追加式，旧版忽略） |
| P4（canonical + Gist） | `git revert a58aa74` → 删除 `data/exports/mihomo-alive.*`（可选） | Gist 远端内容停在最后稳定版；本地 meta 无消费方依赖 |
| P3（聚合/agent/API） | `git revert f8fadae` | `probes`/`probe_observations`/`publish_audit` 三表为**追加式**，旧版代码忽略新表；`data/probe_deadlines.json` 可删 |
| P2（config v3 + 三表） | `git revert 5450252` + 手工恢复两个 `.bak-v3-*` 备份 | config 与 sqlite 都有迁移前快照；迁移函数幂等，重升级安全 |
| 全部（回到 B8） | checkout `2c22db6` 重建镜像 | 见 2.1 前置；先禁用自动发布，核对稳定导出后再恢复调度（§16） |

### 2.3 回滚后

1. 跑一轮，人工核对 `data/exports/` 与节点表结论。
2. 确认 `/api/status` 与日志无新表相关报错（旧版应静默忽略追加表；若不忽略，说明该旧版需要 down migration —— 本工程各批均保证旧版可忽略，见各行"数据后果"列）。
3. 恢复 scheduler。

## 3. NOT_RUN 与证据边界（诚实登记）

| 项 | 状态 | 说明 |
|---|---|---|
| `docker compose config` / 真实起容器 | NOT_RUN | 本开发机无 docker（exit 127）；YAML 经 `yaml.safe_load` 语法校验 |
| agent ↔ Controller 真机联调 | NOT_RUN | 需第二台可达主机；代码路径由 test_probe_dispatch（53 用例，含 HMAC/过期/digest/离线降级）覆盖 |
| Playwright UI 验收 | NOT_RUN | 指导 §14 UI 验收项；现有覆盖 = node --check + 渲染壳测试 + 端点级测试；列入 G1 灰度时人工核对清单 |
| 24h soak | NOT_RUN | 同 19_ACCEPTANCE_MAP §11.3-3 |
