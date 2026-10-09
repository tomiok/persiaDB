//! `cargo xtask progress`: generate `PROGRESS.md` from the leaf checkboxes in `ROADMAP.md`.
//!
//! ROADMAP.md is the single source of truth: leaves carry `[ ]` todo, `[~]` in progress or `[x]` done;
//! everything else (milestone state, graph, ready-next list) is derived. `docs/progress-history.csv` holds
//! one snapshot per day for the burn-up chart.
//!
//! `--record` upserts today's snapshot (UTC date, or `--date YYYY-MM-DD`); `--check` fails if `PROGRESS.md`
//! or the latest snapshot is stale (CI).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

const READY_NEXT_LIMIT: usize = 15;
const COMMAND: &str = "cargo xtask progress";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Todo,
    Doing,
    Done,
}

impl Status {
    fn from_box(c: char) -> Option<Self> {
        match c {
            ' ' => Some(Self::Todo),
            '~' => Some(Self::Doing),
            'x' => Some(Self::Done),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Leaf {
    pub(crate) id: String,
    text: String,
    status: Status,
}

#[derive(Debug)]
pub(crate) struct Milestone {
    pub(crate) key: String,
    title: String,
    pub(crate) deps: Vec<String>,
    pub(crate) leaves: Vec<Leaf>,
}

impl Milestone {
    fn count(&self, status: Status) -> usize {
        self.leaves.iter().filter(|l| l.status == status).count()
    }

    /// "Schema, documents (`persia-engine::schema`)" -> "Schema, documents"
    pub(crate) fn short_title(&self) -> String {
        self.title
            .split(" (")
            .next()
            .unwrap_or_default()
            .replace('"', "'")
    }
}

/// Milestone states. Icon + word always accompany the color (never color alone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Done,
    Doing,
    Ready,
    Blocked,
}

impl State {
    const ALL: [Self; 4] = [Self::Done, Self::Doing, Self::Ready, Self::Blocked];

    fn name(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Doing => "doing",
            Self::Ready => "ready",
            Self::Blocked => "blocked",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Done => "✅ done",
            Self::Doing => "🔨 in progress",
            Self::Ready => "▶️ ready",
            Self::Blocked => "⏳ blocked",
        }
    }

    fn style(self) -> &'static str {
        match self {
            Self::Done => "fill:#0ca30c,stroke:#0b0b0b,color:#0b0b0b",
            Self::Doing => "fill:#fab219,stroke:#0b0b0b,color:#0b0b0b",
            Self::Ready => "fill:#fcfcfb,stroke:#0b0b0b,stroke-width:2px,color:#0b0b0b",
            Self::Blocked => "fill:#e8e8e6,stroke:#9a9a96,stroke-dasharray:4 3,color:#52514e",
        }
    }
}

/// `# M5 — Segment writer & reader` -> ("M5", "Segment writer & reader").
fn parse_milestone(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("# M")?;
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let title = rest.get(digits..)?.strip_prefix(" — ")?;
    (digits > 0 && !title.is_empty())
        .then(|| (line.get(2..3 + digits).unwrap_or_default(), title.trim()))
}

/// `- [x] 1.2.3 text` / `  - 1.2 text` -> (checkbox char, id, text). Ids need at least one dot.
fn parse_item(line: &str) -> Option<(Option<char>, &str, &str)> {
    let rest = line.trim_start().strip_prefix("- ")?;
    let (checkbox, rest) = match rest.strip_prefix('[') {
        Some(after) => {
            let mut chars = after.chars();
            let c = chars.next()?;
            let tail = chars.as_str().strip_prefix("] ")?;
            (Some(c), tail)
        }
        None => (None, rest),
    };
    let (id, text) = rest.split_once(' ')?;
    let parts: Vec<&str> = id.split('.').collect();
    let valid = parts.len() >= 2
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    valid.then_some((checkbox, id, text))
}

