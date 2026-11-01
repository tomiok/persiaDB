//! Sorting hits by score or by columns (SPEC §7.4, ROADMAP 7.5.1): a [`Sort`] is a list of keys checked against the
//! schema, and a view's best `k` matches by it are kept in a bounded heap.
//!
//! In a view, a document's value for a key is its score, its column key (SPEC §5.2.1), or a keyword ordinal: the
//! view's dictionary is in byte order, so ordinals compare as values do there. Only the hits a view keeps get their
//! keyword bytes, which is what hits of different views compare by.

use core::cmp::Ordering;

use super::request::RequestError;
use super::{Matcher, Ticks, Watch};
use crate::schema::{CollectionSchema, FieldId, FieldType};
use crate::segment::{ReadError, SectionKind, SegmentCorruption};
use crate::view::{KeyReader, KeywordReader, SearchView};

/// The most keys a [`Sort`] has (SPEC §7.4).
pub const MAX_SORT_KEYS: usize = 16;

/// The direction of a sort key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// Smallest first.
    Asc,
    /// Largest first.
    Desc,
}

/// Where documents without a value go, whatever the order (SPEC §7.4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Missing {
    /// After every document with a value.
    #[default]
    Last,
    /// Before every document with a value.
    First,
}

/// One key of a [`Sort`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    /// The score (SPEC §7.2).
    Score {
        /// `Desc` ranks the best match first.
        order: Order,
    },
    /// A field's value: a `keyword`, `i64`, `f64`, `datetime` or `bool` field.
    Field {
        /// The field.
        field: FieldId,
        /// The direction.
        order: Order,
        /// Where documents without a value go.
        missing: Missing,
    },
}

impl SortKey {
    /// The score, best first: the default sort.
    pub const fn score() -> Self {
        Self::Score { order: Order::Desc }
    }

    /// `field` in `order`, documents without a value last.
    pub const fn field(field: FieldId, order: Order) -> Self {
        Self::Field {
            field,
            order,
            missing: Missing::Last,
        }
    }

    fn order(self) -> Order {
        match self {
            Self::Score { order } | Self::Field { order, .. } => order,
        }
    }

    fn missing(self) -> Missing {
        match self {
            Self::Score { .. } => Missing::Last,
            Self::Field { missing, .. } => missing,
        }
    }
}

/// What a key reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Score,
    /// An `i64`, `f64`, `datetime` or `bool` column: its keys.
    Column,
    Keyword,
}

/// A hit's value for one sort key (SPEC §7.4): what it ranks by, and what a cursor holds.
#[derive(Debug, Clone, PartialEq)]
pub enum SortValue {
    /// The document has no value in the field.
    Missing,
    /// Its score; `-0.0` ranks as `0.0`.
    Score(f64),
    /// Its column key (SPEC §5.2.1; `bool`: 0 or 1): unsigned order is value order. It is what a cursor compares,
    /// not a display value: `persia_format::for_codec::key_to_i64` (`i64`, `datetime`) and `key_to_f64` decode it,
    /// and the hit's document holds the value as stored.
    Key(u64),
    /// Its keyword value: the smallest it holds for an ascending key, the largest for a descending one.
    Keyword(Vec<u8>),
}

/// How hits rank: keys in order, checked against the schema (SPEC §7.4). Ties after them go by location (§7.3).
///
/// ```
/// use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
/// use persia_engine::search::{Order, Sort, SortKey};
///
/// let schema = SchemaBuilder::new("c")
///     .field("body", FieldKind::Text(TextOptions::default()))
///     .field("price", FieldKind::F64 { stored: true })
///     .build()?;
/// let price = schema.field("price").map(|f| f.id()).ok_or("no field")?;
/// let body = schema.field("body").map(|f| f.id()).ok_or("no field")?;
/// let sort = Sort::new([SortKey::field(price, Order::Asc), SortKey::score()], &schema)?;
/// assert!(!sort.is_score());
/// assert!(Sort::new([SortKey::field(body, Order::Asc)], &schema).is_err()); // text has no column
/// assert!(Sort::new([], &schema)?.is_score());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Sort {
    keys: Vec<(SortKey, Kind)>,
}

