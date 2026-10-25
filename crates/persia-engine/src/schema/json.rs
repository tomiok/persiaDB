//! The JSON form of a collection definition (SPEC §6.2), parsed strictly (ADR-0022): unknown keys, duplicate keys,
//! `null` for an optional key and options foreign to the field's type are all errors.
//!
//! The serde types here mirror the JSON; [`super::validate`] turns them into a [`CollectionSchema`].

use std::collections::BTreeSet;
use std::fmt;
use std::marker::PhantomData;

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{
    AnalyzerRef, CollectionSchema, Fallback, FieldKind, FilterRelaxation, KeywordOptions, Level,
    Levels, MAX_SCHEMA_JSON_LEN, Parts, RankFn, RankingConfig, RelaxationConfig, SchemaError,
    StaticRank, TextOptions, validate,
};

pub(super) fn from_json(json: &str) -> Result<CollectionSchema, SchemaError> {
    from_raw(parse(json)?)
}

fn parse(json: &str) -> Result<RawSchema, SchemaError> {
    if json.len() > MAX_SCHEMA_JSON_LEN {
        return Err(SchemaError::Json(format!(
            "{} bytes, at most {MAX_SCHEMA_JSON_LEN}",
            json.len()
        )));
    }
    let raw: Object<RawSchema> =
        serde_json::from_str(json).map_err(|e| SchemaError::Json(e.to_string()))?;
    Ok(raw.0)
}

pub(super) fn to_json(schema: &CollectionSchema) -> Result<String, SchemaError> {
    serde_json::to_string(&to_raw(schema)).map_err(|e| SchemaError::Json(e.to_string()))
}

/// [`from_json`] for a schema the engine wrote (a manifest, SPEC §8.7): every field must carry its `id` and every
/// text field its analyzer as `"name@version"`, as [`to_json`] writes them. Defaults there would renumber the fields
/// of an evolved schema, or move a field to a newer analyzer version, without anything in the text changing.
pub(super) fn from_stored_json(json: &str) -> Result<CollectionSchema, SchemaError> {
    let raw = parse(json)?;
    for (name, field) in &raw.fields.0 {
        let (id, analyzer) = match field {
            RawField::Text { id, analyzer, .. } => (id, Some(analyzer)),
            RawField::Keyword { id, .. } | RawField::Stored { id } => (id, None),
            RawField::I64(s) | RawField::F64(s) | RawField::Bool(s) | RawField::Datetime(s) => {
                (&s.id, None)
            }
        };
        if id.is_none() {
            return Err(SchemaError::Json(format!(
                "stored schema: field {name:?} has no id"
            )));
        }
        if analyzer.is_some_and(|a| !a.as_deref().is_some_and(|a| a.contains('@'))) {
            return Err(SchemaError::Json(format!(
                "stored schema: field {name:?} does not pin its analyzer version"
            )));
        }
    }
    from_raw(raw)
}

impl Serialize for CollectionSchema {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        to_raw(self).serialize(serializer)
    }
}

/// For trusted, already-bounded input such as a checksummed manifest (ROADMAP 3.3). A `Deserializer` cannot see the
/// input's size, so this path does not apply [`MAX_SCHEMA_JSON_LEN`], and a `serde_json::Value`
/// has already merged duplicate keys: untrusted text goes through [`CollectionSchema::from_json`].
impl<'de> Deserialize<'de> for CollectionSchema {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        from_raw(Object::<RawSchema>::deserialize(deserializer)?.0).map_err(de::Error::custom)
    }
}

/// An optional key that, when present, must hold a `T` (not `null`).
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

/// [`present`] for a struct: an object, never the positional array form.
fn present_object<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    Object::deserialize(d).map(|o| Some(o.0))
}

/// A `T` read from an object only. Derived struct and internally tagged enum visitors also accept an array,
/// taking values by position: that would bypass key names, and so strict parsing (ADR-0022).
struct Object<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for ObjectVisitor<T> {
            type Value = T;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object")
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<T, A::Error> {
                T::deserialize(de::value::MapAccessDeserializer::new(map))
            }
        }

        deserializer
            .deserialize_map(ObjectVisitor(PhantomData))
            .map(Object)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSchema {
    name: String,
    fields: UniqueMap<RawField>,
    #[serde(
        default,
        deserialize_with = "present_object",
        skip_serializing_if = "Option::is_none"
    )]
    ranking: Option<RawRanking>,
    #[serde(
        default,
        deserialize_with = "present_object",
        skip_serializing_if = "Option::is_none"
    )]
    relaxation: Option<RawRelaxation>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    synonyms: Option<Vec<Vec<String>>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    strict: Option<bool>,
}

