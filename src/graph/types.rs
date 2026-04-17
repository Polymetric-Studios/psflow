use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

/// Type descriptor for a port. Used for load-time type checking of connections.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PortType {
    String,
    Bool,
    I64,
    F32,
    Vec(Box<PortType>),
    Map(Box<PortType>),
    /// A domain-specific type identified by name, e.g. "DungeonLayout".
    Domain(std::string::String),
    /// Wildcard type that matches any other type.
    Any,
}

impl PortType {
    /// Check if a value of this type (source) can flow to a port of the target type.
    ///
    /// Compatible means: exact match, Any wildcard, or coercion (i64 -> f32).
    pub fn is_compatible_with(&self, target: &PortType) -> bool {
        match (self, target) {
            (PortType::Any, _) | (_, PortType::Any) => true,
            (a, b) if a == b => true,
            (PortType::I64, PortType::F32) => true,
            (PortType::Vec(a), PortType::Vec(b)) => a.is_compatible_with(b),
            (PortType::Map(a), PortType::Map(b)) => a.is_compatible_with(b),
            _ => false,
        }
    }
}

impl fmt::Display for PortType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PortType::String => write!(f, "string"),
            PortType::Bool => write!(f, "bool"),
            PortType::I64 => write!(f, "i64"),
            PortType::F32 => write!(f, "f32"),
            PortType::Vec(inner) => write!(f, "{inner}[]"),
            PortType::Map(inner) => write!(f, "Map<{inner}>"),
            PortType::Domain(name) => write!(f, "{name}"),
            PortType::Any => write!(f, "any"),
        }
    }
}

impl FromStr for PortType {
    type Err = std::string::String;

    /// Parse a type string: `"string"`, `"Room[]"`, `"Map<Room>"`, etc.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty type string".into());
        }

        if let Some(inner) = s.strip_suffix("[]") {
            return Ok(PortType::Vec(Box::new(inner.parse()?)));
        }

        if let Some(rest) = s.strip_prefix("Map<") {
            let inner = rest
                .strip_suffix('>')
                .ok_or_else(|| format!("malformed Map type: {s}"))?;
            return Ok(PortType::Map(Box::new(inner.parse()?)));
        }

        match s {
            "string" => Ok(PortType::String),
            "bool" => Ok(PortType::Bool),
            "i64" => Ok(PortType::I64),
            "f32" => Ok(PortType::F32),
            "any" => Ok(PortType::Any),
            _ => Ok(PortType::Domain(s.to_string())),
        }
    }
}

/// Runtime value that flows through ports on edges.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum Value {
    String(std::string::String),
    Bool(bool),
    I64(i64),
    F32(f32),
    Vec(Vec<Value>),
    Map(BTreeMap<std::string::String, Value>),
    Domain {
        type_name: std::string::String,
        data: serde_json::Value,
    },
    Null,
}

impl Value {
    /// Returns the `PortType` that corresponds to this runtime value.
    pub fn port_type(&self) -> PortType {
        match self {
            Value::String(_) => PortType::String,
            Value::Bool(_) => PortType::Bool,
            Value::I64(_) => PortType::I64,
            Value::F32(_) => PortType::F32,
            Value::Vec(_) => PortType::Vec(Box::new(PortType::Any)),
            Value::Map(_) => PortType::Map(Box::new(PortType::Any)),
            Value::Domain { type_name, .. } => PortType::Domain(type_name.clone()),
            Value::Null => PortType::Any,
        }
    }

