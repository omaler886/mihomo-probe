"""线上验收：「直连 / 链式」两个测量开关。

后端 200 不等于页面正常，页面能渲染也不等于点击改对了来源。这个脚本用真
浏览器打开线上面板，同时验证三件事：

1. 渲染：表头有「直连」「链式」，每行各一个复选框，缺省即勾选，无 JS 异常。
2. 定位：每一行的复选框 ref 是来源的**唯一 key**，不是 `kind|name`。
   链式聚合与 air 都是 `collection|air`，用 kind|name 会让链式聚合那一行的
   每个开关都改到已停用的 air 上（旧缺陷）。
3. 写入：点一下开关，抓 `POST /api/config` 的请求体，确认变的是目标来源、
   同一个 kind|name 的孪生来源没被动过，然后点回去恢复原状。

用法:
    python tools/verify_measure_switch.py
    python tools/verify_measure_switch.py --url https://...   # 换入口
"""
import argparse
import json
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

CHROME_HINT = pathlib.Path.home() / "AppData/Local/ms-playwright/chromium-1243/chrome-win64/chrome.exe"
DEFAULT_URL = "https://probe.example.com/"
SSH = ["ssh", "-o", "ConnectTimeout=20", "-o", "BatchMode=yes", "vps"]


def remote_token():
    """Read the panel token off vps without printing it."""
    cmd = ("python3 -c \"import json;print(json.load(open("
           "'/srv/mihomo-test/data/config.json'))['auth']['token'])\"")
    p = subprocess.run(SSH + [cmd], capture_output=True, timeout=60)
    tok = p.stdout.decode().strip()
    if not tok:
        sys.exit("取不到 token: " + p.stderr.decode("utf-8", "replace")[:200])
    return tok


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default=DEFAULT_URL)
    ap.add_argument("--token", default="")
    ap.add_argument("--readonly", action="store_true",
                    help="只做渲染与定位断言，不点击开关（不写线上配置）")
    args = ap.parse_args()

    from playwright.sync_api import sync_playwright

    token = args.token or remote_token()
    # `#token=` rather than `?token=`: the fragment is never sent to the server,
    # so the admin token stays out of the Cloudflare Tunnel's request log. The
    # panel reads either form and scrubs it from the address bar on load.
    url = args.url.rstrip("/") + "/#token=" + token

    console_errors, page_errors, config_posts = [], [], []

    with sync_playwright() as p:
        browser = p.chromium.launch(
            headless=True,
            args=["--disable-blink-features=AutomationControlled", "--no-sandbox"])
        page = browser.new_page()
        page.on("console", lambda m: console_errors.append(m.text) if m.type == "error" else None)
        page.on("pageerror", lambda e: page_errors.append(str(e)))

        def on_request(req):
            if req.method == "POST" and "/api/config" in req.url:
                try:
                    config_posts.append(json.loads(req.post_data or "{}"))
                except ValueError:
                    config_posts.append({})
        page.on("request", on_request)

        page.goto(url, wait_until="networkidle", timeout=60000)
        page.wait_for_selector("#src-table tbody tr", timeout=30000)
        page.wait_for_timeout(1500)

        headers = [h.inner_text().strip()
                   for h in page.query_selector_all("#src-table thead th")]
        rows = page.eval_on_selector_all(
            "#src-table tbody tr",
            """els => els.map(tr => {
                 const g = s => { const e = tr.querySelector(s); return e ? e.dataset
                     : null; };
                 const on = g("input[data-on]"), ex = g("input[data-export]");
                 const d = g("input[data-direct]"), c = g("input[data-chain]");
                 const chk = s => { const e = tr.querySelector(s);
                     return e ? e.checked : null; };
                 return {
                   name: (tr.querySelector("td.name") || {}).innerText || "",
                   configured: !!tr.querySelector("input[data-key]"),
                   onRef: on ? on.on : null,
                   directRef: d ? d.direct : null,
                   chainRef: c ? c.chain : null,
                   directOn: chk("input[data-direct]"),
                   chainOn: chk("input[data-chain]"),
                 };
               })""")

        configured = [r for r in rows if r["configured"]]
        checks = []
        checks.append(("表头含「直连」「链式」",
                       "直连" in headers and "链式" in headers, str(headers)))
        checks.append(("表头共 10 列", len(headers) == 10, f"{len(headers)} 列"))
        checks.append(("每行都有直连+链式复选框",
                       all(r["directRef"] and r["chainRef"] for r in rows), f"{len(rows)} 行"))
        checks.append(("缺省即勾选（无显式 false）",
                       all(r["directOn"] and r["chainOn"] for r in rows),
                       str([r["name"] for r in rows if not (r["directOn"] and r["chainOn"])])))
        # Configured sources must be addressed by their unique key. A resource
        # Sub-Store lists but the config lacks has no key yet, so kind|name is
        # the only thing to point at -- that case is expected, not a failure.
        bad_refs = [r["directRef"] for r in configured if "|" in (r["directRef"] or "")]
        checks.append(("已配置来源的 ref 为唯一 key（不含 |）",
                       not bad_refs and len(configured) >= 9,
                       f"{len(configured)} 个已配置，异常 {bad_refs}"))

        # The defect this guards: 链式聚合 and air share kind|name, so a
        # kind|name ref pointed every checkbox at the disabled twin.
        twin_refs = sorted({r["directRef"] for r in rows
                            if r["directRef"] in ("air", "链式聚合")})
        checks.append(("同 kind|name 的孪生来源 ref 互不相同",
                       twin_refs == ["air", "链式聚合"], str(twin_refs)))

        # --- write path: click and inspect the POST body -------------------
        target = next((r for r in rows if r["directRef"] == "链式聚合"), None)
        roundtrip = None
        if args.readonly:
            checks.append(("找到「链式聚合」行（写入验证已跳过）", target is not None,
                           target["directRef"] if target else "未找到"))
        elif target:
            box = page.query_selector('input[data-direct="链式聚合"]')
            config_posts.clear()
            box.click()
            page.wait_for_timeout(4000)
            body = config_posts[-1] if config_posts else {}
            srcs = {s.get("key"): s for s in body.get("sources", [])}
            hit = srcs.get("链式聚合", {})
            twin = srcs.get("air", {})
            roundtrip = {
                "posts": len(config_posts),
                "链式聚合.direct": hit.get("direct"),
                "air.direct": twin.get("direct"),
            }
            checks.append(("点击后 POST 改的是目标来源",
                           hit.get("direct") is False, str(roundtrip)))
            checks.append(("孪生来源未被误改",
                           twin.get("direct") in (None, True), f"air.direct={twin.get('direct')}"))
            # Restore: one more click, then confirm it went back to true.
            config_posts.clear()
            page.query_selector('input[data-direct="链式聚合"]').click()
            page.wait_for_timeout(4000)
            back = {s.get("key"): s for s in (config_posts[-1].get("sources", [])
                                              if config_posts else [])}
            checks.append(("已恢复原值",
                           back.get("链式聚合", {}).get("direct") is True,
                           f"链式聚合.direct={back.get('链式聚合', {}).get('direct')}"))
        else:
            checks.append(("找到「链式聚合」行以做写入验证", False, "未找到"))

        # --- 列头批量开关 --------------------------------------------------
        head = page.eval_on_selector_all(
            "#src-table thead input[data-all]",
            """els => els.map(e => ({field: e.dataset.all, checked: e.checked,
                                     ind: e.indeterminate, disabled: e.disabled}))""")
        by_field = {h["field"]: h for h in head}
        checks.append(("列头 5 个批量开关都在",
                       [h["field"] for h in head]
                       == ["enabled", "export", "relay", "direct", "chain"],
                       str([h["field"] for h in head])))
        # 列头必须与行状态自洽，而不是断言某个固定值：线上数据会变（某个来源被
        # 单独取消勾选是合法的），而「列头说全选、行里却有空的」才是真 bug。
        # 用自洽而不是绝对状态，也让这段不受前一步 direct 往返的时序影响。
        consistency = page.evaluate("""() => {
          const out = [];
          document.querySelectorAll('#src-table thead input[data-all]').forEach(h => {
            const attr = h.dataset.all === 'enabled' ? 'on' : h.dataset.all;
            const boxes = [...document.querySelectorAll(
              `#src-table tbody input[data-${attr}]:not(:disabled)`)];
            const on = boxes.filter(b => b.checked).length;
            out.push({field: h.dataset.all, total: boxes.length, on: on,
                      expectChecked: boxes.length > 0 && on === boxes.length,
                      expectInd: on > 0 && on < boxes.length,
                      actualChecked: h.checked, actualInd: h.indeterminate});
          });
          return out;
        }""")
        mismatched = [c for c in consistency
                      if c["expectChecked"] != c["actualChecked"]
                      or c["expectInd"] != c["actualInd"]]
        checks.append(("列头状态与行状态自洽（全选 / 部分 / 全不选）",
                       not mismatched, str(mismatched or consistency)))
        # 直连 / 链式线上默认全开，这一条是数据断言，只在确实全开时才要求全选。
        all_on = [c["field"] for c in consistency if c["expectChecked"]]
        checks.append(("直连/链式列头显示全选",
                       by_field.get("direct", {}).get("checked") is True
                       and by_field.get("chain", {}).get("checked") is True,
                       f"实测全选的列 {all_on}"))
        # 「中转」列头是什么状态取决于线上数据（可能全勾、也可能部分），所以不单独钉
        # 一个固定值 —— 上面那条「列头与行状态自洽」的通用断言已经覆盖它。
        # 点击验证挑「中转」列：它只影响分类统计口径，既不碰测活也不碰导出，
        # 是这几列里唯一点错也没有副作用的。
        before_sources = page.evaluate(
            """async () => JSON.stringify((await (await api(
                 '/api/status')).json()).config.sources)""")
        before_state = page.eval_on_selector_all(
            "#src-table tbody input[data-relay]:not(:disabled)",
            "els => els.map(e => [e.dataset.relay, e.checked])")
        before_on = [ref for ref, on in before_state if on]
        before_all_on = bool(before_state) and len(before_on) == len(before_state)
        config_posts.clear()
        page.query_selector('#src-table thead input[data-all="relay"]').click()
        page.wait_for_timeout(3000)
        after = page.eval_on_selector_all(
            "#src-table tbody input[data-relay]:not(:disabled)",
            "els => els.map(e => e.checked)")
        head_state = page.eval_on_selector(
            '#src-table thead input[data-all="relay"]',
            "e => ({checked: e.checked, ind: e.indeterminate})")
        # 点一下必须**翻转**：全选 → 全不选；否则 → 全选。钉死方向会随线上数据假失败
        # （09-25 写这段时线上只有 cdn前置 一个勾选，现在 11 个来源全勾）。
        expect_all = not before_all_on
        checks.append((f"点列头 → 本列{'全不选' if before_all_on else '全选'}",
                       bool(after) and all(v == expect_all for v in after),
                       f"点击前全勾={before_all_on}，点击后 {after.count(True)}/{len(after)} 勾选"))
        checks.append(("列头随后与行状态一致",
                       head_state["checked"] == expect_all and head_state["ind"] is False,
                       str(head_state)))
        checks.append(("批量只发一次保存请求", len(config_posts) == 1,
                       f"{len(config_posts)} 次"))

        # 还原：**整份写回**，不要逐个点击。每次点击都会触发 saveSources →
        # renderSources 重渲染，逐个点既慢又会在时序里丢掉 —— 2026-09-27 实测 11 个
        # 来源一个都没恢复，还把线上 relay 全清成了 false（`fix_relay.py --apply` 救回）。
        page.evaluate("""async (raw) => {
          const r = await api('/api/config', {method: 'POST',
                    body: JSON.stringify({sources: JSON.parse(raw)})}).then(r => r.json());
          if (!r.error){ CONFIG.sources = r.sources || CONFIG.sources; renderSources(); }
          return r;
        }""", before_sources)
        page.wait_for_timeout(2500)
        restored = page.eval_on_selector_all(
            "#src-table tbody input[data-relay]:not(:disabled)",
            "els => els.filter(e => e.checked).map(e => e.dataset.relay)")
        checks.append(("中转列已还原到原状",
                       sorted(restored) == sorted(before_on),
                       f"现在勾着 {restored}，原本 {before_on}"))

        # 「启用」列不再有确认框：点一下就是全选，再点就是全不选（三态循环）。
        # **这里不真点** —— 这一列点一下会把几十个来源真的拉进下一轮测活并各产出一个
        # 导出文件，线上验收不该做这种写入。改为断言两条只读特征：
        #   1) 页面里已经没有确认分支（回归到旧实现会被抓住）
        #   2) 本系统自己的输出集合（`collection/probe`）那一行是 disabled —— 它正是
        #      过去列头永远停在横杠上的原因（全选差这一行，47/48）。
        # 点击行为本身由离线的 verify_thall_cycle.py 覆盖。
        html = page.evaluate("() => document.documentElement.outerHTML")
        checks.append(("「启用」列不再走确认框（点击即生效）",
                       "确认启用" not in html and "onBulkChange" not in html,
                       "页面里仍有确认分支" if "确认启用" in html
                       or "onBulkChange" in html else "干净"))
        locked = page.eval_on_selector_all(
            "#src-table tbody input[data-on]",
            "els => els.filter(e => e.disabled).map(e => e.dataset.on)")
        checks.append(("本系统输出集合那一行的「启用」是 disabled",
                       len(locked) >= 1, str(locked)))
        enabled_state = page.eval_on_selector(
            '#src-table thead input[data-all="enabled"]',
            "e => ({checked: e.checked, ind: e.indeterminate})")
        checks.append(("「启用」列头与可勾行自洽（部分选中就该是横杠）",
                       enabled_state["checked"] != enabled_state["ind"],
                       str(enabled_state)))

        checks.append(("无未捕获 JS 异常", not page_errors, (page_errors or ["干净"])[0][:160]))
        checks.append(("无 console 错误",
                       not [e for e in console_errors if "Failed to load" not in e],
                       str(console_errors[:2]) or "干净"))

        browser.close()

    print("=" * 66)
    print(f"页面: {url.split('?')[0]}")
    print("=" * 66)
    ok = True
    for name, passed, detail in checks:
        ok &= bool(passed)
        print(f"  [{'PASS' if passed else 'FAIL'}] {name}: {detail}")
    print("=" * 66)
    print("结论:", "全部通过 ✓" if ok else "存在失败项 ✗")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
