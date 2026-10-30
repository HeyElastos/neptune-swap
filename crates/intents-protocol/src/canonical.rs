use serde::Serialize;
use serde_json::Value;

use crate::Error;

/// JSON with object keys sorted at every level and no whitespace: the bytes a
/// signature covers. Sorting is done here rather than relying on serde_json's
/// map type, which another crate in the build can switch to insertion order.
pub fn to_canonical_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    let value = serde_json::to_value(value).map_err(|e| Error::Malformed(e.to_string()))?;
    let mut out = String::new();
    write(&value, &mut out);
    Ok(out.into_bytes())
}

fn write(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write(item, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}
