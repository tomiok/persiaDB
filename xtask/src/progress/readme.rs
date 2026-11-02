//! The README's progress blocks, regenerated with `PROGRESS.md` so the two cannot disagree.
//!
//! `README.md` carries two marked blocks: the headline count under the title and the milestone table under
//! `## Status`. Text between a block's markers belongs to this module; everything else in the README is
//! hand-written.

use std::collections::BTreeMap;

use anyhow::{Result, bail};

use super::{COMMAND, Milestone, Snapshot, State, Status, is_mvp, milestone_state, pct, totals};

/// The marker pairs, in the order they appear in the README.
const BLOCKS: [&str; 2] = ["summary", "milestones"];

fn begin(name: &str) -> String {
    format!("<!-- BEGIN GENERATED progress:{name} by `{COMMAND}`. Do not edit by hand. -->")
}

fn end(name: &str) -> String {
    format!("<!-- END GENERATED progress:{name} -->")
}

fn summary(milestones: &[Milestone], history: &[Snapshot]) -> String {
    let (done, doing, total) = totals(milestones.iter().filter(|ms| is_mvp(ms)));
    let (post_done, _, post_total) = totals(milestones.iter().filter(|ms| !is_mvp(ms)));
    let snapshot = history.last().map_or_else(
        || "no snapshot recorded yet".to_owned(),
        |r| format!("last snapshot {}", r.date),
    );
    format!(
        "**Pre-alpha.** [`PROGRESS.md`](./PROGRESS.md) counts **{done} of {total}** MVP leaves done ({}), {doing} in progress ({snapshot}). Post-MVP work is {post_done} of {post_total} and sits outside that percentage.",
        pct(done, total)
    )
}

fn table(milestones: &[Milestone]) -> String {
    let by_key: BTreeMap<&str, &Milestone> =
        milestones.iter().map(|ms| (ms.key.as_str(), ms)).collect();
    let mut out = vec![
        "| Milestone | State | Leaves done |".to_owned(),
        "|---|---|---|".to_owned(),
    ];
    for ms in milestones.iter().filter(|ms| is_mvp(ms)) {
        let state: State = milestone_state(ms, &by_key, 0);
        out.push(format!(
            "| **{}** {} | {} | {}/{} |",
            ms.key,
            ms.short_title().replace('|', "\\|"),
            state.label(),
            ms.count(Status::Done),
            ms.leaves.len()
        ));
    }
    out.join("\n")
}

/// Replaces the text between `name`'s markers with `body`. Both markers must appear exactly once, in order.
fn splice(text: &str, name: &str, body: &str) -> Result<String> {
    let (b, e) = (begin(name), end(name));
    if text.matches(&b).count() != 1 || text.matches(&e).count() != 1 {
        bail!("README.md must contain each progress:{name} marker exactly once:\n{b}\n{e}");
    }
    let Some((head, rest)) = text.split_once(&b) else {
        bail!("README.md: progress:{name} begin marker not found");
    };
    let Some((_, tail)) = rest.split_once(&e) else {
        bail!("README.md: progress:{name} end marker comes before its begin marker");
    };
    Ok(format!("{head}{b}\n{body}\n{e}{tail}"))
}

/// `readme` with every progress block regenerated from the ROADMAP and the history.
pub(crate) fn apply(
    readme: &str,
    milestones: &[Milestone],
    history: &[Snapshot],
) -> Result<String> {
    let bodies = [summary(milestones, history), table(milestones)];
    let mut out = readme.to_owned();
    for (name, body) in BLOCKS.iter().zip(bodies) {
        out = splice(&out, name, &body)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::{parse_history, parse_roadmap};

    const ROADMAP: &str = "\
# M0 — Bootstrap
Deps: none.
- [x] 0.1 Done
- [~] 0.2 Doing

# M1 — Format (`persia-format`)
Deps: M0.
- [ ] 1.1 Todo

# M16 — Later (target: v1.1)
Deps: M1.
- [ ] 16.1 Post
";

    fn readme() -> String {
        format!(
            "# Title\n\n{}\nstale\n{}\n\n## Status\n\n{}\n{}\n\ntail\n",
            begin("summary"),
            end("summary"),
            begin("milestones"),
            end("milestones")
        )
    }

    #[test]
    fn fills_both_blocks_and_keeps_the_rest() {
        let ms = parse_roadmap(ROADMAP).unwrap();
        let history = parse_history("date,done,in_progress,total\n2026-11-02,1,1,3\n").unwrap();
        let out = apply(&readme(), &ms, &history).unwrap();
        assert!(out.starts_with("# Title\n\n"));
        assert!(out.ends_with("\n\ntail\n"));
        assert!(!out.contains("stale"));
        assert!(out.contains(
            "[`PROGRESS.md`](./PROGRESS.md) counts **1 of 3** MVP leaves done (33%), 1 in progress (last snapshot 2026-11-02). Post-MVP work is 0 of 1"
        ));
        assert!(out.contains("| **M0** Bootstrap | 🔨 in progress | 1/2 |"));
        assert!(out.contains("| **M1** Format | ⏳ blocked | 0/1 |"));
        assert!(
            !out.contains("M16"),
            "post-MVP milestones stay out of the table"
        );
        // Idempotent: a second pass changes nothing.
        assert_eq!(apply(&out, &ms, &history).unwrap(), out);
    }

    #[test]
    fn missing_duplicate_or_reversed_markers_are_errors() {
        let ms = parse_roadmap(ROADMAP).unwrap();
        let no_summary = readme().replace(&begin("summary"), "");
        assert!(apply(&no_summary, &ms, &[]).is_err());
        let twice = format!("{}{}", readme(), end("milestones"));
        assert!(apply(&twice, &ms, &[]).is_err());
        let reversed = format!(
            "{}\n{}\n{}\n",
            end("summary"),
            begin("summary"),
            begin("milestones")
        ) + &end("milestones");
        assert!(apply(&reversed, &ms, &[]).is_err());
    }
}
