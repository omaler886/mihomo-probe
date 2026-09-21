#!/bin/bash
# Move a mihomo-test deployment to another machine.
#
#   ./migrate.sh pack                 -> writes mihomo-test-<ts>.tar.gz here
#   ./migrate.sh unpack <archive>     -> restores into $PWD (run on the target)
#
# Everything that matters travels inside the archive: ./data carries the SQLite
# ledger, config.json (UI token, sources, alert settings), the core secret and
# the Cloudflare tunnel token, so the target picks up the same dashboard URL and
# the same convergence history. Nothing else has to be re-created.
#
# After unpacking on the target: `docker compose up -d --build`.
set -euo pipefail

NAME="mihomo-test"
MODE="${1:-pack}"

case "$MODE" in
  pack)
    STAMP=$(date -u +%Y%m%dT%H%M%SZ)
    OUT="${NAME}-${STAMP}.tar.gz"
    # __pycache__ and logs are rebuildable noise; the archive is still complete
    # without them because the kernel config is regenerated every round.
    tar -czf "$OUT" \
        --exclude='__pycache__' --exclude='*.log' --exclude='.tmpcheck' \
        --exclude='data/state.db-journal' \
        Dockerfile docker-compose.yml .dockerignore .env \
        README.md install.sh setup_tunnel.py link_substore.py ban_legacy.sh \
        mihomo_test tests data core 2>/dev/null || true
    echo "wrote $OUT ($(du -h "$OUT" | cut -f1))"
    echo
    echo "on the target machine:"
    echo "  mkdir -p /srv/mihomo-test && tar -xzf $OUT -C /srv/mihomo-test"
    echo "  cd /srv/mihomo-test && docker compose up -d --build"
    echo
    echo "note: if the target keeps the project somewhere other than"
    echo "/srv/mihomo-test, also set MIHOMO_TEST_HOST_ROOT in .env to that path."
    ;;
  unpack)
    ARCHIVE="${2:-}"
    [ -n "$ARCHIVE" ] || { echo "usage: $0 unpack <archive>"; exit 1; }
    mkdir -p "$(pwd)"
    tar -xzf "$ARCHIVE" -C "$(pwd)"
    chmod 600 data/*.secret data/tunnel.token 2>/dev/null || true
    echo "restored into $(pwd)"
    echo "next: docker compose up -d --build"
    ;;
  *)
    echo "usage: $0 pack | unpack <archive>"
    exit 1
    ;;
esac