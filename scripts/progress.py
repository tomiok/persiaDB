#!/usr/bin/env python3
"""Generate PROGRESS.md from the leaf checkboxes in ROADMAP.md.

ROADMAP.md is the single source of truth: leaves carry `[ ]` todo, `[~]` in progress
or `[x]` done; everything else (milestone status, graph, ready-next list) is derived.
docs/progress-history.csv holds one snapshot per day for the burn-up chart.

Usage:
  python3 scripts/progress.py             # regenerate PROGRESS.md
  python3 scripts/progress.py --record    # also upsert today's snapshot into the history
  python3 scripts/progress.py --check     # CI: fail if PROGRESS.md or the history is stale

Standard library only, so it runs before the cargo workspace exists.
"""

from __future__ import annotations

import argparse
import csv
import datetime
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ROADMAP = ROOT / "ROADMAP.md"
PROGRESS = ROOT / "PROGRESS.md"
HISTORY = ROOT / "docs" / "progress-history.csv"

MILESTONE_RE = re.compile(r"^# (M\d+) — (.+)$")
DEPS_RE = re.compile(r"^Deps:\s*(.*)$")
ITEM_RE = re.compile(r"^(\s*)- (?:\[(.)\] )?(\d+(?:\.\d+)+) (.*)$")
DEP_TOKEN_RE = re.compile(r"M(\d+)\s*[–-]\s*M(\d+)|M(\d+)")

STATUS = {" ": "todo", "~": "doing", "x": "done"}
READY_NEXT_LIMIT = 15

# Milestone states. Icon + word always accompany the color (never color alone).
STATE_LABEL = {
    "done": "✅ done",
    "doing": "🔨 in progress",
    "ready": "▶️ ready",
    "blocked": "⏳ blocked",
}
STATE_STYLE = {
    "done": "fill:#0ca30c,stroke:#0b0b0b,color:#0b0b0b",
    "doing": "fill:#fab219,stroke:#0b0b0b,color:#0b0b0b",
    "ready": "fill:#fcfcfb,stroke:#0b0b0b,stroke-width:2px,color:#0b0b0b",
    "blocked": "fill:#e8e8e6,stroke:#9a9a96,stroke-dasharray:4 3,color:#52514e",
}


@dataclass
class Leaf:
    id: str
    text: str
    status: str
    line: int


@dataclass
class Milestone:
    key: str
    title: str
    deps: list[str] = field(default_factory=list)
    leaves: list[Leaf] = field(default_factory=list)

    def count(self, status: str) -> int:
        return sum(leaf.status == status for leaf in self.leaves)

    @property
    def short_title(self) -> str:
        # "Schema, documents (`persia-engine::schema`)" -> "Schema, documents"
        return self.title.split(" (")[0].replace('"', "'")


class RoadmapError(Exception):
    pass


def parse_deps(raw: str) -> list[str]:
    # Parenthesised remarks ("(Parallel with M2.)", "(M6 for end-to-end)") are soft hints, not deps.
    raw = re.sub(r"\([^)]*\)", "", raw)
    deps: list[str] = []
    for m in DEP_TOKEN_RE.finditer(raw):
        if m.group(3):
            deps.append(f"M{m.group(3)}")
        else:
            deps.extend(f"M{i}" for i in range(int(m.group(1)), int(m.group(2)) + 1))
    return deps


def parse_roadmap(text: str) -> list[Milestone]:
    lines = text.split("\n")
    items = [(n, m) for n, line in enumerate(lines, 1) if (m := ITEM_RE.match(line))]
    ids = [m.group(3) for _, m in items]
    errors: list[str] = []

    dupes = sorted({i for i in ids if ids.count(i) > 1})
    if dupes:
        errors.append(f"duplicate ids: {', '.join(dupes)}")

    item_at = dict(items)
    milestones: list[Milestone] = []
    for n, line in enumerate(lines, 1):
        if m := MILESTONE_RE.match(line):
            milestones.append(Milestone(m.group(1), m.group(2).strip()))
        elif (m := DEPS_RE.match(line)) and milestones:
            milestones[-1].deps = parse_deps(m.group(1))
        elif n in item_at:
            im = item_at[n]
            box, item_id, rest = im.group(2), im.group(3), im.group(4)
            is_leaf = not any(o.startswith(item_id + ".") for o in ids)
            if not milestones or item_id.split(".")[0] != milestones[-1].key[1:]:
                errors.append(f"line {n}: item {item_id} is outside its milestone")
            elif is_leaf and box is None:
                errors.append(f"line {n}: leaf {item_id} has no status checkbox")
            elif is_leaf and box not in STATUS:
                errors.append(f"line {n}: leaf {item_id} has unknown status [{box}]")
            elif not is_leaf and box is not None:
                errors.append(f"line {n}: {item_id} has children, so its status is derived; remove [{box}]")
            elif is_leaf:
                milestones[-1].leaves.append(Leaf(item_id, rest.strip(), STATUS[box], n))

    known = {ms.key for ms in milestones}
    for ms in milestones:
        errors.extend(f"{ms.key}: unknown dependency {d}" for d in ms.deps if d not in known)
    if errors:
        raise RoadmapError("\n".join(errors))
    return milestones


