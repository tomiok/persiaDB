//! Decoded blocks shared by every reader of a segment, and optionally of many segments (SPEC §10.6, ROADMAP 5.3.11):
//! a block is decoded, and its CRC checked, once while it stays cached, however many term lookups, posting lists,
//! columns and document fetches read it.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use super::super::SectionKind;

/// A cached block: which segment ([`super::Segment::open`] instance), section and block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    segment: u64,
    kind: SectionKind,
    field: u32,
    index: u32,
}

#[derive(Debug)]
struct Entry {
    block: Arc<Vec<u8>>,
    /// When it was last used; an [`State::order`] item with another tick is stale.
    tick: u64,
    cost: u64,
}

#[derive(Debug, Default)]
struct State {
    blocks: HashMap<Key, Entry>,
    /// Uses, oldest first: `(key, tick)`. A block used again is pushed again; the older item goes stale and is
    /// skipped when it reaches the front.
    order: VecDeque<(Key, u64)>,
    tick: u64,
    bytes: u64,
}

impl State {
    fn touch(&mut self, key: Key) -> Option<Arc<Vec<u8>>> {
        let tick = self.tick;
        let entry = self.blocks.get_mut(&key)?;
        entry.tick = tick;
        let block = Arc::clone(&entry.block);
        self.tick = tick.wrapping_add(1);
        self.order.push_back((key, tick));
        self.compact();
        Some(block)
    }

    fn insert(&mut self, key: Key, block: Arc<Vec<u8>>, cost: u64, capacity: u64) {
        let tick = self.tick;
        self.tick = tick.wrapping_add(1);
        self.bytes = self.bytes.saturating_add(cost);
        self.blocks.insert(key, Entry { block, tick, cost });
        self.order.push_back((key, tick));
        while self.bytes > capacity {
            let Some((key, tick)) = self.order.pop_front() else {
                break;
            };
            if self.blocks.get(&key).is_some_and(|e| e.tick == tick)
                && let Some(e) = self.blocks.remove(&key)
            {
                self.bytes = self.bytes.saturating_sub(e.cost);
            }
        }
        self.compact();
    }

    /// Drops the stale items once they outnumber the live ones, so `order` stays linear in the cached blocks.
    fn compact(&mut self) {
        if self.order.len() <= self.blocks.len().saturating_mul(2).saturating_add(64) {
            return;
        }
        let blocks = &self.blocks;
        self.order
            .retain(|(key, tick)| blocks.get(key).is_some_and(|e| e.tick == *tick));
    }
}

/// Counters of a [`BlockCache`], from [`BlockCache::stats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStats {
    /// Reads served from the cache.
    pub hits: u64,
    /// Reads that decoded the block.
    pub misses: u64,
    /// Blocks cached now.
    pub blocks: u64,
    /// Decoded bytes cached now.
    pub bytes: u64,
}

/// A least-recently-used cache of decoded section blocks (SPEC §10.6), bounded in bytes.
///
/// Every open [`super::Segment`] reads its block-framed sections through one, which keeps at most its capacity of
/// decoded blocks for all the segments that use it: the process-wide [`BlockCache::shared`] (of
/// [`BlockCache::DEFAULT_CAPACITY`]) unless [`super::Segment::open_with_cache`] gives it another. A database or server
/// opens all its segments with one cache sized by its configuration (ROADMAP 10.2.5, 11.4.7). Blocks are decoded
/// outside the lock, so readers of other blocks do not wait; two readers that miss the same block at once may both
/// decode it, and one copy is kept.
///
/// The capacity bounds what the cache keeps, not what readers hold: a block handed out stays alive while a reader
/// uses it (a cursor keeps one), even after the cache evicts it.
///
/// ```
/// use std::sync::Arc;
/// use persia_engine::segment::BlockCache;
///
/// let cache = Arc::new(BlockCache::new(64 << 20)); // shared by the segments opened with it
/// assert_eq!(cache.capacity(), 64 << 20);
/// assert_eq!(cache.stats().blocks, 0);
/// ```
#[derive(Debug)]
pub struct BlockCache {
    capacity: u64,
    state: Mutex<State>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl Default for BlockCache {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAPACITY)
    }
}

impl BlockCache {
    /// The capacity of [`BlockCache::shared`], which segments opened with [`super::Segment::open`] share: 8 MiB.
    pub const DEFAULT_CAPACITY: u64 = 8 << 20;

    /// The cache of [`BlockCache::DEFAULT_CAPACITY`] that every segment opened with [`super::Segment::open`] in this
    /// process shares, so however many segments it opens, the cache keeps at most that many decoded bytes for them.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use persia_engine::segment::BlockCache;
    ///
    /// assert!(Arc::ptr_eq(&BlockCache::shared(), &BlockCache::shared()));
    /// assert_eq!(BlockCache::shared().capacity(), BlockCache::DEFAULT_CAPACITY);
    /// ```
    pub fn shared() -> Arc<Self> {
        static SHARED: OnceLock<Arc<BlockCache>> = OnceLock::new();
        Arc::clone(SHARED.get_or_init(|| Arc::new(Self::default())))
    }

