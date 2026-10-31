//! Sorting hits by score or by columns (SPEC §7.4, ROADMAP 7.5.1): a [`Sort`] is a list of keys checked against the
//! schema, and a view's best `k` matches by it are kept in a bounded heap.
//!
//! In a view, a document's value for a key is its score, its column key (SPEC §5.2.1), or a keyword ordinal: the
//! view's dictionary is in byte order, so ordinals compare as values do there. Only the hits a view keeps get their
//! keyword bytes, which is what hits of different views compare by.

use core::cmp::Ordering;

use super::Matcher;
use super::request::RequestError;
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

    /// How `a` ranks against `b` by the keys: `Less` first. Values the sort did not produce compare equal.
    pub(crate) fn compare(&self, a: &[SortValue], b: &[SortValue]) -> Ordering {
        for ((key, _), (a, b)) in self.keys.iter().zip(a.iter().zip(b)) {
            let o = by_key(*key, a.present(), b.present(), || match (a, b) {
                (SortValue::Score(x), SortValue::Score(y)) => (x + 0.0).total_cmp(&(y + 0.0)),
                (SortValue::Key(x), SortValue::Key(y)) => x.cmp(y),
                (SortValue::Keyword(x), SortValue::Keyword(y)) => x.cmp(y),
                _ => Ordering::Equal,
            });
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    }

    fn compare_local(&self, a: &[Local], b: &[Local]) -> Ordering {
        for ((key, _), (a, b)) in self.keys.iter().zip(a.iter().zip(b)) {
            let o = by_key(*key, a.present(), b.present(), || match (a, b) {
                (Local::Score(x), Local::Score(y)) => x.total_cmp(y),
                (Local::Num(x), Local::Num(y)) => x.cmp(y),
                _ => Ordering::Equal,
            });
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    }
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
/// A corruption of the view; a field `view` does not have with the key's type.
pub(crate) fn top_sorted<V: SearchView>(
    sort: &Sort,
    view: &V,
    matcher: &mut (impl Matcher + ?Sized),
    k: usize,
    after: Option<(&[SortValue], Ties)>,
    mut seen: impl FnMut(u32),
) -> Result<Vec<SortedHit>, ReadError> {
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
                        // Checked here, so every ord kept below has its bytes.
                        Some(ord) if words.value(ord).is_none() => {
                            return Err(past_dictionary(key, docid));
                        }
                        Some(ord) => Local::Num(ord),
                        None => Local::Missing,
                    }
                }
            });
        }
        if let Some((cursor, ties)) = after {
            match compare_to_cursor(sort, &scratch, cursor, &readers) {
                Ordering::Less => continue,
                Ordering::Equal if !ties.admit(docid) => continue,
                Ordering::Equal | Ordering::Greater => {}
            }
        }
        // Docids ascend: a match tied with the worst kept one ranks after it.
        if heap
            .worst()
            .is_some_and(|w| sort.compare_local(&scratch, &w.values) != Ordering::Less)
        {
            continue;
        }
        heap.push(
            Entry {
                values: scratch.as_slice().into(),
                docid,
                score,
            },
            |a, b| rank(sort, a, b),
        );
    }
    let entries = heap.into_sorted(|a, b| rank(sort, a, b));
    Ok(entries
        .into_iter()
        .map(|e| SortedHit {
            docid: e.docid,
            score: e.score,
            values: e
                .values
                .iter()
                .zip(&readers)
                .map(|(v, reader)| match (*v, reader) {
                    (Local::Missing, _) => SortValue::Missing,
                    (Local::Score(s), _) => SortValue::Score(s),
                    (Local::Num(ord), Reader::Keywords(words)) => {
                        SortValue::Keyword(words.value(ord).unwrap_or_default().to_vec())
                    }
                    (Local::Num(key), _) => SortValue::Key(key),
                })
                .collect(),
        })
        .collect())
}

/// How a view's values rank against a cursor's: `Greater` after it.
fn compare_to_cursor<K, W: KeywordReader>(
    sort: &Sort,
    values: &[Local],
    cursor: &[SortValue],
    readers: &[Reader<K, W>],
) -> Ordering {
    for (((key, _), reader), (a, b)) in sort.keys.iter().zip(readers).zip(values.iter().zip(cursor))
    {
        let o = by_key(*key, a.present(), b.present(), || match (a, b, reader) {
            (Local::Score(x), SortValue::Score(y), _) => x.total_cmp(&(y + 0.0)),
            (Local::Num(ord), SortValue::Keyword(y), Reader::Keywords(words)) => {
                words.value(*ord).unwrap_or_default().cmp(y.as_slice())
            }
            (Local::Num(x), SortValue::Key(y), _) => x.cmp(y),
            _ => Ordering::Equal,
        });
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

#[derive(Debug, Clone)]
struct Entry {
    values: Box<[Local]>,
    docid: u32,
    score: f64,
}

fn rank(sort: &Sort, a: &Entry, b: &Entry) -> Ordering {
    sort.compare_local(&a.values, &b.values)
        .then(a.docid.cmp(&b.docid))
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
    fn push(&mut self, entry: Entry, rank: impl Fn(&Entry, &Entry) -> Ordering) {
        if self.k == 0 {
            return;
        }
        if self.items.len() < self.k {
            self.items.push(entry);
            let mut i = self.items.len().saturating_sub(1);
            while i > 0 {
                let parent = (i - 1) / 2;
                if !self.later(i, parent, &rank) {
                    break;
                }
                self.items.swap(i, parent);
                i = parent;
            }
            return;
        }
        if let Some(top) = self.items.first_mut() {
            *top = entry;
        }
        let mut i = 0;
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut largest = i;
            if l < self.items.len() && self.later(l, largest, &rank) {
                largest = l;
            }
            if r < self.items.len() && self.later(r, largest, &rank) {
                largest = r;
            }
            if largest == i {
                break;
            }
            self.items.swap(i, largest);
            i = largest;
        }
    }

    /// Whether item `a` ranks after item `b`.
    fn later(&self, a: usize, b: usize, rank: &impl Fn(&Entry, &Entry) -> Ordering) -> bool {
        match (self.items.get(a), self.items.get(b)) {
            (Some(a), Some(b)) => rank(a, b) == Ordering::Greater,
            _ => false,
        }
    }

    fn into_sorted(mut self, rank: impl Fn(&Entry, &Entry) -> Ordering) -> Vec<Entry> {
        self.items.sort_by(|a, b| rank(a, b));
        self.items
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
