//! The cursor token (SPEC §7.4, ADR-0062): a [`Cursor`] as opaque text, the form `search_after` has in a request's
//! JSON and a hit's cursor has in a response.
//!
//! The text is base64url without padding ([`super::base64url`]) of a binary encoding that starts with a version
//! byte and ends with the CRC-32C of every byte before it. It is not an on-disk format: nothing stores a token.
//! Clients hold tokens across requests, though, so the version byte is the compatibility rule: any change to the
//! layout, or to what a field means, takes a new version, and a build refuses the versions it does not read.
//!
//! Version 1, integers little-endian, `varint` the canonical LEB128 of SPEC §4.1:
//!
//! ```text
//! u8      version = 1
//! u8      flags: 1 proximity, 2 has step, 4 has relaxation settings, 8 location is the memtable; others 0
//! u64     origin.seq
//! u64     origin.version (the memtable view's)
//! varint  collection name length, then its UTF-8 bytes
//! varint  segment index (absent when the location is the memtable)
//! varint  docid (u32)
//! varint  number of sort values, then each: u8 tag, 0 missing | 1 score (u64, the f64's bits) | 2 key (u64) |
//!         3 keyword (varint length, bytes)
//! varint  number of sort keys, at most 16, then each: u8 bits, 1 field (else the score) | 2 descending | 4 missing
//!         first (a field's only); then for a field, varint field id
//! varint  step, varint last, varint hit                      (only with "has step")
//! varint  level bits, varint min_results, u8 1 fill | 2 filters last_resort   (only with "has relaxation settings")
//! u32     CRC-32C of every byte above
//! ```
//!
//! Decoding checks the version first (a later version may define everything after it differently), then the
//! checksum, then each field, and accepts only the one encoding of a cursor: no unknown flag, tag or level bit, no
//! overlong varint, no byte left over. So `to_token` of a decoded cursor is the token it was decoded from. The
//! checksum finds damage, not forgery: a token is not authenticated, and a search still checks the cursor it
//! decodes against its own sort, snapshot, collection and settings (SPEC §7.4).

use persia_format::checksum::crc32c;
use persia_format::widen::u64_from_usize;
use persia_format::{Reader, Writer};

use super::base64url;
use crate::db::{Cursor, CursorOrigin, Located, StepCursor};
use crate::relax::RelaxSettings;
use crate::schema::FieldId;
use crate::schema::FilterRelaxation;
use crate::search::{MAX_SORT_KEYS, Missing, Order, SortKey, SortValue};

/// The version of the tokens [`Cursor::to_token`] writes, and the only one [`Cursor::from_token`] reads.
pub const CURSOR_TOKEN_VERSION: u8 = 1;

/// The longest token [`Cursor::from_token`] reads, in bytes of text: a request's JSON is no longer (SPEC §7.1).
pub const MAX_CURSOR_TOKEN_LEN: usize = super::MAX_REQUEST_JSON_LEN;

const PROXIMITY: u8 = 1;
const HAS_STEP: u8 = 2;
const HAS_RELAX: u8 = 4;
const MEMTABLE: u8 = 8;
const FLAGS: u8 = PROXIMITY | HAS_STEP | HAS_RELAX | MEMTABLE;

const MISSING: u8 = 0;
const SCORE: u8 = 1;
const KEY: u8 = 2;
const KEYWORD: u8 = 3;

const FIELD: u8 = 1;
const DESC: u8 = 2;
const MISSING_FIRST: u8 = 4;

const FILL: u8 = 1;
const LAST_RESORT: u8 = 2;

/// Why text is not a cursor token this build reads (SPEC §7.4). Each is the caller's error: a request with such a
/// `search_after` is invalid.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CursorTokenError {
    /// Longer than [`MAX_CURSOR_TOKEN_LEN`].
    #[error("a cursor token of {len} bytes, at most {max}")]
    TooLong {
        /// Its length in bytes.
        len: usize,
        /// [`MAX_CURSOR_TOKEN_LEN`].
        max: usize,
    },
    /// Not base64url without padding: a character outside the alphabet, a length no encoding has, or unused bits set.
    #[error("a cursor token is base64url text without padding, as a search returned it")]
    Encoding,
    /// A version this build does not read: a token from a later version of Persia, or text that is no token.
    #[error("a cursor token of version {version}; this build reads version {CURSOR_TOKEN_VERSION}")]
    Version {
        /// The token's first byte.
        version: u8,
    },
    /// The checksum does not match: the token was cut or changed since a search returned it.
    #[error("a cursor token that was cut or changed: its checksum does not match")]
    Checksum,
    /// A field is not what version 1 defines, under a matching checksum: not a token Persia wrote.
    #[error("a cursor token that is not well formed: {what}")]
    Malformed {
        /// What is wrong.
        what: &'static str,
    },
}