    /// A cache that keeps at most `capacity` decoded bytes. A block larger than that is decoded on each read.
    pub fn new(capacity: u64) -> Self {
        Self {
            capacity,
            state: Mutex::new(State::default()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Most decoded bytes it keeps.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Hits and misses so far, and what it holds now.
    pub fn stats(&self) -> CacheStats {
        let state = self.lock();
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            blocks: u64::try_from(state.blocks.len()).unwrap_or(u64::MAX),
            bytes: state.bytes,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // The state stays consistent whatever panicked while holding the lock: every update is a few plain writes.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Block `index` of section `(kind, field)` of segment instance `segment`, decoded by `decode` on a miss.
    pub(super) fn get<E>(
        &self,
        segment: u64,
        (kind, field, index): (SectionKind, u32, u32),
        decode: impl FnOnce() -> Result<Vec<u8>, E>,
    ) -> Result<Arc<Vec<u8>>, E> {
        let key = Key {
            segment,
            kind,
            field,
            index,
        };
        if let Some(block) = self.lock().touch(key) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(block);
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let block = Arc::new(decode()?);
        let cost = u64::try_from(block.capacity()).unwrap_or(u64::MAX);
        if cost > self.capacity {
            return Ok(block);
        }
        let mut state = self.lock();
        // Another reader may have decoded it meanwhile: keep one copy.
        if let Some(cached) = state.touch(key) {
            return Ok(cached);
        }
        state.insert(key, Arc::clone(&block), cost, self.capacity);
        Ok(block)
    }

    /// Drops the blocks of segment instance `segment` (when it is dropped: nothing can read them again).
    pub(super) fn evict_segment(&self, segment: u64) {
        let mut state = self.lock();
        let mut freed = 0_u64;
        state.blocks.retain(|key, e| {
            let keep = key.segment != segment;
            if !keep {
                freed = freed.saturating_add(e.cost);
            }
            keep
        });
        state.bytes = state.bytes.saturating_sub(freed);
        state.order.retain(|(key, _)| key.segment != segment);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(cache: &BlockCache, segment: u64, index: u32, len: usize) -> Arc<Vec<u8>> {
        let key = (SectionKind::Postings, 1, index);
        cache
            .get(segment, key, || Ok::<_, ()>(vec![0; len]))
            .unwrap()
    }

    #[test]
    fn hits_after_a_miss() {
        let cache = BlockCache::new(1 << 10);
        let a = get(&cache, 0, 0, 100);
        let b = get(&cache, 0, 0, 100);
        assert!(Arc::ptr_eq(&a, &b));
        let s = cache.stats();
        assert_eq!((s.hits, s.misses, s.blocks), (1, 1, 1));
        assert!(s.bytes >= 100);
    }

    #[test]
    fn evicts_the_least_recently_used_within_capacity() {
        let cache = BlockCache::new(300);
        get(&cache, 0, 0, 100);
        get(&cache, 0, 1, 100);
        get(&cache, 0, 0, 100); // 0 is now the most recent
        get(&cache, 0, 2, 100);
        get(&cache, 0, 3, 100); // over capacity: evicts 1
        assert!(cache.stats().bytes <= 300);
        let misses = cache.stats().misses;
        get(&cache, 0, 0, 100);
        assert_eq!(cache.stats().misses, misses, "0 stayed");
        get(&cache, 0, 1, 100);
        assert_eq!(cache.stats().misses, misses + 1, "1 was evicted");
    }

    #[test]
    fn does_not_keep_a_block_larger_than_capacity() {
        let cache = BlockCache::new(10);
        get(&cache, 0, 0, 100);
        assert_eq!(cache.stats().blocks, 0);
    }

    #[test]
    fn keeps_segments_apart_and_evicts_one() {
        let cache = BlockCache::new(1 << 10);
        let a = get(&cache, 0, 0, 10);
        let b = get(&cache, 1, 0, 10);
        assert!(!Arc::ptr_eq(&a, &b));
        cache.evict_segment(0);
        let s = cache.stats();
        assert_eq!(s.blocks, 1);
        get(&cache, 1, 0, 10);
        assert_eq!(cache.stats().hits, 1);
    }

    #[test]
    fn order_stays_bounded_under_repeated_hits() {
        let cache = BlockCache::new(1 << 10);
        get(&cache, 0, 0, 10);
        for _ in 0..10_000 {
            get(&cache, 0, 0, 10);
        }
        assert!(cache.lock().order.len() <= 66);
    }

    #[test]
    fn two_readers_missing_one_block_keep_one_copy() {
        let cache = BlockCache::new(1 << 10);
        // Both readers are inside `decode` at once: both missed.
        let barrier = std::sync::Barrier::new(2);
        let blocks: Vec<Arc<Vec<u8>>> = std::thread::scope(|s| {
            let read = || {
                cache
                    .get(0, (SectionKind::Postings, 1, 0), || {
                        barrier.wait();
                        Ok::<_, ()>(vec![7; 100])
                    })
                    .unwrap()
            };
            let a = s.spawn(read);
            let b = s.spawn(read);
            vec![a.join().unwrap(), b.join().unwrap()]
        });
        assert_eq!(*blocks[0], *blocks[1]);
        let stats = cache.stats();
        assert_eq!((stats.misses, stats.blocks), (2, 1));
        assert_eq!(stats.bytes, u64::try_from(blocks[0].capacity()).unwrap());
        assert!(
            Arc::ptr_eq(&get(&cache, 0, 0, 100), &blocks[0])
                || Arc::ptr_eq(&get(&cache, 0, 0, 100), &blocks[1])
        );
    }

    #[test]
    fn a_decode_error_is_not_cached() {
        let cache = BlockCache::new(1 << 10);
        let key = (SectionKind::Postings, 1, 0);
        assert_eq!(cache.get(0, key, || Err::<Vec<u8>, _>(7)), Err(7));
        assert_eq!(cache.stats().blocks, 0);
    }
}