/// `M1, M3 (Parallel with M2.)`, `M1–M14` -> milestone keys. Parenthesised remarks are hints, not deps.
pub(crate) fn parse_deps(raw: &str) -> Vec<String> {
    // Drop "(...)" remarks; an unclosed "(" keeps the rest as is.
    let mut text = String::new();
    let mut rest = raw;
    while let Some((before, after)) = rest.split_once('(') {
        let Some((_, after_close)) = after.split_once(')') else {
            break;
        };
        text.push_str(before);
        rest = after_close;
    }
    text.push_str(rest);

    let mut deps = Vec::new();
    let mut s = text.as_str();
    while let Some((_, after_m)) = s.split_once('M') {
        let (digits, tail) = split_digits(after_m);
        s = tail;
        let Ok(from) = digits.parse::<u32>() else {
            continue;
        };
        // Optional range: `\s*[–-]\s*M\d+`
        let range_end = tail
            .trim_start()
            .strip_prefix(['–', '-'])
            .and_then(|r| r.trim_start().strip_prefix('M'))
            .map(split_digits)
            .and_then(|(to, after)| to.parse::<u32>().ok().map(|to| (to, after)));
        if let Some((to, after)) = range_end {
            deps.extend((from..=to).map(|i| format!("M{i}")));
            s = after;
        } else {
            deps.push(format!("M{from}"));
        }
    }
    deps
}

/// Splits a leading run of ASCII digits off `s`.
fn split_digits(s: &str) -> (&str, &str) {
    let n = s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    s.split_at_checked(n).unwrap_or((s, ""))
}

/// Parses ROADMAP.md, reporting every structural problem at once.
pub(crate) fn parse_roadmap(text: &str) -> Result<Vec<Milestone>> {
    let lines: Vec<&str> = text.split('\n').collect();
    let items: BTreeMap<usize, (Option<char>, &str, &str)> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| parse_item(l).map(|item| (i + 1, item)))
        .collect();
    let ids: Vec<&str> = items.values().map(|(_, id, _)| *id).collect();
    let mut errors = Vec::new();

    let mut seen = BTreeSet::new();
    let dupes: BTreeSet<&str> = ids.iter().copied().filter(|id| !seen.insert(*id)).collect();
    if !dupes.is_empty() {
        errors.push(format!(
            "duplicate ids: {}",
            dupes.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }

    let mut milestones: Vec<Milestone> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let n = i + 1;
        if let Some((key, title)) = parse_milestone(line) {
            milestones.push(Milestone {
                key: key.to_owned(),
                title: title.to_owned(),
                deps: Vec::new(),
                leaves: Vec::new(),
            });
        } else if let (Some(raw), Some(ms)) = (line.strip_prefix("Deps:"), milestones.last_mut()) {
            ms.deps = parse_deps(raw.trim_start());
        } else if let Some(&(checkbox, id, rest)) = items.get(&n) {
            let prefix = format!("{id}.");
            let is_leaf = !ids.iter().any(|o| o.starts_with(&prefix));
            let in_milestone = milestones
                .last()
                .is_some_and(|ms| id.split('.').next() == ms.key.get(1..));
            let status = checkbox.and_then(Status::from_box);
            match (in_milestone, is_leaf, checkbox) {
                (false, _, _) => {
                    errors.push(format!("line {n}: item {id} is outside its milestone"));
                }
                (true, true, None) => {
                    errors.push(format!("line {n}: leaf {id} has no status checkbox"));
                }
                (true, true, Some(c)) if status.is_none() => {
                    errors.push(format!("line {n}: leaf {id} has unknown status [{c}]"));
                }
                (true, false, Some(c)) => {
                    errors.push(format!(
                        "line {n}: {id} has children, so its status is derived; remove [{c}]"
                    ));
                }
                (true, true, Some(_)) => {
                    if let (Some(ms), Some(status)) = (milestones.last_mut(), status) {
                        ms.leaves.push(Leaf {
                            id: id.to_owned(),
                            text: rest.trim().to_owned(),
                            status,
                        });
                    }
                }
                (true, false, None) => {}
            }
        }
    }

    let known: BTreeSet<&str> = milestones.iter().map(|ms| ms.key.as_str()).collect();
    for ms in &milestones {
        errors.extend(
            ms.deps
                .iter()
                .filter(|d| !known.contains(d.as_str()))
                .map(|d| format!("{}: unknown dependency {d}", ms.key)),
        );
    }
    if !errors.is_empty() {
        bail!("ROADMAP.md is malformed:\n{}", errors.join("\n"));
    }
    Ok(milestones)
}

