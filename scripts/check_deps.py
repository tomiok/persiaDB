#!/usr/bin/env python3
"""Enforce the dependency rules in CLAUDE.md ("Architecture" and "Dependencies").

Internal crates: every workspace member must appear in ALLOWED and may only depend on the crates
listed for it. Normal and build dependencies use ALLOWED[crate]; dev-dependencies may also use
`persia-testutil`, which is never a normal or build dependency.

External crates: every direct dependency must be declared in the root [workspace.dependencies]
(the allowed list) and be inherited with `.workspace = true`, so versions and features live in one
place. Tiers on top of that:
  - BINARY_ONLY crates: only in BINARIES, or as dev-dependencies (tests may use anyhow);
  - DEV_ONLY crates: only as dev-dependencies.
Licenses, advisories and banned crates in the full graph are cargo-deny's job (deny.toml).
Adding a crate or an edge means editing these tables on purpose, in the PR that needs it.

Usage: python3 scripts/check_deps.py   (exit 1 and list violations on failure)
Standard library only.
"""

from __future__ import annotations

import json
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

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

BINARIES = frozenset({"persia-server", "persia-cli"})
BINARY_ONLY = frozenset({"clap", "anyhow", "rustls", "tracing-subscriber"})
DEV_ONLY = frozenset({"proptest", "criterion", "insta", "tempfile", "testcontainers", "hdrhistogram"})

DEP_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")


def workspace_dependencies() -> frozenset[str]:
    manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())
    return frozenset(manifest["workspace"]["dependencies"])


def external_violation(crate: str, target: str, kind: str, external_allowed: frozenset[str]) -> str | None:
    if target not in external_allowed:
        return "not in [workspace.dependencies] (allowed list)"
    if target in DEV_ONLY and kind != "dev":
        return "dev-only crate used outside [dev-dependencies]"
    if target in BINARY_ONLY and kind != "dev" and crate not in BINARIES:
        return "binary-only crate used by a library"
    return None


def violations(metadata: dict, external_allowed: frozenset[str] = frozenset()) -> list[str]:
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
                if target.startswith("persia"):
                    # Internal crates outside the workspace (e.g. a future sdk/rust) must be added deliberately.
                    problem = "internal crate outside the workspace"
                else:
                    problem = external_violation(name, target, kind, external_allowed)
                if problem:
                    errors.append(f"{name} -> {target} ({kind}): {problem}")
                continue
            if kind == "dev":
                ok = target in allowed or (target == TESTUTIL and name != TESTUTIL)
            else:
                ok = target in allowed
            if not ok:
                errors.append(f"{name} -> {target} ({kind}): not allowed")
    return errors


def build_scripts(metadata: dict) -> list[str]:
    """Build scripts are not allowed (CLAUDE.md "Dependencies"): generated code is committed and checked in CI."""
    return [
        f"{pkg['name']}: has a build script ({target['src_path']}); commit generated code instead"
        for pkg in sorted(metadata["packages"], key=lambda p: p["name"])
        for target in pkg.get("targets", [])
        if "custom-build" in target.get("kind", [])
    ]


def dependency_tables(manifest: dict) -> list[tuple[str, dict]]:
    """All dependency tables of a member manifest, including target-specific ones."""
    tables = [(t, manifest.get(t, {})) for t in DEP_TABLES]
    for cfg, section in manifest.get("target", {}).items():
        tables += [(f"target.'{cfg}'.{t}", section.get(t, {})) for t in DEP_TABLES]
    return tables


def not_inherited(crate: str, manifest: dict) -> list[str]:
    """Dependencies that do not use `.workspace = true` (versions/features must live in the root)."""
    return [
        f"{crate} -> {dep} ([{table}]): must be `{dep}.workspace = true`"
        for table, deps in dependency_tables(manifest)
        for dep, spec in sorted(deps.items())
        if not (isinstance(spec, dict) and spec.get("workspace") is True)
    ]


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
    metadata = json.loads(raw)
    errors = violations(metadata, workspace_dependencies()) + build_scripts(metadata)
    for pkg in metadata["packages"]:
        errors += not_inherited(pkg["name"], tomllib.loads(Path(pkg["manifest_path"]).read_text()))
    if errors:
        print("Dependency rule violations (see CLAUDE.md \"Architecture\" and \"Dependencies\"):", file=sys.stderr)
        print("\n".join(f"  {e}" for e in errors), file=sys.stderr)
        return 1
    print(f"dependency rules OK ({len(ALLOWED)} crates)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
