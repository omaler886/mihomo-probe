#!/bin/bash
# Ban the two redundant, silently-failing legacy liveness pipelines.
# Everything is backed up and reversible; see RESTORE.md in the backup dir.
set -u

STAMP=$(date -u +%Y%m%dT%H%M%SZ)
BK="/srv/legacy-ban-backup/$STAMP"
mkdir -p "$BK"
chmod 700 /srv/legacy-ban-backup "$BK" 2>/dev/null || true
echo "backup dir: $BK"

echo
echo "=== 1. host cron ==="
crontab -l > "$BK/crontab.before" 2>/dev/null || true
echo "--- before ---"; cat "$BK/crontab.before"
# keep acme.sh and the canary; drop only the mihomo-health lines
crontab -l 2>/dev/null | grep -v '/srv/mihomo-health/' > "$BK/crontab.after"
crontab "$BK/crontab.after"
echo "--- after ---"; crontab -l
remain=$(crontab -l 2>/dev/null | grep -c '/srv/mihomo-health/')
echo "legacy cron lines remaining: $remain"

echo
echo "=== 2. systemd timer ==="
systemctl disable --now mihomo-healthcheck.timer 2>&1 | sed 's/^/  /'
printf "  timer: active=%s enabled=%s\n" \
  "$(systemctl is-active mihomo-healthcheck.timer 2>&1)" \
  "$(systemctl is-enabled mihomo-healthcheck.timer 2>&1)"

echo
echo "=== 3. legacy probe container ==="
docker stop mihomo-air 2>&1 | sed 's/^/  /'
# keep it from coming back if the stack is ever started by hand
cd /srv/mihomo-health 2>/dev/null && cp -a docker-compose.yml "$BK/" 2>/dev/null || true
printf "  mihomo-air: %s\n" "$(docker ps -a --filter name=mihomo-air --format '{{.Status}}')"

echo
echo "=== 4. nginx public exposure ==="
# This endpoint served node credentials (server/port/uuid/password) behind only
# a secret path, and freezes at a stale snapshot once the cron is gone.
cp -a /etc/nginx/sites-available/sub-store "$BK/sub-store.vhost.before"
if grep -q '^[[:space:]]*include /etc/nginx/snippets/mihomo-health.conf;' /etc/nginx/sites-available/sub-store; then
  sed -i 's|^\([[:space:]]*\)include /etc/nginx/snippets/mihomo-health.conf;|\1# include /etc/nginx/snippets/mihomo-health.conf;  # BANNED legacy|' \
    /etc/nginx/sites-available/sub-store
  echo "  include line commented out"
else
  echo "  include line already gone or reformatted; leaving as-is"
fi
cp -a /etc/nginx/snippets/mihomo-health.conf "$BK/" 2>/dev/null || true
if nginx -t 2>&1 | sed 's/^/  /'; then
  systemctl reload nginx && echo "  nginx reloaded"
else
  echo "  nginx config INVALID -- restoring"
  cp -a "$BK/sub-store.vhost.before" /etc/nginx/sites-available/sub-store
  nginx -t 2>&1 | sed 's/^/  /'
fi

echo
echo "=== 5. orphaned Sub-Store collection ==="
cd /srv/mihomo-test
python3 - "$BK" <<'PY'
import json, sys, urllib.request, urllib.parse
sys.path.insert(0, '/srv/mihomo-test')
from mihomo_test import config as cfgmod
from mihomo_test.store import Client, StoreError

backup = sys.argv[1]
cfg = cfgmod.load()
c = Client(cfg["substore"]["backend"])
# "legacy-alive" is the output collection of the banned per-subscription pipeline:
# empty, owned by it, and referenced by nothing.
target = "legacy-alive"
record = c.collection(target)
if record is None:
    print(f"  {target}: already absent")
else:
    with open(f"{backup}/substore-collection-{target}.json", "w", encoding="utf-8") as fh:
        json.dump(record, fh, ensure_ascii=False, indent=2)
    print(f"  backed up record ({len(record.get('subscriptions') or [])} members, "
          f"remark={record.get('remark')!r})")
    if record.get("subscriptions"):
        print("  HAS MEMBERS -- not deleting, decide manually")
    else:
        try:
            c._request("DELETE", "/api/collection/" + urllib.parse.quote(target, safe=""))
            print(f"  deleted collection {target}")
        except StoreError as exc:
            print(f"  delete failed: {exc}")
PY

cat > "$BK/RESTORE.md" <<EOF
# 回滚 legacy 管线禁用 ($STAMP)

全部动作可逆。按需执行：

1. host cron
   crontab $BK/crontab.before

2. systemd timer (机场 per-subscription 管线)
   systemctl enable --now mihomo-healthcheck.timer

3. 旧探测容器
   systemctl start docker  # 若未运行
   cd /srv/mihomo-health && docker compose up -d mihomo-air

4. nginx 公开端点 (/health/*)
   cp -a $BK/sub-store.vhost.before /etc/nginx/sites-available/sub-store
   nginx -t && systemctl reload nginx

5. Sub-Store 孤儿集合 legacy-alive
   见 $BK/substore-collection-legacy-alive.json（若已删除）

被禁用的东西仍在磁盘上：/srv/mihomo-health, /srv/healthcheck
未改动：devcloud-healthcheck.timer, node-engine 容器, air/legacy-sub-c/legacy-sub-d 等上游订阅,
       Sub-Store 的 SUB_STORE_PRODUCE_CRON (仍在刷新 air 集合)
EOF
echo "  restore notes: $BK/RESTORE.md"