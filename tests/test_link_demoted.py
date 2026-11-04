"""The demoted `link_substore.py` offers no write entrypoint.

Workstreams/18 B5 demoted the script: the create/refresh call into
`engine.link_substore` and the `--remove` DELETE semantics are gone. What
remains is a help text and the printed `probe-cli migrate scan` command --
no config read, no token, no network request, and no Sub-Store write of any
kind. These tests pin the absence, so the create/remove entrypoints cannot
silently come back (F-058: an unattended agent produces a plan, never a
deletion).
"""
import os
import subprocess
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "link_substore.py"


def run_script(*args):
    """Run the script in isolated mode and read it as UTF-8.

    `-I` keeps a stray repo file from shadowing stdlib imports, and
    `MIHOMO_TEST_ROOT` is stripped so the printed command is deterministic.
    The script itself reconfigures piped stdout to UTF-8 (`-I` ignores the
    environment, so the pin lives in code, not in `PYTHONIOENCODING`);
    `errors="replace"` keeps a codepage oddity from raising instead of
    failing an assertion.
    """
    env = {k: v for k, v in os.environ.items()
           if k not in ("MIHOMO_TEST_ROOT", "PYTHONIOENCODING")}
    return subprocess.run(
        [sys.executable, "-I", str(SCRIPT), *args],
        capture_output=True, text=True, encoding="utf-8", errors="replace",
        timeout=30, env=env,
    )


class DemotedScriptTest(unittest.TestCase):
    """Structural and behavioral pins on the demoted helper."""

    def test_the_source_has_no_create_or_delete_semantics_left(self):
        """No app import, no store client, no HTTP: only help text.

        The history in the module docstring legitimately names what was
        removed; this pin is about the CODE -- the old script's imports
        (`mihomo_test.engine`, `store.Client`), its write call (`upsert`)
        and any network plumbing must all be gone.
        """
        source = SCRIPT.read_text(encoding="utf-8")
        for banned in ("mihomo_test", "engine", "upsert", "store_client",
                       "urllib", "requests", "http.client", "_request"):
            self.assertNotIn(banned, source,
                             f"the demoted script still mentions {banned!r}")

    def test_the_remove_entrypoint_is_gone(self):
        result = run_script("--remove")
        self.assertNotEqual(result.returncode, 0,
                            "--remove must not be a working entrypoint")
        combined = result.stdout + result.stderr
        self.assertNotIn("deleted sub", combined)
        self.assertNotIn("deleted collection", combined)
        self.assertIn("unrecognized arguments", combined,
                      "argparse must refuse --remove outright")

    def test_the_default_run_writes_nothing_and_prints_the_entrypoint(self):
        result = run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--scan", result.stdout, "the one entrypoint is named")

    def test_scan_prints_the_exact_migration_command(self):
        result = run_script("--scan")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("probe-cli", result.stdout)
        self.assertIn("migrate scan", result.stdout)
        # The honest-output contract: the script says it writes nothing.
        self.assertIn("migrate apply", result.stdout,
                      "the dry-run-by-default apply is named too")
        self.assertIn("不", result.stdout, "the no-write notice is present")


if __name__ == "__main__":
    unittest.main()
