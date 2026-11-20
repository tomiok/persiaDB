//! The sweep of stale pack temporary files at a read-write open (SPEC §9.3, ADR-0049 item 7; ROADMAP 9.2.6).
//!
//! A pack creation writes `<pack>.tmp.<nonce>.<n>` ([`temp_path`](super::temp_path)) and links it to the pack's
//! name; a crash before its removal, or a removal that failed, leaves that name behind, as a crashed pack rewrite
//! (ROADMAP 9.5.2, which writes its new file under the same pattern) does. None of them is needed: a creation's is an
//! empty pack or a second name of the pack (removing a name never touches the file's data), and a rewrite's file is
//! renamed over the pack only once complete. Removing a rewrite's file relies on SPEC §18.10 (b): nothing is bound
//! into the new pack until its directory sync succeeds, and writing stops if that sync fails, so the file holds no
//! blob a reader can still name. The container's `sweep_stale_files` (SPEC §4.5.1) matches only the
//! container's own names, so the pack sweeps its own, with rules like the container's but stricter: only regular
//! files named exactly as [`temp_path`](super::temp_path) names them (lowercase hex only, where the container
//! accepts either case), and one directory sync after the removals.
//!
//! Only the container's writer opens the pack read-write (ADR-0049 item 8), and it creates or rewrites the pack only
//! through that handle, after the open: when the sweep runs, no creation or rewrite of this pack is in flight, so no
//! temporary file can be in use. That is why the pack needs no lock on its temporary files, unlike the container's
//! (whose creates take their own lock, SPEC §4.5.1).
//!
//! The pack's writer lock enforces that (ROADMAP 9.2.7): a read-write open takes it before it sweeps, on the pack file
//! or beside it. Before the pack exists there is no file to lock on Unix, so two handles of a pack not yet created
//! can both sweep; that case still fails safe: a temporary name the sweep removes before the other handle links (a
//! creation) or renames (a rewrite) makes that step fail, so it reports an error and publishes nothing. A creation
//! whose link already succeeded treats the missing temporary name as removed (`NotFound` is success) and goes on to
//! its directory sync.
//! Either way it never leaves a half-made pack at the pack's name.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::{ContainerFile, parent_of};
use persia_storage::vfs::{FileType, Vfs};

/// Removes the stale temporary files of the pack at `path` (see the module docs) and returns them, in name order.
///
/// Errors do not fail the open: a failed listing, removal or directory sync logs a `tracing::warn!` and the open
/// continues (a failed removal does not stop the others). What is left holds no acknowledged data. The next
/// read-write open does not retry that directory sync: a name a crash brings back is swept again. Refusing the
/// writer for a leftover name would stop the database over a file it never needs.
pub(super) fn sweep_stale_files<F: ContainerFile>(
    vfs: &dyn Vfs<File = F>,
    path: &Path,
) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    let dir = parent_of(path);
    let Some(pack) = path.file_name() else {
        return removed;
    };
    let entries = match vfs.read_dir(dir) {
        Ok(entries) => entries,
        // No directory yet: no pack and nothing beside it.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return removed,
        Err(e) => {
            warn(dir, &e, "list the pack's directory");
            return removed;
        }
    };
    let mut stale: Vec<PathBuf> = entries
        .into_iter()
        .filter(|e| e.kind == FileType::File && is_temp_name(&e.name, pack))
        .map(|e| dir.join(e.name))
        .collect();
    stale.sort();
    for file in stale {
        match vfs.remove_file(&file) {
            Ok(()) => removed.push(file),
            // Gone meanwhile: nothing to do.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn(&file, &e, "remove a stale pack temporary file"),
        }
    }
    // One sync makes the removals durable; without it a crash may bring a name back, for the next open to sweep.
    if !removed.is_empty()
        && let Err(e) = vfs.sync_dir(dir)
    {
        warn(dir, &e, "sync the pack's directory");
    }
    removed
}

fn warn(path: &Path, error: &std::io::Error, what: &'static str) {
    tracing::warn!(path = %path.display(), %error, "could not {what}; the next read-write open sweeps again");
}

/// Whether `name` is `<pack>.tmp.<16 lowercase hex digits>.<decimal counter>`, the name
/// [`temp_path`](super::temp_path) makes beside the pack named `pack`. Compared as bytes, so a pack name that is not
/// UTF-8 is swept too.
fn is_temp_name(name: &OsStr, pack: &OsStr) -> bool {
    let Some(rest) = name
        .as_encoded_bytes()
        .strip_prefix(pack.as_encoded_bytes())
        .and_then(|rest| rest.strip_prefix(b".tmp."))
    else {
        return false;
    };
    let Some((nonce, counter)) = rest.split_at_checked(16) else {
        return false;
    };
    let hex = |b: &u8| b.is_ascii_digit() || (b'a'..=b'f').contains(b);
    let Some(counter) = counter.strip_prefix(b".") else {
        return false;
    };
    nonce.iter().all(hex) && !counter.is_empty() && counter.iter().all(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_names_temp_path_makes_are_temporary() {
        let pack = OsStr::new("app.persia.blobs");
        let made = super::super::temp_path(Path::new("/db/app.persia.blobs"));
        assert!(is_temp_name(made.file_name().unwrap(), pack), "{made:?}");
        for name in [
            "app.persia.blobs.tmp.0123456789abcdef.0",
            "app.persia.blobs.tmp.ffffffffffffffff.18446744073709551615",
        ] {
            assert!(is_temp_name(OsStr::new(name), pack), "{name}");
        }
        for name in [
            "app.persia.blobs",
            "app.persia.blobs.tmp.0123456789abcdef",
            "app.persia.blobs.tmp.0123456789abcdef.",
            "app.persia.blobs.tmp.0123456789abcde.1",
            "app.persia.blobs.tmp.0123456789abcdef0.1",
            "app.persia.blobs.tmp.0123456789ABCDEF.1",
            "app.persia.blobs.tmp.0123456789abcdeg.1",
            "app.persia.blobs.tmp.0123456789abcdef.1x",
            "app.persia.blobs.compact.0123456789abcdef.1",
            "app.persia.blobs.quarantine.0000000000003028.0123456789abcdef.1",
            "app.persia.tmp.0123456789abcdef.1",
            "other.persia.blobs.tmp.0123456789abcdef.1",
            "app.persia.blobs.tmp.0123456789abcdef.1.tmp.0123456789abcdef.1",
        ] {
            assert!(!is_temp_name(OsStr::new(name), pack), "{name}");
        }
    }
}
