//! Analysis allocates nothing once a stream has grown (ROADMAP 4.4.4, 4.4.12; SPEC §6.3).
//!
//! A counting global allocator (ADR-0026) counts the allocations of the current thread. For every built-in analyzer,
//! analyzing a set of values with one `TokenStream` until a round allocates nothing must converge, and from then on
//! no value of the set allocates. The values exercise every filter of the built-in chains: stopwords, stemming
//! (with and without a change), folding (with and without anything to fold), NFKC, apostrophes, elisions, the length
//! guard and non-ASCII lowercasing. Left out, and documented: a token holding `Σ` (lowercasing needs
//! `str::to_lowercase` for its context) and long runs of combining marks (the normalization iterators buffer them).
#![allow(unsafe_code, clippy::unwrap_used)] // ADR-0026: the counting allocator; fixed inputs.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use persia_analysis::{BUILTIN_ANALYZERS, TokenStream, builtin_analyzer};

thread_local! {
    /// Allocations (and reallocations) by this thread. `const`-initialized, so reading it never allocates.
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

/// The system allocator, counting each allocation and reallocation of the calling thread.
struct Counting;

// SAFETY: every method forwards to `System` with the caller's arguments, so `Counting` upholds `GlobalAlloc`'s
// contract exactly when `System` does; the counter is a thread-local `Cell` with a `const` initializer, which never
// allocates or unwinds. `try_with` ignores the thread's teardown, when the thread-local is gone.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the caller's `layout` is forwarded unchanged (non-zero size, per `GlobalAlloc::alloc`).
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: as in `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator, that is from `System`, with this `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: `ptr` came from `System` with `layout`; `new_size` is the caller's, valid per `GlobalAlloc`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn count() {
    let _ignored = ALLOCATIONS.try_with(|n| n.set(n.get().wrapping_add(1)));
}

fn allocations() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

const VALUES: &[&str] = &[
    "The quick brown fox jumped over the lazy dogs, don’t panic: ÁRBOL Ñandú ﬁne Straße 3.14 and connected",
    "Los años de las canciones, los niños corrían rápidamente; ações não são brasileiras",
    "L’été dernier, les enfants jouaient dans les forêts de l'Auvergne et c'était magnifique",
    "Die Häuser der Straße wurden aufeinanderfolgend renoviert, und Grimm'sche Märchen gelesen",
    "Nell’estate degli anni passati, le città italiane erano piene di turisti velocemente",
    "привет мир 日本語のテキスト 가나다 ελληνικη ＡＢＣ ½",
    "",
    "x",
];

#[test]
fn analysis_allocates_nothing_once_the_stream_has_grown() {
    for a in BUILTIN_ANALYZERS {
        let analyzer = builtin_analyzer(a.name, a.version).unwrap();
        let mut stream = TokenStream::new();
        // Allocations per value, per round: they stop for good once every buffer the stream cycles through has
        // grown to what the values need (buffers only grow, within bounds).
        let mut history: Vec<Vec<u64>> = Vec::new();
        let mut quiet = 0;
        while quiet < 4 && history.len() < 64 {
            let round: Vec<u64> = VALUES
                .iter()
                .map(|value| {
                    let before = allocations();
                    analyzer.analyze(value, &mut stream);
                    allocations() - before
                })
                .collect();
            quiet = if round.iter().all(|&n| n == 0) {
                quiet + 1
            } else {
                0
            };
            history.push(round);
        }
        assert_eq!(
            quiet, 4,
            "{}@{}: analysis keeps allocating; allocations per value, per round: {history:?}",
            a.name, a.version
        );
    }
}

/// The counter works: a fresh stream allocates.
#[test]
fn a_fresh_stream_allocates() {
    let analyzer = builtin_analyzer("en", 1).unwrap();
    let before = allocations();
    let mut stream = TokenStream::new();
    analyzer.analyze(VALUES[0], &mut stream);
    assert!(allocations() > before);
}