    /// Check if this runtime value is assignable to a port of the given type.
    ///
    /// Follows the same rules as `PortType::is_compatible_with`: exact match,
    /// `Any` wildcard, `i64`-to-`f32` coercion, and recursive Vec/Map checks.
    /// `Null` is compatible with any type (analogous to `Option::None`).
    pub fn matches_type(&self, port_type: &PortType) -> bool {
        match (self, port_type) {
            (_, PortType::Any) => true,
            (Value::Null, _) => true,
            (Value::String(_), PortType::String) => true,
            (Value::Bool(_), PortType::Bool) => true,
            (Value::I64(_), PortType::I64) => true,
            (Value::I64(_), PortType::F32) => true, // coercion
            (Value::F32(_), PortType::F32) => true,
            (Value::Vec(items), PortType::Vec(inner)) => {
                items.iter().all(|v| v.matches_type(inner))
            }
            (Value::Map(entries), PortType::Map(inner)) => {
                entries.values().all(|v| v.matches_type(inner))
            }
            (Value::Domain { type_name, .. }, PortType::Domain(expected)) => type_name == expected,
            _ => false,
        }
    }
}

impl From<&Value> for serde_json::Value {
    fn from(v: &Value) -> Self {
        match v {
            Value::String(s) => serde_json::Value::String(s.clone()),
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::I64(n) => serde_json::json!(*n),
            Value::F32(f) => serde_json::json!(*f),
            Value::Vec(items) => {
                serde_json::Value::Array(items.iter().map(serde_json::Value::from).collect())
            }
            Value::Map(map) => {
                let obj: serde_json::Map<String, serde_json::Value> = map
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::from(v)))
                    .collect();
                serde_json::Value::Object(obj)
            }
            Value::Domain { data, .. } => data.clone(),
            Value::Null => serde_json::Value::Null,
        }
    }
}

impl From<Value> for serde_json::Value {
    fn from(v: Value) -> Self {
        serde_json::Value::from(&v)
    }
}

impl From<serde_json::Value> for Value {
    fn from(v: serde_json::Value) -> Self {
        match v {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::I64(i)
                } else if let Some(f) = n.as_f64() {
                    Value::F32(f as f32)
                } else {
                    Value::Null
                }
            }
            serde_json::Value::String(s) => Value::String(s),
            serde_json::Value::Array(arr) => Value::Vec(arr.into_iter().map(Value::from).collect()),
            serde_json::Value::Object(map) => {
                let m: BTreeMap<std::string::String, Value> =
                    map.into_iter().map(|(k, v)| (k, Value::from(v))).collect();
                Value::Map(m)
            }
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::I64(a), Value::I64(b)) => a == b,
            (Value::F32(a), Value::F32(b)) => a == b,
            (Value::Vec(a), Value::Vec(b)) => a == b,
            (Value::Map(a), Value::Map(b)) => a == b,
            (
                Value::Domain {
                    type_name: a_name,
                    data: a_data,
                },
                Value::Domain {
                    type_name: b_name,
                    data: b_data,
                },
            ) => a_name == b_name && a_data == b_data,
            (Value::Null, Value::Null) => true,
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// ResultReducer — aggregation strategy for per-step results.
// ---------------------------------------------------------------------------

/// How successive results for the same step should be combined on the blackboard.
///
/// Each variant defines a pure function `(existing, new) -> merged`. `apply` is
/// the canonical evaluator. The reducer is declarative metadata — it carries no
/// runtime dependencies and lives next to [`Value`] for that reason.
///
/// - `Replace` — new value overwrites any existing value.
/// - `Append` — values accumulate into a JSON array. Non-array existing values
///   are wrapped before appending.
/// - `Merge` — object-level shallow merge (last-writer-wins per key). Falls
///   back to `Replace` for non-object pairs.
/// - `Concat` — string concatenation with `\n` separator, or array concat for
///   arrays. Falls back to `Replace` for mixed/other types.
/// - `Promote` — behaves like `Replace` for the stored value, but signals to
///   embedders that the value should ALSO be written under a name-addressable
///   key so downstream consumers can fetch by name. The dual-write itself is
///   handled by the embedder (e.g. `psflow::blackboard::helpers::set_result`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ResultReducer {
    Replace,
    Append,
    Merge,
    Concat,
    Promote,
}

