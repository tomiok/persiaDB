# ADR-0012: Local backend layout: encoded file names, directory marker, link-based `put_if_absent`

- Status: accepted
- Date: 2026-10-14
- Related: SPEC §10.1, §10.1.1; ROADMAP 2.3.1–2.3.5, 2.3.7, 2.4.8, 2.4.11

## Context

The local filesystem backend (`file:///…`) stores one object per file. Keys are a flat namespace (SPEC §10.1):
`a` and `a/b` may both exist, but a directory cannot hold a file and a subdirectory with the same name. Keys are
opaque bytes and must come back exactly as written, but filesystems rewrite names: HFS+ stores names in NFD, APFS
and NTFS are case-insensitive (`A` and `a` are one file), and Windows reserves names such as `con`. The layout is
persistent: once a build writes it, later builds must read it, so it is decided here and pinned by a golden test.

## Decision

1. **Path.** Key `s1/…/sn` is the file `<root>/d(s1)/…/d(sn-1)/f(sn)`. `f(s)` encodes the segment; `d(s) = f(s) + "@"`.
2. **Encoding.** `a`–`z`, `0`–`9`, `.`, `_`, `-` are kept; every other UTF-8 byte becomes `%XX` with uppercase hex.
   File names are ASCII, so normalization has nothing to change. Under case folding, two canonical encodings never
   collide: letters outside escapes are always lowercase and hex digits inside them always uppercase, and the escape
   positions are fixed by the `%`s, which fold to themselves. Decoding accepts only canonical names (re-encoding
   gives the name back), so each key has exactly one file name and foreign files are skipped by `list`.
3. **Directory marker.** `@` is always escaped inside a segment, so a name ending in `@` is a directory and never an
   object. The marker goes on directories rather than objects so that object names, the ones users see in a file
   manager (`1.seg`, `00000000000000000001.mf`), stay unchanged.
4. **Limits on the encoded name.** A name over 255 bytes is `InvalidInput`. This replaces the earlier "segments over
   255 bytes" rule: the limit is the filesystem's, and it applies to what the filesystem sees. A segment of plain
   characters therefore fits in 255 bytes as the last segment and in 254 before a `/`; an escaped byte costs three.
   On Windows, reserved names and names ending in `.` are rejected (uppercase letters and spaces are escaped, so
   `CON` and a trailing space never reach the filesystem; only lowercase `con` and a trailing `.` can).
5. **Temporary files** are named `@tmp.<pid>.<counter>` in the target directory (so `rename` and `link` stay within
   one filesystem). They start with `@`, which no encoded name does, and `list` skips them.
6. **Writes.** `put`: write temp, fsync, `rename` over the object, fsync the directory. `put_if_absent`: write temp,
   fsync, hard-link it to the object name (fails atomically if the name exists, also across processes), remove the
   temp, fsync the directory. On `AlreadyExists` the existing bytes are compared with the caller's (SPEC §10.1).
   Directories are created on first use, and the parent of every directory on the path is fsynced once per store
   instance (remembered in memory), whoever created it: otherwise an object acknowledged by one writer could be lost
   with a directory entry a concurrent writer created but had not synced yet. Directories are never removed.
7. **Reads** use positioned reads on an open file; a rename never changes the bytes of an open file, so `get` and
   `get_range` see one version. Memory-mapped reads are not part of this backend (ROADMAP 2.3.6 lands with the
   container, ADR-0010).

## Consequences

- Plain lowercase keys, which is every key the engine writes (SPEC §10.4), are stored under readable names.
  Uppercase and non-ASCII keys are stored and listed correctly, at three bytes per escaped byte.
- A store copied between Linux, macOS and Windows keeps every key, except the Windows-only rejections in 4.
- `put_if_absent` needs hard links. FAT and some network filesystems lack them and get an `Io` error there; the
  commit protocol needs an atomic create, so emulating it with a check-then-create is not an option (SPEC §10.1).
- Empty directories accumulate after deletes. This is harmless (`list` finds no objects there), and removing them
  would race with a `put` into the same directory.
- A crash can leave temporary files behind. They are invisible to `list` and hold no acknowledged data; sweeping
  them is a follow-up (ROADMAP 2.3.8).
- Windows cannot fsync a directory through the standard library; there the rename's durability follows NTFS
  journaling. ROADMAP 2.4.8 documents and tests it.

## Alternatives considered

- **Marker on objects instead of directories** (`a~` for the object): equally unambiguous, but changes every
  visible file name and moves the 254-byte limit to the last segment, which is the one users name.
- **Hash-named files with the key stored inside**: no name limits, but `list` must open every file and the layout
  is opaque to humans and tools.
- **Percent-encoding only what the filesystem forbids**: shorter names, but keeps uppercase and non-ASCII letters,
  which collide on case-insensitive filesystems and change under NFD normalization.
- **`O_EXCL` create of the final name for `put_if_absent`**: atomic, but readers would see a partly written object
  before the data is complete; `link` publishes a complete, synced file.
