# 安全加固

## 目标
威胁建模 + P0 清零 + Docker Socket 退役（总控 §2.3/§15）。

## P0 批次（R0，全部落地）
1. **/api/status 泄密（S-12 升级为 P0）**：redacted_config 扩展掩 `alert.telegram.token`、`alert.webhook.url`、`substore.backend`（保 scheme+host、路径掩码）；`validate_patch` 丢弃哨兵 `"***"`（auth.token/publish.token/alert.telegram.token/alert.webhook.url/substore.backend），表单回读掩码值再保存不会覆盖真实凭据。测试：test_hardening 新增 4 例。
2. **core.mixed_port 缺省**：DEFAULTS["core"]["mixed_port"]=env MIHOMO_TEST_MIXED_PORT 或 19194（README/MIGRATION_GUIDE 记录的现网值）；NUMERIC_BOUNDS 夹取 (1024,65535)。新装不再 KeyError（原陷阱：core.py:242 直接下标）。
3. **query token 兼容模式**：三通道鉴权保持；admin 域经 query token 命中时每进程一次 warn 日志引导迁移到 X-Auth-Token/Bearer；401 hint 改推头部；publish 域（/api/export/*、/api/probe/nodes）的 query token 是面向 Sub-Store 的既定集成面，不告警。
4. **install.sh 测试门禁**：原 `docker exec … unittest … | tail || true`（管道吞退出码 + 硬吞失败）改为 `docker compose run --rm --no-deps` 一次性容器跑套件，POSIX 安全取 rc，失败 exit 1 且不动现网栈。
5. **secret scanning**：`tools/scan_secrets.py`（工作区 tracked 文件 + `--history` 全历史；模式：私钥头/AKIA/ghp_/xox/sk-/JWT/telegram bot token/云密钥/URL 内 UUID 秘密路径/password 赋值；allowlist 注释与测试夹具）；README 说明。
6. **SECURITY_CREDENTIAL_ROTATION.md**：轮换台账（只记位置与动作，不记值）。
7. **Docker Socket 方案**：见下 ADR-0002。

## ADR-0002 Docker Socket 退役（三步走）
- 现状：应用容器挂 /var/run/docker.sock，用途仅三处——`mihomo -t` 配置校验（docker run --rm 一次性）、内核容器 restart/start/logs、ipmap 一次性内核。面板令牌=主机 root 等价（S-11）。
- 第一步（R3，Python/Rust 通用）：`mihomo -t` 改走常驻内核的 Controller reload 失败兜底已有；校验器改为可选 supervisor 调用，无 docker 时**显式记录降级**而非静默跳过。
- 第二步（R12）：probe-supervisor 最小权限容器：白名单固定参数（validate/reload/restart/status + 固定容器名 + 固定路径），无 shell 拼接；应用→supervisor HTTP/UDS。
- 第三步（R12）：compose 默认不挂 socket；supervisor 单独挂。回滚=恢复 compose 挂载行。

## 威胁建模清单（总控 §15 对照现状）
| 威胁 | 现状 | 处置 |
|---|---|---|
| Docker socket/root | 挂载（S-11） | ADR-0002 |
| SSRF | ipmap fetch_subscription 无内网拦截（S-14，仅 CLI）；store backend 为管理面配置 | R5 补允许策略+私网/元数据拦截 |
| 不可信订阅/YAML | safe_load；大小限制仅 front_text 256KB | R5 响应大小/节点数/字段长度上限 |
| 恶意 DNS | 边界检查有，无 ID/RCODE 校验、指针环靠越界终止 | R4 fixture+fuzz |
| 路径穿越 | validate_key + parent==EXPORT_DIR 双检 | 已控，Rust 对拍 |
| 命令注入 | 无 shell 拼接，固定 argv | 已控；Rust Command 无 shell |
| token 泄露 | S-12 本批修复；export 用 publish token | 双 token 已分离 |
| 日志泄密 | 复核通过 | 持续，scanner 兜底 |
| 公网暴露 | 默认 loopback + tunnel；CORS 白名单 | 已控 |
| 资源耗尽 | mem/pids 限制（内核容器）；events/results 滚动 | R5 订阅上限 |
| 任意重定向 | urllib 默认跟随（ipmap/DoH） | R4/R5 限次 |
| 云元数据 | 未拦截 | R5 |

## 待办清单
- [x] R0：上表 7 项
- [ ] R5：SSRF/订阅上限/重定向限次/元数据拦截
- [ ] R12：supervisor + 容器非 root/只读根/最小 caps + cargo audit/deny + SBOM + 镜像签名

## 测试证据
- test_hardening 新增：StatusRedactionTest×4、MixedPortDefaultTest、QueryTokenCompatTest、SentinelDropTest。
- `python tools/scan_secrets.py` 干净通过。

## 风险与回滚
- 泄密掩码影响面板显示（显示 ***）；validate_patch 哨兵丢弃保证保存不覆盖。回滚=revert 对应提交。

## 下一步
- R5 威胁控制项。