impl Cursor {
    /// This cursor as an opaque token (SPEC §7.4, ADR-0062): what a response gives as a hit's cursor and a request
    /// takes back as `search_after`. Versioned and checksummed, see the module docs of
    /// [`crate::search::request`].
    ///
    /// ```
    /// use persia_engine::db::{Cursor, CursorOrigin, Located};
    /// use persia_engine::search::{SortKey, SortValue};
    /// use persia_engine::search::request::CursorTokenError;
    ///
    /// let cursor = Cursor {
    ///     sort: vec![SortKey::score()],
    ///     values: vec![SortValue::Score(1.5)],
    ///     location: Located::Segment { index: 0, docid: 7 },
    ///     origin: CursorOrigin { seq: 3, version: 0, collection: "items".into() },
    ///     relaxed: None,
    ///     proximity: false,
    ///     relax: None,
    /// };
    /// let token = cursor.to_token();
    /// assert_eq!(Cursor::from_token(&token)?, cursor);
    /// // A token that was cut is an error, never another cursor.
    /// assert!(Cursor::from_token(&token[..token.len() - 2]).is_err());
    /// # Ok::<(), CursorTokenError>(())
    /// ```
    pub fn to_token(&self) -> String {
        base64url::encode(&encode(self))
    }

    /// The cursor `token` holds ([`Self::to_token`]). It never panics, whatever the text.
    ///
    /// # Errors
    /// [`CursorTokenError`] for text that is not a token of [`CURSOR_TOKEN_VERSION`] as a search wrote it. A token of
    /// another collection or snapshot decodes: the search it is given to refuses it
    /// ([`crate::search::RequestError::CursorView`]).
    pub fn from_token(token: &str) -> Result<Self, CursorTokenError> {
        if token.len() > MAX_CURSOR_TOKEN_LEN {
            return Err(CursorTokenError::TooLong {
                len: token.len(),
                max: MAX_CURSOR_TOKEN_LEN,
            });
        }
        let bytes = base64url::decode(token).ok_or(CursorTokenError::Encoding)?;
        decode(&bytes)
    }
}

/// The bytes of `cursor`'s token: version 1 (module docs).
fn encode(cursor: &Cursor) -> Vec<u8> {
    let mut w = Writer::new();
    w.write_u8(CURSOR_TOKEN_VERSION);
    let mut flags = 0;
    if cursor.proximity {
        flags |= PROXIMITY;
    }
    if cursor.relaxed.is_some() {
        flags |= HAS_STEP;
    }
    if cursor.relax.is_some() {
        flags |= HAS_RELAX;
    }
    if matches!(cursor.location, Located::Memtable(_)) {
        flags |= MEMTABLE;
    }
    w.write_u8(flags);
    w.write_u64(cursor.origin.seq);
    w.write_u64(cursor.origin.version);
    write_bytes(&mut w, cursor.origin.collection.as_bytes());
    match cursor.location {
        Located::Memtable(docid) => w.write_varint_u32(docid),
        Located::Segment { index, docid } => {
            w.write_varint_u64(u64_from_usize(index));
            w.write_varint_u32(docid);
        }
    }
    w.write_varint_u64(u64_from_usize(cursor.values.len()));
    for value in &cursor.values {
        match value {
            SortValue::Missing => w.write_u8(MISSING),
            SortValue::Score(score) => {
                w.write_u8(SCORE);
                w.write_u64(score.to_bits());
            }
            SortValue::Key(key) => {
                w.write_u8(KEY);
                w.write_u64(*key);
            }
            SortValue::Keyword(bytes) => {
                w.write_u8(KEYWORD);
                write_bytes(&mut w, bytes);
            }
        }
    }
    w.write_varint_u64(u64_from_usize(cursor.sort.len()));
    for key in &cursor.sort {
        write_key(&mut w, *key);
    }
    if let Some(StepCursor { step, last, hit }) = cursor.relaxed {
        for n in [step, last, hit] {
            w.write_varint_u64(u64_from_usize(n));
        }
    }
    if let Some(settings) = cursor.relax {
        let (levels, min_results, fill, filters) = settings.to_parts();
        w.write_varint_u32(u32::from(levels));
        w.write_varint_u32(min_results);
        let mut bits = 0;
        if fill {
            bits |= FILL;
        }
        if filters == FilterRelaxation::LastResort {
            bits |= LAST_RESORT;
        }
        w.write_u8(bits);
    }
    let checksum = crc32c(w.as_slice());
    w.write_u32(checksum);
    w.into_inner()
}