fn milestone_state(ms: &Milestone, by_key: &BTreeMap<&str, &Milestone>, depth: usize) -> State {
    let (total, done) = (ms.leaves.len(), ms.count(Status::Done));
    if total > 0 && done == total {
        return State::Done;
    }
    if done > 0 || ms.count(Status::Doing) > 0 {
        return State::Doing;
    }
    // A dependency cycle cannot be "done"; the depth bound keeps it from recursing forever.
    let deps_done = depth <= by_key.len()
        && ms.deps.iter().all(|d| {
            by_key
                .get(d.as_str())
                .is_some_and(|dep| milestone_state(dep, by_key, depth + 1) == State::Done)
        });
    if deps_done {
        State::Ready
    } else {
        State::Blocked
    }
}

/// Drop edges implied by others (M15 depends on everything; draw only the direct ones).
fn reduced_edges(milestones: &[Milestone]) -> Vec<(String, String)> {
    let by_key: BTreeMap<&str, &Milestone> =
        milestones.iter().map(|ms| (ms.key.as_str(), ms)).collect();
    let mut edges = Vec::new();
    for ms in milestones {
        for dep in &ms.deps {
            if !reachable_without(&by_key, ms, dep).contains(dep.as_str()) {
                edges.push((dep.clone(), ms.key.clone()));
            }
        }
    }
    edges
}

/// Milestones reachable from `start` through its deps, ignoring the direct edge to `skip_direct`.
fn reachable_without<'a>(
    by_key: &BTreeMap<&str, &'a Milestone>,
    start: &'a Milestone,
    skip_direct: &str,
) -> BTreeSet<&'a str> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<&'a str> = start
        .deps
        .iter()
        .map(String::as_str)
        .filter(|d| *d != skip_direct)
        .collect();
    while let Some(k) = stack.pop() {
        if seen.insert(k)
            && let Some(ms) = by_key.get(k)
        {
            stack.extend(ms.deps.iter().map(String::as_str));
        }
    }
    seen
}

/// `round(num / den)` with ties to even, exactly like Python's `round()` and `:.0f`.
fn div_round_half_even(num: usize, den: usize) -> usize {
    let (q, r) = (num / den, num % den);
    match (2 * r).cmp(&den) {
        std::cmp::Ordering::Greater => q + 1,
        std::cmp::Ordering::Equal => q + (q % 2),
        std::cmp::Ordering::Less => q,
    }
}

fn bar(done: usize, doing: usize, total: usize, width: usize) -> String {
    if total == 0 {
        return "░".repeat(width);
    }
    let full = div_round_half_even(width * done, total);
    let half = (width - full).min(div_round_half_even(width * doing, total));
    format!(
        "{}{}{}",
        "█".repeat(full),
        "▒".repeat(half),
        "░".repeat(width - full - half)
    )
}

fn pct(part: usize, total: usize) -> String {
    if total == 0 {
        "—".to_owned()
    } else {
        format!("{}%", div_round_half_even(100 * part, total))
    }
}

/// One row of `docs/progress-history.csv`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Snapshot {
    date: String,
    done: usize,
    in_progress: usize,
    total: usize,
}

const HISTORY_HEADER: &str = "date,done,in_progress,total";

fn parse_history(text: &str) -> Result<Vec<Snapshot>> {
    let mut lines = text.lines();
    match lines.next() {
        None => return Ok(Vec::new()),
        Some(HISTORY_HEADER) => {}
        Some(other) => bail!("unexpected history header: {other}"),
    }
    lines
        .filter(|l| !l.is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            let [date, done, in_progress, total] = f.as_slice() else {
                bail!("bad history row: {l}")
            };
            Ok(Snapshot {
                date: (*date).to_owned(),
                done: done.parse()?,
                in_progress: in_progress.parse()?,
                total: total.parse()?,
            })
        })
        .collect()
}

