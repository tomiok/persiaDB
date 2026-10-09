"""Tests for check_deps.py. Run: python3 -m unittest discover -s scripts"""

import unittest

from check_deps import ALLOWED, TESTUTIL, violations, workspace_dependencies


def metadata(edges=(), extra_members=()):
    """Workspace with every ALLOWED crate; `edges` = (from, to, kind) with kind None|"dev"|"build"."""
    names = list(ALLOWED) + list(extra_members)
    deps = {n: [] for n in names}
    for src, dst, kind in edges:
        deps[src].append({"name": dst, "kind": kind})
    return {"packages": [{"name": n, "dependencies": deps[n]} for n in names]}


class CheckDepsTest(unittest.TestCase):
    def test_every_allowed_edge_passes(self):
        edges = [(src, dst, None) for src, dsts in ALLOWED.items() for dst in dsts]
        self.assertEqual(violations(metadata(edges)), [])

    def test_upward_edge_rejected(self):
        self.assertEqual(
            violations(metadata([("persia-format", "persia-engine", None)])),
            ["persia-format -> persia-engine (normal): not allowed"],
        )

    def test_layer_skip_rejected(self):
        self.assertEqual(
            violations(metadata([("persia", "persia-format", None)])),
            ["persia -> persia-format (normal): not allowed"],
        )

    def test_engine_must_not_depend_on_blob(self):
        self.assertTrue(violations(metadata([("persia-engine", "persia-blob", None)])))

    def test_build_dependency_follows_normal_rules(self):
        self.assertEqual(
            violations(metadata([("persia-storage", "persia-engine", "build")])),
            ["persia-storage -> persia-engine (build): not allowed"],
        )

    def test_testutil_allowed_as_dev_dependency(self):
        self.assertEqual(violations(metadata([("persia-engine", TESTUTIL, "dev")])), [])

    def test_testutil_rejected_as_normal_dependency(self):
        self.assertEqual(
            violations(metadata([("persia-engine", TESTUTIL, None)])),
            [f"persia-engine -> {TESTUTIL} (normal): not allowed"],
        )

    def test_dev_dependency_cannot_go_upward(self):
        self.assertTrue(violations(metadata([("persia-format", "persia-engine", "dev")])))

    def test_unlisted_member_rejected(self):
        self.assertEqual(
            violations(metadata(extra_members=["persia-new"])),
            ["persia-new: workspace member is not listed in scripts/check_deps.py ALLOWED"],
        )

    def test_unlisted_member_reported_once(self):
        md = metadata(extra_members=["persia-new"])
        md["packages"][-1]["dependencies"].append({"name": "persia-server", "kind": None})
        self.assertEqual(len(violations(md)), 1)

    def test_renamed_dependency_checked_by_package_name(self):
        md = metadata()
        fmt = next(p for p in md["packages"] if p["name"] == "persia-format")
        fmt["dependencies"].append({"name": "persia-engine", "rename": "eng", "kind": None})
        self.assertEqual(violations(md), ["persia-format -> persia-engine (normal): not allowed"])

    def test_target_specific_dependency_checked(self):
        md = metadata()
        fmt = next(p for p in md["packages"] if p["name"] == "persia-format")
        fmt["dependencies"].append({"name": "persia-engine", "kind": None, "target": "cfg(unix)"})
        self.assertTrue(violations(md))

    def test_self_dev_dependency_rejected(self):
        self.assertTrue(violations(metadata([("persia-engine", "persia-engine", "dev")])))
        self.assertTrue(violations(metadata([(TESTUTIL, TESTUTIL, "dev")])))

    def test_internal_crate_outside_workspace_rejected(self):
        self.assertEqual(
            violations(metadata([("persia-server", "persia-client", None)])),
            ["persia-server -> persia-client (normal): internal crate outside the workspace"],
        )

    def test_allowed_external_dependency_passes(self):
        md = metadata([("persia-format", "zstd", None)])
        self.assertEqual(violations(md, frozenset({"zstd"})), [])

    def test_external_dependency_not_in_workspace_table_rejected(self):
        md = metadata([("persia-format", "left-pad", "dev")])
        self.assertEqual(
            violations(md, frozenset({"zstd"})),
            ["persia-format -> left-pad (dev): not in [workspace.dependencies] (allowed list)"],
        )

    def test_real_workspace_table_is_readable(self):
        self.assertIn("zstd", workspace_dependencies())


if __name__ == "__main__":
    unittest.main()