/// One sort key: its bits, then a field's id.
fn write_key(w: &mut Writer, key: SortKey) {
    let desc = |order| if order == Order::Desc { DESC } else { 0 };
    match key {
        SortKey::Score { order } => w.write_u8(desc(order)),
        SortKey::Field {
            field,
            order,
            missing,
        } => {
            let first = if missing == Missing::First {
                MISSING_FIRST
            } else {
                0
            };
            w.write_u8(FIELD | desc(order) | first);
            w.write_varint_u32(field.get());
        }
    }
}

fn write_bytes(w: &mut Writer, bytes: &[u8]) {
    w.write_varint_u64(u64_from_usize(bytes.len()));
    w.write_bytes(bytes);
}

/// The cursor `bytes` encode. Every length is checked against the bytes left before anything is allocated for it.
fn decode(bytes: &[u8]) -> Result<Cursor, CursorTokenError> {
    let Some(&version) = bytes.first() else {
        return Err(malformed("empty"));
    };
    if version != CURSOR_TOKEN_VERSION {
        return Err(CursorTokenError::Version { version });
    }
    let Some((body, stored)) = bytes.split_last_chunk::<4>() else {
        return Err(CursorTokenError::Checksum);
    };
    if crc32c(body) != u32::from_le_bytes(*stored) {
        return Err(CursorTokenError::Checksum);
    }
    let mut r = Reader::new(body);
    r.skip(1).map_err(|_| malformed("empty"))?;
    let flags = r.read_u8().map_err(|_| malformed("no flags"))?;
    if flags & !FLAGS != 0 {
        return Err(malformed("an unknown flag"));
    }
    let origin = |_| malformed("no origin");
    let seq = r.read_u64().map_err(origin)?;
    let version = r.read_u64().map_err(origin)?;
    let collection = std::str::from_utf8(read_bytes(&mut r, "a collection name cut short")?)
        .map_err(|_| malformed("a collection name that is not UTF-8"))?
        .to_owned();
    let location = |_| malformed("no location");
    let location = if flags & MEMTABLE != 0 {
        Located::Memtable(r.read_varint_u32().map_err(location)?)
    } else {
        let index = read_usize(&mut r, "no location")?;
        let docid = r.read_varint_u32().map_err(location)?;
        Located::Segment { index, docid }
    };
    let count = read_usize(&mut r, "no sort values")?;
    // Each value takes a byte at least.
    if count > r.remaining() {
        return Err(malformed("fewer sort values than it states"));
    }
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(read_value(&mut r)?);
    }
    let keys = read_usize(&mut r, "no sort keys")?;
    if keys > MAX_SORT_KEYS {
        return Err(malformed("more sort keys than a sort has"));
    }
    let mut sort = Vec::with_capacity(keys);
    for _ in 0..keys {
        sort.push(read_key(&mut r)?);
    }
    let relaxed = if flags & HAS_STEP != 0 {
        let step = read_usize(&mut r, "no step")?;
        let last = read_usize(&mut r, "no step")?;
        let hit = read_usize(&mut r, "no step")?;
        Some(StepCursor { step, last, hit })
    } else {
        None
    };
    let relax = if flags & HAS_RELAX != 0 {
        Some(read_settings(&mut r)?)
    } else {
        None
    };
    if !r.is_empty() {
        return Err(malformed("bytes after its last field"));
    }
    Ok(Cursor {
        sort,
        values,
        location,
        origin: CursorOrigin {
            seq,
            version,
            collection,
        },
        relaxed,
        proximity: flags & PROXIMITY != 0,
        relax,
    })
}

