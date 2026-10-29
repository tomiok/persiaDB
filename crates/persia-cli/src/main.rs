//! The `persia` command-line tool: inspect, dump, verify, compact, bench.
//!
//! Maintenance commands (ROADMAP 2.3.8, 2.4.19; ADR-0019) list and remove the files a crash or a recovery leaves next
//! to a container or inside a local store. Each found file is printed as one tab-separated line, so the output can be
//! piped: `<action>\t<kind>\t<state>\t<path>`, with `action` `found`, `removed` or `kept`, `kind` `temporary` or
//! `quarantine`, and `state` `free`, `in-use` or `unknown` (whether a live writer holds the file's lock). Control
//! characters in a path are escaped (`\t`, `\n`, `\u{1b}`). A sweep that fails part-way still prints what it removed.
//!
//! Segment commands (ROADMAP 5.4.2, 5.4.3) read one segment file with the schema it was written with: `inspect
//! segment` prints its footer, sections and per-field statistics, and `dump segment` a field's terms or one term's
//! postings (module `segment`).
//!
//! `persia verify` checks a whole database read-only (ROADMAP 6.6.3, module `database`).
#![forbid(unsafe_code)]

mod database;
mod segment;

use std::io::{BufWriter, Write};
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
    /// Prints the structure of an on-disk object.
    #[command(subcommand)]
    Inspect(InspectCommand),
    /// Prints the content of an on-disk object, for debugging.
    #[command(subcommand)]
    Dump(DumpCommand),
    /// Checks a whole database read-only (no lock, no writes, so a live writer may have it open): every check an
    /// open makes, every segment's bytes, one live copy per document id, and the bytes after the last commit.
    /// Fails on any corruption. Uses the built-in analyzers: a database with a custom analyzer does not open here.
    Verify {
        /// The container file.
        path: PathBuf,
    },
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
enum InspectCommand {
    /// A segment file (SPEC §5): footer, sections with their sizes, and per-field statistics.
    Segment {
        /// The segment file.
        path: PathBuf,
        /// The collection schema (JSON) the segment was written with.
        #[arg(long)]
        schema: PathBuf,
        /// Also check the whole segment (every CRC, and the rules no lookup checks), and fail if it is corrupt.
        #[arg(long)]
        verify: bool,
    },
}

#[derive(Debug, Subcommand)]
enum DumpCommand {
    /// A field of a segment file: its terms, or with `--term` that term's postings.
    Segment {
        /// The segment file.
        path: PathBuf,
        /// The collection schema (JSON) the segment was written with.
        #[arg(long)]
        schema: PathBuf,
        /// A `text` or `keyword` field.
        #[arg(long)]
        field: String,
        /// A term, as indexed (after analysis: e.g. lowercased and stemmed for an English `text` field).
        #[arg(long)]
        term: Option<String>,
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
        /// Where whether a file is in use cannot be checked (Windows, or a network or FUSE file system where locks
        /// belong to the process), remove it anyway. Only while no other writer uses the store: a racing `put` would
        /// fail.
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
        Command::Inspect(InspectCommand::Segment {
            path,
            schema,
            verify,
        }) => to_stdout(|out| segment::inspect(out, &path, &schema, verify))?,
        Command::Dump(DumpCommand::Segment {
            path,
            schema,
            field,
            term,
        }) => to_stdout(|out| segment::dump(out, &path, &schema, &field, term.as_deref()))?,
        Command::Verify { path } => to_stdout(|out| database::verify(out, &path))?,
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
                    "persia: whether some files are in use cannot be checked here (no file locks, or locks that \
                     belong to the process, as on NFS); rerun with --force while no other process writes to the store"
                );
            }
        }
    }
    Ok(())
}

/// Runs `f` on a buffered, locked stdout. A closed pipe (`persia dump segment ... | head`) ends the output early
/// without an error.
fn to_stdout(f: impl FnOnce(&mut dyn Write) -> Result<()>) -> Result<()> {
    let mut out = BufWriter::new(std::io::stdout().lock());
    let result = f(&mut out).and_then(|()| out.flush().map_err(Into::into));
    match result {
        Err(e) if is_broken_pipe(&e) => Ok(()),
        other => other,
    }
}

fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
    })
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