def milestone_state(ms: Milestone, by_key: dict[str, Milestone]) -> str:
    total, done = len(ms.leaves), ms.count("done")
    if total and done == total:
        return "done"
    if done or ms.count("doing"):
        return "doing"
    deps_done = all(milestone_state(by_key[d], by_key) == "done" for d in ms.deps)
    return "ready" if deps_done else "blocked"


def reduced_edges(milestones: list[Milestone]) -> list[tuple[str, str]]:
    """Drop edges implied by others (M15 depends on everything; draw only the direct ones)."""
    by_key = {ms.key: ms for ms in milestones}

    def reachable(start: str, skip_direct: str) -> set[str]:
        seen: set[str] = set()
        stack = [d for d in by_key[start].deps if d != skip_direct]
        while stack:
            k = stack.pop()
            if k not in seen:
                seen.add(k)
                stack.extend(by_key[k].deps)
        return seen

    return [(d, ms.key) for ms in milestones for d in ms.deps if d not in reachable(ms.key, d)]


def bar(done: int, doing: int, total: int, width: int = 20) -> str:
    if not total:
        return "░" * width
    full = round(width * done / total)
    half = min(width - full, round(width * doing / total))
    return "█" * full + "▒" * half + "░" * (width - full - half)


def pct(part: int, total: int) -> str:
    return f"{100 * part / total:.0f}%" if total else "—"


def load_history() -> list[dict[str, str]]:
    if not HISTORY.exists():
        return []
    with HISTORY.open(newline="") as f:
        return list(csv.DictReader(f))