fn format_history(rows: &[Snapshot]) -> String {
    let mut out = format!("{HISTORY_HEADER}\n");
    for r in rows {
        let _ = writeln!(out, "{},{},{},{}", r.date, r.done, r.in_progress, r.total);
    }
    out
}

fn totals(milestones: &[Milestone]) -> (usize, usize, usize) {
    let leaves = milestones.iter().flat_map(|ms| &ms.leaves);
    let (mut done, mut doing, mut total) = (0, 0, 0);
    for leaf in leaves {
        total += 1;
        match leaf.status {
            Status::Done => done += 1,
            Status::Doing => doing += 1,
            Status::Todo => {}
        }
    }
    (done, doing, total)
}

#[allow(clippy::too_many_lines)] // one linear template, easier to read in one piece
pub(crate) fn render(milestones: &[Milestone], history: &[Snapshot]) -> String {
    let by_key: BTreeMap<&str, &Milestone> =
        milestones.iter().map(|ms| (ms.key.as_str(), ms)).collect();
    let state: BTreeMap<&str, State> = milestones
        .iter()
        .map(|ms| (ms.key.as_str(), milestone_state(ms, &by_key, 0)))
        .collect();
    let state_of = |ms: &Milestone| {
        state
            .get(ms.key.as_str())
            .copied()
            .unwrap_or(State::Blocked)
    };
    let (done, doing, total) = totals(milestones);
    let last = history
        .last()
        .map_or("never (run with --record)", |r| r.date.as_str());

    let mut out: Vec<String> = vec![
        format!("<!-- GENERATED by `{COMMAND}` from ROADMAP.md. Do not edit by hand. -->"),
        "# Persia DB — Progress".into(),
        String::new(),
        format!(
            "**{done}/{total} leaves done ({})** · {doing} in progress · `{}` · last snapshot: {last}",
            pct(done, total),
            bar(done, doing, total, 30)
        ),
        String::new(),
        "Status lives in [`ROADMAP.md`](./ROADMAP.md) checkboxes (`[ ]` todo · `[~]` in progress · `[x]` done).".into(),
        format!("After changing one, run `{COMMAND} --record`."),
        String::new(),
        "## Milestone graph".into(),
        String::new(),
        "Arrows point from a dependency to the milestone that needs it (implied edges omitted).".into(),
        State::ALL
            .iter()
            .map(|s| format!("{}{}", s.label(), if *s == State::Ready { " (deps done)" } else { "" }))
            .collect::<Vec<_>>()
            .join(" · "),
        String::new(),
        "```mermaid".into(),
        "flowchart LR".into(),
    ];
    for ms in milestones {
        let (d, t, s) = (ms.count(Status::Done), ms.leaves.len(), state_of(ms));
        out.push(format!(
            "  {}[\"{}<br/><b>{}</b> {}<br/>{d}/{t} · {}\"]:::{}",
            ms.key,
            s.label(),
            ms.key,
            ms.short_title(),
            pct(d, t),
            s.name()
        ));
    }
    out.extend(
        reduced_edges(milestones)
            .into_iter()
            .map(|(a, b)| format!("  {a} --> {b}")),
    );
    out.extend(
        State::ALL
            .iter()
            .map(|s| format!("  classDef {} {}", s.name(), s.style())),
    );
    out.extend(["```", "", "## Milestones", ""].map(String::from));
    out.push("| Milestone | State | Progress | Done | Doing | Leaves | Deps |".into());
    out.push("|---|---|---|--:|--:|--:|---|".into());
    for ms in milestones {
        let (d, g, t) = (
            ms.count(Status::Done),
            ms.count(Status::Doing),
            ms.leaves.len(),
        );
        let deps = if ms.deps.is_empty() {
            "—".to_owned()
        } else {
            ms.deps.join(", ")
        };
        out.push(format!(
            "| **{}** {} | {} | `{}` {} | {d} | {g} | {t} | {deps} |",
            ms.key,
            ms.short_title(),
            state_of(ms).label(),
            bar(d, g, t, 20),
            pct(d, t)
        ));
    }

    out.extend(["", "## In progress", ""].map(String::from));
    let doing_leaves: Vec<String> = milestones
        .iter()
        .flat_map(|ms| {
            ms.leaves
                .iter()
                .filter(|l| l.status == Status::Doing)
                .map(move |l| format!("- `{}` {} ({})", l.id, l.text, ms.key))
        })
        .collect();
    if doing_leaves.is_empty() {
        out.push("_Nothing in progress._".into());
    }
    out.extend(doing_leaves);

    out.push(String::new());
    out.push(format!(
        "## Ready next (first {READY_NEXT_LIMIT} todo leaves whose milestone deps are done)"
    ));
    out.push(String::new());
    let ready: Vec<String> = milestones
        .iter()
        .filter(|ms| matches!(state_of(ms), State::Ready | State::Doing))
        .flat_map(|ms| {
            ms.leaves
                .iter()
                .filter(|l| l.status == Status::Todo)
                .map(move |l| format!("- `{}` {} ({})", l.id, l.text, ms.key))
        })
        .collect();
    if ready.is_empty() {
        out.push("_Nothing ready._".into());
    }
    out.extend(ready.iter().take(READY_NEXT_LIMIT).cloned());
    if ready.len() > READY_NEXT_LIMIT {
        out.push(format!("- … and {} more", ready.len() - READY_NEXT_LIMIT));
    }

    out.extend(["", "## Burn-up", ""].map(String::from));
    if history.len() >= 2 {
        // A line needs two points; a single snapshot renders as an empty plot.
        let join =
            |f: fn(&Snapshot) -> String| history.iter().map(f).collect::<Vec<_>>().join(", ");
        let max_total = history.iter().map(|r| r.total).max().unwrap_or(0);
        out.extend(
            [
                "Gray line: total leaves (scope). Blue line: leaves done.",
                "",
                "```mermaid",
                "---",
                "config:",
                "  themeVariables:",
                "    xyChart:",
                "      plotColorPalette: \"#9a9a96, #2a78d6\"",
                "---",
                "xychart-beta",
                "  title \"Leaves: scope vs done\"",
            ]
            .map(String::from),
        );
        out.push(format!(
            "  x-axis [{}]",
            join(|r| format!("\"{}\"", r.date))
        ));
        out.push(format!("  y-axis \"Leaves\" 0 --> {max_total}"));
        out.push(format!("  line [{}]", join(|r| r.total.to_string())));
        out.push(format!("  line [{}]", join(|r| r.done.to_string())));
        out.extend(["```", ""].map(String::from));
    } else if !history.is_empty() {
        out.push(
            "_The chart appears once there are two snapshots (one per day with `--record`)._"
                .into(),
        );
        out.push(String::new());
    }
    if history.is_empty() {
        out.push(format!("_No snapshots yet. Run `{COMMAND} --record`._"));
    } else {
        out.push("| Date | Done | In progress | Total | Done % |".into());
        out.push("|---|--:|--:|--:|--:|".into());
        out.extend(history.iter().map(|r| {
            format!(
                "| {} | {} | {} | {} | {} |",
                r.date,
                r.done,
                r.in_progress,
                r.total,
                pct(r.done, r.total)
            )
        }));
    }
    out.join("\n") + "\n"
}

