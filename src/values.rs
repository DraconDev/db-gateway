use rusqlite::types::{Value as SqlValue, ValueRef};
use serde_json::{Number, Value as JsonValue};

pub fn json_to_sql(value: &JsonValue) -> Result<SqlValue, &'static str> {
    match value {
        JsonValue::Null => Ok(SqlValue::Null),
        JsonValue::Bool(b) => Ok(SqlValue::Integer(i64::from(*b))),
        JsonValue::Number(n) => number_to_sql(n),
        JsonValue::String(s) => Ok(SqlValue::Text(s.clone())),
        JsonValue::Array(values) => Ok(SqlValue::Blob(
            values
                .iter()
                .map(|v| match v.as_u64() {
                    Some(byte) if byte <= u64::from(u8::MAX) => Ok(byte as u8),
                    _ => Err("blob args must be an array of integers 0..=255"),
                })
                .collect::<std::result::Result<Vec<u8>, _>>()?,
        )),
        JsonValue::Object(_) => Err("object args are not supported"),
    }
}

fn number_to_sql(number: &Number) -> Result<SqlValue, &'static str> {
    if let Some(i) = number.as_i64() {
        Ok(SqlValue::Integer(i))
    } else if number.as_u64().is_some() {
        Err("integer out of signed 64-bit range")
    } else if let Some(f) = number.as_f64() {
        Ok(SqlValue::Real(f))
    } else {
        Err("number out of range")
    }
}

pub fn sql_to_json(value: ValueRef<'_>) -> JsonValue {
    match value {
        ValueRef::Null => JsonValue::Null,
        ValueRef::Integer(i) => JsonValue::from(i),
        ValueRef::Real(f) => Number::from_f64(f)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        ValueRef::Text(t) => JsonValue::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => {
            JsonValue::Array(b.iter().map(|&byte| JsonValue::from(byte)).collect())
        }
    }
}
