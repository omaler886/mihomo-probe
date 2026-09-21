"""Alert delivery for things that must not fail silently.

Two channels: a Telegram bot and a generic webhook. Both are optional and both
are configured from the dashboard, so the system can be deployed without
credentials and wired up later.

Every alert goes through a per-key cooldown. Without one, a condition like
"alive count is low" would fire a message every 30 minutes for as long as it
lasts, which is how alerting turns into noise that gets muted.
"""
import json
import time
import urllib.error
import urllib.request

from . import config

STATE_PATH = config.DATA / "alert-state.json"

LEVELS = {"info", "warn", "error"}


class AlertError(RuntimeError):
    pass


def _state():
    try:
        return json.loads(STATE_PATH.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}


def _save(state):
    STATE_PATH.parent.mkdir(parents=True, exist_ok=True)
    tmp = STATE_PATH.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(state, ensure_ascii=False, indent=1), encoding="utf-8")
    tmp.replace(STATE_PATH)


def _cooldown_ok(state, key, minutes, now=None):
    now = now or time.time()
    last = state.get(key)
    if last is None:
        return True
    return (now - float(last)) >= minutes * 60


def telegram_send(cfg_alert, text):
    """Post to the configured Telegram bot; return (ok, detail)."""
    token = (cfg_alert.get("telegram") or {}).get("token", "").strip()
    chat_id = str((cfg_alert.get("telegram") or {}).get("chat_id", "")).strip()
    if not token or not chat_id:
        return False, "telegram 未配置 token/chat_id"
    url = f"https://api.telegram.org/bot{token}/sendMessage"
    payload = json.dumps({"chat_id": chat_id, "text": text,
                          "disable_web_page_preview": True}).encode("utf-8")
    req = urllib.request.Request(url, data=payload, method="POST")
    req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=20) as resp:
            body = resp.read().decode("utf-8", "replace")[:200]
            return resp.status == 200, body
    except urllib.error.HTTPError as exc:
        return False, f"HTTP {exc.code}: {exc.read().decode('utf-8', 'replace')[:200]}"
    except Exception as exc:
        return False, f"{type(exc).__name__}: {str(exc)[:160]}"


def webhook_send(cfg_alert, payload):
    """POST the alert as JSON to a generic webhook; return (ok, detail)."""
    url = str((cfg_alert.get("webhook") or {}).get("url", "")).strip()
    if not url:
        return False, "webhook 未配置 url"
    body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
    req = urllib.request.Request(url, data=body, method="POST")
    req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=20) as resp:
            return 200 <= resp.status < 300, f"HTTP {resp.status}"
    except urllib.error.HTTPError as exc:
        return False, f"HTTP {exc.code}: {exc.read().decode('utf-8', 'replace')[:200]}"
    except Exception as exc:
        return False, f"{type(exc).__name__}: {str(exc)[:160]}"


def send(cfg, key, title, body, level="warn", log=None):
    """Deliver one alert, honouring the per-key cooldown.

    Returns a list of per-channel result strings; an empty list means every
    channel is unconfigured, which is not an error.
    """
    cfg_alert = cfg.get("alert", {})
    if not cfg_alert.get("enabled", False):
        return []
    level = level if level in LEVELS else "warn"
    cooldown = max(1, int(cfg_alert.get("cooldown_minutes", 240)))

    state = _state()
    if not _cooldown_ok(state, key, cooldown):
        if log:
            log("info", f"告警 {key} 处于冷却期，跳过发送")
        return []
    state[key] = time.time()
    _save(state)

    # Labelled UTC and actually UTC: the container runs TZ=Asia/Shanghai, so a
    # bare strftime here used to print local time under a "UTC" label and put
    # every alert 8 hours off for whoever was reading it.
    stamp = time.strftime("%m-%d %H:%M", time.gmtime())
    text = f"[mihomo-test {level.upper()}] {title}\n{body}\n{stamp} UTC"
    payload = {"key": key, "level": level, "title": title, "body": body,
               "source": "mihomo-test", "ts": int(time.time())}

    results = []
    if (cfg_alert.get("telegram") or {}).get("enabled"):
        ok, detail = telegram_send(cfg_alert, text)
        results.append(f"telegram: {'已发送' if ok else detail}")
    if (cfg_alert.get("webhook") or {}).get("enabled"):
        ok, detail = webhook_send(cfg_alert, payload)
        results.append(f"webhook: {'已发送' if ok else detail}")

    if log:
        log(level if level != "error" else "error", f"告警[{key}] {title}"
            + (f"（{'; '.join(results)}）" if results else "（未配置任何通道）"))
    return results


def test_channels(cfg):
    """Send a canary through every configured channel; returns per-channel text."""
    cfg_alert = cfg.get("alert", {})
    if not cfg_alert.get("enabled", False):
        return ["告警未启用（面板里先打开）"]
    text = ("[mihomo-test TEST] 告警通道测试\n"
            "如果你收到这条，说明通道配置正确。\n"
            + time.strftime("%m-%d %H:%M", time.gmtime()) + " UTC")
    payload = {"key": "test", "level": "info", "title": "告警通道测试",
               "body": "如果你收到这条，说明通道配置正确。",
               "source": "mihomo-test", "ts": int(time.time())}
    out = []
    if (cfg_alert.get("telegram") or {}).get("enabled"):
        ok, detail = telegram_send(cfg_alert, text)
        out.append(f"telegram: {'OK' if ok else '失败 ' + detail}")
    if (cfg_alert.get("webhook") or {}).get("enabled"):
        ok, detail = webhook_send(cfg_alert, payload)
        out.append(f"webhook: {'OK' if ok else '失败 ' + detail}")
    return out or ["没有任何通道被启用"]


def reset_cooldown(key=None):
    """Clear the cooldown for one key (or all), so a test alert can be re-sent."""
    state = _state()
    if key is None:
        state = {}
    else:
        state.pop(key, None)
    _save(state)