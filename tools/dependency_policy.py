#!/usr/bin/env python3
"""Dependency policy for the locked graph (G20): sources and bans.

cargo-deny would need a pinned binary and a version-specific schema, and the
policy this project actually has is small enough to state plainly:

  * every registry dependency comes from crates.io -- no git, no unknown
    registry, and no path dependency outside the workspace;
  * nothing on the deny list comes back (each entry names a door this project
    deliberately closed);
  * no registry requirement is a wildcard (*), which makes a build
    irreproducible by construction.

Duplicate crate versions and build scripts are reported, not enforced: the
first is normal in a healthy graph, the second is what a build needs.

Usage:
    python tools/dependency_policy.py             # exits 1 on a violation
    python tools/dependency_policy.py --selftest  # proves the rules bite
"""

from __future__ import annotations

import collections
import json
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"

# Crates that would reopen a door this project deliberately closed.
DENY = {
    "openssl-sys": "rustls is the pinned TLS stack",
    "openssl": "rustls is the pinned TLS stack",
    "native-tls": "rustls is the pinned TLS stack",
    "libgit2-sys": "no git dependency is expected in this graph",
    "cloudflare-zlib": "system zlib bindings are not wanted",
}

def _plain(path):
    """Comparable form of a path: no verbatim prefix, forward slashes, folded case."""
    text = str(path).replace("\\?\\", "")
    return text.replace("\\", "/").rstrip("/").lower()


def inside_workspace(path, package_dir):
    """A path dependency resolves against the package that declares it."""
    try:
        resolved = (package_dir / path).resolve()
    except OSError:
        return False
    root = _plain(ROOT.resolve())
    return _plain(resolved).startswith(root + "/")

def evaluate(graph):
    """Return (violations, duplicate versions, build-script crates)."""
    members = set(graph.get("workspace_members") or [])
    member_names = {
        package.get("name")
        for package in graph.get("packages") or []
        if package.get("id") in members
    }
    violations = []
    versions = collections.defaultdict(set)
    build_scripts = []
    for package in graph.get("packages") or []:
        name = package.get("name") or "?"
        version = package.get("version") or "?"
        is_member = package.get("id") in members
        versions[name].add(version)
        if not is_member:
            source = package.get("source")
            if source is None:
                violations.append(name + " " + version + ": path dependency outside the workspace")
            elif source != CRATES_IO:
                violations.append(name + " " + version + ": non-crates.io source " + str(source))
            if name in DENY:
                violations.append(name + " " + version + ": denied (" + DENY[name] + ")")
            kinds = []
            for target in package.get("targets") or []:
                kinds.extend(target.get("kind") or [])
            if "custom-build" in kinds:
                build_scripts.append(name + " " + version)
        manifest = package.get("manifest_path") or "."
        package_dir = pathlib.Path(manifest).parent
        for dependency in package.get("dependencies") or []:
            requirement = (dependency.get("req") or "").strip()
            path = dependency.get("path")
            if path:
                # A workspace path dependency carries req * by construction, and
                # cargo writes it as an absolute path (which makes a string
                # prefix test fragile across encodings). The rule that matters is
                # that the target is one of this workspace's own crates.
                target = dependency.get("name")
                if target not in member_names:
                    violations.append(
                        name + ": dependency " + str(target) + " is a path dependency outside the workspace"
                    )
                continue
            if requirement == "*":
                violations.append(name + ": dependency " + str(dependency.get('name')) + " uses a wildcard requirement")
            if (dependency.get("source") or "").startswith("git+"):
                violations.append(name + ": dependency " + str(dependency.get('name')) + " comes from git")
    duplicates = sorted(
        (name, sorted(found)) for name, found in versions.items() if len(found) > 1
    )
    return violations, duplicates, build_scripts

def live_graph():
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        sys.stderr.write(result.stderr)
        raise SystemExit("cargo metadata failed")
    return json.loads(result.stdout)

def selftest():
    """Feed the evaluator a graph where every rule is violated at least once."""
    member = str(ROOT / "crates" / "probe-api" / "Cargo.toml")
    graph = {
        "workspace_members": ["probe-api", "probe-config"],
        "packages": [
            {
                "id": "probe-config", "name": "probe-config", "version": "0.1.0",
                "source": None, "manifest_path": str(ROOT / "crates" / "probe-config" / "Cargo.toml"),
                "dependencies": [], "targets": [],
            },
            {
                "id": "probe-api", "name": "probe-api", "version": "0.1.0",
                "source": None, "manifest_path": member,
                "dependencies": [
                    {"name": "probe-config", "req": "*", "path": "../probe-config"},
                    {"name": "anything", "req": "*"},
                    {"name": "from-git", "req": "*", "source": "git+https://example.invalid/x"},
                ],
                "targets": [],
            },
            {
                "id": "openssl-sys 0.9.0", "name": "openssl-sys", "version": "0.9.0",
                "source": CRATES_IO, "manifest_path": str(ROOT / "Cargo.toml"),
                "dependencies": [], "targets": [{"kind": ["custom-build"]}],
            },
            {
                "id": "weird 1.0.0", "name": "weird", "version": "1.0.0",
                "source": "git+https://example.invalid/weird",
                "manifest_path": str(ROOT / "Cargo.toml"),
                "dependencies": [], "targets": [],
            },
            {
                "id": "outside 1.0.0", "name": "outside", "version": "1.0.0",
                "source": None, "manifest_path": str(ROOT / "Cargo.toml"),
                "dependencies": [], "targets": [],
            },
        ],
    }
    violations, _duplicates, build_scripts = evaluate(graph)
    expected = {
        "wildcard": any("wildcard requirement" in v for v in violations),
        "git-dependency": any("comes from git" in v for v in violations),
        "denied": any("denied (rustls" in v for v in violations),
        "git-source": any("non-crates.io source" in v for v in violations),
        "outside-path": any("path dependency outside" in v for v in violations),
        "build-script": any("openssl-sys" in b for b in build_scripts),
    }
    missing = [rule for rule, hit in expected.items() if not hit]
    if missing:
        print("selftest FAILED: rules did not fire: " + ", ".join(missing))
        return 1
    print("selftest ok: " + str(len(violations)) + " violation(s) across " + str(len(expected)) + " rules")
    for violation in violations:
        print("  " + violation)
    return 0

def main():
    if "--selftest" in sys.argv:
        return selftest()
    graph = live_graph()
    violations, duplicates, build_scripts = evaluate(graph)
    members = set(graph.get("workspace_members") or [])
    if violations:
        print("dependency policy: " + str(len(violations)) + " violation(s)")
        for violation in violations:
            print("  " + violation)
        return 1
    third_party = len([p for p in graph.get("packages") or [] if p.get("id") not in members])
    print("dependency policy: clean (" + str(third_party) + " third-party crate(s), "
          + str(len(DENY)) + " denied name(s), 0 wildcard registry requirement(s))")
    if duplicates:
        print("  note: " + str(len(duplicates)) + " crate(s) resolve to more than one version (reported):")
        for name, found in duplicates[:8]:
            print("    " + name + ": " + ", ".join(found))
    if build_scripts:
        print("  note: " + str(len(build_scripts)) + " crate(s) run a build script")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
