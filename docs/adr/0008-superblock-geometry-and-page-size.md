# ADR-0008: Superblock slots on separate pages, advisory page size, monotonic format minor

- Status: accepted
- Date: 2026-10-11
- Related: SPEC §4.2, §4.3, §4.4, §4.6, §15; ADR-0007; ROADMAP 1.6.4, 2.4.1, 2.4.3

## Context

ADR-0007 left three questions open:

1. **Slot geometry.** SPEC §4.2 put both 2048-byte superblock slots in one 4 KiB page (4096..8192). The A/B scheme
   only works if a failed write to one slot cannot damage the other. On a 4Kn device, or any device that writes a
   4 KiB unit atomically or not at all, power loss during a commit can leave that whole sector unreadable. Both
   slots are then gone and the container cannot be opened. LMDB, which §4.3 follows, keeps its two meta pages on
   separate pages for this reason.
2. **`page_size`.** The header has a `page_size` field, but no offset depended on it.
3. **`format_minor` on rewrite.** Nothing said which minor a writer records when it rewrites a container written
   by a newer minor, for example during compaction (§4.6).

No container has been written by any build yet: the container adapter (ROADMAP 2.4) does not exist, and no format
version has been released. Changing the layout now costs nothing. This ADR therefore amends format major 1 instead
of bumping it, as ADR-0006 did for block framing. A new major would leave major 1 as a format that no build ever
wrote.

## Decision

1. **Each slot is a full 4096-byte page.** Slot A is at 4096 and slot B at 8192. Each slot is 4096 bytes, with the
   CRC at 4092 and bytes 48..4092 reserved. The first frame starts at 12288. The header page, slot A and slot B
   share no 4096-byte sector, so a torn or unreadable sector loses at most one of them. One commit writes one
   aligned page. 4096 bytes is the largest physical sector in common use. The offsets are fixed and do not depend
   on `page_size`, so a reader finds the slots without trusting the header.
2. **`page_size` is advisory.** It is the alignment a writer may give Segment frame payloads, using Padding frames
   (SPEC §4.4), so that a mapped segment starts on a page boundary. No offset a reader computes depends on it, and
   a container may be rewritten with a different value. ADR-0007's validation stays: a power of two in
   `4096..=65536`, otherwise corruption. The field is CRC-protected, so a bad value means a broken writer.
3. **`format_minor` never decreases.** A writer records `max(container minor, own minor)`, including when it
   rewrites the container. Minors are additive (SPEC §15): a writer of an older minor may write a newer-minor
   container, and any feature it could not maintain must instead be gated by a `rocompat_flags` bit, which makes
   that writer read-only. The field therefore names the newest minor that has written the container.
   `Header::for_write` applies the rule.

## Consequences

- The fixed prefix grows from 8 KiB to 12 KiB per container.
- A commit writes 4 KiB instead of 2 KiB. Both are a single page write with one `fdatasync`.
- Devices with sectors larger than 4 KiB could still put two pages in one sector. Such devices are rare and usually
  emulate 4 KiB writes. Supporting them would need slots placed by `page_size`, behind a new major.
- Readers cannot use `format_minor` to decide what to read. Feature flags are the only mechanism for that, as SPEC
  §15 intends.

## Alternatives considered

- **Keep 2048-byte slots and pad each to its own page**: the same geometry, but with 2 KiB of dead zeros per slot to
  validate. One slot per page is simpler.
- **Place the slots by `page_size`**: the slots could only be found after trusting the header's `page_size`, so a
  corrupt header would also hide both superblocks.
- **Drop `page_size`**: it is harmless and gives writers somewhere to record mmap alignment. Removing it would
  leave 4 reserved bytes and cost the hint.
- **Bump `format_major` to 2**: no major-1 container was ever written, so there is nothing to stay compatible with.
- **Record the writer's own minor (allow decreases)**: the container would then forget that a newer minor wrote
  it, which is useful for diagnosing compatibility problems.