fn read_value(r: &mut Reader<'_>) -> Result<SortValue, CursorTokenError> {
    let cut = |_| malformed("a sort value cut short");
    Ok(match r.read_u8().map_err(cut)? {
        MISSING => SortValue::Missing,
        SCORE => SortValue::Score(f64::from_bits(r.read_u64().map_err(cut)?)),
        KEY => SortValue::Key(r.read_u64().map_err(cut)?),
        KEYWORD => SortValue::Keyword(read_bytes(r, "a sort value cut short")?.to_vec()),
        _ => return Err(malformed("an unknown kind of sort value")),
    })
}

fn read_key(r: &mut Reader<'_>) -> Result<SortKey, CursorTokenError> {
    let cut = |_| malformed("a sort key cut short");
    let bits = r.read_u8().map_err(cut)?;
    if bits & !(FIELD | DESC | MISSING_FIRST) != 0 {
        return Err(malformed("an unknown kind of sort key"));
    }
    let order = if bits & DESC == 0 {
        Order::Asc
    } else {
        Order::Desc
    };
    if bits & FIELD == 0 {
        if bits & MISSING_FIRST != 0 {
            return Err(malformed("a score key with a placement of missing values"));
        }
        return Ok(SortKey::Score { order });
    }
    let missing = if bits & MISSING_FIRST == 0 {
        Missing::Last
    } else {
        Missing::First
    };
    let field = FieldId::new(r.read_varint_u32().map_err(cut)?);
    Ok(SortKey::Field {
        field,
        order,
        missing,
    })
}

fn read_settings(r: &mut Reader<'_>) -> Result<RelaxSettings, CursorTokenError> {
    let cut = |_| malformed("relaxation settings cut short");
    let levels = u16::try_from(r.read_varint_u32().map_err(cut)?)
        .map_err(|_| malformed("an unknown relaxation level"))?;
    let min_results = r.read_varint_u32().map_err(cut)?;
    let bits = r.read_u8().map_err(cut)?;
    if bits & !(FILL | LAST_RESORT) != 0 {
        return Err(malformed("an unknown relaxation setting"));
    }
    let filters = if bits & LAST_RESORT == 0 {
        FilterRelaxation::Never
    } else {
        FilterRelaxation::LastResort
    };
    RelaxSettings::from_parts(levels, min_results, bits & FILL != 0, filters)
        .ok_or(malformed("an unknown relaxation level"))
}

/// A varint length and that many bytes.
fn read_bytes<'a>(r: &mut Reader<'a>, what: &'static str) -> Result<&'a [u8], CursorTokenError> {
    let len = read_usize(r, what)?;
    r.read_bytes(len).map_err(|_| malformed(what))
}

fn read_usize(r: &mut Reader<'_>, what: &'static str) -> Result<usize, CursorTokenError> {
    let n = r.read_varint_u64().map_err(|_| malformed(what))?;
    usize::try_from(n).map_err(|_| malformed(what))
}

