#!/usr/bin/env python3
"""Scan the repository for real-looking credentials. CI gate; exits 1 on a hit.

Scope by default: every git-tracked file in the working tree. `--history`
additionally walks every commit (`git grep` per rev) -- the full-history sweep
that SECURITY_REVIEW §"凭据与历史" requires before anything is pushed anywhere.

What it looks for (patterns only, never values): PEM private keys, cloud
access keys (AKIA, ASIA, AIza, ghp_/gho_/xox/sk-), JWTs, Telegram bot tokens,
generic `password/secret/token: "…"` assignments with real-looking values, and
UUID-shaped secret paths inside URLs (the Sub-Store backend style, where the
path segment *is* the credential).

Allowlisting: a line is skipped when it carries one of the MARKERS (placeholder
words, test fixtures, the mask itself) -- credentials live in .env / data/ /
environment, never in tracked files, so a hit is a defect, not a warning.

Usage:
    python3 tools/scan_secrets.py             # working tree (tracked files)
    python3 tools/scan_secrets.py --history   # + every commit (slow on big repos)
"""
import argparse
import re
import subprocess
import sys
from pathlib import Path

# (name, compiled pattern, marker-less hit policy). All patterns are shape
# detectors with high specificity; broad ones (password=) require a value that
# does not look like a placeholder or a template.
PATTERNS = [
    ("pem-private-key", re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----")),
    ("aws-access-key", re.compile(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b")),
    ("google-api-key", re.compile(r"\bAIza[0-9A-Za-z_\-]{35}\b")),
    ("github-token", re.compile(r"\bgh[pousr]_[A-Za-z0-9]{36,255}\b")),
    ("slack-token", re.compile(r"\bxox[baprs]-[A-Za-z0-9\-]{10,}\b")),
    ("openai-style-key", re.compile(r"\bsk-[A-Za-z0-9_\-]{20,}\b")),
    ("jwt", re.compile(r"\beyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\b")),
    # Telegram bot tokens: digits ':' alnum(35) -- chat ids and webhook urls
    # in tests never match this shape with the marker filter in place.
    ("telegram-bot-token", re.compile(r"\b[0-9]{8,10}:[A-Za-z0-9_\-]{35}\b")),
    # UUID/secret path inside a URL: the Sub-Store backend embeds its
    # credential as a path segment. Placeholder deployments use words
    # ("backend", "example"), which the marker filter drops.
    ("url-secret-path", re.compile(
        r"https?://[^\s\"'<>]*\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-"
        r"[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b")),
    ("assigned-secret", re.compile(
        r"""(?i)\b(password|secret|api[_-]?key|private[_-]?key)\b\s*[:=]\s*["']?"""
        r"""(?!["']?\s*$)(?!\*)(?!\{\{)(?!\$\{)(?!\bsession\b)(?!\bprocess\b)"""
        r"""[A-Za-z0-9+/=_\-]{16,}["']?""")),
]

# A line matching any of these is considered a placeholder / fixture / comment
# and skipped. Kept explicit: an allowlist you can read is one you can audit.
MARKERS = (
    "example", "placeholder", "demo", "sample", "dummy", "fixture",
    "your-", "yours-", "<token", "<订阅", "xxx", "changeme", "change-me",
    "redacted", "[REDACTED]", "env var", "环境变量", "占位", "示意",
    "***", "MASK", "not a real", "fake", "stub", "mock", "test-only",
    "secret path", "SECRET_MASK", "unGuessable", "unguessable",
)

# Files that document security itself and must be able to quote a pattern.
ALLOWLIST_FILES = ("tools/scan_secrets.py", ".env.example")


def _tracked_files():
    out = subprocess.run(["git", "ls-files", "-z"], capture_output=True)
    if out.returncode != 0:
        sys.exit(f"git ls-files failed: {out.stderr.decode(errors='replace')}")
    return [Path(p) for p in out.stdout.decode().split("\0") if p]


def _clean_line(line):
    return line or ""


def _is_allowlisted(path):
    return str(path).replace("\\", "/") in ALLOWLIST_FILES


def _marker_hit(line):
    low = line.lower()
    return any(marker.lower() in low for marker in MARKERS)


def scan_text(path, text, findings):
    for lineno, raw in enumerate(text.splitlines(), 1):
        line = _clean_line(raw)
        for name, pattern in PATTERNS:
            if pattern.search(line) and not _marker_hit(line):
                findings.append((str(path), lineno, name,
                                 line.strip()[:90]))


def scan_worktree(findings):
    for path in _tracked_files():
        if _is_allowlisted(path) or not path.is_file():
            continue
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError as exc:
            print(f"  skip (unreadable) {path}: {exc}")
            continue
        scan_text(path, text, findings)


def scan_history(findings):
    """Every commit, unpacked via `git archive` and scanned with the same
    Python patterns the worktree scan uses. git grep is not an option here:
    its POSIX regex engine rejects the lookarounds and \b semantics the
    patterns rely on, and the two engines disagreeing would make history
    results incomparable with worktree results.
    """
    import tempfile

    revs = subprocess.run(["git", "rev-list", "--all"], capture_output=True,
                          text=True)
    if revs.returncode != 0:
        sys.exit(f"git rev-list failed: {revs.stderr}")
    revs = revs.stdout.split()
    print(f"  history: {len(revs)} revisions")
    for rev in revs:
        arc = subprocess.run(["git", "archive", rev], capture_output=True)
        if arc.returncode != 0:
            sys.exit(f"git archive {rev} failed: {arc.stderr.decode(errors='replace')}")
        with tempfile.TemporaryDirectory(prefix="secret-history-") as tmp:
            untar = subprocess.run(["tar", "-xf", "-", "-C", tmp],
                                   input=arc.stdout, capture_output=True)
            if untar.returncode != 0:
                sys.exit(f"tar extract {rev} failed: {untar.stderr.decode(errors='replace')}")
            for path in Path(tmp).rglob("*"):
                if not path.is_file():
                    continue
                rel = path.relative_to(tmp)
                if _is_allowlisted(rel):
                    continue
                text = path.read_text(encoding="utf-8", errors="replace")
                scan_text(f"{rev[:10]}:{rel}", text, findings)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--history", action="store_true",
                        help="also scan every commit (git grep per rev)")
    args = parser.parse_args()

    findings = []
    scan_worktree(findings)
    if args.history:
        scan_history(findings)

    if findings:
        print(f"SECRET SCAN: {len(findings)} finding(s)\n")
        for path, lineno, name, excerpt in findings:
            where = f"{path}:{lineno}" if lineno else path
            print(f"  [{name}] {where}")
            print(f"      {excerpt}")
        print("\nRefuse to commit: move the value to .env / data/ (gitignored),"
              " rotate if it was ever pushed.")
        return 1
    print("secret scan: clean (tracked files"
          + (" + full history" if args.history else "") + ")")
    return 0


if __name__ == "__main__":
    sys.exit(main())
