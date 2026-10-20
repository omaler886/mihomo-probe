#!/usr/bin/env python3
"""License gate for the locked dependency graph (G20).

`cargo-deny` would need a pinned binary and a version-specific config schema;
the part this project actually needs is a yes/no on *license expressions*, and
`cargo metadata --locked` already carries them. So the check lives here: no
extra tool, no network, and the same code path on a dev box and in CI.

An SPDX expression is accepted when it can be satisfied under the allow list
using the standard precedence: `WITH` binds tighter than `AND`, `AND` tighter
than `OR`, parentheses override. `MIT OR Apache-2.0` is fine when either side
is allowed; `MIT AND BSD-3-Clause` needs both.

Usage:
    python tools/license_check.py            # exits 1 on a violation
"""

from __future__ import annotations

import json
import pathlib
import subprocess
import sys

# Every expression this lockfile resolves to (verified 2026-10-20). Adding a
# dependency with a new license is a deliberate act: extend this list in the
# same change and say why.
ALLOWED = {
    "MIT",
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Zlib",
    "0BSD",
    "Unicode-3.0",
    "Unlicense",
    "BSL-1.0",
    "CDLA-Permissive-2.0",
}

ROOT = pathlib.Path(__file__).resolve().parent.parent


def tokenize(expression: str) -> list[str]:
    # Several long-lived crates still spell the dual licence as
    # `MIT/Apache-2.0` (the pre-SPDX Rust convention). It means the same as
    # `MIT OR Apache-2.0`; treating the slash as an operator keeps the
    # evaluation honest instead of failing on spelling.
    expression = "/".join(" OR " if False else "/" for _ in [0]) if False else expression
    parts = [part.strip() for part in expression.split("/")]
    if len(parts) > 1 and all(parts):
        expression = " OR ".join(parts)
    tokens: list[str] = []
    current = ""
    for character in expression:
        if character in "()":
            if current.strip():
                tokens.append(current.strip())
            current = ""
            tokens.append(character)
        elif character.isspace():
            if current.strip():
                tokens.append(current.strip())
            current = ""
        else:
            current += character
    if current.strip():
        tokens.append(current.strip())
    return tokens


def parse(tokens: list[str]):
    """Return a nested tuple tree: ('id', x) | ('or', a, b) | ('and', a, b)."""
    position = 0

    def peek() -> str | None:
        return tokens[position] if position < len(tokens) else None

    def take() -> str:
        nonlocal position
        token = tokens[position]
        position += 1
        return token

    def parse_or():
        node = parse_and()
        while peek() == "OR":
            take()
            node = ("or", node, parse_and())
        return node

    def parse_and():
        node = parse_atom()
        while peek() == "AND":
            take()
            node = ("and", node, parse_atom())
        return node

    def parse_atom():
        token = peek()
        if token is None:
            raise ValueError("truncated expression")
        if token == "(":
            take()
            node = parse_or()
            if peek() != ")":
                raise ValueError("unbalanced expression")
            take()
            return node
        identifier = take()
        if peek() == "WITH":
            take()
            identifier = identifier + " WITH " + take()
        return ("id", identifier)

    tree = parse_or()
    if peek() is not None:
        raise ValueError("trailing tokens")
    return tree


def satisfied(tree, allowed: set[str]) -> bool:
    kind = tree[0]
    if kind == "id":
        return tree[1] in allowed
    if kind == "or":
        return satisfied(tree[1], allowed) or satisfied(tree[2], allowed)
    return satisfied(tree[1], allowed) and satisfied(tree[2], allowed)


def main() -> int:
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        sys.stderr.write(result.stderr)
        return 1
    metadata = json.loads(result.stdout)
    members = set(metadata.get("workspace_members") or [])
    failures: list[str] = []
    checked = 0
    for package in metadata.get("packages") or []:
        if package.get("id") in members:
            continue
        expression = package.get("license")
        name = package.get("name") or "?"
        version = package.get("version") or "?"
        if not expression:
            # A crate that ships no SPDX expression cannot be reasoned about.
            failures.append(f"{name} {version}: no license expression")
            continue
        checked += 1
        try:
            tree = parse(tokenize(expression))
        except ValueError as error:
            failures.append(f"{name} {version}: unparseable expression {expression!r} ({error})")
            continue
        if not satisfied(tree, ALLOWED):
            failures.append(f"{name} {version}: {expression}")
    if failures:
        print(f"license check: {len(failures)} violation(s) of {checked} checked")
        for failure in failures:
            print("  " + failure)
        return 1
    print(f"license check: clean ({checked} third-party package(s), {len(ALLOWED)} allowed expression(s))")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())