//! `cargo xtask github-setup`: create or update GitHub labels and milestones from ROADMAP.md.
//!
//! Dry run by default: prints the `gh` commands. `--apply` runs them (needs `gh auth login`).
//! Idempotent: labels use `gh label create --force`; milestones that already exist (by title) are skipped.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::progress::parse_roadmap;

const DEFAULT_REPO: &str = "tomiok/persiaDB";
const AREAS: [&str; 9] = [
    "format", "storage", "engine", "analysis", "blob", "server", "sdk", "ci", "docs",
];
const TYPES: [&str; 7] = ["feat", "test", "bench", "fuzz", "docs", "chore", "bug"];

// Neutral, distinct hues per label family (identity only; GitHub shows the name next to the color).
const MILESTONE_COLOR: &str = "1f6feb";
const AREA_COLOR: &str = "8250df";
const TYPE_COLOR: &str = "bf8700";

/// (name, color, description) for every label ROADMAP.md asks for.
fn planned_labels(milestone_keys: &[String]) -> Vec<(String, &'static str, String)> {
    let milestones = milestone_keys.iter().map(|k| {
        (
            format!("milestone:{k}"),
            MILESTONE_COLOR,
            format!("ROADMAP milestone {k}"),
        )
    });
    let areas = AREAS
        .iter()
        .map(|a| (format!("area:{a}"), AREA_COLOR, format!("Area: {a}")));
    let types = TYPES
        .iter()
        .map(|t| (format!("type:{t}"), TYPE_COLOR, format!("Type: {t}")));
    milestones.chain(areas).chain(types).collect()
}

fn label_commands(repo: &str, labels: &[(String, &str, String)]) -> Vec<Vec<String>> {
    labels
        .iter()
        .map(|(name, color, desc)| {
            [
                "gh",
                "label",
                "create",
                name,
                "--repo",
                repo,
                "--color",
                color,
                "--description",
                desc,
                "--force",
            ]
            .map(str::to_owned)
            .to_vec()
        })
        .collect()
}

fn milestone_commands(
    repo: &str,
    milestones: &[(String, String)],
    existing: &BTreeSet<String>,
) -> Vec<Vec<String>> {
    milestones
        .iter()
        .filter(|(title, _)| !existing.contains(title))
        .map(|(title, desc)| {
            vec![
                "gh".into(),
                "api".into(),
                format!("repos/{repo}/milestones"),
                "-f".into(),
                format!("title={title}"),
                "-f".into(),
                format!("description={desc}"),
            ]
        })
        .collect()
}

/// POSIX shell quoting for the dry-run listing.
fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@%_+=:,./-".contains(&b));
    if safe {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

fn existing_milestones(repo: &str) -> Result<BTreeSet<String>> {
    let out = Command::new("gh")
        .args([
            "api",
            "--paginate",
            &format!("repos/{repo}/milestones?state=all"),
            "--jq",
            ".[].title",
        ])
        .output()
        .context("running gh")?;
    if !out.status.success() {
        bail!(
            "cannot list milestones of {repo}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Entry point. Returns whether the command passed.
pub(crate) fn run(root: &Path, args: &[String]) -> Result<bool> {
    let (mut repo, mut apply) = (DEFAULT_REPO.to_owned(), false);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--apply" => apply = true,
            "--repo" => repo.clone_from(it.next().context("--repo needs OWNER/NAME")?),
            other => bail!(
                "unknown argument {other} (usage: cargo xtask github-setup [--repo OWNER/NAME] [--apply])"
            ),
        }
    }

    let milestones = parse_roadmap(
        &std::fs::read_to_string(root.join("ROADMAP.md")).context("reading ROADMAP.md")?,
    )?;
    let keys: Vec<String> = milestones.iter().map(|ms| ms.key.clone()).collect();
    let labels = planned_labels(&keys);
    let titles: Vec<(String, String)> = milestones
        .iter()
        .map(|ms| {
            let deps = if ms.deps.is_empty() {
                "none".to_owned()
            } else {
                ms.deps.join(", ")
            };
            (
                format!("{} — {}", ms.key, ms.short_title()),
                format!("Deps: {deps}. See ROADMAP.md."),
            )
        })
        .collect();
    let existing = if apply {
        existing_milestones(&repo)?
    } else {
        BTreeSet::new()
    };

    let new_milestones = milestone_commands(&repo, &titles, &existing);
    for cmd in label_commands(&repo, &labels).iter().chain(&new_milestones) {
        if !apply {
            println!(
                "{}",
                cmd.iter()
                    .map(|a| shell_quote(a))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            continue;
        }
        let (program, rest) = cmd.split_first().context("empty command")?;
        let out = Command::new(program)
            .args(rest)
            .output()
            .context("running gh")?;
        if !out.status.success() {
            bail!(
                "failed: {}\n{}",
                cmd.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    let verb = if apply {
        "applied"
    } else {
        "planned (dry run; pass --apply)"
    };
    eprintln!(
        "{} labels, {} new milestones {verb}",
        labels.len(),
        new_milestones.len()
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_cover_the_roadmap_scheme() {
        let ms = parse_roadmap(include_str!("../../ROADMAP.md")).unwrap();
        let keys: Vec<String> = ms.iter().map(|m| m.key.clone()).collect();
        let names: BTreeSet<String> = planned_labels(&keys)
            .into_iter()
            .map(|(n, _, _)| n)
            .collect();
        for expected in [
            "milestone:M0",
            "milestone:M15",
            "area:format",
            "area:docs",
            "type:feat",
            "type:bug",
        ] {
            assert!(names.contains(expected), "{expected}");
        }
    }

    #[test]
    fn labels_are_idempotent_upserts() {
        for cmd in label_commands("o/r", &planned_labels(&["M0".into()])) {
            assert_eq!(cmd[..3], ["gh", "label", "create"]);
            assert_eq!(cmd.last().map(String::as_str), Some("--force"));
            assert!(cmd.windows(2).any(|w| w == ["--repo", "o/r"]));
        }
    }

    #[test]
    fn existing_milestones_are_skipped() {
        let existing = BTreeSet::from(["M0 — A".to_owned()]);
        let cmds = milestone_commands(
            "o/r",
            &[("M0 — A".into(), "d".into()), ("M1 — B".into(), "d".into())],
            &existing,
        );
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains(&"title=M1 — B".to_owned()));
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(shell_quote("repos/o/r"), "repos/o/r");
        assert_eq!(shell_quote("title=M0 — A"), "'title=M0 — A'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
