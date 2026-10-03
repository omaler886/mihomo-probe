//! The JS bootstrap evaluated once per engine context, ported from
//! subs-check-pro's `substore/init.js` (which bridges QuickJS to the Loon
//! host API that `sub-store.min.js` expects), plus the polyfills QuickJS
//! lacks that Loon's JavaScriptCore provides for free.
//!
//! Evaluation order (see `build_init_script`):
//!   1. WHATWG URL/URLSearchParams polyfill (QuickJS has neither; the bundle
//!      uses `new URL(...)` on the script path).
//!   2. atob/btoa, TextEncoder/TextDecoder, crypto.getRandomValues/
//!      randomUUID, setTimeout/clearTimeout (QuickJS core provides none of
//!      them; all shimmed to host bindings or pure JS).
//!   3. The Loon host surface: `$loon`, `$script`, `$request`, `$argument`,
//!      `$persistentStore`, `$notification`, `console`, `$httpClient`,
//!      `$done`, `__run_sub_store_script` — the same contract subs-check-pro
//!      implements, with the bundle source inlined into the last function.

/// WHATWG-flavoured `URL`/`URLSearchParams` for QuickJS, written in place of
/// the npm url-polyfill (which turned out to parse exclusively through DOM
/// anchor elements — unusable without a `document`). Covers what the Sub-Store
/// bundle actually does on the script path: absolute URL parsing (userinfo,
/// IPv6 hosts, ports), relative resolution against a base, dot-segment
/// normalization, origin, and searchParams access.
const TEMPLATE_URL: &str = r##"
(function (global) {
    const SPECIAL_PORTS = { http: 80, https: 443, ws: 80, wss: 443, ftp: 21 };
    function isSpecial(scheme) { return Object.prototype.hasOwnProperty.call(SPECIAL_PORTS, scheme); }

    function pctDecode(s) {
        try { return decodeURIComponent(s); } catch (e) { return s; }
    }
    function pctEncode(s) {
        try { return encodeURIComponent(s); } catch (e) { return s; }
    }

    function removeDotSegments(path) {
        let output = [];
        const segs = path.split("/");
        let trailing = path.endsWith("/") || path.endsWith("/.") || path.endsWith("/..");
        for (let i = 0; i < segs.length; i++) {
            const seg = segs[i];
            if (seg === ".") { continue; }
            if (seg === "..") { if (output.length > 1) output.pop(); continue; }
            if (i === 0 && seg === "") { output.push(seg); continue; }
            output.push(seg);
        }
        let out = output.join("/");
        if (trailing && !out.endsWith("/")) out += "/";
        return out;
    }

    function parseAuthority(rest) {
        // rest starts with "//"; returns {authority, pathAndRest, userinfo, host, port}
        let end = rest.length;
        for (let i = 2; i < rest.length; i++) {
            const c = rest[i];
            if (c === "/" || c === "?" || c === "#") { end = i; break; }
        }
        const authority = rest.slice(2, end);
        const after = rest.slice(end);
        let userinfo = null, hostport = authority;
        const at = authority.lastIndexOf("@");
        if (at >= 0) { userinfo = authority.slice(0, at); hostport = authority.slice(at + 1); }
        let host = hostport, port = null;
        if (hostport.startsWith("[")) {
            const close = hostport.indexOf("]");
            if (close >= 0) {
                host = hostport.slice(0, close + 1);
                const restAfter = hostport.slice(close + 1);
                if (restAfter.startsWith(":")) port = restAfter.slice(1);
            }
        } else {
            const colon = hostport.lastIndexOf(":");
            if (colon >= 0) { host = hostport.slice(0, colon); port = hostport.slice(colon + 1); }
        }
        return { userinfo, host: host.toLowerCase(), port: port === "" ? null : port, after };
    }

    class URLSearchParams {
        constructor(init) {
            this._entries = [];
            if (init) {
                let s = String(init);
                if (s.startsWith("?")) s = s.slice(1);
                if (s !== "") {
                    for (const pair of s.split("&")) {
                        if (pair === "") continue;
                        const eq = pair.indexOf("=");
                        if (eq < 0) this._entries.push([pctDecode(pair.replace(/\+/g, " ")), ""]);
                        else this._entries.push([pctDecode(pair.slice(0, eq).replace(/\+/g, " ")), pctDecode(pair.slice(eq + 1).replace(/\+/g, " "))]);
                    }
                }
            }
        }
        append(k, v) { this._entries.push([String(k), String(v)]); }
        get(k) { for (const [a, b] of this._entries) if (a === String(k)) return b; return null; }
        getAll(k) { return this._entries.filter(e => e[0] === String(k)).map(e => e[1]); }
        has(k) { return this._entries.some(e => e[0] === String(k)); }
        set(k, v) {
            const key = String(k), val = String(v);
            let first = true;
            this._entries = this._entries.filter(e => {
                if (e[0] !== key) return true;
                if (first) { first = false; e[1] = val; return true; }
                return false;
            });
        }
        delete(k) { this._entries = this._entries.filter(e => e[0] !== String(k)); }
        forEach(cb, thisArg) { for (const [k, v] of this._entries) cb.call(thisArg, v, k, this); }
        toString() {
            return this._entries.map(([k, v]) => pctEncode(k) + (v === "" ? "" : "=" + pctEncode(v))).join("&");
        }
        get size() { return this._entries.length; }
    }
    if (typeof Symbol !== "undefined" && Symbol.iterator) {
        URLSearchParams.prototype[Symbol.iterator] = function* () { yield* this._entries; };
    }

    class URL {
        constructor(url, base) {
            url = String(url);
            if (base !== undefined && base !== null && base !== "") base = String(base);
            const absolute = /^[A-Za-z][A-Za-z0-9+.\-]*:/.test(url);
            let scheme, rest;
            if (absolute) {
                const colon = url.indexOf(":");
                scheme = url.slice(0, colon).toLowerCase();
                rest = url.slice(colon + 1);
            } else {
                if (base === undefined || base === null || base === "") {
                    throw new TypeError("Invalid URL: '" + url + "' without base");
                }
                const b = new URL(base);
                if (/^\/\//.test(url)) {          // protocol-relative
                    scheme = b.scheme; rest = url;
                } else if (url.startsWith("/")) { // absolute path
                    scheme = b.scheme;
                    rest = "//" + b._authority + url + (b._search || "") + (b._hash || "");
                    // query/hash from url take precedence:
                    const h = url.indexOf("#");
                    const q = url.indexOf("?");
                    if (h >= 0 || q >= 0) {
                        rest = "//" + b._authority + url;
                    }
                } else if (url.startsWith("?")) { // query only
                    scheme = b.scheme;
                    rest = "//" + b._authority + b.pathname + url;
                } else if (url.startsWith("#")) { // fragment only
                    scheme = b.scheme;
                    rest = "//" + b._authority + b.pathname + (b.search || "") + url;
                } else {                          // relative path
                    scheme = b.scheme;
                    const dir = b.pathname.slice(0, b.pathname.lastIndexOf("/") + 1) || "/";
                    rest = "//" + b._authority + dir + url;
                }
            }

            this._scheme = scheme;
            if (/^\/\//.test(rest)) {
                const parsed = parseAuthority(rest);
                this._authority = parsed.userinfo !== null
                    ? parsed.userinfo + "@" + parsed.host + (parsed.port !== null ? ":" + parsed.port : "")
                    : parsed.host + (parsed.port !== null ? ":" + parsed.port : "");
                this._userinfo = parsed.userinfo;
                this._host = parsed.host;
                this._port = parsed.port;
                const authLen = 2 + this._authority.length;
                let pathAndMore = rest.slice(authLen);
                const hashAt = pathAndMore.indexOf("#");
                if (hashAt >= 0) { this._hash = pathAndMore.slice(hashAt); pathAndMore = pathAndMore.slice(0, hashAt); }
                else this._hash = "";
                const queryAt = pathAndMore.indexOf("?");
                if (queryAt >= 0) { this._search = pathAndMore.slice(queryAt); pathAndMore = pathAndMore.slice(0, queryAt); }
                else this._search = "";
                this._pathname = isSpecial(scheme) && pathAndMore === "" ? "/" : removeDotSegments(pathAndMore);
                this._opaque = false;
            } else {
                // Opaque path (e.g. mailto:x or vless:uuid@host — no //)
                this._authority = null;
                this._userinfo = null; this._host = ""; this._port = null;
                const hashAt = rest.indexOf("#");
                if (hashAt >= 0) { this._hash = rest.slice(hashAt); rest = rest.slice(0, hashAt); }
                else this._hash = "";
                const queryAt = rest.indexOf("?");
                if (queryAt >= 0) { this._search = rest.slice(queryAt); rest = rest.slice(0, queryAt); }
                else this._search = "";
                this._pathname = rest;
                this._opaque = true;
            }
            this._searchParams = new URLSearchParams(this._search);
        }

        get protocol() { return this._scheme + ":"; }
        get scheme() { return this._scheme; }
        get username() { return this._userinfo === null ? "" : pctDecode(this._userinfo.split(":")[0] || ""); }
        get password() {
            if (this._userinfo === null) return "";
            const i = this._userinfo.indexOf(":");
            return i < 0 ? "" : pctDecode(this._userinfo.slice(i + 1));
        }
        get host() { return this._port !== null && this._port !== String(SPECIAL_PORTS[this._scheme]) ? this._host + ":" + this._port : this._host; }
        get hostname() { return this._host; }
        get port() { return this._port !== null && this._port !== String(SPECIAL_PORTS[this._scheme]) ? this._port : ""; }
        get pathname() { return this._pathname; }
        get search() { return this._search; }
        get hash() { return this._hash; }
        get origin() {
            if (isSpecial(this._scheme)) {
                const port = this._port !== null && this._port !== String(SPECIAL_PORTS[this._scheme]) ? ":" + this._port : "";
                return this._scheme + "://" + this._host + port;
            }
            return "null";
        }
        get href() {
            let out = this._scheme + ":";
            if (this._opaque) return out + this._pathname + this._search + this._hash;
            out += "//" + (this._userinfo !== null ? this._userinfo + "@" : "") + this._host;
            if (this._port !== null && this._port !== String(SPECIAL_PORTS[this._scheme])) out += ":" + this._port;
            out += this._pathname + this._search + this._hash;
            return out;
        }
        get searchParams() { return this._searchParams; }
        set href(v) {
            const parsed = new URL(String(v));
            this._scheme = parsed._scheme; this._authority = parsed._authority;
            this._userinfo = parsed._userinfo; this._host = parsed._host; this._port = parsed._port;
            this._pathname = parsed._pathname; this._search = parsed._search; this._hash = parsed._hash;
            this._opaque = parsed._opaque; this._searchParams = new URLSearchParams(this._search);
        }
        toString() { return this.href; }
        toJSON() { return this.href; }
    }

    global.URL = URL;
    global.URLSearchParams = URLSearchParams;
})(globalThis);
"##;