impl ResultReducer {
    /// Apply the reducer to produce a merged value.
    ///
    /// `existing` is the value previously stored for this key (if any); `new`
    /// is the incoming value. Returns the merged value to be stored.
    pub fn apply(
        &self,
        existing: Option<&serde_json::Value>,
        new: serde_json::Value,
    ) -> serde_json::Value {
        match self {
            ResultReducer::Replace => new,

            ResultReducer::Append => match existing {
                Some(serde_json::Value::Array(arr)) => {
                    let mut out = arr.clone();
                    out.push(new);
                    serde_json::Value::Array(out)
                }
                Some(other) => serde_json::Value::Array(vec![other.clone(), new]),
                None => serde_json::Value::Array(vec![new]),
            },

            ResultReducer::Merge => match (existing, &new) {
                (Some(serde_json::Value::Object(base)), serde_json::Value::Object(incoming)) => {
                    let mut out = base.clone();
                    for (k, v) in incoming {
                        out.insert(k.clone(), v.clone());
                    }
                    serde_json::Value::Object(out)
                }
                _ => new,
            },

            ResultReducer::Concat => match (existing, &new) {
                (Some(serde_json::Value::String(a)), serde_json::Value::String(b)) => {
                    serde_json::Value::String(format!("{a}\n{b}"))
                }
                (Some(serde_json::Value::Array(a)), serde_json::Value::Array(b)) => {
                    let mut out = a.clone();
                    out.extend(b.iter().cloned());
                    serde_json::Value::Array(out)
                }
                _ => new,
            },

            // Promote behaves identically to Replace for the stored value.
            // Dual-write to a named key is handled by the embedder.
            ResultReducer::Promote => new,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_builtin_types() {
        assert_eq!("string".parse::<PortType>().unwrap(), PortType::String);
        assert_eq!("bool".parse::<PortType>().unwrap(), PortType::Bool);
        assert_eq!("i64".parse::<PortType>().unwrap(), PortType::I64);
        assert_eq!("f32".parse::<PortType>().unwrap(), PortType::F32);
        assert_eq!("any".parse::<PortType>().unwrap(), PortType::Any);
    }

    #[test]
    fn parse_domain_type() {
        assert_eq!(
            "DungeonLayout".parse::<PortType>().unwrap(),
            PortType::Domain("DungeonLayout".into())
        );
    }

    #[test]
    fn parse_vec_type() {
        assert_eq!(
            "Room[]".parse::<PortType>().unwrap(),
            PortType::Vec(Box::new(PortType::Domain("Room".into())))
        );
    }

    #[test]
    fn parse_nested_vec_type() {
        assert_eq!(
            "Room[][]".parse::<PortType>().unwrap(),
            PortType::Vec(Box::new(PortType::Vec(Box::new(PortType::Domain(
                "Room".into()
            )))))
        );
    }

    #[test]
    fn parse_map_type() {
        assert_eq!(
            "Map<Room>".parse::<PortType>().unwrap(),
            PortType::Map(Box::new(PortType::Domain("Room".into())))
        );
    }

    #[test]
    fn parse_empty_is_error() {
        assert!("".parse::<PortType>().is_err());
        assert!("  ".parse::<PortType>().is_err());
    }

    #[test]
    fn display_round_trips() {
        let types = [
            PortType::String,
            PortType::Bool,
            PortType::I64,
            PortType::F32,
            PortType::Any,
            PortType::Domain("Room".into()),
            PortType::Vec(Box::new(PortType::Domain("Room".into()))),
            PortType::Map(Box::new(PortType::F32)),
        ];
        for ty in &types {
            let s = ty.to_string();
            let parsed: PortType = s.parse().unwrap();
            assert_eq!(&parsed, ty, "round-trip failed for {s}");
        }
    }

    #[test]
    fn compatibility_exact_match() {
        assert!(PortType::String.is_compatible_with(&PortType::String));
        assert!(PortType::I64.is_compatible_with(&PortType::I64));
        let domain = PortType::Domain("Room".into());
        assert!(domain.is_compatible_with(&domain));
    }

    #[test]
    fn compatibility_any_wildcard() {
        assert!(PortType::Any.is_compatible_with(&PortType::String));
        assert!(PortType::I64.is_compatible_with(&PortType::Any));
    }

    #[test]
    fn compatibility_coercion() {
        assert!(PortType::I64.is_compatible_with(&PortType::F32));
        assert!(!PortType::F32.is_compatible_with(&PortType::I64));
    }

    #[test]
    fn compatibility_mismatch() {
        assert!(!PortType::String.is_compatible_with(&PortType::Bool));
        assert!(!PortType::I64.is_compatible_with(&PortType::String));
        assert!(!PortType::Domain("A".into()).is_compatible_with(&PortType::Domain("B".into())));
    }

    #[test]
    fn compatibility_recursive_vec() {
        let a = PortType::Vec(Box::new(PortType::I64));
        let b = PortType::Vec(Box::new(PortType::F32));
        let c = PortType::Vec(Box::new(PortType::String));
        assert!(a.is_compatible_with(&b)); // Vec<i64> -> Vec<f32> via coercion
        assert!(!a.is_compatible_with(&c));
    }

    #[test]
    fn value_port_type() {
        assert_eq!(Value::String("hi".into()).port_type(), PortType::String);
        assert_eq!(Value::Bool(true).port_type(), PortType::Bool);
        assert_eq!(Value::I64(42).port_type(), PortType::I64);
        assert_eq!(Value::F32(3.14).port_type(), PortType::F32);
        assert_eq!(Value::Null.port_type(), PortType::Any);
        assert_eq!(
            Value::Domain {
                type_name: "Room".into(),
                data: serde_json::json!({})
            }
            .port_type(),
            PortType::Domain("Room".into())
        );
    }

    #[test]
    fn matches_type_exact() {
        assert!(Value::String("hi".into()).matches_type(&PortType::String));
        assert!(Value::Bool(true).matches_type(&PortType::Bool));
        assert!(Value::I64(42).matches_type(&PortType::I64));
        assert!(Value::F32(3.14).matches_type(&PortType::F32));
    }

    #[test]
    fn matches_type_any_wildcard() {
        assert!(Value::String("x".into()).matches_type(&PortType::Any));
        assert!(Value::I64(1).matches_type(&PortType::Any));
    }

    #[test]
    fn matches_type_null_matches_anything() {
        assert!(Value::Null.matches_type(&PortType::String));
        assert!(Value::Null.matches_type(&PortType::I64));
        assert!(Value::Null.matches_type(&PortType::Domain("Room".into())));
    }

    #[test]
    fn matches_type_i64_to_f32_coercion() {
        assert!(Value::I64(10).matches_type(&PortType::F32));
        assert!(!Value::F32(1.0).matches_type(&PortType::I64));
    }

    #[test]
    fn matches_type_vec_recursive() {
        let v = Value::Vec(vec![Value::I64(1), Value::I64(2)]);
        assert!(v.matches_type(&PortType::Vec(Box::new(PortType::I64))));
        assert!(v.matches_type(&PortType::Vec(Box::new(PortType::F32)))); // coercion
        assert!(!v.matches_type(&PortType::Vec(Box::new(PortType::String))));
    }

    #[test]
    fn matches_type_empty_vec() {
        let v = Value::Vec(vec![]);
        assert!(v.matches_type(&PortType::Vec(Box::new(PortType::String))));
    }

    #[test]
    fn matches_type_map_recursive() {
        let mut m = BTreeMap::new();
        m.insert("a".into(), Value::String("x".into()));
        let v = Value::Map(m);
        assert!(v.matches_type(&PortType::Map(Box::new(PortType::String))));
        assert!(!v.matches_type(&PortType::Map(Box::new(PortType::I64))));
    }

    #[test]
    fn matches_type_domain() {
        let v = Value::Domain {
            type_name: "Room".into(),
            data: serde_json::json!({}),
        };
        assert!(v.matches_type(&PortType::Domain("Room".into())));
        assert!(!v.matches_type(&PortType::Domain("Tile".into())));
    }

    #[test]
    fn matches_type_mismatch() {
        assert!(!Value::String("x".into()).matches_type(&PortType::Bool));
        assert!(!Value::Bool(true).matches_type(&PortType::I64));
        assert!(!Value::I64(1).matches_type(&PortType::String));
    }

    #[test]
    fn value_serde_round_trip() {
        let values = vec![
            Value::String("hello".into()),
            Value::Bool(true),
            Value::I64(42),
            Value::F32(3.14),
            Value::Null,
            Value::Vec(vec![Value::I64(1), Value::I64(2)]),
        ];
        for val in &values {
            let json = serde_json::to_string(val).unwrap();
            let parsed: Value = serde_json::from_str(&json).unwrap();
            assert_eq!(&parsed, val);
        }
    }

    // -- ResultReducer tests --

    #[test]
    fn reducer_replace() {
        let existing = serde_json::json!("old");
        let new = serde_json::json!("new");
        assert_eq!(
            ResultReducer::Replace.apply(Some(&existing), new.clone()),
            new
        );
        assert_eq!(ResultReducer::Replace.apply(None, new.clone()), new);
    }

    #[test]
    fn reducer_append_array_extends() {
        let existing = serde_json::json!(["a", "b"]);
        let result = ResultReducer::Append.apply(Some(&existing), serde_json::json!("c"));
        assert_eq!(result, serde_json::json!(["a", "b", "c"]));
    }

    #[test]
    fn reducer_append_scalar_wraps() {
        let existing = serde_json::json!("scalar");
        let result = ResultReducer::Append.apply(Some(&existing), serde_json::json!("new"));
        assert_eq!(result, serde_json::json!(["scalar", "new"]));
    }

    #[test]
    fn reducer_append_none_creates_singleton() {
        let result = ResultReducer::Append.apply(None, serde_json::json!("only"));
        assert_eq!(result, serde_json::json!(["only"]));
    }

    #[test]
    fn reducer_merge_objects() {
        let base = serde_json::json!({"a": 1, "b": 2});
        let incoming = serde_json::json!({"b": 99, "c": 3});
        let result = ResultReducer::Merge.apply(Some(&base), incoming);
        assert_eq!(result, serde_json::json!({"a": 1, "b": 99, "c": 3}));
    }

    #[test]
    fn reducer_merge_non_objects_falls_back_to_replace() {
        let result = ResultReducer::Merge.apply(None, serde_json::json!("fallback"));
        assert_eq!(result, serde_json::json!("fallback"));
    }

    #[test]
    fn reducer_concat_strings() {
        let a = serde_json::json!("hello");
        let b = serde_json::json!("world");
        let result = ResultReducer::Concat.apply(Some(&a), b);
        assert_eq!(result, serde_json::json!("hello\nworld"));
    }

    #[test]
    fn reducer_concat_arrays() {
        let a = serde_json::json!([1, 2]);
        let b = serde_json::json!([3, 4]);
        let result = ResultReducer::Concat.apply(Some(&a), b);
        assert_eq!(result, serde_json::json!([1, 2, 3, 4]));
    }

    #[test]
    fn reducer_promote_behaves_like_replace() {
        let existing = serde_json::json!("old");
        let new = serde_json::json!("new");
        let result = ResultReducer::Promote.apply(Some(&existing), new.clone());
        assert_eq!(result, new);
    }

    #[test]
    fn reducer_serde_round_trip() {
        for r in &[
            ResultReducer::Replace,
            ResultReducer::Append,
            ResultReducer::Merge,
            ResultReducer::Concat,
            ResultReducer::Promote,
        ] {
            let json = serde_json::to_string(r).unwrap();
            let parsed: ResultReducer = serde_json::from_str(&json).unwrap();
            assert_eq!(&parsed, r);
        }
    }
}