impl Default for Sort {
    fn default() -> Self {
        Self::score()
    }
}

impl Sort {
    /// By score, best first.
    pub fn score() -> Self {
        Self {
            keys: vec![(SortKey::score(), Kind::Score)],
        }
    }

    /// By `keys` in order; none is [`Self::score`].
    ///
    /// # Errors
    /// [`RequestError::TooManySortKeys`] above [`MAX_SORT_KEYS`]; [`RequestError::SortField`] for a field not in
    /// `schema` or without a column (`text`, `stored`).
    pub fn new(
        keys: impl IntoIterator<Item = SortKey>,
        schema: &CollectionSchema,
    ) -> Result<Self, RequestError> {
        let keys: Vec<SortKey> = keys.into_iter().collect();
        if keys.len() > MAX_SORT_KEYS {
            return Err(RequestError::TooManySortKeys {
                count: keys.len(),
                max: MAX_SORT_KEYS,
            });
        }
        if keys.is_empty() {
            return Ok(Self::score());
        }
        let keys = keys
            .into_iter()
            .map(|key| {
                let kind = match key {
                    SortKey::Score { .. } => Kind::Score,
                    SortKey::Field { field, .. } => {
                        let f = schema.field_by_id(field).ok_or(RequestError::SortField {
                            field,
                            reason: "no such field",
                        })?;
                        match f.field_type() {
                            FieldType::Keyword => Kind::Keyword,
                            FieldType::I64
                            | FieldType::F64
                            | FieldType::Datetime
                            | FieldType::Bool => Kind::Column,
                            FieldType::Text | FieldType::Stored => {
                                return Err(RequestError::SortField {
                                    field,
                                    reason: "the field has no column",
                                });
                            }
                        }
                    }
                };
                Ok((key, kind))
            })
            .collect::<Result<_, RequestError>>()?;
        Ok(Self { keys })
    }

    /// The keys, in order.
    pub fn keys(&self) -> impl ExactSizeIterator<Item = SortKey> + '_ {
        self.keys.iter().map(|&(k, _)| k)
    }

    /// Whether it is [`Self::score`]: the order pruning ranks by (SPEC §7.3).
    pub fn is_score(&self) -> bool {
        matches!(*self.keys, [(SortKey::Score { order: Order::Desc }, _)])
    }

    /// Checks that `values` (a cursor's) has one value per key, of the key's type.
    ///
    /// # Errors
    /// [`RequestError::Cursor`] when it does not.
    pub(crate) fn check(&self, values: &[SortValue]) -> Result<(), RequestError> {
        if values.len() != self.keys.len() {
            return Err(RequestError::Cursor {
                reason: "not one value per sort key",
            });
        }
        let fits = self.keys.iter().zip(values).all(|(&(_, kind), v)| {
            matches!(
                (kind, v),
                (Kind::Score, SortValue::Score(s)) if !s.is_nan()
            ) || matches!(
                (kind, v),
                (Kind::Column, SortValue::Key(_) | SortValue::Missing)
                    | (Kind::Keyword, SortValue::Keyword(_) | SortValue::Missing)
            )
        });
        if fits {
            Ok(())
        } else {
            Err(RequestError::Cursor {
                reason: "a value of another type than its sort key",
            })
        }
    }

    /// How `a` ranks against `b` by the keys: `Less` first.
    ///
    /// # Errors
    /// [`RequestError::MismatchedSortValue`] when both values are present and not of the key's type. [`Self::check`]
    /// rejects such a cursor, and a search does not produce one. They are not a tie: `Ordering::Equal` would skip or
    /// repeat a hit. `unreachable!` is denied on this crate, so every build returns the error.
    pub(crate) fn compare(
        &self,
        a: &[SortValue],
        b: &[SortValue],
    ) -> Result<Ordering, RequestError> {
        for ((key, _), (a, b)) in self.keys.iter().zip(a.iter().zip(b)) {
            let o = order_present(*key, a.present(), b.present(), || match (a, b) {
                (SortValue::Score(x), SortValue::Score(y)) => Ok((x + 0.0).total_cmp(&(y + 0.0))),
                (SortValue::Key(x), SortValue::Key(y)) => Ok(x.cmp(y)),
                (SortValue::Keyword(x), SortValue::Keyword(y)) => Ok(x.cmp(y)),
                _ => Err(RequestError::MismatchedSortValue),
            })?;
            if o != Ordering::Equal {
                return Ok(o);
            }
        }
        Ok(Ordering::Equal)
    }

    fn compare_local(&self, a: &[Local], b: &[Local]) -> Result<Ordering, RequestError> {
        for ((key, _), (a, b)) in self.keys.iter().zip(a.iter().zip(b)) {
            let o = order_present(*key, a.present(), b.present(), || match (a, b) {
                (Local::Score(x), Local::Score(y)) => Ok(x.total_cmp(y)),
                (Local::Num(x), Local::Num(y)) => Ok(x.cmp(y)),
                _ => Err(RequestError::MismatchedSortValue),
            })?;
            if o != Ordering::Equal {
                return Ok(o);
            }
        }
        Ok(Ordering::Equal)
    }
}

