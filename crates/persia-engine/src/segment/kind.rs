//! Section kinds and how each is stored (SPEC §5.3, ROADMAP 5.2.9).

/// The `field_id` of a section that belongs to the whole segment rather than one field (SPEC §5.3).
pub const SEGMENT_FIELD: u32 = u32::MAX;

/// What a section holds; its `section_kind` code in the section table (SPEC §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SectionKind {
    /// Term → term-info index, an FST (SPEC §5.3.1).
    TermDict,
    /// Fixed-width term records (SPEC §5.3.1).
    TermInfo,
    /// Docids, in 128-blocks (SPEC §5.2.1).
    Postings,
    /// Term frequencies, blocks aligned with `Postings` (SPEC §5.3.2).
    Freqs,
    /// Per-block skip and block-max entries (SPEC §5.3.2).
    Skip,
    /// Token positions (SPEC §5.3.3).
    Positions,
    /// Quantized field lengths (SPEC §5.3.4).
    Norms,
    /// Doc values (SPEC §5.3.5).
    Columns,
    /// Stored fields, one row per document (SPEC §5.3.6).
    DocStore,
    /// Document id → docid, an FST (SPEC §5.3.6).
    IdMap,
    /// Docid → document id (SPEC §5.3.6).
    IdList,
    /// Docid → blob reference (SPEC §5.3.6).
    BlobRefs,
}

impl SectionKind {
    /// Every kind, in code order.
    pub const ALL: [Self; 12] = [
        Self::TermDict,
        Self::TermInfo,
        Self::Postings,
        Self::Freqs,
        Self::Skip,
        Self::Positions,
        Self::Norms,
        Self::Columns,
        Self::DocStore,
        Self::IdMap,
        Self::IdList,
        Self::BlobRefs,
    ];

    /// The kind's code on disk.
    pub const fn code(self) -> u16 {
        match self {
            Self::TermDict => 1,
            Self::TermInfo => 2,
            Self::Postings => 3,
            Self::Freqs => 4,
            Self::Skip => 5,
            Self::Positions => 6,
            Self::Norms => 7,
            Self::Columns => 8,
            Self::DocStore => 9,
            Self::IdMap => 10,
            Self::IdList => 11,
            Self::BlobRefs => 12,
        }
    }

    /// The kind with code `code`, if any.
    pub const fn from_code(code: u16) -> Option<Self> {
        Some(match code {
            1 => Self::TermDict,
            2 => Self::TermInfo,
            3 => Self::Postings,
            4 => Self::Freqs,
            5 => Self::Skip,
            6 => Self::Positions,
            7 => Self::Norms,
            8 => Self::Columns,
            9 => Self::DocStore,
            10 => Self::IdMap,
            11 => Self::IdList,
            12 => Self::BlobRefs,
            _ => return None,
        })
    }

    /// Whether the section is block-framed (SPEC §5.2). The others, the two FSTs, are their bytes followed by a
    /// CRC32C of them, since an FST is read whole and in place.
    pub const fn is_block_framed(self) -> bool {
        !matches!(self, Self::TermDict | Self::IdMap)
    }

    /// Whether the section belongs to the segment ([`SEGMENT_FIELD`]) rather than to a field.
    pub const fn is_segment_level(self) -> bool {
        matches!(
            self,
            Self::DocStore | Self::IdMap | Self::IdList | Self::BlobRefs
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip_and_are_ordered() {
        for (i, kind) in SectionKind::ALL.into_iter().enumerate() {
            assert_eq!(usize::from(kind.code()), i + 1);
            assert_eq!(SectionKind::from_code(kind.code()), Some(kind));
        }
        assert_eq!(SectionKind::from_code(0), None);
        assert_eq!(SectionKind::from_code(13), None);
        assert!(SectionKind::ALL.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn only_the_fsts_are_raw() {
        let raw: Vec<_> = SectionKind::ALL
            .into_iter()
            .filter(|k| !k.is_block_framed())
            .collect();
        assert_eq!(raw, [SectionKind::TermDict, SectionKind::IdMap]);
    }
}
