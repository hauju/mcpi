//! Deterministic JSON serialization, used to content-address snapshots.
//!
//! `serde_json`'s object map is a `BTreeMap` by default — but the
//! `preserve_order` feature swaps it for an `IndexMap`, at which point key
//! order follows insertion order. Cargo unifies features across the graph, so
//! any dependency turning that on would silently change every digest and make
//! the app report a schema change on a server that did not move. Sorting keys
//! explicitly makes the digest depend on content and nothing else.

use serde_json::Value;

/// Serialize `value` with object keys sorted at every level.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_value(value, &mut out);
    out
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                // Defer to serde for string escaping rather than hand-rolling it.
                out.push_str(&Value::String((*key).clone()).to_string());
                out.push(':');
                write_value(&map[*key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        // Scalars have exactly one JSON rendering, which `Display` produces.
        scalar => out.push_str(&scalar.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn key_order_does_not_affect_output() {
        let a: Value = serde_json::from_str(r#"{"b":1,"a":{"d":2,"c":3}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a":{"c":3,"d":2},"b":1}"#).unwrap();
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert_eq!(canonical_json(&a), r#"{"a":{"c":3,"d":2},"b":1}"#);
    }

    #[test]
    fn array_order_is_preserved() {
        // Arrays are ordered in JSON, so reordering one is a real difference.
        assert_ne!(
            canonical_json(&json!([1, 2])),
            canonical_json(&json!([2, 1]))
        );
    }

    #[test]
    fn strings_stay_escaped() {
        let v = json!({ "quote\"key": "tab\there" });
        assert_eq!(canonical_json(&v), r#"{"quote\"key":"tab\there"}"#);
    }
}
