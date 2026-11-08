# syntax=docker/dockerfile:1
# The app steers its own kernel container (restart it, run `mihomo -t` config
# checks), so it needs a docker CLI talking to the host daemon over a mounted
# socket. Everything else is stdlib plus PyYAML.
#
# Workstreams/34 R7：Rust 构建段。probe-cli（Rust 全量重构线）在独立 stage
# 构建，产物放进运行镜像；默认 CMD 仍是 Python（生产切换属部署决策），切
# 换用 compose override：`command: /usr/local/bin/probe-cli --root
# /srv/mihomo-test serve --host 127.0.0.1 --port 8088`。构建在 linux/amd64
# 上进行，与 CI 的 ubuntu runner 同 ABI（musl 交叉留给发布批次）。

FROM docker:cli AS dockercli

FROM rust:1.99-slim AS rustbuild
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY migrations ./migrations
RUN cargo build --release --locked -p probe-cli

FROM python:3.11-slim

# tzdata so log and panel timestamps can follow the user's wall clock.
# Dependencies come from requirements.txt -- copy it alone first so the pip
# layer is only rebuilt when the dependency list actually changes, not on
# every source edit.
RUN apt-get update \
 && apt-get install -y --no-install-recommends tzdata \
 && rm -rf /var/lib/apt/lists/*

COPY requirements.txt ./
RUN pip install --no-cache-dir -r requirements.txt

COPY --from=dockercli /usr/local/bin/docker /usr/local/bin/docker
COPY --from=rustbuild /build/target/release/probe-cli /usr/local/bin/probe-cli

WORKDIR /srv/mihomo-test
COPY mihomo_test ./mihomo_test
COPY tests ./tests
# tools/ 不在请求路径上,但 tests/test_hardening.CdnBuildGuardTest 用
# importlib 加载 tools/build_web.py 来验证构建产物——漏了它,容器内的
# unittest discover 会对着一套根本不存在的脚本报 7 个错。
COPY tools ./tools

ENV MIHOMO_TEST_ROOT=/srv/mihomo-test \
    MIHOMO_TEST_HOST_ROOT=/srv/mihomo-test \
    TZ=Asia/Shanghai \
    PYTHONUNBUFFERED=1

# Loopback only. The dashboard is published through cloudflared, which shares
# the host network namespace; binding 0.0.0.0 here would put the UI on the
# host's public addresses.
CMD ["python3", "-m", "mihomo_test", "serve", "--host", "127.0.0.1", "--port", "8088"]