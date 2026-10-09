"""The pinned toolchain is exact and equals the MSRV. Run: python3 -m unittest discover -s scripts"""

import re
import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TOOLCHAIN = tomllib.loads((ROOT / "rust-toolchain.toml").read_text())["toolchain"]
WORKSPACE = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]


class ToolchainTest(unittest.TestCase):
    def test_channel_is_an_exact_release(self):
        # "1.99" would float across patch releases and break reproducibility.
        self.assertRegex(TOOLCHAIN["channel"], r"^\d+\.\d+\.\d+$")

    def test_dod_gate_components_installed(self):
        self.assertLessEqual({"rustfmt", "clippy"}, set(TOOLCHAIN.get("components", [])))

    def test_msrv_equals_pinned_toolchain(self):
        msrv = WORKSPACE["package"]["rust-version"]
        self.assertEqual(msrv, TOOLCHAIN["channel"], "update rust-version and rust-toolchain.toml together")

    def test_nightly_is_dated(self):
        # Nightly-only tools (Miri, cargo-fuzz) must be reproducible too (CLAUDE.md "Toolchain").
        nightly = (ROOT / ".github" / "nightly-toolchain").read_text().strip()
        self.assertRegex(nightly, r"^nightly-\d{4}-\d{2}-\d{2}$")

    def test_every_member_inherits_msrv(self):
        manifests = sorted(m for pattern in WORKSPACE["members"] for m in ROOT.glob(f"{pattern}/Cargo.toml"))
        self.assertTrue(manifests)
        for manifest in manifests:
            package = tomllib.loads(manifest.read_text())["package"]
            self.assertEqual(package.get("rust-version"), {"workspace": True}, manifest)


if __name__ == "__main__":
    unittest.main()
