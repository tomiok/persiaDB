//! What a search asks for beside its query (SPEC §7.4, §7.5, §7.5.1): the errors of a sort, a page, a cursor, a facet
//! or a projection that does not fit the schema, and the projection itself.

use std::collections::BTreeSet;

use crate::document::Document;
use crate::schema::{CollectionSchema, FieldId};
use crate::segment::ReadError;

/// The largest `offset + limit` of a page (SPEC §7.4): deeper pages use a cursor.
pub const MAX_WINDOW: usize = 10_000;

/// Why a search request cannot run.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RequestError {
    /// A sort key on a field that is not in the schema or has no column (SPEC §7.4).
    #[error("cannot sort by field {field:?}: {reason}")]
    SortField {
        /// The field.
        field: FieldId,
        /// Why.
        reason: &'static str,
    },
    /// More sort keys than [`super::MAX_SORT_KEYS`].
    #[error("{count} sort keys, more than the {max} allowed")]
    TooManySortKeys {
        /// The keys given.
        count: usize,
        /// The most allowed.
        max: usize,
    },
    /// A page whose `offset + limit` is above [`MAX_WINDOW`] (SPEC §7.4).
    #[error(
        "offset {offset} + limit {limit} is above the window of {max}: page deeper with a cursor"
    )]
    Window {
        /// The offset asked for.
        offset: usize,
        /// The limit asked for.
        limit: usize,
        /// [`MAX_WINDOW`].
        max: usize,
    },
    /// A cursor whose values do not match the sort's keys in number or type (SPEC §7.4).
    #[error("a cursor that does not match the sort: {reason}")]
    Cursor {
        /// Why.
        reason: &'static str,
    },
    /// A facet on a field that is not a `keyword` field with `facet: true` (SPEC §7.5).
    #[error("cannot facet on field {field:?}: {reason}")]
    FacetField {
        /// The field.
        field: FieldId,
        /// Why.
        reason: &'static str,
    },
    /// A facet size outside `1..=`[`super::MAX_FACET_SIZE`].
    #[error("facet size {size} is outside 1..={max}")]
    FacetSize {
        /// The size asked for.
        size: usize,
        /// The largest allowed.
        max: usize,
    },
    /// A projected name that is neither a stored field nor, in a collection that is not `strict`, an extra field
    /// (SPEC §7.5.1).
    #[error("cannot return field {name:?}: {reason}")]
    Projection {
        /// The name.
        name: String,
        /// Why.
        reason: &'static str,
    },
    /// A view could not be read.
    #[error(transparent)]
    Read(#[from] ReadError),
}

/// Which stored fields a hit returns (SPEC §7.5.1); its id and blob reference always come back.
///
/// ```
/// use persia_engine::schema::{FieldKind, SchemaBuilder};
/// use persia_engine::search::Projection;
///
/// let schema = SchemaBuilder::new("c").field("n", FieldKind::I64 { stored: false }).build()?;
/// assert!(Projection::fields(["n"], &schema).is_err()); // not stored
/// assert!(Projection::fields(["other"], &schema).is_err()); // a strict collection has no extra fields
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Projection {
    /// Every stored field and extra field.
    #[default]
    All,
    /// None: the id and the blob reference only.
    None,
    /// These fields and extra fields.
    Fields {
        /// Fields of the schema, all stored.
        fields: BTreeSet<FieldId>,
        /// Names of extra fields.
        extra: BTreeSet<String>,
    },
}

impl Projection {
    /// The fields named `names`, each a field of `schema` with `stored: true` or, when `schema` is not `strict`, any
    /// other name, an extra field.
    ///
    /// # Errors
    /// [`RequestError::Projection`] for a field that is not stored, or a name outside a `strict` schema.
    pub fn fields<S: AsRef<str>>(
        names: impl IntoIterator<Item = S>,
        schema: &CollectionSchema,
    ) -> Result<Self, RequestError> {
        let (mut fields, mut extra) = (BTreeSet::new(), BTreeSet::new());
        for name in names {
            let name = name.as_ref();
            match schema.field(name) {
                Some(field) if field.kind().stored() => {
                    fields.insert(field.id());
                }
                Some(_) => {
                    return Err(RequestError::Projection {
                        name: name.into(),
                        reason: "the field is not stored",
                    });
                }
                None if schema.strict() => {
                    return Err(RequestError::Projection {
                        name: name.into(),
                        reason: "no such field in a strict collection",
                    });
                }
                None => {
                    extra.insert(name.to_owned());
                }
            }
        }
        Ok(Self::Fields { fields, extra })
    }

    /// `document` with only the projected fields.
    pub(crate) fn apply(&self, document: &mut Document) {
        match self {
            Self::All => {}
            Self::None => document.retain(|_| false, |_| false),
            Self::Fields { fields, extra } => {
                document.retain(|f| fields.contains(&f), |name| extra.contains(name));
            }
        }
    }
}