/// The Loon host bridge. Placeholders `__HOST_*__` are replaced once at
/// context creation; everything here must stay in sync with the Rust bindings
/// registered in `engine.rs`.
const TEMPLATE_HEAD: &str = r#"
const $loon = {"deviceName":"iPhone 16 Pro","systemVersion":"18.0","loonVersion":"3.2.1(750)","build":"750","backendName":"mihomo-probe"};
const $script = {"name":"Sub-Store","startTime":Date.now()};
var $request = {};
var $argument = "";

const $persistentStore = {
    read: function (key) {
        let val = __ps_read(key);
        return val === undefined || val === null ? null : val;
    },
    write: function (val, key) {
        if (val === null || val === undefined) {
            __ps_write(null, key);
        } else {
            __ps_write(typeof val === "string" ? val : JSON.stringify(val), key);
        }
        return true;
    }
};

const $notification = {
    post: function (title, subtitle, content) {
        __notify_post(String(title ?? ""), String(subtitle ?? ""), String(content ?? ""));
    }
};

// Fold repeated messages: a runaway warn/error loop crossing the FFI boundary
// once per line is the main QuickJS performance trap (same reason
// subs-check-pro built this exact throttle).
const __log_cache = {};
function __throttled_log(level, ...args) {
    let msg = args.map(String).join(" ");
    let key = level + ":" + msg;
    __log_cache[key] = (__log_cache[key] || 0) + 1;
    if (__log_cache[key] <= 3) {
        __console_log(level, msg);
    } else if (__log_cache[key] === 4) {
        __console_log(level, msg + " (repeated, collapsed)");
    }
}

