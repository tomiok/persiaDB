# ADR-0017: Mapped container reads: an opt-in window over the committed frames

- Status: proposed
- Date: 2026-10-17
- Related: SPEC §4.4, §4.6, §4.7; amends ADR-0010 (decisions 2 and 6); ADR-0016; ROADMAP 2.3.6, 2.3.10

## Context

ADR-0010 allowed one module outside `persia-format` to call `MmapFile::map`: "the local backend's mapped-read
module", for ranges SPEC §4.7 makes immutable. It left three questions to ROADMAP 2.3.6: where exactly the call
site lives, how much to map at once (one mapping per frame, or one window over `12288..log_end`), and when to map
(opt-in, or automatically on local disk file systems).

## Decision

1. **Call site.** `persia_storage::container::mapped` is the only module outside `persia-format` with `unsafe`. The
   crate root is `#![deny(unsafe_code)]` instead of `forbid` so that module can `allow` it; every other module of the
   crate still rejects it. The mapping function is private to the container and only ever gets the container's own
   file and a superblock the container read or committed on it, so no safe caller can map an arbitrary range.
2. **One window over `12288..log_end`.** A container with mapped reads holds a single read-only mapping of its
   committed frames. A frame read takes the frame's bytes from the window if the window covers them and reads them
   with `pread` otherwise (frames after `log_end`, which recovery may still truncate). After a commit the window is
   mapped again up to the new `log_end` (mappings never grow in place, ADR-0010); after an in-place compaction it is
   mapped on the new file. A failed remap after a commit keeps the old window, which stays valid for the frames it
   covers; after a compaction the old window is dropped, since its offsets belong to the old file.
3. **Opt-in only, for now.** `Container::<File>::enable_mapped_reads` turns it on; the default stays `pread`.
   Choosing automatically needs the file system type (`statfs`/`f_type` on Linux, `MNT_LOCAL` on the BSDs and macOS,
   `GetDriveTypeW` on Windows), which the standard library does not expose and which would need FFI (`unsafe`) or a
   new dependency. ROADMAP 2.3.10 tracks it; until then the embedder decides.
4. **Zero-copy reads.** `Container::read_frame_source` returns the payload as a `ByteSource`: for an uncompressed
   frame, a slice of the window (or of the `pread` buffer) at its absolute file offset; for a compressed one, the
   decompressed bytes at offset 0, since they lie nowhere in the file. Every read still validates the frame (header,
   CRC, bomb limit): mapped bytes are trusted no more than read ones.

## Consequences

- One `mmap` per commit, not per frame: few mappings (one VMA), and the cost of mapping does not grow with the
  number of frames. A reader that holds any slice keeps its whole window alive (ADR-0010); remapping after a commit
  briefly keeps two windows over the same pages.
- The window needs address space for all committed frames. A 32-bit process may fail to map a large container;
  `enable_mapped_reads` then returns the error, and a failed remap silently falls back to `pread` for newer frames.
- On Windows a mapping covers the whole file at the time it was made (memmap2 creates the file mapping object
  with size 0), and Windows refuses to shrink a file below a live mapping object. While a reader holds a window, a
  read-write open elsewhere that needs to truncate a torn or corrupt tail fails with an I/O error and changes
  nothing; it succeeds once the mapping is gone. This costs availability, not safety. Unix is unaffected: the tail
  lies past the window.
- Measured with `cargo bench -p persia-storage --bench container_read` (warm page cache, Linux): a 4 KiB frame
  2.2 µs with `pread`, 0.78 µs mapped; a 1 MiB frame 252 µs and 164 µs (the CRC dominates large frames).

## Alternatives considered

- **One mapping per frame**: maps only what is read, but costs an `mmap`/`munmap` and a VMA per frame, and pays the
  syscall on every uncached read, which is what mapping avoids.
- **Remapping lazily on read**: needs interior mutability (a lock or `ArcSwap`) on the read path; remapping in
  `commit`, which already takes `&mut self`, needs neither.
- **A mapping method on `ContainerFile`**: the trait is public, so it would be a safe way to map any file range,
  the unsoundness ADR-0010 rejected.
- **Automatic choice now, by parsing `/proc/self/mountinfo`**: Linux only, and racy against remounts; not worth it
  before 2.3.10 picks a portable approach.
