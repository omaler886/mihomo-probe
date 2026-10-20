#!/usr/bin/env python3
"""Generate a CycloneDX 1.4 SBOM for this workspace from `cargo metadata`.

Why a script instead of a cargo plugin: the SBOM has to be producible on a
machine that has nothing but the pinned toolchain (no `cargo install`, no
network beyond the already-locked crates), and the input -- the locked
dependency graph -- is exactly what `cargo metadata --locked` already prints.
The output is deterministic apart from `metadata.timestamp`, so two runs on
the same lockfile differ only in that field.

Usage:
    python tools/sbom.py                 # writes sbom.cdx.json
    python tools/sbom.py --output x.json # writes x.json
"""

from __future__ import annotations

import argparse
import datetime
import json
import os
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SCHEMA = "http://cyclonedx.org/schema/bom-1.4.schema.json"


def cargo_metadata(workspace: pathlib.Path) -> dict:
    """Run `cargo metadata --locked`; --locked is the point (no silent bumps)."""
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=workspace,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        sys.stderr.write(result.stderr)
        raise SystemExit("cargo metadata failed")
    return json.loads(result.stdout)


def component_for(package: dict) -> dict:
    name = package.get("name") or "unknown"
    version = package.get("version") or "0.0.0"
    component = {
        "type": "library",
        "name": name,
        "version": version,
        "purl": f"pkg:cargo/{name}@{version}",
    }
    # `license` is an SPDX expression; `license_file` is the fallback for
    # crates that ship a file instead. Both land in CycloneDX's two allowed
    # shapes (expression vs named license).
    expression = package.get("license")
    if expression:
        component["licenses"] = [{"expression": expression}]
    else:
        files = package.get("license_file")
        if files:
            component["licenses"] = [{"license": {"name": files}}]
        else:
            component["licenses"] = [{"license": {"name": "UNKNOWN"}}]
    return component


def build(workspace: pathlib.Path) -> dict:
    metadata = cargo_metadata(workspace)
    members = set(metadata.get("workspace_members") or [])
    third_party = [
        package
        for package in metadata.get("packages") or []
        if package.get("id") not in members
    ]
    # Deterministic order: name, then version, so a diff of two SBOMs shows
    # only real dependency changes.
    components = sorted(
        (component_for(package) for package in third_party),
        key=lambda component: (component["name"], component["version"]),
    )
    members_meta = [
        package for package in metadata.get("packages") or [] if package.get("id") in members
    ]
    root = members_meta[0] if members_meta else {}
    epoch = os.environ.get("SOURCE_DATE_EPOCH")
    if epoch:
        stamp = datetime.datetime.fromtimestamp(int(epoch), datetime.timezone.utc)
    else:
        stamp = datetime.datetime.now(datetime.timezone.utc)
    return {
        "bomFormat": "CycloneDX",
        "specVersion": "1.4",
        "$schema": SCHEMA,
        "version": 1,
        "metadata": {
            "timestamp": stamp.strftime("%Y-%m-%dT%H:%M:%SZ"),
            "tools": [
                {
                    "vendor": "mihomo-probe",
                    "name": "tools/sbom.py",
                    "version": "1",
                }
            ],
            "component": {
                "type": "application",
                "name": root.get("name") or "mihomo-probe",
                "version": root.get("version") or "0.0.0",
                "properties": [
                    {
                        "name": "workspace:members",
                        "value": ",".join(sorted(p.get("name", "?") for p in members_meta)),
                    }
                ],
            },
        },
        "components": components,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description="Generate a CycloneDX SBOM")
    parser.add_argument("--output", default="sbom.cdx.json", help="output path")
    parser.add_argument("--workspace", default=str(ROOT), help="workspace root")
    args = parser.parse_args()
    document = build(pathlib.Path(args.workspace))
    target = pathlib.Path(args.output)
    if not target.is_absolute():
        target = pathlib.Path(args.workspace) / target
    target.write_text(json.dumps(document, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"sbom: {len(document['components'])} component(s) -> {target}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())