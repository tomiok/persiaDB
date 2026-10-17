//! The `persia` command-line tool: inspect, dump, verify, compact, bench.
//!
//! Maintenance commands (ROADMAP 2.3.8, 2.4.19; ADR-0018) list and remove the files a crash or a recovery leaves next
//! to a container or inside a local store. Each found file is printed as one tab-separated line, so the output can be
//! piped: `<action>\t<kind>\t<state>\t<path>`, with `action` `found`, `removed` or `kept`, `kind` `temporary` or
//! `quarantine`, and `state` `free`, `in-use` or `unknown` (whether a live writer holds the file's lock).
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
    /// Lists the temporary and quarantine files next to a container. Opens nothing.
    Files {
        /// The container file.
        path: PathBuf,
    },
    /// Removes the stale temporary files of crashed creates and compactions. Opens the container read-write (which
    /// recovers it, as any writer would), so it fails while another writer has it open.
    Sweep {
        /// The container file.
        path: PathBuf,
        /// Also remove the quarantine copies of corrupt tails. They may hold acknowledged writes: save or inspect
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
            print_files("found", &container_files(&path)?);
        }
        Command::Container(ContainerCommand::Sweep { path, quarantine }) => {
            container_sweep(&path, quarantine)?;
        }
        Command::Store(StoreCommand::Files { root }) => {
            print_files("found", &open_store(&root)?.temporaries()?);
        }
        Command::Store(StoreCommand::Sweep { root, force }) => {
            let swept = open_store(&root)?.sweep_temporaries(force)?;
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
    // Listed after the open, so a quarantine copy its recovery made is included.
    let mut files = container_files(path)?;
    let mut removed = c
        .sweep_stale_files()
        .context("removing stale temporary files")?;
    if quarantine {
        removed.extend(
            c.remove_quarantine_files()
                .context("removing quarantine files")?,
        );
    }
    for f in &mut files {
        f.removed = removed.contains(&f.path);
    }
    let (removed, kept): (Vec<_>, Vec<_>) = files.into_iter().partition(|f| f.removed);
    print_files("removed", &removed);
    print_files("kept", &kept);
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
        println!("{action}\t{kind}\t{state}\t{}", f.path.display());
    }
}
