# ADR-0009: Frame validation, scan end classes and compressed frame payloads

- Status: accepted
- Date: 2026-10-11
- Related: SPEC §4.4, §5.2, §15; ADR-0005, ADR-0007; ROADMAP 1.7, 2.4.4, 2.4.9

## Context

SPEC §4.4 gave the frame layout and said that "any frame failing checksum or length validation terminates the
scan". Implementing the frame scanner (ROADMAP 1.7) left four things undefined:

1. **Unknown `flags` bits.** Unknown kinds are skipped, but nothing said what a reader does with a flag it does not
   know. A flag can change how the payload must be read (bit0 means zstd), so skipping the bit is not safe.
2. **Why a scan stopped.** ROADMAP 1.7.2.1 asks to tell a clean end, a torn tail and mid-file corruption apart.
   Recovery truncates in every case, but a torn tail is the normal result of a crash during an append, while
   corruption means data that was written is gone. Operators need to know which one happened.
3. **Compressed payloads.** Flags bit0 marks a zstd payload, but the frame header has no uncompressed length, so a
   reader had no bound to check before it decompresses (SPEC §17, decompression bombs). SPEC §5.2 listed this as
   open.
4. **Stale frames after truncation.** Truncation was only "logical". If a writer appends at the truncation point
   without shortening the file, valid frames from before the crash can sit right behind the new ones. A later scan
   would accept them.

No container has been written yet, so SPEC §4.4 is amended in place (SPEC §15, ADR-0008).

## Decision

1. **Validation order**: (1) at least 32 bytes and the right magic, (2) total length fits in `u64` and in the
   input, (3) CRC, (4) zero padding and `reserved`. Any failure makes the frame *invalid*. A frame that passes and
   sets an unknown `flags` bit is *unsupported*. It is never truncated, because it was fully written by a newer
   writer. New flags also need an `incompat_flags` bit (SPEC §15), so this case only arises with a writer that
   broke that rule. Unknown kinds, kind 0 included, are skipped as before.
2. **Scan end classes**, decided only from the bytes, at the offset `o` where the scan stopped:
   - *clean end*: the input ends exactly at a frame boundary;
   - *torn tail*: fewer than 32 bytes remain, or the frame's length runs past the end of the input, or nothing but
     zeros follows the frame's end. A frame with a bad magic has no known end, so it is a torn tail only if every
     byte from `o` is zero;
   - *corruption*: an invalid frame followed by at least one non-zero byte;
   - *unsupported*: as above.

   This follows the usual crash model for appends: sectors are written whole or not at all, and a file can grow
   before its data reaches the disk, in which case the missing data reads as zeros. Under that model a crash leaves
   either a short file or zeros, which is a torn tail. Non-zero bytes after an invalid frame mean that something
   written later survived, so data was lost. That is either bit rot, or writeback that reordered frames which were
   never synced (and so never acknowledged). The class is for reporting. Recovery truncates at `o` for torn tails
   and corruption alike.
3. **Compressed payload layout**: `uncompressed_len u64`, then exactly one zstd frame that follows the `Zstd` block
   rules of SPEC §5.2 (declared content size equals `uncompressed_len`, no skippable frames, no trailing bytes).
   The reader passes a per-kind maximum and `decompress_block` rejects a larger value before allocating. Scanning
   checks framing only. A bad zstd payload inside a CRC-valid frame is corruption and is found when the payload is
   decoded.
4. **Physical truncation before appending**: after recovery stops at `o`, the writer truncates the file to `o`
   and syncs it before appending (ROADMAP 2.4.4). What `frame_seq` numbers, and whether a scan should also stop at
   a `frame_seq` that does not increase, is left to ROADMAP 2.4.9, which owns the WAL and rewrite semantics it
   depends on.

## Consequences

- A newer writer's frame is never destroyed by an older reader. The older reader refuses to open the container
  instead.
- The torn-tail check reads the rest of the input once, when the scan stops. That is linear in the tail and only
  happens on the failure path.
- Under the crash model, a torn tail is never reported as corruption. The reverse can happen: a damaged length
  field can push a frame's declared end past the end of the file, and that frame is reported as a torn tail. Both
  are truncated the same way, so only the report differs.
- A compressed frame costs 8 more bytes than the zstd frame. Segment frames gain nothing from it, since their
  sections already compress per block and are read in place, but the format does not forbid it.
- Readers must choose a bomb limit per kind. The engine sets these limits when it defines each kind's payload.

## Alternatives considered

- **Unknown flags are corruption**: recovery would then truncate a newer writer's valid frames, and everything
  after them.
- **Ignore unknown flags**: a reader could misread a payload whose encoding the flag changes.
- **Classify by searching for a later valid frame** ("valid data follows" means corruption): an adversarial input
  makes this quadratic, since every 8-byte offset can hold a magic with a long length. It also adds nothing under
  the crash model, where a torn region is zeros.
- **Use only the zstd frame's declared content size**: the length would be read from inside the compressed data.
  An explicit field matches the block header (SPEC §5.2), and the bound check stays independent of the codec.
- **Make the scanner check `frame_seq`**: it would need rules that do not exist yet (first value, rewrites);
  deferred to ROADMAP 2.4.9.
