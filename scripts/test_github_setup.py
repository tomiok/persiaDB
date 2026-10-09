"""Tests for github_setup.py (no network). Run: python3 -m unittest discover -s scripts"""

import unittest

from github_setup import AREAS, TYPES, label_commands, milestone_commands, planned_labels
from progress import ROADMAP, parse_roadmap


class GithubSetupTest(unittest.TestCase):
    def test_labels_cover_roadmap_scheme(self):
        keys = [ms.key for ms in parse_roadmap(ROADMAP.read_text())]
        names = {name for name, _, _ in planned_labels(keys)}
        self.assertIn("milestone:M0", names)
        self.assertIn("milestone:M15", names)
        self.assertTrue({f"area:{a}" for a in AREAS} <= names)
        self.assertTrue({f"type:{t}" for t in TYPES} <= names)
        self.assertIn("type:bug", names)  # used by .github/ISSUE_TEMPLATE/bug-report.yml

    def test_labels_are_idempotent_upserts(self):
        for cmd in label_commands("o/r", planned_labels(["M0"])):
            self.assertEqual(cmd[:3], ["gh", "label", "create"])
            self.assertIn("--force", cmd)
            self.assertEqual(cmd[cmd.index("--repo") + 1], "o/r")

    def test_existing_milestones_are_skipped(self):
        cmds = milestone_commands("o/r", [("M0 — A", "d"), ("M1 — B", "d")], existing={"M0 — A"})
        self.assertEqual(len(cmds), 1)
        self.assertIn("title=M1 — B", cmds[0])


if __name__ == "__main__":
    unittest.main()
