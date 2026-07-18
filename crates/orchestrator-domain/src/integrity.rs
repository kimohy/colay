use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum IntegrityError {
    #[error("cannot serialize integrity-protected value: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Produces deterministic canonical JSON. Object keys use RFC 8785 UTF-16 ordering;
/// `serde_json`'s finite-number rendering supplies the shortest round-trippable form.
///
/// # Errors
///
/// Returns [`IntegrityError`] when `value` cannot be represented as JSON.
pub fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, IntegrityError> {
    let value = serde_json::to_value(value)?;
    let mut output = Vec::new();
    write_value(&value, &mut output)?;
    Ok(output)
}

/// Hashes the canonical JSON representation of `value` with SHA-256.
///
/// # Errors
///
/// Returns [`IntegrityError`] when `value` cannot be represented as JSON.
pub fn canonical_sha256<T: Serialize>(value: &T) -> Result<String, IntegrityError> {
    let canonical = canonical_json(value)?;
    Ok(hex::encode(Sha256::digest(canonical)))
}

fn write_value(value: &Value, output: &mut Vec<u8>) -> Result<(), serde_json::Error> {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(value) => output.extend_from_slice(if *value { b"true" } else { b"false" }),
        Value::Number(value) => output.extend_from_slice(value.to_string().as_bytes()),
        Value::String(value) => output.extend_from_slice(serde_json::to_string(value)?.as_bytes()),
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_value(value, output)?;
            }
            output.push(b']');
        }
        Value::Object(values) => {
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_by(|(left, _), (right, _)| left.encode_utf16().cmp(right.encode_utf16()));
            output.push(b'{');
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                output.extend_from_slice(serde_json::to_string(key)?.as_bytes());
                output.push(b':');
                write_value(value, output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::canonical_json;

    #[test]
    fn canonicalization_orders_nested_keys() -> Result<(), Box<dyn std::error::Error>> {
        let canonical = canonical_json(&json!({"z": {"b": 1, "a": 2}, "a": true}))?;
        assert_eq!(
            String::from_utf8(canonical)?,
            r#"{"a":true,"z":{"a":2,"b":1}}"#
        );
        Ok(())
    }
}