/// One field; the `type` key selects the variant, and each variant accepts only its type's options.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum RawField {
    Text {
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        id: Option<u32>,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        analyzer: Option<String>,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        boost: Option<f64>,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        positions: Option<bool>,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        k1: Option<f64>,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        b: Option<f64>,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        stored: Option<bool>,
    },
    Keyword {
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        id: Option<u32>,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        facet: Option<bool>,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        stored: Option<bool>,
    },
    I64(RawScalar),
    F64(RawScalar),
    Bool(RawScalar),
    Datetime(RawScalar),
    Stored {
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        id: Option<u32>,
    },
}

/// Options of `i64`, `f64`, `bool` and `datetime` fields.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScalar {
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    id: Option<u32>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    stored: Option<bool>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRanking {
    #[serde(
        default,
        deserialize_with = "present_object",
        skip_serializing_if = "Option::is_none"
    )]
    static_rank: Option<RawStaticRank>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStaticRank {
    field: String,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    weight: Option<f64>,
    #[serde(
        rename = "fn",
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    function: Option<RankFn>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRelaxation {
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    enabled: Option<bool>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    min_results: Option<u32>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    levels: Option<Levels>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    fallback: Option<Fallback>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    fill: Option<bool>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    filters: Option<FilterRelaxation>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    budget_ms: Option<u32>,
}

fn from_raw(raw: RawSchema) -> Result<CollectionSchema, SchemaError> {
    let fields = raw
        .fields
        .0
        .into_iter()
        .map(|(name, field)| {
            let (id, kind) = kind_from_raw(&name, field)?;
            Ok((name, id, kind))
        })
        .collect::<Result<Vec<_>, SchemaError>>()?;
    let ranking = RankingConfig {
        static_rank: raw.ranking.and_then(|r| r.static_rank).map(|s| {
            let default = StaticRank::new(s.field);
            StaticRank {
                weight: s.weight.unwrap_or(default.weight),
                function: s.function.unwrap_or(default.function),
                field: default.field,
            }
        }),
    };
    let d = RelaxationConfig::default();
    let relaxation = raw
        .relaxation
        .map_or_else(RelaxationConfig::default, |r| RelaxationConfig {
            enabled: r.enabled.unwrap_or(d.enabled),
            min_results: r.min_results.unwrap_or(d.min_results),
            levels: r.levels.unwrap_or_default(),
            fallback: r.fallback.unwrap_or(d.fallback),
            fill: r.fill.unwrap_or(d.fill),
            filters: r.filters.unwrap_or(d.filters),
            budget_ms: r.budget_ms.unwrap_or(d.budget_ms),
        });
    let parts = Parts {
        name: raw.name,
        ranking,
        relaxation,
        synonyms: raw.synonyms.unwrap_or_default(),
        strict: raw.strict.unwrap_or(true),
    };
    validate::build(parts, fields)
}

fn kind_from_raw(field: &str, raw: RawField) -> Result<(Option<u32>, FieldKind), SchemaError> {
    let stored = |s: Option<bool>| s.unwrap_or(true);
    Ok(match raw {
        RawField::Text {
            id,
            analyzer,
            boost,
            positions,
            k1,
            b,
            stored: s,
        } => {
            let d = TextOptions::default();
            let analyzer = match analyzer {
                None => d.analyzer,
                Some(text) => {
                    AnalyzerRef::parse(&text).ok_or_else(|| SchemaError::UnknownAnalyzer {
                        field: field.to_owned(),
                        analyzer: text,
                    })?
                }
            };
            let options = TextOptions {
                analyzer,
                boost: boost.unwrap_or(d.boost),
                positions: positions.unwrap_or(d.positions),
                k1: k1.unwrap_or(d.k1),
                b: b.unwrap_or(d.b),
                stored: stored(s),
            };
            (id, FieldKind::Text(options))
        }
        RawField::Keyword {
            id,
            facet,
            stored: s,
        } => {
            let d = KeywordOptions::default();
            let options = KeywordOptions {
                facet: facet.unwrap_or(d.facet),
                stored: s.unwrap_or(d.stored),
            };
            (id, FieldKind::Keyword(options))
        }
        RawField::I64(s) => (
            s.id,
            FieldKind::I64 {
                stored: stored(s.stored),
            },
        ),
        RawField::F64(s) => (
            s.id,
            FieldKind::F64 {
                stored: stored(s.stored),
            },
        ),
        RawField::Bool(s) => (
            s.id,
            FieldKind::Bool {
                stored: stored(s.stored),
            },
        ),
        RawField::Datetime(s) => (
            s.id,
            FieldKind::Datetime {
                stored: stored(s.stored),
            },
        ),
        RawField::Stored { id } => (id, FieldKind::Stored),
    })
}

/// Every option written out, so the JSON means the same to a release whose defaults differ (ADR-0022).
fn to_raw(schema: &CollectionSchema) -> RawSchema {
    let fields = schema
        .fields()
        .iter()
        .map(|f| {
            let id = Some(f.id().get());
            let scalar = |stored: bool| RawScalar {
                id,
                stored: Some(stored),
            };
            let raw = match f.kind() {
                FieldKind::Text(o) => RawField::Text {
                    id,
                    analyzer: Some(o.analyzer.to_string()),
                    boost: Some(o.boost),
                    positions: Some(o.positions),
                    k1: Some(o.k1),
                    b: Some(o.b),
                    stored: Some(o.stored),
                },
                FieldKind::Keyword(o) => RawField::Keyword {
                    id,
                    facet: Some(o.facet),
                    stored: Some(o.stored),
                },
                FieldKind::I64 { stored } => RawField::I64(scalar(*stored)),
                FieldKind::F64 { stored } => RawField::F64(scalar(*stored)),
                FieldKind::Bool { stored } => RawField::Bool(scalar(*stored)),
                FieldKind::Datetime { stored } => RawField::Datetime(scalar(*stored)),
                FieldKind::Stored => RawField::Stored { id },
            };
            (f.name().to_owned(), raw)
        })
        .collect();
    let r = schema.relaxation();
    RawSchema {
        name: schema.name().to_owned(),
        fields: UniqueMap(fields),
        ranking: schema.ranking().static_rank.as_ref().map(|s| RawRanking {
            static_rank: Some(RawStaticRank {
                field: s.field.clone(),
                weight: Some(s.weight),
                function: Some(s.function),
            }),
        }),
        relaxation: Some(RawRelaxation {
            enabled: Some(r.enabled),
            min_results: Some(r.min_results),
            levels: Some(r.levels.clone()),
            fallback: Some(r.fallback),
            fill: Some(r.fill),
            filters: Some(r.filters),
            budget_ms: Some(r.budget_ms),
        }),
        synonyms: Some(schema.synonyms().to_vec()),
        strict: Some(schema.strict()),
    }
}

/// A JSON object kept in order, rejecting duplicate keys (`serde_json`'s maps keep the last one silently).
struct UniqueMap<T>(Vec<(String, T)>);

impl<T: Serialize> Serialize for UniqueMap<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in &self.0 {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for UniqueMap<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueMapVisitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for UniqueMapVisitor<T> {
            type Value = UniqueMap<T>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object with distinct keys")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut seen = BTreeSet::new();
                let mut entries = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !seen.insert(key.clone()) {
                        return Err(de::Error::custom(format!("duplicate field {key:?}")));
                    }
                    entries.push((key, map.next_value::<Object<T>>()?.0));
                }
                Ok(UniqueMap(entries))
            }
        }

        deserializer.deserialize_map(UniqueMapVisitor(PhantomData))
    }
}

impl Serialize for Levels {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Default => serializer.serialize_str("default"),
            Self::Custom(levels) => serializer.collect_seq(levels),
        }
    }
}

impl<'de> Deserialize<'de> for Levels {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct LevelsVisitor;

        impl<'de> Visitor<'de> for LevelsVisitor {
            type Value = Levels;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(r#""default" or an array of level names"#)
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Levels, E> {
                if value == "default" {
                    Ok(Levels::Default)
                } else {
                    Err(E::invalid_value(de::Unexpected::Str(value), &self))
                }
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Levels, A::Error> {
                let mut levels = Vec::new();
                while let Some(level) = seq.next_element::<Level>()? {
                    levels.push(level);
                }
                Ok(Levels::Custom(levels))
            }
        }

        deserializer.deserialize_any(LevelsVisitor)
    }
}
