//! Which schema changes a live collection accepts (SPEC §6.2.4).

use super::{CollectionSchema, Field, FieldId, FieldKind, Parts};

/// A new definition changes something already indexed (SPEC §6.2.4).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvolutionError {
    /// The collection name differs.
    #[error("collection {current:?} cannot be renamed to {proposed:?}")]
    CollectionRenamed {
        /// The current name.
        current: String,
        /// The name in the new definition.
        proposed: String,
    },
    /// A current field is missing; fields are never removed in v1.
    #[error("field {field:?} cannot be removed")]
    FieldRemoved {
        /// The field.
        field: String,
    },
    /// A property that shapes what is indexed changed; that needs a reindex into a new collection.
    #[error(
        "field {field:?}: {property} cannot change from {current} to {proposed} (reindex into a new collection)"
    )]
    FieldChanged {
        /// The field.
        field: String,
        /// `type`, `analyzer`, `positions`, `facet` or `stored`.
        property: &'static str,
        /// The current value.
        current: String,
        /// The value in the new definition.
        proposed: String,
    },
}

impl CollectionSchema {
    /// The schema that replaces `self` with the definition `proposed` (SPEC §6.2.4).
    ///
    /// Fields are matched by name: current fields keep their ids and take `proposed`'s query-time options
    /// (`boost`, `k1`, `b`); added fields get the next ids in ascending byte order of their names; `proposed`'s
    /// ids are ignored. Ranking, relaxation, synonyms and `strict` come from `proposed`.
    ///
    /// ```
    /// use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
    ///
    /// let text = || FieldKind::Text(TextOptions::default());
    /// let v1 = SchemaBuilder::new("docs").field("title", text()).build()?;
    /// let v2 = v1.evolve(&SchemaBuilder::new("docs").field("title", text()).field("body", text()).build()?)?;
    /// // "body" sorts first, yet "title" keeps id 0.
    /// assert_eq!(v2.field("title").map(|f| f.id().get()), Some(0));
    /// assert_eq!(v2.field("body").map(|f| f.id().get()), Some(1));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`EvolutionError`] if the name changes, a field is removed, or a field's type, analyzer, `positions`,
    /// `facet` or `stored` changes.
    pub fn evolve(&self, proposed: &CollectionSchema) -> Result<CollectionSchema, EvolutionError> {
        if self.name != proposed.name {
            return Err(EvolutionError::CollectionRenamed {
                current: self.name.clone(),
                proposed: proposed.name.clone(),
            });
        }
        let mut fields = Vec::with_capacity(proposed.fields.len());
        for current in &self.fields {
            let next =
                proposed
                    .field(&current.name)
                    .ok_or_else(|| EvolutionError::FieldRemoved {
                        field: current.name.clone(),
                    })?;
            check_unchanged(current, next)?;
            fields.push(Field {
                id: current.id,
                name: current.name.clone(),
                kind: next.kind.clone(),
            });
        }
        // `by_name` iterates in ascending byte order, which assigns the added fields' ids.
        let added = proposed
            .by_name
            .keys()
            .filter(|name| self.field(name).is_none())
            .filter_map(|name| proposed.field(name));
        for field in added {
            // At most MAX_FIELDS fields, so the position fits in u32.
            let id = u32::try_from(fields.len()).unwrap_or(u32::MAX);
            fields.push(Field {
                id: FieldId(id),
                name: field.name.clone(),
                kind: field.kind.clone(),
            });
        }
        let parts = Parts {
            name: proposed.name.clone(),
            ranking: proposed.ranking.clone(),
            relaxation: proposed.relaxation.clone(),
            synonyms: proposed.synonyms.clone(),
            strict: proposed.strict,
        };
        Ok(CollectionSchema::assemble(parts, fields))
    }
}

fn check_unchanged(current: &Field, next: &Field) -> Result<(), EvolutionError> {
    let changed = |property, current: String, proposed: String| {
        Err(EvolutionError::FieldChanged {
            field: next.name.clone(),
            property,
            current,
            proposed,
        })
    };
    let (a, b) = (current.field_type(), next.field_type());
    if a != b {
        return changed("type", a.to_string(), b.to_string());
    }
    match (&current.kind, &next.kind) {
        (FieldKind::Text(a), FieldKind::Text(b)) => {
            if a.analyzer != b.analyzer {
                return changed("analyzer", a.analyzer.to_string(), b.analyzer.to_string());
            }
            if a.positions != b.positions {
                return changed(
                    "positions",
                    a.positions.to_string(),
                    b.positions.to_string(),
                );
            }
        }
        (FieldKind::Keyword(a), FieldKind::Keyword(b)) if a.facet != b.facet => {
            return changed("facet", a.facet.to_string(), b.facet.to_string());
        }
        _ => {}
    }
    let (a, b) = (current.kind.stored(), next.kind.stored());
    if a != b {
        return changed("stored", a.to_string(), b.to_string());
    }
    Ok(())
}
