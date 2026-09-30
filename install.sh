#!/bin/bash
# Deploy / redeploy mihomo-test as a Docker stack.
#
#   docker compose build && docker compose up -d
#
# The host systemd unit from the previous (non-container) install is disabled
# if present, so the two cannot fight over port 8088 and the round lock.
set -euo pipefail

ROOT="${MIHOMO_TEST_ROOT:-/srv/mihomo-test}"
PORT="${PORT:-8088}"
cd "$ROOT"

echo "== files =="
ls -1 Dockerfile docker-compose.yml requirements.txt 2>/dev/null | sed 's/^/  /'

echo
echo "== python syntax (host check; the real test run happens in the image) =="
python3 -m py_compile mihomo_test/*.py && echo "  python ok"

echo
echo "== retire the host systemd unit (superseded by the container) =="
if systemctl list-unit-files 2>/dev/null | grep -q '^mihomo-test.service'; then
  systemctl disable --now mihomo-test >/dev/null 2>&1 || true
  echo "  mihomo-test.service disabled"
else
  echo "  not present, nothing to do"
fi

echo
echo "== build (the running stack is not touched until the tests pass) =="
docker compose build mihomo-test
echo
echo "== test suite (one-off container; failure ABORTS the deploy) =="
# Gate, not decoration. The old shape ran the suite against the *already
# replaced* stack through `docker exec ... | tail -3 || true`: the pipe handed
# the exit status of tail (always 0) to the shell and `|| true` swallowed the
# rest, so a red suite changed nothing. Now the suite runs in a one-off
# container before anything is recreated, and the rc is captured POSIX-safely
# (no `set -o pipefail` reliance in the image's /bin/sh).
if ! docker compose run --rm --no-deps mihomo-test sh -c \
    'cd /srv/mihomo-test && python3 -m unittest discover -s tests > /tmp/tests.log 2>&1; rc=$?; tail -5 /tmp/tests.log; exit $rc'; then
  echo "  test suite FAILED -- deploy aborted, running stack left untouched"
  exit 1
fi
docker compose up -d --force-recreate mihomo-probe mihomo-test
echo
echo "== dependencies (declared in requirements.txt) =="
docker exec mihomo-test python3 -c \
  'import yaml; print("  pyyaml", yaml.__version__)'
docker compose up -d cloudflared-probe
sleep 4
docker ps --format '{{.Names}} | {{.Status}}' | grep -E 'mihomo|cloudflared' | sed 's/^/  /'

echo
echo "== health (dashboard is loopback-only; cloudflared publishes it) =="
for i in $(seq 1 20); do
  if curl -fsS --max-time 4 "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then
    echo "  healthz OK after ${i}s"
    break
  fi
  sleep 1
done
curl -fsS --max-time 5 "http://127.0.0.1:$PORT/healthz" && echo

echo
echo "== next steps =="
echo "  dashboard : http://127.0.0.1:$PORT/?token=\$(python3 -c \"import json;print(json.load(open('$ROOT/data/config.json'))['auth']['token'])\")"
echo "  logs      : docker logs -f mihomo-test"
echo "  run round : docker exec mihomo-test python3 -m mihomo_test round"
echo "  tunnel    : data/tunnel.token (create with setup_tunnel.py if missing)"