/// `key`'s order of two values. `present` runs only when both have a value, and may reject a pair whose types
/// the key did not produce.
fn order_present(
    key: SortKey,
    a_present: bool,
    b_present: bool,
    present: impl FnOnce() -> Result<Ordering, RequestError>,
) -> Result<Ordering, RequestError> {
    if a_present && b_present {
        let ordering = present()?;
        return Ok(by_key(key, true, true, || ordering));
    }
    Ok(by_key(key, a_present, b_present, || Ordering::Equal))
}

/// `key`'s order of two values: a missing one where `missing` puts it, two present ones by `present`.
fn by_key(
    key: SortKey,
    a_present: bool,
    b_present: bool,
    present: impl FnOnce() -> Ordering,
) -> Ordering {
    let missing_first = key.missing() == Missing::First;
    match (a_present, b_present) {
        (false, false) => Ordering::Equal,
        (false, true) if missing_first => Ordering::Less,
        (false, true) => Ordering::Greater,
        (true, false) if missing_first => Ordering::Greater,
        (true, false) => Ordering::Less,
        (true, true) => match key.order() {
            Order::Asc => present(),
            Order::Desc => present().reverse(),
        },
    }
}

impl SortValue {
    fn present(&self) -> bool {
        !matches!(self, Self::Missing)
    }
}

/// A document's value for a key within one view: its score (`-0.0` as `0.0`), or a column key or keyword ordinal.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Local {
    Missing,
    Score(f64),
    Num(u64),
}

impl Local {
    fn present(self) -> bool {
        self != Self::Missing
    }
}

/// A hit of one view, sorted: its docid, score and sort values.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SortedHit {
    pub(crate) docid: u32,
    pub(crate) score: f64,
    pub(crate) values: Vec<SortValue>,
}

/// Which matches tied with a cursor on every key rank after it, in one view: those of a view after the cursor's
/// location, none of one before it, and in its own the docids above its.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ties {
    All,
    None,
    Above(u32),
}

impl Ties {
    pub(crate) fn admit(self, docid: u32) -> bool {
        match self {
            Self::All => true,
            Self::None => false,
            Self::Above(d) => docid > d,
        }
    }
}

enum Reader<K, W> {
    Score,
    Keys(K),
    Keywords(W),
}

