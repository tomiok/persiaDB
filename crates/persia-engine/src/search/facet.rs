//! Facet counts over `keyword` fields (SPEC §7.5, ROADMAP 7.5.3): per view, the matching documents' values are
//! counted by ordinal from the view's keyword column; the views' counts merge by value bytes, so they are exact, and
//! the values with the highest counts are returned.

use std::collections::HashMap;

use roaring::RoaringBitmap;

use super::request::RequestError;
use crate::schema::{CollectionSchema, Field, FieldId, FieldKind};
use crate::segment::ReadError;
use crate::view::{KeywordReader, SearchView};

/// The most values a facet returns (SPEC §7.5).
pub const MAX_FACET_SIZE: usize = 1_000;

/// The values a facet returns unless asked otherwise (SPEC §7.5).
pub const DEFAULT_FACET_SIZE: usize = 10;

/// A facet to count: a `keyword` field with `facet: true`, and how many values to return.
///
/// ```
/// use persia_engine::schema::{FieldKind, KeywordOptions, SchemaBuilder};
/// use persia_engine::search::FacetRequest;
///
/// let schema = SchemaBuilder::new("c")
///     .field("brand", FieldKind::Keyword(KeywordOptions { facet: true, ..KeywordOptions::default() }))
///     .field("sku", FieldKind::Keyword(KeywordOptions::default()))
///     .build()?;
/// let brand = schema.field("brand").map(|f| f.id()).ok_or("no field")?;
/// let sku = schema.field("sku").map(|f| f.id()).ok_or("no field")?;
/// assert_eq!(FacetRequest::new(brand, 5, &schema)?.size(), 5);
/// assert!(FacetRequest::new(sku, 5, &schema).is_err()); // not `facet: true`
/// assert!(FacetRequest::new(brand, 0, &schema).is_err());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FacetRequest {
    field: FieldId,
    size: usize,
}

impl FacetRequest {
    /// The `size` most frequent values of `field` among the matches.
    ///
    /// # Errors
    /// [`RequestError::FacetField`] for a field that is not a `keyword` field with `facet: true`;
    /// [`RequestError::FacetSize`] for a size outside `1..=`[`MAX_FACET_SIZE`].
    pub fn new(
        field: FieldId,
        size: usize,
        schema: &CollectionSchema,
    ) -> Result<Self, RequestError> {
        match schema.field_by_id(field).map(Field::kind) {
            Some(FieldKind::Keyword(o)) if o.facet => {}
            Some(FieldKind::Keyword(_)) => {
                return Err(RequestError::FacetField {
                    field,
                    reason: "the field does not have `facet: true`",
                });
            }
            Some(_) => {
                return Err(RequestError::FacetField {
                    field,
                    reason: "not a keyword field",
                });
            }
            None => {
                return Err(RequestError::FacetField {
                    field,
                    reason: "no such field",
                });
            }
        }
        if !(1..=MAX_FACET_SIZE).contains(&size) {
            return Err(RequestError::FacetSize {
                size,
                max: MAX_FACET_SIZE,
            });
        }
        Ok(Self { field, size })
    }

    /// The field.
    pub fn field(&self) -> FieldId {
        self.field
    }

    /// How many values it returns at most.
    pub fn size(&self) -> usize {
        self.size
    }
}

/// One value of a facet and the matches holding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FacetValue {
    /// The keyword value.
    pub value: Vec<u8>,
    /// The live matching documents that hold it.
    pub count: u64,
}

/// A facet's counts: its values with the highest counts, then ascending bytes (SPEC §7.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FacetCounts {
    /// The field.
    pub field: FieldId,
    /// At most the request's `size` values, every count above 0.
    pub values: Vec<FacetValue>,
}

/// One facet's counts, summed over the views added so far.
#[derive(Debug, Clone)]
pub(crate) struct FacetCounter {
    request: FacetRequest,
    totals: HashMap<Vec<u8>, u64>,
}

impl FacetCounter {
    pub(crate) fn new(request: FacetRequest) -> Self {
        Self {
            request,
            totals: HashMap::new(),
        }
    }

    /// Adds the values of the documents `docs` of `view` (its live matches).
    ///
    /// # Errors
    /// A corruption of the view; a field `view` does not have as a `keyword` field.
    pub(crate) fn add<V: SearchView>(
        &mut self,
        view: &V,
        docs: &RoaringBitmap,
    ) -> Result<(), ReadError> {
        if docs.is_empty() {
            return Ok(());
        }
        let mut words = view.keywords(self.request.field)?;
        // By ordinal: a dense array when the dictionary is no larger than the matches (or small), else a map.
        let len = words.len();
        let dense = len <= docs.len().max(1 << 16);
        let mut by_ord = vec![
            0_u64;
            if dense {
                usize::try_from(len).unwrap_or(0)
            } else {
                0
            }
        ];
        let mut sparse: HashMap<u64, u64> = HashMap::new();
        for docid in docs {
            for &ord in words.ords(docid)? {
                let n = match by_ord.get_mut(usize::try_from(ord).unwrap_or(usize::MAX)) {
                    Some(n) => n,
                    None => sparse.entry(ord).or_default(),
                };
                *n = n.saturating_add(1);
            }
        }
        let counted = (0_u64..).zip(by_ord).filter(|&(_, n)| n > 0).chain(sparse);
        for (ord, n) in counted {
            let Some(value) = words.value(ord) else {
                continue;
            };
            match self.totals.get_mut(value) {
                Some(total) => *total = total.saturating_add(n),
                None => {
                    self.totals.insert(value.to_vec(), n);
                }
            }
        }
        Ok(())
    }

    /// The values with the highest counts, then ascending bytes.
    pub(crate) fn finish(self) -> FacetCounts {
        let mut values: Vec<FacetValue> = self
            .totals
            .into_iter()
            .map(|(value, count)| FacetValue { value, count })
            .collect();
        let by_rank = |a: &FacetValue, b: &FacetValue| {
            b.count.cmp(&a.count).then_with(|| a.value.cmp(&b.value))
        };
        if values.len() > self.request.size {
            values.select_nth_unstable_by(self.request.size, by_rank);
            values.truncate(self.request.size);
        }
        values.sort_unstable_by(by_rank);
        FacetCounts {
            field: self.request.field,
            values,
        }
    }
}
