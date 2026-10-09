#!/usr/bin/env python3
"""Create or update GitHub labels and milestones from ROADMAP.md (ROADMAP 0.3.4).

Dry run by default: prints the `gh` commands it would run. Pass --apply to run them.
Idempotent: labels use `gh label create --force` (create or update); milestones that already
exist (matched by title) are left alone.

Usage:
  python3 scripts/github_setup.py [--repo OWNER/NAME]            # show the plan
  python3 scripts/github_setup.py [--repo OWNER/NAME] --apply    # do it (needs `gh auth login`)
Standard library only.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys

from progress import ROADMAP, parse_roadmap

DEFAULT_REPO = "tomiok/persiaDB"

AREAS = ["format", "storage", "engine", "analysis", "blob", "server", "sdk", "ci", "docs"]
TYPES = ["feat", "test", "bench", "fuzz", "docs", "chore", "bug"]

# Neutral, distinct hues per label family (identity only; GitHub shows the name next to the color).
COLORS = {"milestone": "1f6feb", "area": "8250df", "type": "bf8700"}


def planned_labels(milestone_keys: list[str]) -> list[tuple[str, str, str]]:
    """(name, color, description) for every label ROADMAP.md asks for."""
    labels = [(f"milestone:{k}", COLORS["milestone"], f"ROADMAP milestone {k}") for k in milestone_keys]
    labels += [(f"area:{a}", COLORS["area"], f"Area: {a}") for a in AREAS]
    labels += [(f"type:{t}", COLORS["type"], f"Type: {t}") for t in TYPES]
    return labels


def label_commands(repo: str, labels: list[tuple[str, str, str]]) -> list[list[str]]:
    return [
        ["gh", "label", "create", name, "--repo", repo, "--color", color, "--description", desc, "--force"]
        for name, color, desc in labels
    ]


def milestone_commands(repo: str, milestones: list[tuple[str, str]], existing: set[str]) -> list[list[str]]:
    return [
        ["gh", "api", f"repos/{repo}/milestones", "-f", f"title={title}", "-f", f"description={desc}"]
        for title, desc in milestones
        if title not in existing
    ]


def existing_milestones(repo: str) -> set[str]:
    out = subprocess.run(
        ["gh", "api", "--paginate", f"repos/{repo}/milestones?state=all", "--jq", ".[].title"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return {line for line in out.splitlines() if line}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--repo", default=DEFAULT_REPO)
    ap.add_argument("--apply", action="store_true", help="run the commands instead of printing them")
    args = ap.parse_args()

    milestones = parse_roadmap(ROADMAP.read_text())
    labels = planned_labels([ms.key for ms in milestones])
    titles = [(f"{ms.key} — {ms.short_title}", f"Deps: {', '.join(ms.deps) or 'none'}. See ROADMAP.md.") for ms in milestones]

    try:
        existing = existing_milestones(args.repo) if args.apply else set()
    except (OSError, subprocess.CalledProcessError) as e:
        print(f"cannot list milestones of {args.repo}: {e}", file=sys.stderr)
        return 1

    new_milestones = milestone_commands(args.repo, titles, existing)
    commands = label_commands(args.repo, labels) + new_milestones
    for cmd in commands:
        if not args.apply:
            print(" ".join(json.dumps(c) if " " in c else c for c in cmd))
            continue
        result = subprocess.run(cmd, capture_output=True, text=True, check=False)
        if result.returncode != 0:
            print(f"failed: {' '.join(cmd)}\n{result.stderr}", file=sys.stderr)
            return 1
    verb = "applied" if args.apply else "planned (dry run; pass --apply)"
    print(f"{len(labels)} labels, {len(new_milestones)} new milestones {verb}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