/// The best `k` matches of `matcher` in `view` by `sort`, best first, ties by docid; with `after`, only the matches
/// ranking after the cursor's values (ties per [`Ties`]). `seen` is called with every match, in docid order.
///
/// # Errors
/// A corruption of the view; a field `view` does not have with the key's type; a keyword ordinal with no bytes;
/// [`RequestError::MismatchedSortValue`] if a kept value is not of its key's type.
pub(crate) fn top_sorted<V: SearchView>(
    sort: &Sort,
    view: &V,
    matcher: &mut (impl Matcher + ?Sized),
    k: usize,
    after: Option<(&[SortValue], Ties)>,
    mut seen: impl FnMut(u32),
    watch: &mut impl Watch,
) -> Result<Vec<SortedHit>, RequestError> {
    // No bounds to prune a column sort by: only an interrupt matters.
    watch.check()?;
    let mut ticks = Ticks::default();
    let mut readers = sort
        .keys
        .iter()
        .map(|&(key, kind)| {
            Ok(match (key, kind) {
                (SortKey::Field { field, .. }, Kind::Column) => Reader::Keys(view.keys(field)?),
                (SortKey::Field { field, .. }, Kind::Keyword) => {
                    Reader::Keywords(view.keywords(field)?)
                }
                _ => Reader::Score,
            })
        })
        .collect::<Result<Vec<_>, ReadError>>()?;
    let mut heap = Heap::new(k);
    let mut scratch: Vec<Local> = Vec::with_capacity(sort.keys.len());
    while let Some(docid) = matcher.next()? {
        if ticks.due() {
            watch.check()?;
        }
        seen(docid);
        if k == 0 {
            continue;
        }
        let score = matcher.score()? + 0.0;
        scratch.clear();
        for (&(key, _), reader) in sort.keys.iter().zip(&mut readers) {
            scratch.push(match reader {
                Reader::Score => Local::Score(score),
                Reader::Keys(keys) => keys.key(docid)?.map_or(Local::Missing, Local::Num),
                Reader::Keywords(words) => {
                    let ords = words.ords(docid)?;
                    let ord = match key.order() {
                        Order::Asc => ords.first(),
                        Order::Desc => ords.last(),
                    };
                    match ord.copied() {
                        // Fail before the heap keeps the ord. Materializing the hit checks again.
                        Some(ord) => {
                            keyword_bytes(words, ord, key, docid)?;
                            Local::Num(ord)
                        }
                        None => Local::Missing,
                    }
                }
            });
        }
        if let Some((cursor, ties)) = after {
            match compare_to_cursor(sort, &scratch, cursor, &readers, docid)? {
                Ordering::Less => continue,
                Ordering::Equal if !ties.admit(docid) => continue,
                Ordering::Equal | Ordering::Greater => {}
            }
        }
        // Docids ascend: a match tied with the worst kept one ranks after it.
        if let Some(worst) = heap.worst()
            && sort.compare_local(&scratch, worst.locals())? != Ordering::Less
        {
            continue;
        }
        // The values are copied into the entry. A push does not allocate a `Box<[Local]>`.
        let entry = Entry::new(&scratch, docid, score);
        heap.push(&entry, |a, b| rank(sort, a, b))?;
    }
    let entries = heap.into_sorted(sort)?;
    let mut hits = Vec::with_capacity(entries.len());
    for entry in entries {
        let mut values = Vec::with_capacity(usize::from(entry.len));
        for ((local, reader), &(key, _)) in entry.locals().iter().zip(&readers).zip(&sort.keys) {
            values.push(to_sort_value(*local, reader, key, entry.docid)?);
        }
        hits.push(SortedHit {
            docid: entry.docid,
            score: entry.score,
            values,
        });
    }
    Ok(hits)
}

/// The bytes of keyword ordinal `ord`. `None` is corruption, never an empty value that sorts first.
fn keyword_bytes<W: KeywordReader>(
    words: &W,
    ord: u64,
    key: SortKey,
    docid: u32,
) -> Result<&[u8], ReadError> {
    words.value(ord).ok_or_else(|| past_dictionary(key, docid))
}

