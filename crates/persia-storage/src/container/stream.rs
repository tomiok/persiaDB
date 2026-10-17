//! Appending a frame whose payload is written in pieces (ROADMAP 2.4.13; SPEC §4.4 "Streaming and ranged access").

use persia_format::frame::{FrameEncoder, FrameKind};

use super::{Container, ContainerFile, Error, FrameRef, Result};

impl<F: ContainerFile> Container<F> {
    /// Starts appending a frame of `kind` whose payload is exactly `payload_len` bytes, written in chunks through
    /// the returned [`FrameWriter`], so a large Segment never has to be held in memory (SPEC §4.4). The payload is
    /// stored uncompressed: a compressed length is unknown until compression ends.
    ///
    /// The frame gets the next `frame_seq` and goes at [`Container::end`], like [`Container::append`]; it is part of
    /// the container only once [`FrameWriter::finish`] returns. A writer dropped before that, or a failed write,
    /// poisons the handle: a partial frame is on the file, and only a reopen's recovery may decide what it is.
    ///
    /// ```
    /// use persia_format::frame::FrameKind;
    /// use persia_storage::container::Container;
    ///
    /// # let dir = tempfile::tempdir()?;
    /// let mut c = Container::create(dir.path().join("app.persia"))?;
    /// let mut w = c.append_stream(FrameKind::Segment, 11)?;
    /// w.write(b"hello ")?;
    /// w.write(b"world")?;
    /// let segment = w.finish()?;
    /// c.sync()?;
    /// assert_eq!(c.read_frame(segment.offset, 1 << 20)?.payload, b"hello world");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`Error::ReadOnly`], [`Error::Poisoned`], [`Error::InvalidInput`] once `frame_seq` or the file offset would
    /// be exhausted, or [`Error::Io`] writing the header (which poisons the handle).
    pub fn append_stream(
        &mut self,
        kind: FrameKind,
        payload_len: u64,
    ) -> Result<FrameWriter<'_, F>> {
        self.writable()?;
        let frame_seq = self.next_frame_seq;
        // Fails exactly when `frame_seq = u64::MAX`, which is never written (SPEC §4.4).
        if frame_seq.checked_add(1).is_none() {
            return Err(Error::InvalidInput("frame_seq exhausted (ADR-0013)".into()));
        }
        let encoder = FrameEncoder::new(kind, frame_seq, payload_len)
            .map_err(|_| Error::InvalidInput("container offset exhausted".into()))?;
        let offset = self.end;
        if offset.checked_add(encoder.total_len()).is_none() {
            return Err(Error::InvalidInput("container offset exhausted".into()));
        }
        let header = *encoder.header();
        // The writer exists before the first write, so even a failed header write leaves it to poison the handle.
        let mut writer = FrameWriter {
            container: self,
            encoder: Some(encoder),
            frame: FrameRef {
                offset,
                len: 0,
                kind,
                frame_seq,
            },
            at: offset,
        };
        writer.write_raw("append", &header)?;
        Ok(writer)
    }
}

/// A frame being appended in pieces, from [`Container::append_stream`]. It borrows the container, so nothing else is
/// appended meanwhile.
///
/// Dropping it before [`FrameWriter::finish`] poisons the container ([`Error::Poisoned`]).
#[derive(Debug)]
pub struct FrameWriter<'a, F: ContainerFile> {
    container: &'a mut Container<F>,
    /// `None` once finished.
    encoder: Option<FrameEncoder>,
    frame: FrameRef,
    /// Where the next bytes go.
    at: u64,
}

impl<F: ContainerFile> FrameWriter<'_, F> {
    /// Payload bytes still expected.
    pub fn remaining(&self) -> u64 {
        self.encoder.as_ref().map_or(0, FrameEncoder::remaining)
    }

    /// Writes the next payload bytes. Not durable until [`Container::sync`] after [`FrameWriter::finish`].
    ///
    /// # Errors
    /// [`Error::Poisoned`] after an earlier failure, [`Error::Encode`] if `chunk` would take the payload past its declared length (nothing is written, and the
    /// writer stays usable), or [`Error::Io`], which poisons the container.
    pub fn write(&mut self, chunk: &[u8]) -> Result<()> {
        // After a failed write the handle is poisoned: nothing more goes to the file.
        self.container.writable()?;
        let Some(encoder) = self.encoder.as_mut() else {
            return Err(Error::Poisoned);
        };
        encoder.update(chunk)?;
        self.write_raw("append", chunk)
    }

    /// Writes the padding, CRC and `reserved` bytes and adds the frame to the container: it then counts like an
    /// [`append`](Container::append)ed frame, waiting for [`Container::sync`].
    ///
    /// # Errors
    /// [`Error::Encode`] if fewer payload bytes were written than declared, or [`Error::Io`]; both poison the
    /// container, since a partial frame is on the file.
    pub fn finish(mut self) -> Result<FrameRef> {
        self.container.writable()?;
        let encoder = self.encoder.take().ok_or(Error::Poisoned)?;
        let total = encoder.total_len();
        let trailer = match encoder.finish() {
            Ok(trailer) => trailer,
            Err(err) => {
                self.container.poisoned = true;
                return Err(err.into());
            }
        };
        self.write_raw("append", &trailer)?;
        let frame = FrameRef {
            len: total,
            ..self.frame
        };
        let c = &mut *self.container;
        debug_assert_eq!(self.at, frame.end());
        c.frames.push(frame);
        c.end = self.at;
        // `append_stream` checked that this fits.
        c.next_frame_seq = frame.frame_seq.saturating_add(1);
        c.dirty = true;
        Ok(frame)
    }

    /// Writes `bytes` at the current position; a failure poisons the container.
    fn write_raw(&mut self, op: &'static str, bytes: &[u8]) -> Result<()> {
        let at = self.at;
        self.container.write_op(op, |f| f.write_all_at(bytes, at))?;
        // Within the total length `append_stream` checked.
        self.at = at.saturating_add(bytes.len() as u64);
        Ok(())
    }
}

impl<F: ContainerFile> Drop for FrameWriter<'_, F> {
    fn drop(&mut self) {
        if self.encoder.is_some() {
            // An unfinished frame is on the file after `end`; the next append would land on top of it and leave its
            // remains behind, which recovery would read as corruption (SPEC §4.4).
            self.container.poisoned = true;
        }
    }
}
