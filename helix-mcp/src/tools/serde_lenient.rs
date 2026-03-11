//! Lenient deserializers for MCP tool parameters.
//!
//! LLMs sometimes send numeric fields as strings (`"42"` instead of `42`) or
//! arrays as stringified JSON. These helpers accept both forms.

use serde::de::{self, Deserialize, DeserializeOwned, Deserializer};

/// Deserialize a `usize` that may arrive as a JSON number or a string like `"42"`.
pub fn string_or_usize<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    match &v {
        serde_json::Value::Number(n) => n
            .as_u64()
            .map(|n| n as usize)
            .ok_or_else(|| de::Error::custom(format!("expected non-negative integer, got {v}"))),
        serde_json::Value::String(s) => s.parse::<usize>().map_err(de::Error::custom),
        _ => Err(de::Error::custom(format!(
            "expected number or string, got {v}"
        ))),
    }
}

/// Same but for `Option<usize>`.
pub fn string_or_usize_opt<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    match &v {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(n) => n
            .as_u64()
            .map(|n| Some(n as usize))
            .ok_or_else(|| de::Error::custom(format!("expected non-negative integer, got {v}"))),
        serde_json::Value::String(s) if s.is_empty() => Ok(None),
        serde_json::Value::String(s) => s.parse::<usize>().map(Some).map_err(de::Error::custom),
        _ => Err(de::Error::custom(format!(
            "expected number or string, got {v}"
        ))),
    }
}

/// Same for `Option<u8>`.
pub fn string_or_u8_opt<'de, D>(deserializer: D) -> Result<Option<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    match &v {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(n) => n
            .as_u64()
            .and_then(|n| u8::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| de::Error::custom(format!("expected u8, got {v}"))),
        serde_json::Value::String(s) if s.is_empty() => Ok(None),
        serde_json::Value::String(s) => s.parse::<u8>().map(Some).map_err(de::Error::custom),
        _ => Err(de::Error::custom(format!(
            "expected number or string, got {v}"
        ))),
    }
}

/// Deserialize `Vec<T>` that may arrive as a JSON array or a stringified array.
pub fn string_or_vec<'de, T, D>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    T: DeserializeOwned,
    D: Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    match v {
        serde_json::Value::Array(_) => serde_json::from_value(v).map_err(de::Error::custom),
        serde_json::Value::String(s) => serde_json::from_str(&s).map_err(de::Error::custom),
        other => Err(de::Error::custom(format!(
            "expected array or string, got {other}"
        ))),
    }
}
