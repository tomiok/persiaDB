"""Checks .gitignore/.gitattributes behave as intended. Run: python3 -m unittest discover -s scripts"""

import subprocess
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def git(*args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True, check=False)


def ignored(path: str) -> bool:
    return git("check-ignore", "--no-index", "-q", path).returncode == 0


def attr(path: str, name: str) -> str:
    # Output: "<path>: <attr>: <value>"
    return git("check-attr", name, "--", path).stdout.rsplit(": ", 1)[-1].strip()


class RepoFilesTest(unittest.TestCase):
    def test_must_commit_paths_are_not_ignored(self):
        for path in [
            "tests/fixtures/v1/basic.persia",
            "tests/fixtures/v1/basic.persia.blobs",
            "crates/persia-format/proptest-regressions/frame.txt",
            "crates/persia-format/src/snapshots/header.snap",
            "fuzz/corpus/frame_scanner/seed1",
            "Cargo.lock",
            ".claude/settings.json",
        ]:
            self.assertFalse(ignored(path), path)

    def test_build_and_scratch_outputs_are_ignored(self):
        for path in [
            "target/debug/persia",
            "fuzz/artifacts/frame_scanner/crash-1",
            "app.persia",
            "app.persia.blobs",
            "scripts/__pycache__/x.pyc",
            "crates/persia-format/src/snapshots/header.snap.new",
            ".claude/settings.local.json",
        ]:
            self.assertTrue(ignored(path), path)

    def test_fixtures_are_binary(self):
        # `binary` = -text -diff -merge: no EOL conversion can corrupt golden bytes.
        for path in ["tests/fixtures/v1/basic.persia", "tests/fixtures/v1/frame.txt", "fuzz/corpus/x/seed", "a.bin"]:
            self.assertEqual(attr(path, "text"), "unset", path)
            self.assertEqual(attr(path, "diff"), "unset", path)

    def test_source_is_lf_text(self):
        self.assertEqual(attr("crates/persia/src/lib.rs", "eol"), "lf")
        self.assertEqual(attr("crates/persia/src/lib.rs", "text"), "auto")


if __name__ == "__main__":
    unittest.main()
