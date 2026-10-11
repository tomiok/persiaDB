# ADR-0007: Container header and superblock validation

- Status: accepted (amended by ADR-0008: slot geometry, `page_size`, `format_minor` on rewrite)
- Date: 2026-10-11
- Related: SPEC §4.2, §4.3, §15, §17; ROADMAP 1.6.1–1.6.3, 2.4.1, 2.4.4

## Context

SPEC §4.2–4.3 gave the byte layout of the container header and the two superblock slots, but not how a reader
validates them: whether reserved bytes must be zero, which `page_size` values are legal, whether an unknown
`format_major` is corruption, and what to do when both slots carry the same `seq`. No format version has been
released, so this fixes the rules rather than changing the layout.

## Decision

1. **Check order for the header**: magic, then `format_major`, then `header_crc`, then the fields. A future major may
   move every field including the CRC, so the major is read from the raw bytes before the CRC is verified; a
   mismatched CRC in a file with an unknown major is reported as *unsupported*, not as corruption.
2. **A separate error class, `Unsupported`**, for an unknown `format_major` or unknown `incompat_flags` bits. The file
   may be fine; this build is too old. It is neither corruption nor a caller error, and callers can tell users
   to upgrade instead of to restore a backup.
3. **Unknown `rocompat_flags` bits are not an error**: decoding succeeds and reports that the container must be
   opened read-only. No incompat or rocompat bits are assigned yet; a writer sets only bits it knows.
4. **`page_size` is a power of two in `4096..=65536`**. The header and slots occupy fixed offsets whatever the page size, so
   smaller pages buy nothing; larger ones are not useful for mmap or `O_DIRECT` and would only widen what a
   hostile header can ask for.
5. **Reserved bytes are zero** (header 52..4096; slot 48..2044, 48..4092 since ADR-0008), and non-zero reserved bytes are
   corruption, as for frames (SPEC §4.4). New fields arrive with a flag or a minor bump, which tells readers how
   to read them; garbage there means the writer was broken.
6. **Slot selection** is a pure function of the two decode results: the valid slot with the higher `seq` wins;
   with equal `seq`, byte-identical slots pick A (a freshly created container initialises both slots the same,
   ROADMAP 2.4.1) and different ones are corruption, since two commits never share a `seq` and nothing says which
   slot is right. Both invalid is corruption ("no valid superblock").
7. **Slot decoding is structural only** (CRC and reserved bytes). Offsets (`manifest_off`, `log_end`, ...) are
   checked against the actual container by the container reader (ROADMAP 2.4.4), which knows the file length.

## Consequences

- A bit flip in `format_major` reads as "unsupported", not "corrupt". Accepted: the alternative risks calling a
  newer, valid file corrupt, which invites users to "repair" it. Major 0 is included: one rule ("any major but
  1"), and a header with a valid magic and major 0 was never written by any build.
- An equal-`seq` conflict makes a container unopenable even though one slot may be right. Correctness over
  availability (CLAUDE.md pillar 5); `persia verify` can show both slots.
- Slot selection never looks at offsets, so a slot with a valid CRC and absurd offsets is selected and then
  rejected by recovery rather than falling back to the other slot. Falling back would silently drop the
  latest commit; a CRC-valid slot with wrong contents is a writer bug that must surface.
- `read_active` reports only that no slot is valid, not why; diagnostics decode each slot on its own.

## Open questions (resolved by ADR-0008)

- **Slot geometry.** Both slots share the 4 KiB page 4096..8192, so a torn or unreadable sector on a 4Kn device can
  lose both at once; LMDB keeps its two meta pages on separate pages. Moving slot B to its own aligned page (and
  the first frame to 12288) is a layout change and gets its own ADR.
- **What `page_size` governs**, given that the header, slot and first-frame offsets are fixed.
- **`format_minor` on rewrite**: whether a writer of minor *m* may rewrite (e.g. compact, SPEC §4.6) a container
  of a newer minor, and which minor it records.

## Alternatives considered

- **Verify the CRC first, then the major** (what `Checksummed<T>` alone does): misreports every future-major file as
  corrupt.
- **Prefer slot B (or the later-written slot) on equal `seq`**: the reader cannot know which was written later.
- **Fall back to the other slot when the active one has bad offsets**: hides writer bugs and can roll back a
  durable commit.
