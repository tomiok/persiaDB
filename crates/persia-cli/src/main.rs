//! The `persia` command-line tool: inspect, dump, verify, compact, import, search, bench.
//!
//! Maintenance commands (ROADMAP 2.3.8, 2.4.19, 9.2.10; ADR-0019) list and remove the files a crash or a recovery leaves
//! next to a container (its blob pack's included) or inside a local store. Each found file is printed as one
//! tab-separated line, so the output can be piped: `<action>\t<kind>\t<state>\t<path>`, with `action` `found`,
//! `removed` or `kept`, `kind` `temporary`, `quarantine`, `lock` (the blob pack's writer lock file) or `leases` (its
//! reader lease directory; neither is ever removed), and `state` `free`, `in-use` or `unknown` (whether a live writer
//! holds the file's lock). Control characters in a path are escaped (`\t`, `\n`, `\u{1b}`). A sweep that fails
//! part-way still prints what it removed.
//!
//! Segment commands (ROADMAP 5.4.2, 5.4.3) read one segment file with the schema it was written with: `inspect
//! segment` prints its footer, sections and per-field statistics, and `dump segment` a field's terms or one term's
//! postings (module `segment`).
//!
//! `persia inspect container` prints a container's superblock, the frames recovery reads and, by document id, the WAL
//! records replay applies (ROADMAP 6.6.10, module `container`). `persia verify` checks a whole database read-only
//! (ROADMAP 6.6.3, module `database`) before it opens the blob pack, then its blobs (ROADMAP 9.5.3, ADR-0053): every
//! blob a live document or a WAL record from `wal_start` on references must be bound in the blob pack, and by default
//! every frame of the pack is checksummed (`--quick` skips that scan, `--deep` also re-hashes every referenced blob).
//! The pack open and the scan bound each frame's length by its kind before reading it (ROADMAP 9.2.14). Only the local
//! layout is checked (the remote one is ROADMAP 13.2.6).
//!
//! Database commands (ROADMAP 10.4.1) complete the tree. `persia stats` prints what a read-only open reports: the last
//! commit, the container's live and dead space, the WAL after the last commit, the blob pack's size, and each
//! collection's schema, documents and segments (module `stats`, which defines its `--json` shape; it is the
//! whole-database inspection, so there is no `inspect database`). `persia dump documents` writes a collection's live
//! documents as JSON Lines in the output form of SPEC §6.4.1, in storage order (module `documents`). `persia compact`
//! rewrites the container without its dead frames (SPEC §4.6, module `compact`).
//!
//! For quick experiments (ROADMAP 10.4.2), through the embedded API: `persia import` reads JSON Lines (a file, or the
//! standard input) into a collection, one document per line in the form `dump documents` writes, and stops at the first
//! bad line, naming it (module `import`); `persia search` runs one query, user text or a request JSON, on a read-only
//! open (module `search`).
//!
//! `persia gc` (ROADMAP 10.4.5, module `gc`) collects the blob pack on request (SPEC §9.5), through the internal
//! layer's `BlobDatabase::gc`: mark, sweep, pack rewrite. It reports what it deleted, what it kept, the pack's size
//! before and after, and the reader leases that may hold the sweep back. `compact`, `import` and `gc` are the database
//! commands that open read-write.
//!
//! The command tree. A file or directory is the positional argument (`<PATH>`; `<ROOT>` for a store), every other
//! argument is a `--flag`, and `--json` asks for one JSON object on one line where a command has one:
//!
//! ```text
//! persia container files <PATH>
//! persia container sweep <PATH> [--quarantine]
//! persia store files <ROOT>
//! persia store sweep <ROOT> [--force]
//! persia inspect container <PATH> [--id <ID>]
//! persia inspect segment <PATH> --schema <SCHEMA> [--verify]
//! persia dump segment <PATH> --schema <SCHEMA> --field <FIELD> [--term <TERM>]
//! persia dump documents <PATH> --collection <COLLECTION>
//! persia verify <PATH> [--quick | --deep]
//! persia stats <PATH> [--json]
//! persia compact <PATH> [--json]
//! persia gc <PATH> [--grace <DURATION>] [--json]
//! persia import <PATH> --collection <COLLECTION> [--input <FILE>] [--schema <SCHEMA>] [--json]
//! persia search <PATH> --collection <COLLECTION> (--query <TEXT> | --request <JSON>) [--json]
//! persia bench [--dir <DIR>] [--docs <DOCS>] [--queries <QUERIES>] [--seed <SEED>] [--json]
//! ```
//!
//! Output goes to stdout and a failure's reason to stderr. A reader that closes the pipe early (`persia dump
//! documents ... | head`) ends the output without an error.
//!
//! Exit status (ROADMAP 6.6.7, SPEC §8.6): 0 on success, 3 when the command failed on corrupt stored bytes (a database,
//! container, segment or blob pack that breaks its format) or on a broken blob reference (`verify`: a document that
//! references a blob the pack lacks, or whose bytes do not match it, SPEC §9.5), 4 when the file is not a Persia
//! container (its header magic differs, or it is too short to hold the magic; SPEC §4.2), 2 for a usage error, and 1
//! for any other failure (a missing file, an I/O error, a lock held by a writer, data written by a newer build, a
//! `--schema` that is not the segment's, a logged record that replay cannot apply; for `verify`, a blob pack of
//! another database (`PackOfAnotherDatabase`) or of a newer format (`PackUnsupported`), and a database a live writer
//! committed to while `verify` ran, which reports "changed during verify, rerun" instead of a dangling reference). A
//! script separates a wrong file (4), damage (3), and any other failed run (1).
//!
//! The status of each command that takes a container, by what it finds at `<PATH>` (tested as a table in
//! `tests/commands.rs`):
//!
//! | Command | A sound database | No file | Not a container | A damaged container | A live writer holds it |
//! |---|---|---|---|---|---|
//! | `verify` | 0 | 1 | 4 | 3 | 0 |
//! | `stats` | 0 | 1 | 4 | 3 | 0 |
//! | `dump documents` | 0 | 1 | 4 | 3 | 0 |
//! | `inspect container` | 0 | 1 | 4 | 3 | 0 |
//! | `compact` | 0 | 1 | 4 | 3 | 1 |
//! | `gc` | 0 | 1 | 4 | 3 | 1 |
//! | `import` | 0 | 1 | 4 | 3 | 1 |
//! | `search` | 0 | 1 | 4 | 3 | 0 |
//! | `container sweep` | 0 | 1 | 4 | 3 | 1 |
//! | `container files` | 0 | 1 | 0 | 0 | 0 |
//!
//! The read-only commands (`search` among them) take no lock, so a live writer does not fail them; `compact`, `import`,
//! `gc` and `container sweep` open read-write and fail with status 1 while another writer has the database. `container
//! files` opens nothing: it lists the files beside any existing file. `inspect container` reports damage after the
//! last commit on its rows and still exits 0 (it is a debugging aid; `verify` is the check). Besides the table: `dump
//! documents` exits 1 for a collection the database lacks, `compact` exits 1 when its recovery would flush any
//! collection while a collection whose schema names an analyzer registered by the application holds a replayed record
//! (it has the built-in ones only; it compacts such a database otherwise, ROADMAP 10.4.4, ADR-0068), `import` and
//! `search` exit 1 for a collection the database lacks (`import` creates it with `--schema`) and for a schema naming a
//! registered analyzer, `import` exits 1 at the first bad line of its input (naming it, after committing the lines
//! before it; a blob reference that names a stored blob with another size or chunk size is such a line, and a put
//! that finds stored damage exits 3, ROADMAP 10.1.33), `import` and `search` read the embedded API's `ErrorCode` for
//! their status (3 for `Corrupt` only), `gc` exits 1 for a schema naming a registered analyzer and for a `--grace`
//! below 2 h (2 for a malformed one) and 0 when reader leases held its sweep back (the report says so), the segment
//! commands exit 3 for a corrupt segment file and 1 for a missing one or a `--schema` that is not the segment's, and
//! the store commands exit 1 for a root that is not a directory.
//!
//! `persia bench` (ROADMAP 10.4.3, module `bench`) is a quick micro-benchmark: it indexes a seeded synthetic corpus
//! into a new database in a scratch directory it creates and removes, then runs a seeded query mix, and reports
//! documents per second, the commit time and each query kind's latency percentiles. **Its numbers are local guidance
//! only, not a published benchmark** (reproducible numbers live in `docs/benchmarks.md`); its help and its output
//! both say so. It takes no container: it exits 0, or 1 on any failure (a `--dir` that is not a directory, an I/O
//! error), and never opens an existing database.
#![forbid(unsafe_code)]

