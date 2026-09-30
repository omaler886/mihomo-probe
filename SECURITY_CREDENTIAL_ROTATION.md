# 凭据轮换台账（Security Credential Rotation Ledger）

> 只记录**哪里有凭据、什么时候轮换、怎么轮换**，绝不记录凭据本身。
> 真实值只存在于部署侧（.env / data/ / 环境变量 / 对应平台后台）。
> 2026-09-30 仓库已完成全历史脱敏重写与强推（旧远端 SHA 2428cbf → 新 03cb6ea）；
> 推送前的全历史扫描（tools/scan_secrets.py --history）确认 git 历史无可利用凭据残留。

## 状态图例
未轮换 = 从未换过；已轮换 = 记录日期起已换发；无需 = 不承载秘密。

## 台账

| # | 凭据 | 存放位置 | 风险面 | 状态 / 日期 | 轮换动作 |
|---|---|---|---|---|---|
| 1 | 面板管理令牌 `auth.token` | `data/config.json`（或 `MIHOMO_TEST_TOKEN`） | 泄露 = 面板 + docker socket 等价主机 root（S-11） | 未轮换 / — | 面板设置不可改（MIN_TOKEN_LEN 保护）；轮换 = 编辑 `data/config.json` 的 `auth.token` 后 `POST /api/reload` 或重启容器；同步通知所有面板使用者 |
| 2 | 只读发布令牌 `publish.token` | `data/config.json` | 泄露 = 可读全部节点导出（节点凭据随之泄露） | 未轮换 / — | 编辑 `data/config.json` 的 `publish.token` 后 reload；随后必须重按 §09 重建 Sub-Store 联动（URL 里带旧 token） |
| 3 | 内核 API secret `core.secret` | `data/core.secret`（0600） | 本机 loopback 面；泄露可操纵内核 Controller | 未轮换 / — | 删除 `data/core.secret` 后重启应用（自动重生成）并重建内核容器 |
| 4 | Sub-Store 后端密钥路径 | `SUBSTORE_BACKEND`（.env） | 泄露 = 他人可读写你的 Sub-Store 全部订阅（含节点凭据） | 未轮换 / — | 在 Sub-Store 侧改后端 secret 路径 → 更新 .env → `docker compose up -d mihomo-test` |
| 5 | Cloudflare Tunnel token | `data/tunnel.token`（0600，setup_tunnel.py 写入） | 泄露 = 可把任意服务发布到你的隧道域名 | 未轮换 / — | CF Zero Trust 面板删隧道重建，或 setup_tunnel.py 重跑；更新 `data/tunnel.token` 后 `docker compose up -d cloudflared-probe` |
| 6 | Cloudflare API 凭据 | 宿主机 `/root/.acme.sh/account.conf`（setup_tunnel.py 读取） | 泄露 = 整个 CF 账户（DNS/隧道） | 未轮换 / — | CF 面板换 API Token（建议改用最小权限 Zone-scoped Token 而非 Global Key），更新 account.conf |
| 7 | Telegram Bot Token | 面板设置（`alert.telegram.token`） | 泄露 = 可冒用 bot 发消息（读面有限） | 未轮换 / — | @BotFather `/revoke` 换发 → 面板设置更新（保存路径已做掩码哨兵保护） |
| 8 | 节点凭据（UUID/密码/REALITY pbk、sid） | 上游订阅 / 自建节点配置 | 泄露 = 节点被盗用 | 已轮换 / 2026-09-30（发行前处置，见 SECURITY_REVIEW §A） | 订阅侧换发；本仓库从不存值 |
| 9 | 真实域名 / 主机标识 | 部署侧 | 非凭据，但放大泄露面 | 已脱敏 / 2026-09-30（bdee1ec 全量清洗，占位符见记忆映射表） | 如再发现入库，沿用占位符映射并轮换 #2/#4 |
| 10 | VPS SSH 凭据 | 宿主机 / 运维本机 | 主机完全控制 | 未轮换 / — | 常规 SSH key 轮换流程，与本仓库无关；记录以示完整 |

## 轮换触发条件（满足其一必须执行）
- 任何快照/日志/截图包含真实 token（SECURITY_REVIEW S-04 曾触发 → #2 需在部署侧执行）。
- `.env` / `data/` 曾被打包发给不可信方（migrate.sh 包内含凭据，传输通道必须可信）。
- 对应平台在日志里出现异常调用。

## 与代码的联动
- `mihomo_test/config.py` `_ensure_tokens`：缺失/过短自动生成，长度下限 MIN_TOKEN_LEN=16。
- `mihomo_test/server.py` `redacted_config`：轮询载荷一律 `***`（2026-09-30 起，含 telegram/webhook/backend 路径）。
- `tools/scan_secrets.py`：提交前/CI 扫描；`--history` 用于推送前全历史复核。
