# syntax=docker/dockerfile:1
# The app steers its own kernel container (restart it, run `mihomo -t` config
# checks), so it needs a docker CLI talking to the host daemon over a mounted
# socket. Everything else is stdlib plus PyYAML.

FROM docker:cli AS dockercli

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

WORKDIR /srv/mihomo-test
COPY mihomo_test ./mihomo_test
COPY tests ./tests

ENV MIHOMO_TEST_ROOT=/srv/mihomo-test \
    MIHOMO_TEST_HOST_ROOT=/srv/mihomo-test \
    TZ=Asia/Shanghai \
    PYTHONUNBUFFERED=1

# Loopback only. The dashboard is published through cloudflared, which shares
# the host network namespace; binding 0.0.0.0 here would put the UI on the
# host's public addresses.
CMD ["python3", "-m", "mihomo_test", "serve", "--host", "127.0.0.1", "--port", "8088"]