mod bench;
mod compact;
mod container;
mod database;
mod documents;
mod gc;
mod import;
mod open;
mod search;
mod segment;
mod stats;

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use persia::internal::documents::Error as BlobDatabaseError;
use persia::internal::gc::GcError;
use persia::internal::verify::{Scan, VerifyError};
use persia_engine::db::DatabaseError;
use persia_engine::segment::ReadError;
use persia_format::{Corruption, Error as FormatError};
use persia_storage::container::{Container, Error as ContainerError, Mode};
use persia_storage::{InUse, LocalStorage, SideFile, SideFileKind};

#[derive(Debug, Parser)]
#[command(
    name = "persia",
    version,
    about = "Persia DB command-line tool",
    after_help = "Exit status: 0 success; 1 another failure; 2 usage error; 3 corrupt Persia data or a missing referenced blob; 4 the file is not a Persia container"
)]
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
    /// Those document checks run before the blob pack is opened. Then its blobs: every blob a live document or a
    /// WAL record references must be bound in the blob pack (`<name>.persia.blobs`), and every frame's checksum
    /// and chunk's CRC32C of the pack is checked. The open and that scan bound each frame's length by its kind before
    /// reading it (ROADMAP 9.2.14). Exits with status 3 on corruption or a missing referenced blob, 4 when the file is
    /// not a Persia container, and 1 on any other failure (see the crate docs).
    Verify {
        /// The container file.
        path: PathBuf,
        /// Skip the checksum scan of the blob pack: only check that every referenced blob is bound.
        #[arg(long, conflicts_with = "deep")]
        quick: bool,
        /// Also read every referenced blob whole and check its SHA-256 against its id.
        #[arg(long)]
        deep: bool,
    },
    /// Rewrites a container without its dead frames (SPEC §4.6) and prints its size before and after. Opens the
    /// database read-write, as a writer does, so it fails (status 1) while another writer has it open. It commits no
    /// document of its own; as with any writer's open, recovery may flush a WAL backlog larger than the default
    /// memtable or retire a cut commit (SPEC §8.6). The new file is renamed over the old one, so the path names a
    /// complete database at every instant.
    /// It has the built-in analyzers only: a database whose schema names an analyzer the application registered is
    /// compacted without analyzing that field (its WAL records stay in the WAL), unless recovery would flush while such
    /// a collection holds a WAL record (status 1; open it with the application and commit, then compact).
    /// The blob pack (`<name>.persia.blobs`) is not touched. Do not run it during a copy of the files taken with the
    /// database in use (SPEC §4.8). Read-only handles of other processes keep reading the old file until they
    /// refresh, which holds a writer's blob garbage collection back meanwhile (SPEC §9.5). Exits with status 3 on
    /// corruption, 4 when the file is not a Persia container, and 1 on any other failure.
    Compact {
        /// The container file.
        path: PathBuf,
        /// Write one JSON object (its keys are listed in the README's command-line section) instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Collects the blob pack (`<name>.persia.blobs`, SPEC §9.5): marks every blob a document or a WAL record still
    /// references, deletes the others with their staged chunks, and rewrites the pack when enough is dropped. Prints
    /// what it deleted, what it kept and why, the pack's size before and after, and the reader leases it found. Opens
    /// the database read-write, as a writer does, so it fails (status 1) while another writer has it open. A read-only
    /// handle of another process that has not refreshed since the last commit holds the sweep back: nothing is deleted,
    /// and the report says so. A database without a pack has nothing to collect. It has the built-in analyzers only: a
    /// schema naming an analyzer the application registered fails the open (status 1). Exits with status 3 on
    /// corruption, 4 when the file is not a Persia container, and 1 on any other failure.
    Gc {
        /// The container file.
        path: PathBuf,
        /// How long an unmarked object of a remote store is kept after it was written, as <number><unit> with unit s,
        /// m, h or d; at least 2h (twice the longest bind budget, SPEC §9.4), else the command fails. The local pack
        /// keeps no ages: it drops what is unmarked at once (SPEC §9.3).
        #[arg(long, default_value = "24h", value_parser = gc::parse_grace)]
        grace: std::time::Duration,
        /// Write one JSON object (its keys are listed in the README's command-line section) instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Imports JSON Lines into a collection, for quick experiments: one document per line, in the form `persia dump
    /// documents` writes (SPEC §6.4.1), each put as an upsert by id, committed at the end. Opens the database
    /// read-write, as a writer does, so it fails (status 1) while another writer has it open. The collection must
    /// exist unless `--schema` is given. Empty and blank lines are skipped. THE FIRST BAD LINE STOPS THE IMPORT (not
    /// UTF-8, over 16 MiB, not JSON, not a document of the schema, refused by the database, or referencing a blob this
    /// database's blob store does not hold): the documents of the lines before it are committed, nothing from it on,
    /// and the error names the line (1-based). Exits with status 3 on corruption, 4 when the file is not a Persia
    /// container, and 1 on any other failure.
    Import(import::Options),
    /// Runs one search of a collection, for quick experiments: `--query` searches the user text with every default
    /// (relaxed, so by default never empty when the collection has a document; 20 hits), `--request` takes a request
    /// JSON (SPEC §7.1.4). Read-only, without the writer lock, so a live writer may have the database open; it takes a
    /// reader lease beside the blob pack. Prints each hit's rank, score and document, or with `--json` the response
    /// JSON of SPEC §7.5.2. Exits with status 3 on corruption, 4 when the file is not a Persia container, and 1 on any
    /// other failure.
    #[command(group(clap::ArgGroup::new("what").required(true).args(["query", "request"])))]
    Search {
        /// The container file.
        path: PathBuf,
        /// The collection to search.
        #[arg(long)]
        collection: String,
        /// The user text (SPEC §7.1.1).
        #[arg(long)]
        query: Option<String>,
        /// A request JSON (SPEC §7.1.4), such as '{"q": "lamp", "limit": 5, "relax": false}'.
        #[arg(long)]
        request: Option<String>,
        /// Write the response JSON (SPEC §7.5.2) on one line instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Prints what a read-only open of a database reports (no lock, no writes, so a live writer may have it open):
    /// the last commit, the container's live and dead space, whether a compaction is due with the DEFAULT thresholds
    /// (40% and 1 MiB dead; an application's own are not stored in the file), the WAL after the last commit, the
    /// blob pack's size, and each collection's schema, documents and segments. Exits with status 3 on corruption, 4
    /// when the file is not a Persia container, and 1 on any other failure.
    Stats {
        /// The container file.
        path: PathBuf,
        /// Write one JSON object (a stable shape: its keys are listed in the README's command-line section) instead
        /// of text.
        #[arg(long)]
        json: bool,
    },
    /// Runs a quick micro-benchmark of this build on this machine: indexes a seeded synthetic corpus into a new
    /// database, commits it, and runs a seeded query mix (term, terms, phrase, filter, typo). Prints documents per
    /// second, the commit time and each query kind's latency percentiles (p50, p90, p99, max). THE NUMBERS ARE LOCAL
    /// GUIDANCE ONLY, NOT A PUBLISHED BENCHMARK: they depend on this machine, its load and this build, and one run is
    /// one sample (reproducible numbers live in docs/benchmarks.md). The same seed gives the same corpus and queries.
    /// The database is created in a new scratch directory that is removed afterwards, also on failure; no existing
    /// database is opened. Exits with status 1 on any failure.
    Bench {
        /// Where the scratch directory is created [default: the system temporary directory].
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Documents in the corpus.
        #[arg(long, default_value_t = bench::DEFAULT_DOCS, value_parser = clap::value_parser!(u32).range(1..))]
        docs: u32,
        /// Queries in the mix, a fifth of each kind; at most 10000000, since each one's latency is kept.
        #[arg(long, default_value_t = bench::DEFAULT_QUERIES,
              value_parser = clap::value_parser!(u32).range(..=i64::from(bench::MAX_QUERIES)))]
        queries: u32,
        /// The seed of the corpus and of the queries.
        #[arg(long, default_value_t = bench::DEFAULT_SEED)]
        seed: u64,
        /// Write one JSON object (a stable shape: its keys are listed in the README's command-line section) instead
        /// of text.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ContainerCommand {
    /// Lists the temporary and quarantine files next to a container, and those of its blob pack: its temporary
    /// files, its writer lock file (`<name>.persia.blobs.lock`) and its reader lease directory. Takes no writer
    /// lock: it only probes each temporary file's lock, and the pack's, for an instant (a create racing with it may
    /// then fail with "locked" and be retried).
    Files {
        /// The container file.
        path: PathBuf,
    },
    /// Removes the stale temporary files of crashed creates and compactions, and of crashed blob pack creations and
    /// rewrites (only while no pack writer holds the pack's lock). Opens the container read-write (which recovers it,
    /// as any writer would), so it fails while another writer has it open. The pack's lock file and lease directory
    /// are never removed.
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
    /// A container file (SPEC §4): the active superblock, every frame from `wal_start` (offset, length, `frame_seq`,
    /// kind, whether after `log_end`) with the `seq` of each Manifest frame, and with `--id` the WAL records that
    /// put or delete that document id. Read-only, without the writer lock.
    Container {
        /// The container file.
        path: PathBuf,
        /// Also list the WAL records of this document id.
        #[arg(long)]
        id: Option<String>,
    },
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
    /// The live documents of one collection of a database, as JSON Lines (one document per line, in the output form
    /// of SPEC §6.4.1: what `get` returns, so stored fields only, and a blob as its reference). Read-only, without
    /// the writer lock. Order: the segments oldest first, each by docid, then the documents of the WAL records not
    /// yet in a segment, as logged. The segments' documents are streamed one at a time; the WAL's are in memory
    /// already, as the open replayed them. Exits with status 3 on corruption, 4 when
    /// the file is not a Persia container, and 1 on any other failure (a collection the database lacks included).
    Documents {
        /// The container file.
        path: PathBuf,
        /// The collection to dump.
        #[arg(long)]
        collection: String,
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
            ExitCode::from(status_of(&err))
        }
    }
}