/// Days since 1970-01-01 -> (year, month, day), proleptic Gregorian (H. Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn today_utc() -> Result<String> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("clock before 1970")?
        .as_secs();
    let days = i64::try_from(secs / 86_400).context("date out of range")?;
    let (y, m, d) = civil_from_days(days);
    Ok(format!("{y:04}-{m:02}-{d:02}"))
}

fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                *c == b'-'
            } else {
                c.is_ascii_digit()
            }
        })
}

/// Entry point. Returns whether the command passed.
pub(crate) fn run(root: &Path, args: &[String]) -> Result<bool> {
    let (mut record, mut check, mut date) = (false, false, None);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--record" => record = true,
            "--check" => check = true,
            "--date" => date = Some(it.next().context("--date needs YYYY-MM-DD")?.clone()),
            other => bail!(
                "unknown argument {other} (usage: {COMMAND} [--record [--date YYYY-MM-DD] | --check])"
            ),
        }
    }
    if record && check {
        bail!("--record and --check are mutually exclusive");
    }

    let roadmap = std::fs::read_to_string(root.join("ROADMAP.md")).context("reading ROADMAP.md")?;
    let milestones = parse_roadmap(&roadmap)?;
    let history_path = root.join("docs/progress-history.csv");
    let progress_path = root.join("PROGRESS.md");
    let mut history = match std::fs::read_to_string(&history_path) {
        Ok(text) => parse_history(&text)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).context("reading docs/progress-history.csv"),
    };
    let (done, doing, total) = totals(&milestones);

    if record {
        let date = match date {
            Some(d) if is_iso_date(&d) => d,
            Some(d) => bail!("--date must be YYYY-MM-DD, got {d}"),
            None => today_utc()?,
        };
        history.retain(|r| r.date != date);
        history.push(Snapshot {
            date,
            done,
            in_progress: doing,
            total,
        });
        history.sort_by(|a, b| a.date.cmp(&b.date));
        std::fs::write(&history_path, format_history(&history))
            .context("writing docs/progress-history.csv")?;
    }

    let rendered = render(&milestones, &history);
    if check {
        let mut stale = Vec::new();
        let current = history
            .last()
            .is_some_and(|r| (r.done, r.in_progress, r.total) == (done, doing, total));
        if !current {
            stale.push("docs/progress-history.csv has no snapshot matching the current ROADMAP");
        }
        if std::fs::read_to_string(&progress_path).ok().as_deref() != Some(rendered.as_str()) {
            stale.push("PROGRESS.md is out of date");
        }
        if !stale.is_empty() {
            eprintln!("{}\nRun: {COMMAND} --record", stale.join("\n"));
            return Ok(false);
        }
        println!("progress up to date: {done}/{total} done, {doing} in progress");
        return Ok(true);
    }

    std::fs::write(&progress_path, rendered).context("writing PROGRESS.md")?;
    println!(
        "PROGRESS.md: {done}/{total} done ({}), {doing} in progress",
        pct(done, total)
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROADMAP: &str = "\
# M0 — Bootstrap
Deps: none.

## 0.1 Group
- [x] 0.1.1 Done leaf
- [~] 0.1.2 Doing leaf
- 0.1.3 Parent
  - [ ] 0.1.3.1 Child

# M1 — Format (`persia-format`)
Deps: M0. (Parallel with M2.)
- [ ] 1.1 Todo leaf

# M2 — Later
Deps: M0–M1
- [ ] 2.1 Leaf
";

    #[test]
    fn parses_milestones_leaves_and_deps() {
        let ms = parse_roadmap(ROADMAP).unwrap();
        let keys: Vec<_> = ms.iter().map(|m| m.key.as_str()).collect();
        assert_eq!(keys, ["M0", "M1", "M2"]);
        assert_eq!(
            ms[0]
                .leaves
                .iter()
                .map(|l| l.id.as_str())
                .collect::<Vec<_>>(),
            ["0.1.1", "0.1.2", "0.1.3.1"]
        );
        assert_eq!(ms[0].leaves[1].status, Status::Doing);
        assert_eq!(ms[1].deps, ["M0"]);
        assert_eq!(ms[1].short_title(), "Format");
        assert_eq!(ms[2].deps, ["M0", "M1"]);
    }

    #[test]
    fn deps_parsing() {
        assert_eq!(parse_deps("none."), Vec::<String>::new());
        assert_eq!(parse_deps("M1, M3, M4."), ["M1", "M3", "M4"]);
        assert_eq!(parse_deps("M5 (M6 for end-to-end)."), ["M5"]);
        assert_eq!(parse_deps("M1–M3 (can start early)."), ["M1", "M2", "M3"]);
        assert_eq!(
            parse_deps("M11 (stable proto); M13 for shard-awareness."),
            ["M11", "M13"]
        );
        assert_eq!(parse_deps("M2 - M4"), ["M2", "M3", "M4"]);
    }

    #[test]
    fn reports_every_structural_error() {
        let bad = "# M0 — A\n- 0.1 no box\n- [x] 0.2 parent\n  - [?] 0.2.1 bad\n- [ ] 0.2.1 dup\n- [ ] 1.1 wrong milestone\n# M1 — B\nDeps: M9\n";
        let err = parse_roadmap(bad).unwrap_err().to_string();
        for expected in [
            "duplicate ids: 0.2.1",
            "line 2: leaf 0.1 has no status checkbox",
            "line 3: 0.2 has children, so its status is derived; remove [x]",
            "line 4: leaf 0.2.1 has unknown status [?]",
            "line 6: item 1.1 is outside its milestone",
            "M1: unknown dependency M9",
        ] {
            assert!(err.contains(expected), "missing {expected:?} in:\n{err}");
        }
    }

    #[test]
    fn item_syntax() {
        assert_eq!(
            parse_item("- [x] 1.2.3 text"),
            Some((Some('x'), "1.2.3", "text"))
        );
        assert_eq!(parse_item("  - 1.2 Parent"), Some((None, "1.2", "Parent")));
        assert_eq!(parse_item("- 1 not an id"), None);
        assert_eq!(parse_item("- 1.2. trailing dot"), None);
        assert_eq!(parse_item("- [ab] 1.2 x"), None);
        assert_eq!(parse_item("## 1.2 heading"), None);
    }

    #[test]
    fn rounding_matches_python_half_even() {
        // Python: round(2.5) == 2, round(3.5) == 4, f"{12.5:.0f}" == "12"
        assert_eq!(div_round_half_even(5, 2), 2);
        assert_eq!(div_round_half_even(7, 2), 4);
        assert_eq!(pct(1, 8), "12%");
        assert_eq!(pct(3, 8), "38%");
        assert_eq!(pct(0, 0), "—");
        assert_eq!(bar(1, 1, 4, 10), "██▒▒░░░░░░");
        assert_eq!(bar(0, 0, 0, 3), "░░░");
    }

    #[test]
    fn states_and_reduced_edges() {
        let ms = parse_roadmap(ROADMAP).unwrap();
        let by_key: BTreeMap<&str, &Milestone> = ms.iter().map(|m| (m.key.as_str(), m)).collect();
        assert_eq!(milestone_state(&ms[0], &by_key, 0), State::Doing);
        assert_eq!(milestone_state(&ms[1], &by_key, 0), State::Blocked);
        // M2 -> {M0, M1}: M0 -> M2 is implied through M1.
        assert_eq!(
            reduced_edges(&ms),
            [("M0".into(), "M1".into()), ("M1".into(), "M2".into())]
        );
    }

    #[test]
    fn history_round_trip() {
        let text = "date,done,in_progress,total\n2026-10-07,1,2,389\n2026-10-08,3,0,390\n";
        let rows = parse_history(text).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(format_history(&rows), text);
        assert!(parse_history("nope\n").is_err());
        assert!(parse_history("date,done,in_progress,total\n2026-10-07,x,0,1\n").is_err());
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_735), (2026, 10, 9));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert!(is_iso_date("2026-10-09"));
        assert!(!is_iso_date("2026-1-09"));
    }

    #[test]
    fn real_roadmap_parses_and_renders() {
        let ms = parse_roadmap(include_str!("../../ROADMAP.md")).unwrap();
        assert!(ms.len() >= 16);
        let out = render(&ms, &[]);
        assert!(out.starts_with("<!-- GENERATED by `cargo xtask progress`"));
        assert!(out.contains("_No snapshots yet."));
    }
}
