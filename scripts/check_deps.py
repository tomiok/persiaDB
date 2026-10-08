#!/usr/bin/env python3
"""Enforce the internal crate dependency direction (CLAUDE.md "Architecture").

Every workspace member must appear in ALLOWED, and may only depend on the crates listed
for it. Rules:
  - normal and build dependencies: only the crates in ALLOWED[crate];
  - dev-dependencies: the same, plus `persia-testutil`;
  - `persia-testutil` is never a normal or build dependency.
Adding a crate or an edge means editing ALLOWED on purpose, in the PR that needs it.

Usage: python3 scripts/check_deps.py   (exit 1 and list violations on failure)
Standard library only.
"""

from __future__ import annotations

import json
import subprocess
import sys

TESTUTIL = "persia-testutil"

# Direct internal dependencies each crate may have. Layers may not be skipped:
# e.g. `persia` reaches persia-format only through persia-engine / persia-blob.
ALLOWED: dict[str, frozenset[str]] = {
    "persia-format": frozenset(),
    "persia-storage": frozenset({"persia-format"}),
    "persia-analysis": frozenset(),
    "persia-engine": frozenset({"persia-format", "persia-storage", "persia-analysis"}),
    "persia-blob": frozenset({"persia-format", "persia-storage"}),
    "persia": frozenset({"persia-engine", "persia-blob"}),
    "persia-proto": frozenset(),
    "persia-server": frozenset({"persia", "persia-proto"}),
    "persia-cli": frozenset({"persia"}),
    TESTUTIL: frozenset(),
}


def violations(metadata: dict) -> list[str]:
    members = {p["name"] for p in metadata["packages"]}
    errors = [
        f"{name}: workspace member is not listed in scripts/check_deps.py ALLOWED"
        for name in sorted(members - ALLOWED.keys())
    ]
    for pkg in sorted(metadata["packages"], key=lambda p: p["name"]):
        name = pkg["name"]
        if name not in ALLOWED:
            continue  # already reported above
        allowed = ALLOWED[name]
        for dep in pkg["dependencies"]:
            # `name` is the real package name even when the dependency is renamed.
            target, kind = dep["name"], dep["kind"] or "normal"
            if target not in members:
                # Internal crates outside the workspace (e.g. a future sdk/rust) must be added deliberately.
                if target.startswith("persia"):
                    errors.append(f"{name} -> {target} ({kind}): internal crate outside the workspace")
                continue  # external crates are cargo-deny's job
            if kind == "dev":
                ok = target in allowed or (target == TESTUTIL and name != TESTUTIL)
            else:
                ok = target in allowed
            if not ok:
                errors.append(f"{name} -> {target} ({kind}): not allowed")
    return errors


def main() -> int:
    try:
        raw = subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--no-deps"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as e:
        print(f"cargo metadata failed: {e}", file=sys.stderr)
        print(getattr(e, "stderr", "") or "", file=sys.stderr)
        return 1
    errors = violations(json.loads(raw))
    if errors:
        print("Dependency direction violations (see CLAUDE.md \"Architecture\"):", file=sys.stderr)
        print("\n".join(f"  {e}" for e in errors), file=sys.stderr)
        return 1
    print(f"dependency direction OK ({len(ALLOWED)} crates)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