const console = {
    log: function (...args) { __throttled_log("log", ...args); },
    info: function (...args) { __throttled_log("info", ...args); },
    warn: function (...args) { __throttled_log("warn", ...args); },
    error: function (...args) { __throttled_log("error", ...args); },
    debug: function (...args) { __throttled_log("debug", ...args); },
    trace: function (...args) { __throttled_log("debug", ...args); }
};

const $httpClient = {};
const __http_callbacks = {};
let __http_req_id = 0;

["get", "post", "put", "patch", "delete", "head", "options"].forEach(method => {
    $httpClient[method] = function (options, callback) {
        let req = typeof options === "string" ? { url: options } : options;
        let reqId = "req_" + (++__http_req_id);
        if (callback) { __http_callbacks[reqId] = callback; }
        __go_http_request_async(method.toUpperCase(), JSON.stringify(req), reqId);
    };
});

function __dispatch_http_response(reqId, resMetaJson) {
    let cb = __http_callbacks[reqId];
    if (!cb) return;
    delete __http_callbacks[reqId];
    let res = JSON.parse(resMetaJson);
    let err = res.error !== undefined ? res.error : null;
    let resp = res.response !== undefined ? res.response : null;
    if (resp && res.reqId !== undefined) {
        // The body crosses via a side channel keyed by go-side id: serializing
        // megabyte bodies through the meta JSON would double peak memory.
        resp.body = __go_http_request_body(res.reqId);
    }
    cb(err, resp, resp ? resp.body : null);
}