def save_history(rows: list[dict[str, str]]) -> None:
    HISTORY.parent.mkdir(parents=True, exist_ok=True)
    with HISTORY.open("w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=["date", "done", "in_progress", "total"], lineterminator="\n")
        w.writeheader()
        w.writerows(rows)


def totals(milestones: list[Milestone]) -> tuple[int, int, int]:
    leaves = [leaf for ms in milestones for leaf in ms.leaves]
    return (
        sum(leaf.status == "done" for leaf in leaves),
        sum(leaf.status == "doing" for leaf in leaves),
        len(leaves),
    )


def render(milestones: list[Milestone], history: list[dict[str, str]]) -> str:
    by_key = {ms.key: ms for ms in milestones}
    state = {ms.key: milestone_state(ms, by_key) for ms in milestones}
    done, doing, total = totals(milestones)
    last = history[-1]["date"] if history else "never (run with --record)"

    out = [
        "<!-- GENERATED by scripts/progress.py from ROADMAP.md. Do not edit by hand. -->",
        "# Persia DB — Progress",
        "",
        f"**{done}/{total} leaves done ({pct(done, total)})** · {doing} in progress · "
        f"`{bar(done, doing, total, 30)}` · last snapshot: {last}",
        "",
        "Status lives in [`ROADMAP.md`](./ROADMAP.md) checkboxes (`[ ]` todo · `[~]` in progress · `[x]` done).",
        "After changing one, run `python3 scripts/progress.py --record`.",
        "",
        "## Milestone graph",
        "",
        "Arrows point from a dependency to the milestone that needs it (implied edges omitted).",
        " · ".join(STATE_LABEL[s] + (" (deps done)" if s == "ready" else "") for s in STATE_LABEL),
        "",
        "```mermaid",
        "flowchart LR",
    ]
    for ms in milestones:
        d, t = ms.count("done"), len(ms.leaves)
        label = f"{STATE_LABEL[state[ms.key]]}<br/><b>{ms.key}</b> {ms.short_title}<br/>{d}/{t} · {pct(d, t)}"
        out.append(f'  {ms.key}["{label}"]:::{state[ms.key]}')
    out.extend(f"  {a} --> {b}" for a, b in reduced_edges(milestones))
    out.extend(f"  classDef {s} {style}" for s, style in STATE_STYLE.items())
    out += ["```", "", "## Milestones", ""]
    out += [
        "| Milestone | State | Progress | Done | Doing | Leaves | Deps |",
        "|---|---|---|--:|--:|--:|---|",
    ]
    for ms in milestones:
        d, g, t = ms.count("done"), ms.count("doing"), len(ms.leaves)
        out.append(
            f"| **{ms.key}** {ms.short_title} | {STATE_LABEL[state[ms.key]]} | `{bar(d, g, t)}` {pct(d, t)} "
            f"| {d} | {g} | {t} | {', '.join(ms.deps) or '—'} |"
        )

    out += ["", "## In progress", ""]
    doing_leaves = [(ms, leaf) for ms in milestones for leaf in ms.leaves if leaf.status == "doing"]
    out += [f"- `{leaf.id}` {leaf.text} ({ms.key})" for ms, leaf in doing_leaves] or ["_Nothing in progress._"]

    out += ["", f"## Ready next (first {READY_NEXT_LIMIT} todo leaves whose milestone deps are done)", ""]
    ready = [
        (ms, leaf)
        for ms in milestones
        if state[ms.key] in ("ready", "doing")
        for leaf in ms.leaves
        if leaf.status == "todo"
    ]
    out += [f"- `{leaf.id}` {leaf.text} ({ms.key})" for ms, leaf in ready[:READY_NEXT_LIMIT]] or ["_Nothing ready._"]
    if len(ready) > READY_NEXT_LIMIT:
        out.append(f"- … and {len(ready) - READY_NEXT_LIMIT} more")

    out += ["", "## Burn-up", ""]
    if len(history) >= 2:  # a line needs two points; a single snapshot renders as an empty plot
        dates = ", ".join(f'"{r["date"]}"' for r in history)
        out += [
            "Gray line: total leaves (scope). Blue line: leaves done.",
            "",
            "```mermaid",
            "---",
            "config:",
            "  themeVariables:",
            "    xyChart:",
            '      plotColorPalette: "#9a9a96, #2a78d6"',
            "---",
            "xychart-beta",
            '  title "Leaves: scope vs done"',
            f"  x-axis [{dates}]",
            f'  y-axis "Leaves" 0 --> {max(int(r["total"]) for r in history)}',
            f"  line [{', '.join(r['total'] for r in history)}]",
            f"  line [{', '.join(r['done'] for r in history)}]",
            "```",
            "",
        ]
    elif history:
        out += ["_The chart appears once there are two snapshots (one per day with `--record`)._", ""]
    if history:
        out += [
            "| Date | Done | In progress | Total | Done % |",
            "|---|--:|--:|--:|--:|",
        ]
        out += [
            f"| {r['date']} | {r['done']} | {r['in_progress']} | {r['total']} | {pct(int(r['done']), int(r['total']))} |"
            for r in history
        ]
    else:
        out.append("_No snapshots yet. Run `python3 scripts/progress.py --record`._")
    return "\n".join(out) + "\n"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    mode = ap.add_mutually_exclusive_group()
    mode.add_argument("--record", action="store_true", help="upsert today's snapshot into the history")
    mode.add_argument("--check", action="store_true", help="fail if PROGRESS.md or the history is stale")
    ap.add_argument("--date", help="snapshot date for --record (YYYY-MM-DD, default today)")
    args = ap.parse_args()

    try:
        milestones = parse_roadmap(ROADMAP.read_text())
    except RoadmapError as e:
        print(f"ROADMAP.md is malformed:\n{e}", file=sys.stderr)
        return 1

    history = load_history()
    done, doing, total = totals(milestones)

    if args.record:
        date = args.date or datetime.date.today().isoformat()
        history = [r for r in history if r["date"] != date]
        history.append({"date": date, "done": str(done), "in_progress": str(doing), "total": str(total)})
        history.sort(key=lambda r: r["date"])
        save_history(history)

    rendered = render(milestones, history)

    if args.check:
        stale = []
        if not history or (history[-1]["done"], history[-1]["in_progress"], history[-1]["total"]) != (
            str(done), str(doing), str(total)
        ):
            stale.append(f"{HISTORY.relative_to(ROOT)} has no snapshot matching the current ROADMAP")
        if not PROGRESS.exists() or PROGRESS.read_text() != rendered:
            stale.append("PROGRESS.md is out of date")
        if stale:
            print("\n".join(stale) + "\nRun: python3 scripts/progress.py --record", file=sys.stderr)
            return 1
        print(f"progress up to date: {done}/{total} done, {doing} in progress")
        return 0

    PROGRESS.write_text(rendered)
    print(f"PROGRESS.md: {done}/{total} done ({pct(done, total)}), {doing} in progress")
    return 0


if __name__ == "__main__":
    sys.exit(main())
