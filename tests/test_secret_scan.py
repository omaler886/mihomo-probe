"""Regression coverage for the repository's credential gate."""

import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from tools import scan_secrets


class SecretScanTest(unittest.TestCase):
    """The scanner must inspect new code and never print the value it detects."""

    def test_findings_do_not_retain_secret_values(self):
        """Return a location and rule identifier, never the matched credential."""
        value = "sk-" + "A" * 32
        findings = []
        scan_secrets.scan_text("module.py", f'token = "{value}"', findings)
        self.assertTrue(findings)
        self.assertEqual(findings[0][3], "[REDACTED]")
        self.assertNotIn(value, repr(findings))

    def test_untracked_scan_uses_git_ignore_rules_and_deduplicates(self):
        """Ask Git for tracked and non-ignored untracked paths exactly once."""
        result = subprocess.CompletedProcess([], 0, b"old.py\0new.py\0old.py\0", b"")
        with mock.patch.object(scan_secrets.subprocess, "run", return_value=result) as run:
            paths = scan_secrets._tracked_files(include_untracked=True)
        self.assertEqual(paths, [Path("old.py"), Path("new.py")])
        command = run.call_args.args[0]
        self.assertIn("--others", command)
        self.assertIn("--exclude-standard", command)

    def test_new_files_are_actually_scanned(self):
        """Exercise file reading, not just the Git enumeration command."""
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "new.py"
            path.write_text('token = "sk-' + "A" * 32 + '"', encoding="utf-8")
            findings = []
            with mock.patch.object(scan_secrets, "_tracked_files", return_value=[path]):
                scan_secrets.scan_worktree(findings, include_untracked=True)
            self.assertTrue(findings)
            self.assertEqual(findings[0][0], str(path))

    def test_unreadable_files_cannot_report_a_clean_scan(self):
        """An unavailable tracked file is a gate failure rather than a silent skip."""
        path = Path("unreadable.py")
        findings = []
        with mock.patch.object(scan_secrets, "_tracked_files", return_value=[path]), \
                mock.patch.object(Path, "is_file", return_value=True), \
                mock.patch.object(Path, "read_text", side_effect=PermissionError("denied")):
            scan_secrets.scan_worktree(findings)
        self.assertEqual(findings, [("unreadable.py", 0, "unreadable-file", "PermissionError")])


if __name__ == "__main__":
    unittest.main()