const $done = function (val) {
    let bodyStr = undefined;
    if (val && typeof val === "object") {
        if (val.response && val.response.body !== undefined) {
            bodyStr = typeof val.response.body === "string" ? val.response.body : JSON.stringify(val.response.body);
            delete val.response.body;
        } else if (val.body !== undefined) {
            bodyStr = typeof val.body === "string" ? val.body : JSON.stringify(val.body);
            delete val.body;
        }
    }
    let metaJson = val ? JSON.stringify(val) : "{}";
    __done(metaJson, bodyStr || "");
    // Resolve the runner promise from JS: the host never re-enters the engine
    // just to unblock a finished request (deviation from subs-check-pro,
    // whose Go binding evals the resolve call from Rust).
    if (globalThis.__resolve_done) { globalThis.__resolve_done(); }
};

function __timer_fire(id) {
    const fn = globalThis.__timers[id];
    delete globalThis.__timers[id];
    if (typeof fn === "function") {
        try { fn(); } catch (e) { console.error("timer", String(e)); }
    }
}
"#;

/// Pure-JS polyfills for globals Loon/JSC provide but QuickJS core does not.
const TEMPLATE_POLYFILLS: &str = r#"
const __B64A = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
function __b64_from_latin1(s) {
    let out = [];
    let i = 0;
    for (; i + 2 < s.length; i += 3) {
        const n = (s.charCodeAt(i) << 16) | (s.charCodeAt(i + 1) << 8) | s.charCodeAt(i + 2);
        out.push(__B64A[(n >> 18) & 63], __B64A[(n >> 12) & 63], __B64A[(n >> 6) & 63], __B64A[n & 63]);
    }
    if (i + 1 === s.length) {
        const n = s.charCodeAt(i) << 16;
        out.push(__B64A[(n >> 18) & 63], __B64A[(n >> 12) & 63], "=", "=");
    } else if (i + 2 === s.length) {
        const n = (s.charCodeAt(i) << 16) | (s.charCodeAt(i + 1) << 8);
        out.push(__B64A[(n >> 18) & 63], __B64A[(n >> 12) & 63], __B64A[(n >> 6) & 63], "=");
    }
    return out.join("");
}
function atob(s) {
    s = String(s).replace(/[^A-Za-z0-9+/=]/g, "");
    let out = "";
    for (let i = 0; i + 3 < s.length; i += 4) {
        const c = [s[i], s[i + 1], s[i + 2], s[i + 3]].map(ch => {
            const v = __B64A.indexOf(ch);
            return v < 0 ? 0 : v;
        });
        const n = (c[0] << 18) | (c[1] << 12) | (c[2] << 6) | c[3];
        out += String.fromCharCode((n >> 16) & 255);
        if (s[i + 2] !== "=") out += String.fromCharCode((n >> 8) & 255);
        if (s[i + 3] !== "=") out += String.fromCharCode(n & 255);
    }
    return out;
}
function btoa(s) { return __b64_from_latin1(String(s)); }

if (typeof globalThis.TextEncoder === "undefined") {
    globalThis.TextEncoder = function TextEncoder() {};
    globalThis.TextEncoder.prototype.encoding = "utf-8";
    globalThis.TextEncoder.prototype.encode = function (s) {
        // encodeURIComponent escapes every non-ASCII byte to %XX in UTF-8,
        // so walking %XX pairs yields exactly the UTF-8 byte sequence.
        const e = encodeURIComponent(String(s));
        const out = new Uint8Array(e.length);
        let j = 0;
        for (let i = 0; i < e.length; i++) {
            if (e[i] === "%") { out[j++] = parseInt(e.substr(i + 1, 2), 16); i += 2; }
            else out[j++] = e.charCodeAt(i);
        }
        return out.subarray(0, j);
    };
}
if (typeof globalThis.TextDecoder === "undefined") {
    globalThis.TextDecoder = function TextDecoder(enc) { this.encoding = String(enc || "utf-8").toLowerCase(); };
    globalThis.TextDecoder.prototype.decode = function (input) {
        let bytes;
        if (input instanceof ArrayBuffer) bytes = new Uint8Array(input);
        else if (input instanceof Uint8Array) bytes = input;
        else if (input && input.buffer instanceof ArrayBuffer)
            bytes = new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
        else bytes = new Uint8Array(0);
        let out = "";
        for (let i = 0; i < bytes.length; i++) {
            const b = bytes[i];
            if (b < 0x80) out += String.fromCharCode(b);
            else if (b < 0xe0) { out += String.fromCharCode(((b & 31) << 6) | (bytes[i + 1] & 63)); i += 1; }
            else if (b < 0xf0) {
                out += String.fromCharCode(((b & 15) << 12) | ((bytes[i + 1] & 63) << 6) | (bytes[i + 2] & 63));
                i += 2;
            } else {
                const cp = ((b & 7) << 18) | ((bytes[i + 1] & 63) << 12) | ((bytes[i + 2] & 63) << 6) | (bytes[i + 3] & 63);
                i += 3;
                const o = cp - 0x10000;
                out += String.fromCharCode(0xd800 + (o >> 10), 0xdc00 + (o & 1023));
            }
        }
        return out;
    };
}

