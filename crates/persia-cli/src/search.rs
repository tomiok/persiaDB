//! `persia search` (ROADMAP 10.4.2): one search of a collection, for quick experiments, through the embedded API
//! (`persia::Db` opened read-only, `Collection::search` and `search_json`).
//!
//! **The query.** `--query <TEXT>` searches the user text (SPEC §7.1.1) with the defaults: the collection's
//! searchable fields, its ranking and relaxation (so, by default, never zero results when the collection has a
//! document, SPEC §7.6), the default page of 20 hits (SPEC §7.4). `--request <JSON>` takes a whole request JSON
//! instead (SPEC §7.1.4: `q`, `fields`, `filter`, `sort`, `limit`, `relax`, ...). Exactly one of the two is given.
//!
//! **The database.** It opens read-only, without the writer lock (SPEC §8.4), so a live writer may have it open: the
//! search sees what that writer had made durable when the command started. As every read-only handle of the embedded
//! API it takes a reader lease beside the blob pack (SPEC §9.3), so it creates `<name>.persia.blobs.leases/` where
//! there is none. A query is analyzed with the analyzers of the collection's schema, and the command has the
//! built-in ones only: a schema that names a registered analyzer (SPEC §6.2.2) fails the open (status 1).
//!
//! **Output.** Text: `key  value` rows (`hits`, `relaxation` with the level and what each step applied, `truncated`
//! when the relaxation budget or the query limits cut something), an empty line, then one line per hit, best first:
//! its rank, its score and its document in the output form of SPEC §6.4.1, tab-separated. `--json` writes the
//! response JSON of SPEC §7.5.2 on one line (`Collection::search`'s `SearchResponse::to_json`: `hits`, `facets`,
//! `relaxation`, `query_truncated`).

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use persia::{Collection, Db, OpenOptions, SearchRequest, SearchResponse};

/// What to search: the user text or a request JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// `--query`: the user text with every default.
    Text(String),
    /// `--request`: a request JSON (SPEC §7.1.4).
    Request(String),
}

impl Query {
    /// The query of `--query` (`text`) or `--request` (`request`): exactly one is given.
    pub fn of(text: Option<String>, request: Option<String>) -> Result<Self> {
        match (text, request) {
            (Some(text), None) => Ok(Self::Text(text)),
            (None, Some(request)) => Ok(Self::Request(request)),
            _ => bail!("give exactly one of --query and --request"),
        }
    }
}

/// Searches the collection `collection` of the database at `path` and writes the hits to `out`, as the response JSON
/// with `json`.
pub fn search(
    out: &mut dyn Write,
    path: &Path,
    collection: &str,
    query: &Query,
    json: bool,
) -> Result<()> {
    if !path.is_file() {
        bail!("{} is not a container file", path.display());
    }
    let db = OpenOptions::new()
        .read_only(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let found = find(&db, path, collection)?;
    let response = match query {
        Query::Text(text) => found.search(&SearchRequest::new(text.as_str())),
        Query::Request(request) => found.search_json(request),
    }
    .with_context(|| format!("searching the collection {collection:?}"))?;
    if json {
        writeln!(out, "{}", response.to_json()?)?;
    } else {
        write_text(out, &found, &response)?;
    }
    db.close()
        .with_context(|| format!("closing {}", path.display()))
}

/// The collection `name` of `db`, or an error naming the ones there are.
fn find(db: &Db, path: &Path, name: &str) -> Result<Collection> {
    let names = db.collection_names()?;
    if names.iter().any(|n| n == name) {
        return Ok(db.collection(name)?);
    }
    let names: Vec<String> = names.iter().map(|n| format!("{n:?}")).collect();
    bail!(
        "{} has no collection {name:?} (it has: {})",
        path.display(),
        if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(", ")
        }
    );
}

fn write_text(
    out: &mut dyn Write,
    collection: &Collection,
    response: &SearchResponse,
) -> Result<()> {
    let relaxation = &response.relaxation;
    let level = if relaxation.applied.is_empty() {
        relaxation.level.to_string()
    } else {
        format!("{} ({})", relaxation.level, relaxation.applied.join(", "))
    };
    let mut rows = vec![
        ("hits", response.hits.len().to_string()),
        ("relaxation", level),
    ];
    let truncated: Vec<&str> = [
        (relaxation.truncated, "relaxation budget"),
        (response.query_truncated, "query"),
    ]
    .into_iter()
    .filter_map(|(cut, what)| cut.then_some(what))
    .collect();
    if !truncated.is_empty() {
        rows.push(("truncated", truncated.join(", ")));
    }
    for (key, value) in rows {
        writeln!(out, "{key:<18}{value}")?;
    }
    writeln!(out)?;
    let schema = collection.schema()?;
    for (rank, hit) in response.hits.iter().enumerate() {
        let document = hit.document.to_json(&schema)?;
        writeln!(
            out,
            "{}\t{:.4}\t{document}",
            rank.saturating_add(1),
            hit.score
        )?;
    }
    Ok(())
}