/// The exit status of a command that failed on corrupt stored bytes (crate docs).
const EXIT_CORRUPT: u8 = 3;
/// The exit status of a command whose file is not a Persia container (crate docs). Distinct from [`EXIT_CORRUPT`]:
/// the format calls a wrong header magic corruption (SPEC §4.2), and a script still needs to tell that file from a
/// damaged database.
const EXIT_NOT_CONTAINER: u8 = 4;

/// The process status of a failed command (crate docs). A file that is not a container is also corruption at the
/// format layer, so that check comes first.
fn status_of(err: &anyhow::Error) -> u8 {
    if is_not_container(err) {
        EXIT_NOT_CONTAINER
    } else if is_corruption(err) {
        EXIT_CORRUPT
    } else {
        1
    }
}

/// Whether `err` reports corrupt stored bytes: the outermost Persia error in its chain decides, since it knows
/// what its sources mean (an I/O error reading a segment is not a corruption). A command of the embedded API
/// (`import`, `search`) fails with its `persia::Error`, whose stable code decides (ROADMAP 10.1.33): a caller's
/// blob reference that does not match its blob is `InvalidArgument` there, though the layers below may still
/// call the same mismatch read from a stored document damage.
fn is_corruption(err: &anyhow::Error) -> bool {
    err.chain()
        .find_map(|c| {
            if let Some(e) = c.downcast_ref::<persia::Error>() {
                Some(e.code() == persia::ErrorCode::Corrupt)
            } else if let Some(e) = c.downcast_ref::<DatabaseError>() {
                Some(e.is_corruption())
            } else if let Some(e) = c.downcast_ref::<VerifyError>() {
                Some(e.is_corruption())
            } else if let Some(e) = c.downcast_ref::<BlobDatabaseError>() {
                Some(e.is_corruption())
            } else if let Some(e) = c.downcast_ref::<GcError>() {
                Some(gc::is_corruption(e))
            } else if let Some(e) = c.downcast_ref::<ReadError>() {
                Some(e.is_corruption())
            } else {
                c.downcast_ref::<ContainerError>()
                    .map(ContainerError::is_corruption)
            }
        })
        .unwrap_or(false)
}

