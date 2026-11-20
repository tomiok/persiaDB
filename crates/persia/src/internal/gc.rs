//! Garbage collection of the blob store (SPEC §9.5; ROADMAP 9.5.1, 9.5.2, 9.5.8): the mark over the engine's live
//! references and the pin set, then the sweep. Filled in by the GC batch.
