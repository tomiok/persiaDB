"""The MSRV (`rust-version`) must equal the pinned toolchain. Run: python3 -m unittest discover -s scripts"""

import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


class ToolchainTest(unittest.TestCase):
    def test_msrv_equals_pinned_toolchain(self):
        channel = tomllib.loads((ROOT / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
        msrv = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["rust-version"]
        self.assertEqual(msrv, channel, "update rust-version and rust-toolchain.toml together")

    def test_every_crate_inherits_msrv(self):
        for manifest in sorted((ROOT / "crates").glob("*/Cargo.toml")):
            package = tomllib.loads(manifest.read_text())["package"]
            self.assertEqual(package.get("rust-version"), {"workspace": True}, manifest)


if __name__ == "__main__":
    unittest.main()