/// Whether the outermost Persia error says the file is not a Persia container: the header magic differs, or the
/// file ends before those bytes can be read (SPEC §4.2). A header that has the magic and then fails its CRC is
/// damage, not this.
fn is_not_container(err: &anyhow::Error) -> bool {
    err.chain()
        .find_map(|c| {
            if let Some(e) = c.downcast_ref::<DatabaseError>() {
                Some(database_not_container(e))
            } else if c.downcast_ref::<ReadError>().is_some() {
                // A segment command: its bytes are not a container header.
                Some(false)
            } else if let Some(e) = c.downcast_ref::<ContainerError>() {
                Some(container_not_container(e))
            } else {
                c.downcast_ref::<FormatError>().map(format_not_container)
            }
        })
        .unwrap_or(false)
}

fn database_not_container(err: &DatabaseError) -> bool {
    match err {
        DatabaseError::Container(e) => container_not_container(e),
        _ => false,
    }
}

fn container_not_container(err: &ContainerError) -> bool {
    match err {
        ContainerError::Format(e) => format_not_container(e),
        _ => false,
    }
}

/// [`Corruption::BadMagic`], or an end of input before the 8-byte magic (SPEC §4.2). Anything past the magic,
/// including a short header that already matched it, is a damaged container.
fn format_not_container(err: &FormatError) -> bool {
    match err.corruption() {
        Some((_, Corruption::BadMagic)) => true,
        Some((0, Corruption::UnexpectedEof { available, .. })) => {
            let magic_len = u64::try_from(persia_format::header::MAGIC.len()).unwrap_or(u64::MAX);
            available < magic_len
        }
        _ => false,
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Inspect(InspectCommand::Segment {
            path,
            schema,
            verify,
        }) => to_stdout(|out| segment::inspect(out, &path, &schema, verify))?,
        Command::Inspect(InspectCommand::Container { path, id }) => {
            to_stdout(|out| container::inspect(out, &path, id.as_deref()))?;
        }
        Command::Dump(DumpCommand::Segment {
            path,
            schema,
            field,
            term,
        }) => to_stdout(|out| segment::dump(out, &path, &schema, &field, term.as_deref()))?,
        Command::Dump(DumpCommand::Documents { path, collection }) => {
            to_stdout(|out| documents::dump(out, &path, &collection))?;
        }
        Command::Compact { path, json } => to_stdout(|out| compact::compact(out, &path, json))?,
        Command::Gc { path, grace, json } => to_stdout(|out| gc::gc(out, &path, grace, json))?,
        Command::Stats { path, json } => to_stdout(|out| stats::stats(out, &path, json))?,
        Command::Import(options) => to_stdout(|out| import::import(out, &options))?,
        Command::Search {
            path,
            collection,
            query,
            request,
            json,
        } => {
            let query = search::Query::of(query, request)?;
            to_stdout(|out| search::search(out, &path, &collection, &query, json))?;
        }
        Command::Bench {
            dir,
            docs,
            queries,
            seed,
            json,
        } => {
            let options = bench::Options {
                dir,
                docs,
                queries,
                seed,
                json,
            };
            to_stdout(|out| bench::bench(out, &options))?;
        }
        Command::Verify { path, quick, deep } => {
            let scan = match (quick, deep) {
                (true, _) => Scan::Quick,
                (_, true) => Scan::Deep,
                _ => Scan::Checksums,
            };
            to_stdout(|out| database::verify(out, &path, scan))?;
        }
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
            SideFileKind::Lock => "lock",
            SideFileKind::Leases => "leases",
            // A kind a newer persia-storage lists: printed, never mistaken for one of these.
            _ => "other",
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

#[cfg(test)]
mod tests {
    use anyhow::Context;

    use super::*;

    fn wrapped(e: impl std::error::Error + Send + Sync + 'static) -> anyhow::Error {
        Err::<(), _>(e).context("verifying x").unwrap_err()
    }

    #[test]
    fn only_corruption_exits_with_its_status() {
        assert!(is_corruption(&wrapped(DatabaseError::CorruptTail {
            offset: 8
        })));
        assert!(!is_corruption(&wrapped(DatabaseError::Poisoned)));
        assert!(!is_corruption(&wrapped(ContainerError::Locked)));
        // Context around a Persia error does not hide it; the outer one decides for its sources.
        assert!(!is_corruption(
            &wrapped(DatabaseError::Container(ContainerError::Locked)).context("a")
        ));
        assert!(!is_corruption(&anyhow::anyhow!("no such file")));
        assert_eq!(status_of(&anyhow::anyhow!("no such file")), 1);
    }

    fn bad_magic() -> FormatError {
        FormatError::Corrupt {
            offset: 0,
            reason: Corruption::BadMagic,
        }
    }

    #[test]
    fn a_file_that_is_not_a_container_exits_with_4() {
        let magic = wrapped(ContainerError::Format(bad_magic()));
        // The format calls a wrong magic corruption (SPEC §4.2); the CLI still exits 4, not 3.
        assert!(is_corruption(&magic));
        assert!(is_not_container(&magic));
        assert_eq!(status_of(&magic), EXIT_NOT_CONTAINER);

        let through_open = wrapped(DatabaseError::Container(
            ContainerError::Format(bad_magic()),
        ))
        .context("opening x");
        assert!(is_not_container(&through_open));
        assert_eq!(status_of(&through_open), EXIT_NOT_CONTAINER);

        // Shorter than the magic: there is no container to be damaged.
        let short = wrapped(ContainerError::Format(FormatError::Corrupt {
            offset: 0,
            reason: Corruption::UnexpectedEof {
                needed: 8,
                available: 0,
            },
        }));
        assert!(is_not_container(&short));
        assert_eq!(status_of(&short), EXIT_NOT_CONTAINER);

        // The magic matched, then the header ended: a truncated container, exit 3.
        let truncated = wrapped(ContainerError::Format(FormatError::Corrupt {
            offset: 8,
            reason: Corruption::UnexpectedEof {
                needed: 2,
                available: 1,
            },
        }));
        assert!(!is_not_container(&truncated));
        assert_eq!(status_of(&truncated), EXIT_CORRUPT);

        let checksum = wrapped(ContainerError::Format(FormatError::Corrupt {
            offset: 12,
            reason: Corruption::ChecksumMismatch {
                stored: 1,
                computed: 2,
            },
        }));
        assert!(!is_not_container(&checksum));
        assert_eq!(status_of(&checksum), EXIT_CORRUPT);
    }
}
