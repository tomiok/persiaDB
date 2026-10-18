//! The `persia` command-line tool: inspect, dump, verify, compact, bench.
//!
//! Maintenance commands (ROADMAP 2.3.8, 2.4.19; ADR-0018) list and remove the files a crash or a recovery leaves next
//! to a container or inside a local store. Each found file is printed as one tab-separated line, so the output can be
//! piped: `<action>\t<kind>\t<state>\t<path>`, with `action` `found`, `removed` or `kept`, `kind` `temporary` or
//! `quarantine`, and `state` `free`, `in-use` or `unknown` (whether a live writer holds the file's lock). Control
//! characters in a path are escaped (`\t`, `\n`, `\u{1b}`). A sweep that fails part-way still prints what it removed.
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use persia_storage::container::{Container, Error as ContainerError, Mode};
use persia_storage::{InUse, LocalStorage, SideFile, SideFileKind};

#[derive(Debug, Parser)]
#[command(name = "persia", version, about = "Persia DB command-line tool")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// A single-file container (`app.persia`).
    #[command(subcommand)]
    Container(ContainerCommand),
    /// A local object store (a directory, `file://`).
    #[command(subcommand)]
    Store(StoreCommand),
}

#[derive(Debug, Subcommand)]
enum ContainerCommand {
    /// Lists the temporary and quarantine files next to a container. Takes no writer lock: it only probes each
    /// temporary file's lock for an instant (a create racing with it may then fail with "locked" and be retried).
    Files {
        /// The container file.
        path: PathBuf,
    },
    /// Removes the stale temporary files of crashed creates and compactions. Opens the container read-write (which
    /// recovers it, as any writer would), so it fails while another writer has it open.
    Sweep {
        /// The container file.
        path: PathBuf,
        /// Also remove the quarantine copies of corrupt tails that existed before this command (those its own open
        /// makes are kept, for you to see first). They may hold acknowledged writes: save or inspect
        /// them first.
        #[arg(long)]
        quarantine: bool,
    },
}

#[derive(Debug, Subcommand)]
enum StoreCommand {
    /// Lists the temporary files of `put`s under a local store.
    Files {
        /// The store's root directory.
        root: PathBuf,
    },
    /// Removes the temporary files that crashed `put`s left, skipping those a live writer holds.
    Sweep {
        /// The store's root directory.
        root: PathBuf,
        /// Where files cannot be locked (Windows), remove them anyway. Only while no other process writes to the
        /// store: a racing `put` would fail.
        #[arg(long)]
        force: bool,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("persia: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Container(ContainerCommand::Files { path }) => {
            if !path.is_file() {
                bail!("{} is not a container file", path.display());
            }
            print_files("found", &container_files(&path)?);
        }
        Command::Container(ContainerCommand::Sweep { path, quarantine }) => {
            container_sweep(&path, quarantine)?;
        }
        Command::Store(StoreCommand::Files { root }) => {
            print_files("found", &open_store(&root)?.temporaries()?);
        }
        Command::Store(StoreCommand::Sweep { root, force }) => {
            let store = open_store(&root)?;
            let before = store.temporaries()?;
            let swept = match store.sweep_temporaries(force) {
                Ok(swept) => swept,
                Err(err) => return Err(partial(&before, err.into())),
            };
            let (removed, kept): (Vec<_>, Vec<_>) = swept.into_iter().partition(|f| f.removed);
            print_files("removed", &removed);
            print_files("kept", &kept);
            if !force && kept.iter().any(|f| f.in_use == InUse::Unknown) {
                eprintln!(
                    "persia: files cannot be locked here, so whether they are in use is unknown; \
                     rerun with --force while no other process writes to the store"
                );
            }
        }
    }
    Ok(())
}

fn container_files(path: &Path) -> Result<Vec<SideFile>> {
    Container::side_files(path)
        .with_context(|| format!("listing the files next to {}", path.display()))
}

fn container_sweep(path: &Path, quarantine: bool) -> Result<()> {
    if !path.is_file() {
        bail!("{} is not a container file", path.display());
    }
    // Only quarantine copies that existed before this open, which an operator could have inspected (ADR-0014).
    let seen: Vec<PathBuf> = container_files(path)?
        .into_iter()
        .filter(|f| f.kind == SideFileKind::Quarantine)
        .map(|f| f.path)
        .collect();
    let c = match Container::open(path, Mode::ReadWrite) {
        Ok(c) => c,
        Err(ContainerError::Locked) => {
            bail!(
                "{} is open by a writer; sweep it once that is closed",
                path.display()
            )
        }
        Err(err) => return Err(err).with_context(|| format!("opening {}", path.display())),
    };
    // Listed after the open, so a quarantine copy its recovery made is reported (as kept).
    let mut files = container_files(path)?;
    let swept = c
        .sweep_stale_files()
        .context("removing stale temporary files")
        .and_then(|mut removed| {
            if quarantine {
                removed.extend(
                    c.remove_quarantine_files(&seen)
                        .context("removing quarantine files")?,
                );
            }
            Ok(removed)
        });
    let removed = match swept {
        Ok(removed) => removed,
        Err(err) => return Err(partial(&files, err)),
    };
    for f in &mut files {
        f.removed = removed.contains(&f.path);
    }
    let (removed, kept): (Vec<_>, Vec<_>) = files.into_iter().partition(|f| f.removed);
    print_files("removed", &removed);
    print_files("kept", &kept);
    let made = kept
        .iter()
        .filter(|f| f.kind == SideFileKind::Quarantine && !seen.contains(&f.path))
        .count();
    if made > 0 {
        eprintln!(
            "persia: opening the container quarantined a corrupt tail into {made} new file(s), kept for inspection"
        );
    }
    Ok(())
}

/// Opens an existing store: [`LocalStorage::open`] would create a missing root.
fn open_store(root: &Path) -> Result<LocalStorage> {
    if !root.is_dir() {
        bail!("{} is not a directory", root.display());
    }
    LocalStorage::open(root).with_context(|| format!("opening the store at {}", root.display()))
}

fn print_files(action: &str, files: &[SideFile]) {
    for f in files {
        let kind = match f.kind {
            SideFileKind::Temporary => "temporary",
            SideFileKind::Quarantine => "quarantine",
        };
        let state = match f.in_use {
            InUse::No => "free",
            InUse::Yes => "in-use",
            InUse::Unknown => "unknown",
        };
        println!("{action}\t{kind}\t{state}\t{}", escaped(&f.path));
    }
}

/// A sweep failed with `err` part-way: prints the files of `listed` it removed before failing (those gone now),
/// since they stay removed, and returns the error.
fn partial(listed: &[SideFile], err: anyhow::Error) -> anyhow::Error {
    let gone: Vec<SideFile> = listed
        .iter()
        .filter(|f| !f.path.exists())
        .map(|f| SideFile {
            removed: true,
            ..f.clone()
        })
        .collect();
    print_files("removed", &gone);
    err
}

/// `path` with its control characters escaped (`\t`, `\n`, `\u{..}`), so each file stays one line of four fields.
fn escaped(path: &Path) -> String {
    let mut out = String::new();
    for c in path.display().to_string().chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}
