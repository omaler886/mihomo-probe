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

## CI 门禁（2026-10-01 核对 `.github/workflows/ci.yml` 实际内容）
外部 Go 方案 §十一给的命令是 Go 专用（`go vet` / `-race` / `govulncheck` / `golangci-lint`），
**不能照抄**——Rust 侧等价物不同，逐条对照如下：

| 方案建议 | 本仓库现状 | Rust 等价物 / 处置 |
|---|---|---|
| `gofmt -w .` | ✅ 已有 `cargo fmt --all -- --check` | — |
| `go vet ./...` | ✅ 已有 `clippy -D warnings` | — |
| `go test ./...` | ✅ 已有 `cargo test --workspace --locked` | — |
| `go test -race` | ❌ 未做 | Rust **无 `-race`**。等价：`cargo +nightly miri test`（UB/数据竞争）、loom（并发模型）。R12 定 |
| `go test -count=3` | ❌ 未做 | Rust 无内建 `-count`；用脚本重复跑 3 次（或 `cargo nextest`）。R12 |
| `-coverprofile` | ❌ 未做 | `cargo llvm-cov --workspace`。R12 |
| `-bench . -benchmem` | ❌ 未做 | `cargo bench`（criterion），基线入库。R12 |
| `staticcheck` / `golangci-lint` | ✅ clippy 已覆盖主体 | 可选加 `cargo machete`（未用依赖）。低优先 |
| `govulncheck` | ❌ 未做 | `cargo audit` + `cargo deny check advisories`。**R12 必做** |
| `gitleaks detect` | ✅ 已有 `tools/scan_secrets.py`（含 `--history`） | 已等效，无需换工具 |
| SBOM | ❌ 未做 | `cargo cyclonedx` 或 `cargo sbom`。R12 |
| 镜像签名 | ❌ 未做 | R12（cosign） |

### 发布门禁（R12 冻结）
单元测试通过 · Golden Test 无未审核差异 · 并发检查（Miri/loom）通过 · Secret scan 通过 ·
数据库迁移与回滚演练通过 · 容器健康检查通过 · Shadow 差异在允许范围内 ·
二进制写入 Probe commit + 依赖版本（`probe-cli version` 可查）。

## 待办清单
- [x] R0：install.sh 门禁修复
- [ ] R2：probe-cli migrate check/backup/apply/verify/rollback
- [ ] R12：覆盖率 / bench 基线 / `cargo audit` + `cargo deny` / SBOM / 镜像签名
- [ ] R12：并发检查选型（Miri vs loom）并接入 CI
- [ ] R10：CI 归档 JUnit XML + JSON 汇总（见 13）
- [ ] 发布：README/DEPLOY 文档与命令一致性复核（§21"文档与实际命令一致"）

## 风险与回滚
- migrate.sh 打包含凭据（.env/config.json/token），文档已注明传输责任面；R12 评估拆分凭据包。

## 下一步
- R2 迁移 CLI。
