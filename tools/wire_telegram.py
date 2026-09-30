#!/usr/bin/env python3
"""Wire mihomo-test alerts to a Telegram bot + chat pair you already have.

Two credential sources, tried in order:

1. An existing notifier script of yours that stores ``BOT =`` / ``CHAT =`` at
   the top level -- point ``TG_WATCHER_FILE`` at it. Reusing its proven pair
   means the canary message lands in a chat that already receives from it.
2. ``/root/.acme.sh/account.conf`` (``SAVED_TELEGRAM_BOT_APITOKEN`` /
   ``SAVED_TELEGRAM_BOT_CHATID``), the keys acme.sh's Telegram hook writes.

Either way the credentials are verified against the Telegram API (getMe) and
written into the app config. Secrets are never printed -- only the bot's
public username and success/failure. You can also skip this script entirely
and fill bot token + chat id in the panel's settings page.
"""
import json
import os
import re
import sys
import urllib.error
import urllib.request

sys.path.insert(0, "/srv/mihomo-test")
from mihomo_test import config as cfgmod  # noqa: E402

ACME_CONF = os.environ.get("ACME_CONF", "/root/.acme.sh/account.conf")
WATCHER_FILE = os.environ.get("TG_WATCHER_FILE", "")


def read_pairs(path):
    """Pull KEY='value' / KEY="value" assignments out of a shell-style file."""
    try:
        text = open(path, encoding="utf-8", errors="replace").read()
    except OSError as exc:
        print(f"  cannot read {path}: {exc}")
        return {}
    out = {}
    for match in re.finditer(r'^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.+?)\s*$',
                             text, re.M):
        key, raw = match.group(1), match.group(2)
        raw = raw.strip()
        if len(raw) >= 2 and raw[0] == raw[-1] and raw[0] in "'\"":
            raw = raw[1:-1]
        if raw:
            out[key] = raw
    return out


def mask(value):
    """Describe a secret without revealing it."""
    return f"<{len(value)} 字符>"


def main():
    token = chat_id = None
    if WATCHER_FILE:
        try:
            text = open(WATCHER_FILE, encoding="utf-8", errors="replace").read()
            m = re.search(r"^BOT\s*=\s*['\"]([^'\"]+)['\"]", text, re.M)
            n = re.search(r"^CHAT\s*=\s*['\"]([^'\"]+)['\"]", text, re.M)
            token, chat_id = (m.group(1) if m else None), (n.group(1) if n else None)
        except OSError as exc:
            print(f"  cannot read {WATCHER_FILE}: {exc}")

    print("提取结果:")
    print(f"  token   : {'找到 ' + mask(token) if token else '未找到'}")
    print(f"  chat_id : {'找到 ' + mask(chat_id) if chat_id else '未找到'}")
    if not token or not chat_id:
        pairs = read_pairs(ACME_CONF)

        def pick(name):
            return pairs.get("SAVED_" + name) or pairs.get(name)

        token = token or pick("TELEGRAM_BOT_APITOKEN")
        chat_id = chat_id or pick("TELEGRAM_BOT_CHATID")
        print(f"  回退 acme.sh: token={'有' if token else '无'} chat_id={'有' if chat_id else '无'}")
    if not token or not chat_id:
        print("  凭据不全，中止")
        return 1

    def call(path, payload=None):
        url = f"https://api.telegram.org/bot{token}{path}"
        body = json.dumps(payload).encode() if payload is not None else None
        req = urllib.request.Request(url, data=body, method="POST" if body else "GET")
        req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req, timeout=25) as resp:
                return resp.status, json.load(resp)
        except urllib.error.HTTPError as exc:
            return exc.code, json.loads(exc.read().decode("utf-8", "replace") or "{}")
        except Exception as exc:
            return 0, {"error": str(exc)[:150]}

    status, me = call("/getMe")
    if status != 200 or not me.get("ok"):
        print(f"  getMe 失败: HTTP {status} {json.dumps(me)[:200]}")
        print("  —— 凭据不可用，不写入配置")
        return 1
    bot = me["result"]
    print(f"  getMe   : OK，bot=@{bot.get('username')} (id={bot.get('id')})")

    status, sent = call("/sendMessage",
                        {"chat_id": chat_id,
                         "text": "[mihomo-test] 告警通道接入测试\n"
                                 "这条消息来自 vps 上的节点测活中心。\n"
                                 "之后护栏触发/轮次异常/存活骤降都会发到这里。"})
    if status != 200 or not sent.get("ok"):
        print(f"  sendMessage 失败: HTTP {status} {json.dumps(sent)[:250]}")
        print("  —— 常见原因：bot 未加入该对话，或 chat_id 不是这个 bot 可写的目标")
        return 1
    print(f"  sendMessage: OK，message_id={sent['result'].get('message_id')}")

    cfg = cfgmod.load()
    cfg["alert"]["enabled"] = True
    cfg["alert"]["telegram"] = {"enabled": True, "token": token, "chat_id": chat_id}
    if not cfg["alert"].get("alive_floor"):
        # a floor only makes sense once there is a baseline; 100 of 376 is a
        # collapse, not noise
        cfg["alert"]["alive_floor"] = 0
    cfgmod.save(cfg)
    print("  已写入 config.json 并启用 Telegram 告警")
    return 0


if __name__ == "__main__":
    sys.exit(main())