/// `local` as a sort value. A keyword ordinal with no bytes is [`ReadError::Section`], not an empty keyword.
fn to_sort_value<K, W: KeywordReader>(
    local: Local,
    reader: &Reader<K, W>,
    key: SortKey,
    docid: u32,
) -> Result<SortValue, RequestError> {
    Ok(match (local, reader) {
        (Local::Missing, _) => SortValue::Missing,
        (Local::Score(score), Reader::Score) => SortValue::Score(score),
        (Local::Num(ord), Reader::Keywords(words)) => {
            SortValue::Keyword(keyword_bytes(words, ord, key, docid)?.to_vec())
        }
        (Local::Num(value), Reader::Keys(_)) => SortValue::Key(value),
        (Local::Score(_) | Local::Num(_), _) => return Err(RequestError::MismatchedSortValue),
    })
}

/// How a view's values rank against a cursor's: `Greater` after it.
///
/// # Errors
/// A keyword ordinal with no bytes, or a present value of a type the key did not produce.
fn compare_to_cursor<K, W: KeywordReader>(
    sort: &Sort,
    values: &[Local],
    cursor: &[SortValue],
    readers: &[Reader<K, W>],
    docid: u32,
) -> Result<Ordering, RequestError> {
    for (((key, _), reader), (a, b)) in sort.keys.iter().zip(readers).zip(values.iter().zip(cursor))
    {
        let o = order_present(*key, a.present(), b.present(), || match (a, b, reader) {
            (Local::Score(x), SortValue::Score(y), Reader::Score) => Ok(x.total_cmp(&(y + 0.0))),
            (Local::Num(ord), SortValue::Keyword(y), Reader::Keywords(words)) => {
                Ok(keyword_bytes(words, *ord, *key, docid)?.cmp(y.as_slice()))
            }
            (Local::Num(x), SortValue::Key(y), Reader::Keys(_)) => Ok(x.cmp(y)),
            _ => Err(RequestError::MismatchedSortValue),
        })?;
        if o != Ordering::Equal {
            return Ok(o);
        }
    }
    Ok(Ordering::Equal)
}

/// One kept hit. Sort values sit in the entry (`len` of them): a push copies the array and does not allocate.
/// Column-sort pruning is ROADMAP 7.5.6 and is not this copy.
#[derive(Debug, Clone, Copy)]
struct Entry {
    values: [Local; MAX_SORT_KEYS],
    len: u8,
    docid: u32,
    score: f64,
}

impl Entry {
    fn new(values: &[Local], docid: u32, score: f64) -> Self {
        let mut stored = [Local::Missing; MAX_SORT_KEYS];
        let n = values.len().min(stored.len());
        debug_assert_eq!(n, values.len(), "a sort has more keys than MAX_SORT_KEYS");
        if let (Some(dst), Some(src)) = (stored.get_mut(..n), values.get(..n)) {
            dst.copy_from_slice(src);
        }
        Self {
            values: stored,
            len: u8::try_from(n).unwrap_or(u8::MAX),
            docid,
            score,
        }
    }

    fn locals(&self) -> &[Local] {
        let n = usize::from(self.len).min(self.values.len());
        self.values.get(..n).unwrap_or(&[])
    }
}

fn rank(sort: &Sort, a: &Entry, b: &Entry) -> Result<Ordering, RequestError> {
    Ok(sort
        .compare_local(a.locals(), b.locals())?
        .then(a.docid.cmp(&b.docid)))
}

/// At most `k` entries, the one that ranks last on top: a binary heap ordered by a comparator that needs the sort.
#[derive(Debug)]
struct Heap {
    k: usize,
    items: Vec<Entry>,
}

impl Heap {
    fn new(k: usize) -> Self {
        Self {
            k,
            // A deep page should not reserve memory for hits that never come.
            items: Vec::with_capacity(k.min(1024)),
        }
    }

    /// The entry a better one replaces, once `k` are kept.
    fn worst(&self) -> Option<&Entry> {
        if self.items.len() < self.k {
            return None;
        }
        self.items.first()
    }

