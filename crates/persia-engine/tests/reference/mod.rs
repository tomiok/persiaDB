//! The naive in-memory reference implementation the engine is tested against differentially (CLAUDE.md, SPEC §7.2):
//! written from the SPEC text alone, over plain documents, with no index, no quantization tables and no shortcuts.

pub mod bm25f;
