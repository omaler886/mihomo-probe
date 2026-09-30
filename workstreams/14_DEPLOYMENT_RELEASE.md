# 部署与发布

## 目标
迁移模式 legacy→shadow→audit→rust→rust-only 与发布供应链（总控 §18/§17）。

## 当前实现审计
- 部署：docker compose 三容器（app/kernel/tunnel）全 host 网络；install.sh（R0 修复测试门禁）；migrate.sh pack/unpack（含 data/ 全量，凭据随包，传输面靠操作者）；setup_tunnel.py（acme.sh 凭据复用，token 0600）；ban_legacy.sh（旧管线下线，占位符化）。
- 环境变量：SUBSTORE_BACKEND/MIHOMO_TEST_TOKEN/MIHOMO_TEST_HOST_ROOT/MIHOMO_TEST_CORE_CONTAINER/TUNNEL_TOKEN/TZ；R0 增 MIHOMO_TEST_MIXED_PORT。
- 已知部署约定：内核 mixed-port 19194、API 19190、车道 19200+、ipmap 19191/19300+/19494；全部 loopback。

## 迁移模式现状
- 当前 = legacy（Python 发布）。R1 起 Rust 以 shadow 工件存在（可独立构建运行，不接流量）。
- 切 rust 的前置门禁见总控 §18（迁移回滚演练/shadow 多轮/契约/实机联调/真内核/快照/安全/文档一致）。

## 待办清单
- [x] R0：install.sh 门禁修复
- [ ] R2：probe-cli migrate check/backup/apply/verify/rollback
- [ ] R12：CI/SBOM/cargo audit/deny/镜像签名
- [ ] 发布：README/DEPLOY 文档与命令一致性复核（§21"文档与实际命令一致"）

## 风险与回滚
- migrate.sh 打包含凭据（.env/config.json/token），文档已注明传输责任面；R12 评估拆分凭据包。

## 下一步
- R2 迁移 CLI。