    /// Keeps `entry`, dropping the worst when `k` are kept; the caller checked it ranks before the worst.
    fn push(
        &mut self,
        entry: &Entry,
        rank: impl Fn(&Entry, &Entry) -> Result<Ordering, RequestError>,
    ) -> Result<(), RequestError> {
        if self.k == 0 {
            return Ok(());
        }
        if self.items.len() < self.k {
            self.items.push(*entry);
            let mut i = self.items.len().saturating_sub(1);
            while i > 0 {
                let parent = i.saturating_sub(1) / 2;
                if !self.later(i, parent, &rank)? {
                    break;
                }
                self.items.swap(i, parent);
                i = parent;
            }
            return Ok(());
        }
        if let Some(top) = self.items.first_mut() {
            *top = *entry;
        }
        let mut i = 0;
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut largest = i;
            if l < self.items.len() && self.later(l, largest, &rank)? {
                largest = l;
            }
            if r < self.items.len() && self.later(r, largest, &rank)? {
                largest = r;
            }
            if largest == i {
                break;
            }
            self.items.swap(i, largest);
            i = largest;
        }
        Ok(())
    }

    /// Whether item `a` ranks after item `b`.
    fn later(
        &self,
        a: usize,
        b: usize,
        rank: &impl Fn(&Entry, &Entry) -> Result<Ordering, RequestError>,
    ) -> Result<bool, RequestError> {
        match (self.items.get(a), self.items.get(b)) {
            (Some(a), Some(b)) => Ok(rank(a, b)? == Ordering::Greater),
            _ => Ok(false),
        }
    }

    /// Best first. Present values at one key must share a type: `sort_by` cannot report an error, and a comparator
    /// that sometimes fails is not a total order.
    fn into_sorted(self, sort: &Sort) -> Result<Vec<Entry>, RequestError> {
        if !locals_agree(&self.items) {
            return Err(RequestError::MismatchedSortValue);
        }
        let mut items = self.items;
        items.sort_by(|a, b| match rank(sort, a, b) {
            Ok(ordering) => ordering,
            Err(_) => a.docid.cmp(&b.docid),
        });
        Ok(items)
    }
}

/// A keyword ord of `docid` with no value in its view's dictionary: corruption, never an empty value ranked.
fn past_dictionary(key: SortKey, docid: u32) -> ReadError {
    let field = match key {
        SortKey::Field { field, .. } => field.get(),
        SortKey::Score { .. } => 0,
    };
    ReadError::Section {
        kind: SectionKind::Columns,
        field,
        position: u64::from(docid),
        reason: SegmentCorruption::Invalid {
            what: "a keyword ord past its dictionary",
        },
    }
}

