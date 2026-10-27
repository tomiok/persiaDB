//! The rules of SPEC §6.2.1–§6.2.3; every way of building a schema goes through [`build`].

use std::collections::BTreeSet;

use super::{
    CollectionSchema, Field, FieldId, FieldKind, FieldType, Levels, MAX_FIELDS, MAX_NAME_LEN,
    MAX_SYNONYM_BYTES, MAX_SYNONYM_GROUP_LEN, MAX_SYNONYM_GROUPS, MAX_SYNONYM_TERM_LEN,
    NameProblem, Parts, SchemaError,
};

const COLLECTION_PATTERN: &str = "[a-z][a-z0-9_-]*";
const FIELD_PATTERN: &str = "[A-Za-z][A-Za-z0-9_]*";
/// Names every document has (SPEC §6.1), reserved in any letter case.
const RESERVED_FIELDS: [&str; 2] = ["id", "blob"];

/// Validates a definition; `id`s are either all absent (assigned from the names) or all present.
pub(super) fn build(
    mut parts: Parts,
    fields: impl IntoIterator<Item = (String, Option<u32>, FieldKind)>,
) -> Result<CollectionSchema, SchemaError> {
    check_collection_name(&parts.name)?;
    let mut fields: Vec<_> = fields.into_iter().collect();
    if fields.len() > MAX_FIELDS {
        return Err(SchemaError::TooManyFields {
            count: fields.len(),
        });
    }
    let mut names = BTreeSet::new();
    for (name, _, kind) in &mut fields {
        check_field_name(name)?;
        if !names.insert(name.as_str()) {
            return Err(SchemaError::DuplicateField { name: name.clone() });
        }
        check_kind(name, kind)?;
    }
    let fields = assign_ids(fields)?;
    let schema_fields = |name: &str| {
        fields
            .iter()
            .find(|f| f.name == name)
            .map(Field::field_type)
    };
    check_ranking(&mut parts, schema_fields)?;
    check_relaxation(&parts)?;
    check_synonyms(&parts.synonyms)?;
    Ok(CollectionSchema::assemble(parts, fields))
}

fn check_collection_name(name: &str) -> Result<(), SchemaError> {
    collection_name_problem(name).map_or(Ok(()), |problem| {
        Err(SchemaError::CollectionName {
            name: name.to_owned(),
            problem,
        })
    })
}

/// What is wrong with collection name `name`, if anything; WAL records name collections too (SPEC §8.2.1).
pub(crate) fn collection_name_problem(name: &str) -> Option<NameProblem> {
    name_problem(name).or_else(|| {
        let mut bytes = name.bytes();
        let ok = bytes.next().is_some_and(|b| b.is_ascii_lowercase())
            && bytes
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        (!ok).then_some(NameProblem::InvalidChars {
            pattern: COLLECTION_PATTERN,
        })
    })
}

fn check_field_name(name: &str) -> Result<(), SchemaError> {
    field_name_problem(name).map_or(Ok(()), |problem| {
        Err(SchemaError::FieldName {
            name: name.to_owned(),
            problem,
        })
    })
}

/// What is wrong with field name `name`, if anything; documents' extra fields follow the same rule (SPEC §6.4.1).
pub(crate) fn field_name_problem(name: &str) -> Option<NameProblem> {
    name_problem(name).or_else(|| {
        if name.starts_with('_') || RESERVED_FIELDS.iter().any(|r| r.eq_ignore_ascii_case(name)) {
            return Some(NameProblem::Reserved);
        }
        let mut bytes = name.bytes();
        let ok = bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
            && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_');
        (!ok).then_some(NameProblem::InvalidChars {
            pattern: FIELD_PATTERN,
        })
    })
}

fn name_problem(name: &str) -> Option<NameProblem> {
    if name.is_empty() {
        Some(NameProblem::Empty)
    } else if name.len() > MAX_NAME_LEN {
        Some(NameProblem::TooLong { len: name.len() })
    } else {
        None
    }
}

fn check_kind(field: &str, kind: &mut FieldKind) -> Result<(), SchemaError> {
    let FieldKind::Text(text) = kind else {
        return Ok(());
    };
    let option = |o: &str| format!("fields.{field}.{o}");
    check_f64(
        &option("boost"),
        text.boost,
        |v| v > 0.0 && v <= 1000.0,
        "0 < boost <= 1000",
    )?;
    text.k1 = check_f64(
        &option("k1"),
        text.k1,
        |v| (0.0..=10.0).contains(&v),
        "0 <= k1 <= 10",
    )?;
    text.b = check_f64(
        &option("b"),
        text.b,
        |v| (0.0..=1.0).contains(&v),
        "0 <= b <= 1",
    )?;
    Ok(())
}

/// Checks `value` (NaN and infinities never pass) and returns it with `-0.0` read as `0.0` (SPEC §6.2.2).
fn check_f64(
    option: &str,
    value: f64,
    ok: impl FnOnce(f64) -> bool,
    allowed: &'static str,
) -> Result<f64, SchemaError> {
    if value.is_finite() && ok(value) {
        Ok(value + 0.0)
    } else {
        Err(SchemaError::OutOfRange {
            option: option.to_owned(),
            value: format!("{value:?}"),
            allowed,
        })
    }
}

