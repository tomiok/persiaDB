//! Which schema changes a live collection accepts (SPEC §6.2.4).

use super::{CollectionSchema, Field, FieldId, FieldKind, Parts, TextOptions};

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
    /// (`boost`, `k1`, `b`); an analyzer `proposed` names without a version binds to the version the field has
    /// (SPEC §6.2.4); added fields get the next ids in ascending byte order of their names; `proposed`'s
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
            let kind = bind_analyzer_version(current, next);
            check_unchanged(current, &next.name, &kind)?;
            fields.push(Field {
                id: current.id,
                name: current.name.clone(),
                kind,
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

/// `next`'s kind, with an unversioned analyzer bound to the version `current` was indexed with when both name the same
/// analyzer (SPEC §6.2.4): re-applying an unchanged definition after a release adds a newer version is not a change.
fn bind_analyzer_version(current: &Field, next: &Field) -> FieldKind {
    match (&current.kind, &next.kind) {
        (FieldKind::Text(a), FieldKind::Text(b))
            if !b.analyzer.is_pinned() && a.analyzer.name() == b.analyzer.name() =>
        {
            FieldKind::Text(TextOptions {
                analyzer: a.analyzer.clone(),
                ..b.clone()
            })
        }
        (_, kind) => kind.clone(),
    }
}

fn check_unchanged(current: &Field, name: &str, next: &FieldKind) -> Result<(), EvolutionError> {
    let changed = |property, current: String, proposed: String| {
        Err(EvolutionError::FieldChanged {
            field: name.to_owned(),
            property,
            current,
            proposed,
        })
    };
    let (a, b) = (current.field_type(), next.field_type());
    if a != b {
        return changed("type", a.to_string(), b.to_string());
    }
    match (&current.kind, next) {
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
    let (a, b) = (current.kind.stored(), next.stored());
    if a != b {
        return changed("stored", a.to_string(), b.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{AnalyzerRef, SchemaBuilder};

    fn with_analyzer(analyzer: AnalyzerRef) -> CollectionSchema {
        let options = TextOptions {
            analyzer,
            ..TextOptions::default()
        };
        SchemaBuilder::new("docs")
            .field("title", FieldKind::Text(options))
            .build()
            .unwrap()
    }

    fn analyzer_of(schema: &CollectionSchema) -> AnalyzerRef {
        match schema.field("title").map(Field::kind) {
            Some(FieldKind::Text(o)) => o.analyzer.clone(),
            other => panic!("{other:?}"),
        }
    }

    /// SPEC §6.2.4: a field indexed with `es@2` (a version this build does not ship, standing in for an older or
    /// newer one) accepts an unversioned `es`, and keeps `es@2`; a pinned different version or another analyzer is a
    /// change.
    #[test]
    fn unversioned_analyzer_binds_to_the_field_version() {
        let es = AnalyzerRef::parse("es").unwrap();
        let mut current = with_analyzer(es.clone());
        // Build with a valid analyzer, then move the field to version 2 as a stored schema of another build would.
        if let Some(Field {
            kind: FieldKind::Text(o),
            ..
        }) = current.fields.first_mut()
        {
            o.analyzer = es.at_unchecked(2);
        }
        let evolved = current.evolve(&with_analyzer(es.clone())).unwrap();
        assert_eq!(analyzer_of(&evolved), es.at_unchecked(2));
        assert_eq!(evolved.hash(), current.hash());

        for (proposed, shown) in [("es@1", "es@1"), ("en", "en@1"), ("en@1", "en@1")] {
            let proposed = with_analyzer(AnalyzerRef::parse(proposed).unwrap());
            assert_eq!(
                current.evolve(&proposed),
                Err(EvolutionError::FieldChanged {
                    field: "title".to_owned(),
                    property: "analyzer",
                    current: "es@2".to_owned(),
                    proposed: shown.to_owned(),
                })
            );
        }
    }
}