fn malformed(what: &'static str) -> CursorTokenError {
    CursorTokenError::Malformed { what }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relax::{RelaxOptions, RelaxPlan};
    use crate::schema::RelaxationConfig;

    fn plain() -> Cursor {
        Cursor {
            sort: vec![SortKey::score()],
            values: vec![SortValue::Score(1.5)],
            location: Located::Segment { index: 2, docid: 7 },
            origin: CursorOrigin {
                seq: 3,
                version: 9,
                collection: "items".into(),
            },
            relaxed: None,
            proximity: false,
            relax: None,
        }
    }

    /// Every field set: each kind of sort value, a step, relaxation settings, proximity, the memtable.
    fn full() -> Cursor {
        let options = RelaxOptions {
            min_results: Some(5),
            fill: Some(true),
            filters: Some(FilterRelaxation::LastResort),
            ..RelaxOptions::default()
        };
        let plan = RelaxPlan::new(&RelaxationConfig::default(), &options).unwrap();
        Cursor {
            sort: vec![
                SortKey::Field {
                    field: FieldId::new(3),
                    order: Order::Asc,
                    missing: Missing::First,
                },
                SortKey::Score { order: Order::Asc },
                SortKey::Field {
                    field: FieldId::new(u32::MAX),
                    order: Order::Desc,
                    missing: Missing::Last,
                },
                SortKey::Field {
                    field: FieldId::new(0),
                    order: Order::Desc,
                    missing: Missing::First,
                },
            ],
            values: vec![
                SortValue::Missing,
                SortValue::Score(-0.0),
                SortValue::Key(u64::MAX),
                SortValue::Keyword(b"nike\xff".to_vec()),
            ],
            location: Located::Memtable(u32::MAX),
            origin: CursorOrigin {
                seq: u64::MAX,
                version: 1 << 40,
                collection: "zapatillas ñ".into(),
            },
            relaxed: Some(StepCursor {
                step: 2,
                last: 300,
                hit: 4,
            }),
            proximity: true,
            relax: plan.settings(),
        }
    }

    /// The tokens of version 1, as this version wrote them: a change to the encoding fails here, and takes a new
    /// version (ADR-0062).
    #[test]
    fn golden_tokens_of_version_1() {
        let golden = [
            (
                plain(),
                "AQADAAAAAAAAAAkAAAAAAAAABWl0ZW1zAgcBAQAAAAAAAPg_AQKTxg4c",
            ),
            (
                full(),
                "AQ___________wAAAAAAAQAADXphcGF0aWxsYXMgw7H_____DwQAAQAAAAAAAACAAv__________A\
                 wVuaWtl_wQFAwAD_____w8HAAKsAgR_BQPl6RfX",
            ),
        ];
        for (cursor, token) in golden {
            assert_eq!(cursor.to_token(), token);
            let decoded = Cursor::from_token(token).unwrap();
            // `-0.0 == 0.0`, so compare the token too: it holds the sign.
            assert_eq!(decoded, cursor);
            assert_eq!(decoded.to_token(), token);
        }
    }

    #[test]
    fn a_score_keeps_its_bits() {
        for score in [-0.0, 0.0, f64::MAX, f64::MIN_POSITIVE, f64::INFINITY] {
            let cursor = Cursor {
                values: vec![SortValue::Score(score)],
                ..plain()
            };
            let decoded = Cursor::from_token(&cursor.to_token()).unwrap();
            let [SortValue::Score(got)] = decoded.values[..] else {
                panic!("{decoded:?}")
            };
            assert_eq!(got.to_bits(), score.to_bits());
        }
        // A NaN, which no search produces, comes back as written: the search's sort check refuses it.
        let nan = Cursor {
            values: vec![SortValue::Score(f64::NAN)],
            ..plain()
        };
        let token = nan.to_token();
        assert_eq!(Cursor::from_token(&token).unwrap().to_token(), token);
    }

    #[test]
    fn another_version_is_refused_before_anything_else_is_read() {
        let mut bytes = encode(&plain());
        for version in [0, 2, 0xff] {
            bytes[0] = version;
            // The checksum no longer matches either: the version is what the error names.
            assert_eq!(
                decode(&bytes),
                Err(CursorTokenError::Version { version }),
                "{version}"
            );
            // And with a matching checksum.
            let body = bytes.len() - 4;
            let sum = crc32c(&bytes[..body]).to_le_bytes();
            bytes[body..].copy_from_slice(&sum);
            assert_eq!(decode(&bytes), Err(CursorTokenError::Version { version }));
        }
        assert_eq!(
            Cursor::from_token(""),
            Err(CursorTokenError::Malformed { what: "empty" })
        );
    }

    #[test]
    fn a_cut_or_changed_token_fails_its_checksum() {
        for cursor in [plain(), full()] {
            let bytes = encode(&cursor);
            // Cut anywhere after the version byte.
            for len in 1..bytes.len() {
                assert_eq!(
                    decode(&bytes[..len]),
                    Err(CursorTokenError::Checksum),
                    "{len}"
                );
            }
            // One bit flipped anywhere after the version byte.
            for at in 1..bytes.len() {
                for bit in 0..8 {
                    let mut damaged = bytes.clone();
                    damaged[at] ^= 1 << bit;
                    assert_eq!(
                        decode(&damaged),
                        Err(CursorTokenError::Checksum),
                        "{at}:{bit}"
                    );
                }
            }
        }
    }

    /// `bytes` (a token's, less its checksum) with a matching checksum.
    fn summed(mut bytes: Vec<u8>) -> Vec<u8> {
        let sum = crc32c(&bytes);
        bytes.extend(sum.to_le_bytes());
        bytes
    }

    fn body(cursor: &Cursor) -> Vec<u8> {
        let mut bytes = encode(cursor);
        bytes.truncate(bytes.len() - 4);
        bytes
    }

    #[test]
    fn only_the_one_encoding_of_a_cursor_decodes() {
        let what = |bytes: Vec<u8>| match decode(&summed(bytes)) {
            Err(CursorTokenError::Malformed { what }) => what,
            other => panic!("{other:?}"),
        };
        let base = body(&plain());
        // An unknown flag.
        for flag in [16, 32, 64, 128] {
            let mut bytes = base.clone();
            bytes[1] |= flag;
            assert_eq!(what(bytes), "an unknown flag", "{flag}");
        }
        // Bytes after the last field, and a field cut short under a matching checksum.
        let mut longer = base.clone();
        longer.push(0);
        assert_eq!(what(longer), "bytes after its last field");
        assert_eq!(
            what(base[..base.len() - 1].to_vec()),
            "a sort key cut short"
        );
        assert_eq!(what(base[..base.len() - 2].to_vec()), "no sort keys");
        assert_eq!(
            what(base[..base.len() - 3].to_vec()),
            "a sort value cut short"
        );
        assert_eq!(what(vec![1]), "no flags");
        assert_eq!(what(vec![1, 0, 0]), "no origin");
        assert_eq!(what(base[..10].to_vec()), "no origin");
        assert_eq!(what(base[..18].to_vec()), "a collection name cut short");
        assert_eq!(what(base[..20].to_vec()), "a collection name cut short");
        // `items` ends at 24, the segment index is one byte, the docid one.
        assert_eq!(what(base[..24].to_vec()), "no location");
        assert_eq!(what(base[..25].to_vec()), "no location");
        assert_eq!(what(base[..26].to_vec()), "no sort values");
        // An unknown kind of sort value: the tag follows the count, at 27.
        let mut tagged = base.clone();
        tagged[27] = 4;
        assert_eq!(what(tagged), "an unknown kind of sort value");
        // More values stated than bytes left: nothing is allocated for them.
        let mut many = base[..26].to_vec();
        many.extend([0xff, 0xff, 0xff, 0xff, 0x0f]);
        assert_eq!(what(many), "fewer sort values than it states");
        // An overlong varint (`80 00` for 0) is not the one encoding.
        let mut overlong = base[..26].to_vec();
        overlong.extend([0x80, 0x00]);
        assert_eq!(what(overlong), "no sort values");
        // A collection name that is not UTF-8.
        let mut name = base.clone();
        name[19] = 0xff;
        assert_eq!(what(name), "a collection name that is not UTF-8");
    }

    #[test]
    fn steps_and_settings_are_checked_field_by_field() {
        let what = |bytes: Vec<u8>| match decode(&summed(bytes)) {
            Err(CursorTokenError::Malformed { what }) => what,
            other => panic!("{other:?}"),
        };
        let stepped = Cursor {
            relaxed: Some(StepCursor {
                step: 1,
                last: 2,
                hit: 2,
            }),
            ..plain()
        };
        let base = body(&stepped);
        for cut in 1..=3 {
            assert_eq!(what(base[..base.len() - cut].to_vec()), "no step", "{cut}");
        }
        let plan = RelaxPlan::new(&RelaxationConfig::default(), &RelaxOptions::default()).unwrap();
        let relaxing = Cursor {
            relax: plan.settings(),
            ..plain()
        };
        let base = body(&relaxing);
        let end = base.len();
        for cut in 1..=3 {
            assert_eq!(
                what(base[..end - cut].to_vec()),
                "relaxation settings cut short",
                "{cut}"
            );
        }
        // The last byte holds `fill` and `filters`: any other bit is unknown.
        for bit in [4, 8, 128] {
            let mut bytes = base.clone();
            bytes[end - 1] |= bit;
            assert_eq!(what(bytes), "an unknown relaxation setting", "{bit}");
        }
        // The default plan runs L0 to L5: bits 0 to 5, one byte. Bit 7 names no level, in 16 bits or beyond them.
        assert_eq!(base[end - 3], 0b0011_1111);
        let mut unknown = base[..end - 3].to_vec();
        unknown.extend([0xbf, 0x01, base[end - 2], base[end - 1]]);
        assert_eq!(what(unknown), "an unknown relaxation level");
        let mut wide = base[..end - 3].to_vec();
        wide.extend([0xbf, 0x80, 0x04, base[end - 2], base[end - 1]]);
        assert_eq!(what(wide), "an unknown relaxation level");
        // Each setting comes back as written.
        for (fill, filters) in [
            (false, FilterRelaxation::Never),
            (true, FilterRelaxation::Never),
            (false, FilterRelaxation::LastResort),
            (true, FilterRelaxation::LastResort),
        ] {
            let options = RelaxOptions {
                fill: Some(fill),
                filters: Some(filters),
                min_results: Some(7),
                ..RelaxOptions::default()
            };
            let plan = RelaxPlan::new(&RelaxationConfig::default(), &options).unwrap();
            let cursor = Cursor {
                relax: plan.settings(),
                ..plain()
            };
            let decoded = Cursor::from_token(&cursor.to_token()).unwrap();
            assert_eq!(decoded, cursor);
            let settings = decoded.relax.unwrap();
            assert_eq!(
                (settings.fill(), settings.filters(), settings.min_results()),
                (fill, filters, 7)
            );
        }
    }

    /// The sort keys (SPEC §7.4): each field, order and placement comes back as written, and only the one encoding of
    /// each key decodes.
    #[test]
    fn sort_keys_are_checked_key_by_key() {
        let what = |bytes: Vec<u8>| match decode(&summed(bytes)) {
            Err(CursorTokenError::Malformed { what }) => what,
            other => panic!("{other:?}"),
        };
        for order in [Order::Asc, Order::Desc] {
            for missing in [Missing::Last, Missing::First] {
                let cursor = Cursor {
                    sort: vec![
                        SortKey::Field {
                            field: FieldId::new(300),
                            order,
                            missing,
                        },
                        SortKey::Score { order },
                    ],
                    values: vec![SortValue::Key(1), SortValue::Score(2.0)],
                    ..plain()
                };
                assert_eq!(Cursor::from_token(&cursor.to_token()).unwrap(), cursor);
            }
        }
        // `plain`'s body ends with one key, the score descending: bits 2.
        let base = body(&plain());
        let end = base.len();
        assert_eq!(base[end - 2..], [1, DESC]);
        for bits in [8, 16, 128] {
            let mut bytes = base.clone();
            bytes[end - 1] |= bits;
            assert_eq!(what(bytes), "an unknown kind of sort key", "{bits}");
        }
        let mut placed = base.clone();
        placed[end - 1] |= MISSING_FIRST;
        assert_eq!(
            what(placed),
            "a score key with a placement of missing values"
        );
        // A field key cut before its id.
        let mut field = base.clone();
        field[end - 1] = FIELD;
        assert_eq!(what(field), "a sort key cut short");
        // More keys than a sort has, and more than the bytes left.
        let mut many = base[..end - 2].to_vec();
        many.push(17);
        many.extend([0; 17]);
        assert_eq!(what(many), "more sort keys than a sort has");
        let mut short = base[..end - 2].to_vec();
        short.push(16);
        assert_eq!(what(short), "a sort key cut short");
        // No key at all decodes (a forged cursor): the search refuses it as a cursor of another sort.
        let mut none = base[..end - 2].to_vec();
        none.push(0);
        assert_eq!(decode(&summed(none)).unwrap().sort, []);
    }

    #[test]
    fn text_that_is_no_token() {
        assert_eq!(
            Cursor::from_token("not a token!"),
            Err(CursorTokenError::Encoding)
        );
        // Standard base64's padding and alphabet are not the token's.
        let token = plain().to_token();
        assert_eq!(
            Cursor::from_token(&format!("{token}=")),
            Err(CursorTokenError::Encoding)
        );
        let long = "A".repeat(MAX_CURSOR_TOKEN_LEN + 1);
        assert_eq!(
            Cursor::from_token(&long),
            Err(CursorTokenError::TooLong {
                len: MAX_CURSOR_TOKEN_LEN + 1,
                max: MAX_CURSOR_TOKEN_LEN
            })
        );
        // At the bound it is read (and is no token of version 1: its first byte is 0).
        let at = "A".repeat(MAX_CURSOR_TOKEN_LEN);
        assert_eq!(
            Cursor::from_token(&at),
            Err(CursorTokenError::Version { version: 0 })
        );
        // Four bytes or fewer have no room for a checksum.
        assert_eq!(decode(&[1, 0, 0, 0]), Err(CursorTokenError::Checksum));
        assert_eq!(decode(&[1]), Err(CursorTokenError::Checksum));
    }
}