fn check_u32(
    option: &str,
    value: u32,
    range: std::ops::RangeInclusive<u32>,
    allowed: &'static str,
) -> Result<(), SchemaError> {
    if range.contains(&value) {
        Ok(())
    } else {
        Err(SchemaError::OutOfRange {
            option: option.to_owned(),
            value: value.to_string(),
            allowed,
        })
    }
}

/// Orders fields by id: given ids must be exactly `0..n`; without ids, names in ascending byte order.
fn assign_ids(
    mut fields: Vec<(String, Option<u32>, FieldKind)>,
) -> Result<Vec<Field>, SchemaError> {
    let field_count = fields.len();
    let given = fields.iter().filter(|(_, id, _)| id.is_some()).count();
    let ids_error = |problem: String| SchemaError::FieldIds {
        field_count,
        problem,
    };
    if given == 0 {
        fields.sort_by(|a, b| a.0.cmp(&b.0));
    } else if given < field_count {
        return Err(ids_error(format!(
            "{given} of {field_count} fields have an id"
        )));
    } else {
        fields.sort_by_key(|(_, id, _)| *id);
    }
    fields
        .into_iter()
        .enumerate()
        .map(|(position, (name, id, kind))| {
            let expected = u32::try_from(position)
                .map_err(|_| ids_error(format!("position {position} does not fit in u32")))?;
            match id {
                Some(id) if id != expected => Err(ids_error(format!(
                    "field {name:?} has id {id}, expected {expected}"
                ))),
                _ => Ok(Field {
                    id: FieldId(expected),
                    name,
                    kind,
                }),
            }
        })
        .collect()
}

fn check_ranking(
    parts: &mut Parts,
    field_type: impl Fn(&str) -> Option<FieldType>,
) -> Result<(), SchemaError> {
    let Some(rank) = &mut parts.ranking.static_rank else {
        return Ok(());
    };
    let problem = match field_type(&rank.field) {
        None => Some("no such field"),
        Some(FieldType::I64 | FieldType::F64 | FieldType::Datetime) => None,
        Some(_) => Some("not an i64, f64 or datetime field"),
    };
    if let Some(problem) = problem {
        return Err(SchemaError::StaticRankField {
            field: rank.field.clone(),
            problem,
        });
    }
    rank.weight = check_f64(
        "ranking.static_rank.weight",
        rank.weight,
        |v| (0.0..=1000.0).contains(&v),
        "0 <= weight <= 1000",
    )?;
    Ok(())
}

fn check_relaxation(parts: &Parts) -> Result<(), SchemaError> {
    let relax = &parts.relaxation;
    check_u32(
        "relaxation.min_results",
        relax.min_results,
        1..=10_000,
        "1..=10000",
    )?;
    check_u32(
        "relaxation.budget_ms",
        relax.budget_ms,
        1..=60_000,
        "1..=60000",
    )?;
    if let Levels::Custom(levels) = &relax.levels {
        for pair in levels.windows(2) {
            if let [a, b] = pair {
                if a == b {
                    return Err(SchemaError::Levels {
                        problem: "a level is listed twice",
                    });
                }
                if a > b {
                    return Err(SchemaError::Levels {
                        problem: "levels must be in cascade order (synonyms, drop_terms, prefix, fuzzy)",
                    });
                }
            }
        }
    }
    Ok(())
}

fn check_synonyms(groups: &[Vec<String>]) -> Result<(), SchemaError> {
    if groups.len() > MAX_SYNONYM_GROUPS {
        return Err(SchemaError::Synonyms {
            group: groups.len(),
            problem: format!("{} groups, at most {MAX_SYNONYM_GROUPS}", groups.len()),
        });
    }
    let mut total = 0usize;
    for (group, terms) in groups.iter().enumerate() {
        let error = |problem: String| SchemaError::Synonyms { group, problem };
        if !(2..=MAX_SYNONYM_GROUP_LEN).contains(&terms.len()) {
            return Err(error(format!(
                "{} terms, a group has 2..={MAX_SYNONYM_GROUP_LEN}",
                terms.len()
            )));
        }
        let mut seen = BTreeSet::new();
        for term in terms {
            if term.trim().is_empty()
                || term.len() > MAX_SYNONYM_TERM_LEN
                || term.chars().any(char::is_control)
            {
                return Err(error(format!(
                    "term {term:?} must be 1..={MAX_SYNONYM_TERM_LEN} bytes, not only whitespace, without control characters"
                )));
            }
            if !seen.insert(term.as_str()) {
                return Err(error(format!("term {term:?} is listed twice")));
            }
            total = total.saturating_add(term.len());
            if total > MAX_SYNONYM_BYTES {
                return Err(error(format!(
                    "synonym terms exceed {MAX_SYNONYM_BYTES} bytes in total"
                )));
            }
        }
    }
    Ok(())
}