if (typeof globalThis.crypto === "undefined" || typeof globalThis.crypto.randomUUID !== "function") {
    globalThis.crypto = globalThis.crypto || {};
    globalThis.crypto.getRandomValues = function (arr) {
        const b64 = __rand_bytes(arr.length);
        const raw = atob(b64);
        for (let i = 0; i < arr.length; i++) arr[i] = raw.charCodeAt(i) & 0xff;
        return arr;
    };
    globalThis.crypto.randomUUID = function () {
        const raw = atob(__rand_bytes(16));
        const b = [];
        for (let i = 0; i < 16; i++) b.push(raw.charCodeAt(i));
        b[6] = (b[6] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;
        const h = b.map(x => x.toString(16).padStart(2, "0")).join("");
        return h.slice(0, 8) + "-" + h.slice(8, 12) + "-" + h.slice(12, 16) + "-" + h.slice(16, 20) + "-" + h.slice(20);
    };
}

globalThis.__timers = {};
let __timer_seq = 0;
globalThis.setTimeout = function (fn, ms) {
    const id = ++__timer_seq;
    globalThis.__timers[id] = fn;
    __host_timer_set(id, Math.max(0, Number(ms) || 0));
    return id;
};
globalThis.clearTimeout = function (id) { delete globalThis.__timers[id]; };
"#;

/// Runner evaluated per request (fresh `$request`/`$argument`, fresh timer
/// table). Unlike subs-check-pro there is no in-JS `setTimeout` reject guard:
/// the host owns the deadline via the runtime interrupt handler, so a runaway
/// script is actually torn down instead of racing the wall clock.
const RUNNER: &str = r#"
new Promise((resolve, reject) => {
    globalThis.__resolve_done = resolve;
    globalThis.__timers = {};
    // Same in-JS guard subs-check-pro sets: rejects the promise a little
    // before the host's interrupt deadline so a hung bundle surfaces as a
    // script error instead of a wall-clock timeout.
    setTimeout(() => reject(new Error("Sub-Store script timeout inside QuickJS")), 179000);
    try {
        $request = JSON.parse(__req_json);
        $argument = __arg_str;
        __run_sub_store_script();
    } catch (e) {
        try { console.error("runner: " + String(e && e.stack || e)); } catch (_) {}
        reject(e);
    }
});
"#;

pub fn runner_script() -> &'static str {
    RUNNER
}

/// Compose the once-per-context script: polyfills, host bridge, and the
/// Sub-Store bundle inlined into `__run_sub_store_script` (function scope so
/// the bundle's top-level state resets on every call — this is what makes the
/// shared-context execution model safe).
pub fn build_init_script(bundle_src: &str) -> String {
    let mut script = String::with_capacity(bundle_src.len() + TEMPLATE_HEAD.len() + 8192);
    script.push_str(TEMPLATE_URL);
    script.push_str("\n;\n");
    script.push_str(TEMPLATE_POLYFILLS);
    script.push_str("\n;\n");
    script.push_str(TEMPLATE_HEAD);
    script.push_str("\nfunction __run_sub_store_script() {\n");
    script.push_str(bundle_src);
    script.push_str("\n}\n");
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_contains_all_host_bindings() {
        let script = build_init_script("// noop bundle");
        for binding in [
            "__ps_read",
            "__ps_write",
            "__notify_post",
            "__console_log",
            "__go_http_request_async",
            "__go_http_request_body",
            "__host_timer_set",
            "__rand_bytes",
            "__done",
            "__run_sub_store_script",
        ] {
            assert!(script.contains(binding), "template lost binding {binding}");
        }
    }
}
