//! Flexible deserializers — the server sometimes encodes numeric fields as JSON
//! numbers and sometimes as strings. These accept either.

use serde::{Deserialize, Deserializer};

#[derive(Deserialize)]
#[serde(untagged)]
enum NumOrStr {
    Num(i64),
    Str(String),
}

fn parse(v: NumOrStr) -> std::result::Result<i64, std::num::ParseIntError> {
    match v {
        NumOrStr::Num(n) => Ok(n),
        NumOrStr::Str(s) => {
            if s.is_empty() {
                Ok(0)
            } else {
                s.parse::<i64>()
            }
        }
    }
}

/// Deserialize an `i64` from a JSON number or numeric string.
pub fn i64_flexible<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<i64, D::Error> {
    let v = NumOrStr::deserialize(d)?;
    parse(v).map_err(serde::de::Error::custom)
}

/// Deserialize an `Option<i64>`; missing/null/empty → `None`.
pub fn opt_i64_flexible<'de, D: Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<i64>, D::Error> {
    let v = Option::<NumOrStr>::deserialize(d)?;
    match v {
        None => Ok(None),
        Some(NumOrStr::Str(s)) if s.is_empty() => Ok(None),
        Some(other) => parse(other).map(Some).map_err(serde::de::Error::custom),
    }
}

/// Deserialize a `bool` from the server's `"1"`/`"0"` (string or number) flags.
pub fn int_bool<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<bool, D::Error> {
    let v = NumOrStr::deserialize(d)?;
    Ok(parse(v).map_err(serde::de::Error::custom)? != 0)
}

/// Deserialize a `String`, mapping JSON `null` (and absent, via `default`) to "".
/// The server sends `null` for empty album fields like `cover`/`members`.
pub fn nullable_string<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}