/// Whether every entry has the same number of values, and each key is at most one present type (`Score` or `Num`).
/// A missing value may sit beside either; two present types at one key must not be sorted as a tie.
fn locals_agree(entries: &[Entry]) -> bool {
    let Some(width) = entries.first().map(|entry| entry.locals().len()) else {
        return true;
    };
    if entries.iter().any(|entry| entry.locals().len() != width) {
        return false;
    }
    for key in 0..width {
        let mut score = false;
        let mut num = false;
        for entry in entries {
            match entry.locals().get(key) {
                Some(Local::Score(_)) => score = true,
                Some(Local::Num(_)) => num = true,
                _ => {}
            }
        }
        if score && num {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{FieldKind, KeywordOptions, SchemaBuilder};

    fn keyword_sort() -> (Sort, SortKey) {
        let schema = SchemaBuilder::new("c")
            .field("tag", FieldKind::Keyword(KeywordOptions::default()))
            .build()
            .unwrap();
        let tag = schema.field("tag").unwrap().id();
        let key = SortKey::field(tag, Order::Asc);
        let sort = Sort::new([key], &schema).unwrap();
        (sort, key)
    }

    /// A dictionary that reports ordinals and then has no bytes for them.
    struct NoBytes;

    impl KeywordReader for NoBytes {
        fn ords(&mut self, _: u32) -> Result<&[u64], ReadError> {
            Ok(&[0])
        }

        fn value(&self, _: u64) -> Option<&[u8]> {
            None
        }

        fn len(&self) -> u64 {
            0
        }
    }

    struct Bytes(&'static [u8]);

    impl KeywordReader for Bytes {
        fn ords(&mut self, _: u32) -> Result<&[u64], ReadError> {
            Ok(&[0])
        }

        fn value(&self, ord: u64) -> Option<&[u8]> {
            (ord == 0).then_some(self.0)
        }

        fn len(&self) -> u64 {
            1
        }
    }

    #[test]
    fn compare_orders_missing_and_rejects_a_mismatched_type() {
        let schema = SchemaBuilder::new("c")
            .field("n", FieldKind::I64 { stored: true })
            .build()
            .unwrap();
        let n = schema.field("n").unwrap().id();
        let sort = Sort::new([SortKey::field(n, Order::Asc)], &schema).unwrap();
        assert_eq!(
            sort.compare(&[SortValue::Missing], &[SortValue::Key(1)])
                .unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            sort.compare(&[SortValue::Key(1)], &[SortValue::Key(2)])
                .unwrap(),
            Ordering::Less
        );
        assert!(matches!(
            sort.compare(&[SortValue::Key(1)], &[SortValue::Keyword(b"a".to_vec())]),
            Err(RequestError::MismatchedSortValue)
        ));

        let scores = Sort::score();
        assert_eq!(
            scores
                .compare(&[SortValue::Score(1.0)], &[SortValue::Score(2.0)])
                .unwrap(),
            Ordering::Greater
        );
        assert!(matches!(
            scores.compare(&[SortValue::Score(1.0)], &[SortValue::Key(1)]),
            Err(RequestError::MismatchedSortValue)
        ));
        assert!(matches!(
            scores.compare_local(&[Local::Score(1.0)], &[Local::Num(1)]),
            Err(RequestError::MismatchedSortValue)
        ));
        assert_eq!(
            scores
                .compare_local(&[Local::Missing], &[Local::Score(1.0)])
                .unwrap(),
            Ordering::Greater
        );
    }

    #[test]
    fn a_keyword_ord_with_no_bytes_is_an_error() {
        let (sort, key) = keyword_sort();
        let err = keyword_bytes(&NoBytes, 0, key, 4).unwrap_err();
        assert!(matches!(
            err,
            ReadError::Section {
                reason: SegmentCorruption::Invalid { what },
                ..
            } if what == "a keyword ord past its dictionary"
        ));

        let readers: [Reader<(), NoBytes>; 1] = [Reader::Keywords(NoBytes)];
        let err = compare_to_cursor(
            &sort,
            &[Local::Num(0)],
            &[SortValue::Keyword(b"a".to_vec())],
            &readers,
            4,
        )
        .unwrap_err();
        assert!(matches!(err, RequestError::Read(ReadError::Section { .. })));

        let err = to_sort_value(
            Local::Num(0),
            &Reader::<(), NoBytes>::Keywords(NoBytes),
            key,
            4,
        )
        .unwrap_err();
        assert!(matches!(err, RequestError::Read(ReadError::Section { .. })));

        let readers: [Reader<(), Bytes>; 1] = [Reader::Keywords(Bytes(b"m"))];
        assert_eq!(
            compare_to_cursor(
                &sort,
                &[Local::Num(0)],
                &[SortValue::Keyword(b"a".to_vec())],
                &readers,
                4,
            )
            .unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            to_sort_value(
                Local::Num(0),
                &Reader::<(), Bytes>::Keywords(Bytes(b"m")),
                key,
                4
            )
            .unwrap(),
            SortValue::Keyword(b"m".to_vec())
        );
    }

    #[test]
    fn compare_to_cursor_rejects_a_value_of_the_wrong_type() {
        let (sort, _) = keyword_sort();
        let readers: [Reader<(), Bytes>; 1] = [Reader::Keywords(Bytes(b"m"))];
        let err = compare_to_cursor(&sort, &[Local::Num(0)], &[SortValue::Key(1)], &readers, 0)
            .unwrap_err();
        assert!(matches!(err, RequestError::MismatchedSortValue));
    }
}
