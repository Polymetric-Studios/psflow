//! Bidirectional conversion between psflow `Value` and Rhai `Dynamic`.

use crate::graph::types::Value;
use rhai::Dynamic;
use std::collections::BTreeMap;

/// Convert a psflow `Value` into a Rhai `Dynamic`.
///
/// Mapping:
/// - String → String
/// - Bool → bool
/// - I64 → INT (i64)
/// - F32 → FLOAT (f64, widened)
/// - Vec → Array
/// - Map → Map (rhai::Map is BTreeMap<SmartString, Dynamic>)
/// - Domain → via `rhai::serde::to_dynamic`
/// - Null → UNIT `()`
pub fn value_to_dynamic(v: &Value) -> Dynamic {
    match v {
        Value::String(s) => Dynamic::from(s.clone()),
        Value::Bool(b) => Dynamic::from(*b),
        Value::I64(n) => Dynamic::from(*n),
        Value::F32(f) => Dynamic::from(*f as f64),
        Value::Vec(items) => {
            let arr: rhai::Array = items.iter().map(value_to_dynamic).collect();
            Dynamic::from_array(arr)
        }
        Value::Map(map) => {
            let rhai_map: rhai::Map = map
                .iter()
                .map(|(k, v)| (k.clone().into(), value_to_dynamic(v)))
                .collect();
            Dynamic::from_map(rhai_map)
        }
        Value::Domain { data, .. } => rhai::serde::to_dynamic(data).unwrap_or(Dynamic::UNIT),
        Value::Null => Dynamic::UNIT,
    }
}

/// Convert a Rhai `Dynamic` back into a psflow `Value`.
///
/// Mapping:
/// - String → String
/// - bool → Bool
/// - INT (i64) → I64
/// - FLOAT (f64) → F32 (truncated)
/// - Array → Vec
/// - Map → Map
/// - UNIT → Null
/// - Other → attempts serde round-trip to Domain, falls back to Null
pub fn dynamic_to_value(d: Dynamic) -> Value {
    if d.is_unit() {
        return Value::Null;
    }
    if d.is_bool() {
        return Value::Bool(d.as_bool().unwrap());
    }
    if d.is_int() {
        return Value::I64(d.as_int().unwrap());
    }
    if d.is_float() {
        return Value::F32(d.as_float().unwrap() as f32);
    }
    if d.is_string() {
        return Value::String(d.into_string().unwrap());
    }
    if d.is_array() {
        let arr = d.into_array().unwrap();
        return Value::Vec(arr.into_iter().map(dynamic_to_value).collect());
    }
    if d.is_map() {
        let map: rhai::Map = d.cast();
        let m: BTreeMap<String, Value> = map
            .into_iter()
            .map(|(k, v)| (k.to_string(), dynamic_to_value(v)))
            .collect();
        return Value::Map(m);
    }

    // Fallback: try serde round-trip for custom types
    match rhai::serde::from_dynamic::<serde_json::Value>(&d) {
        Ok(json) => Value::from(json),
        Err(_) => Value::Null,
    }
}

/// Convert a psflow `Outputs` map into a Rhai `Map` for use as a script scope variable.
pub fn outputs_to_rhai_map(outputs: &crate::execute::Outputs) -> rhai::Map {
    outputs
        .iter()
        .map(|(k, v)| (k.clone().into(), value_to_dynamic(v)))
        .collect()
}

/// Convert a Rhai `Map` back into psflow `Outputs`.
pub fn rhai_map_to_outputs(map: rhai::Map) -> crate::execute::Outputs {
    map.into_iter()
        .map(|(k, v)| (k.to_string(), dynamic_to_value(v)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_round_trip() {
        let v = Value::String("hello".into());
        let d = value_to_dynamic(&v);
        assert_eq!(d.clone().into_string().unwrap(), "hello");
        assert_eq!(dynamic_to_value(d), v);
    }

    #[test]
    fn bool_round_trip() {
        let v = Value::Bool(true);
        let d = value_to_dynamic(&v);
        assert!(d.as_bool().unwrap());
        assert_eq!(dynamic_to_value(d), v);
    }

    #[test]
    fn i64_round_trip() {
        let v = Value::I64(42);
        let d = value_to_dynamic(&v);
        assert_eq!(d.as_int().unwrap(), 42);
        assert_eq!(dynamic_to_value(d), v);
    }

    #[test]
    fn f32_through_f64_round_trip() {
        let v = Value::F32(2.5);
        let d = value_to_dynamic(&v);
        // Widened to f64
        let f = d.as_float().unwrap();
        assert!((f - 2.5f64).abs() < 0.001);
        // Back to f32 (truncated)
        let back = dynamic_to_value(d);
        match back {
            Value::F32(f) => assert!((f - 2.5).abs() < 0.01),
            other => panic!("expected F32, got {other:?}"),
        }
    }

    #[test]
    fn vec_round_trip() {
        let v = Value::Vec(vec![Value::I64(1), Value::I64(2), Value::I64(3)]);
        let d = value_to_dynamic(&v);
        assert!(d.is_array());
        let back = dynamic_to_value(d);
        assert_eq!(back, v);
    }

    #[test]
    fn map_round_trip() {
        let mut m = BTreeMap::new();
        m.insert("a".into(), Value::Bool(true));
        m.insert("b".into(), Value::I64(99));
        let v = Value::Map(m);
        let d = value_to_dynamic(&v);
        assert!(d.is_map());
        let back = dynamic_to_value(d);
        assert_eq!(back, v);
    }

    #[test]
    fn null_round_trip() {
        let v = Value::Null;
        let d = value_to_dynamic(&v);
        assert!(d.is_unit());
        assert_eq!(dynamic_to_value(d), v);
    }

    #[test]
    fn domain_via_serde() {
        let v = Value::Domain {
            type_name: "Room".into(),
            data: serde_json::json!({"width": 10, "height": 5}),
        };
        let d = value_to_dynamic(&v);
        // Should be a map with the domain data
        assert!(d.is_map());
        let back = dynamic_to_value(d);
        // Comes back as a Map since Dynamic doesn't preserve Domain wrapper
        match back {
            Value::Map(m) => {
                assert_eq!(m.get("width"), Some(&Value::I64(10)));
                assert_eq!(m.get("height"), Some(&Value::I64(5)));
            }
            other => panic!("expected Map, got {other:?}"),
        }
    }

    #[test]
    fn nested_vec_of_maps() {
        let mut m1 = BTreeMap::new();
        m1.insert("x".into(), Value::I64(1));
        let mut m2 = BTreeMap::new();
        m2.insert("x".into(), Value::I64(2));
        let v = Value::Vec(vec![Value::Map(m1.clone()), Value::Map(m2.clone())]);
        let d = value_to_dynamic(&v);
        let back = dynamic_to_value(d);
        assert_eq!(back, v);
    }

    #[test]
    fn outputs_round_trip() {
        let mut outputs = crate::execute::Outputs::new();
        outputs.insert("name".into(), Value::String("test".into()));
        outputs.insert("count".into(), Value::I64(5));

        let rhai_map = outputs_to_rhai_map(&outputs);
        let back = rhai_map_to_outputs(rhai_map);
        assert_eq!(back, outputs);
    }

    // -- 3.T.12: Edge case tests --

    #[test]
    fn f32_nan_round_trip() {
        let v = Value::F32(f32::NAN);
        let d = value_to_dynamic(&v);
        let back = dynamic_to_value(d);
        // NaN doesn't equal itself, so check variant and NaN-ness
        match back {
            Value::F32(f) => assert!(f.is_nan()),
            other => panic!("expected F32, got {other:?}"),
        }
    }

    #[test]
    fn f32_infinity_round_trip() {
        let v = Value::F32(f32::INFINITY);
        let d = value_to_dynamic(&v);
        let back = dynamic_to_value(d);
        match back {
            Value::F32(f) => assert!(f.is_infinite() && f.is_sign_positive()),
            other => panic!("expected F32, got {other:?}"),
        }
    }

    #[test]
    fn f32_neg_infinity_round_trip() {
        let v = Value::F32(f32::NEG_INFINITY);
        let d = value_to_dynamic(&v);
        let back = dynamic_to_value(d);
        match back {
            Value::F32(f) => assert!(f.is_infinite() && f.is_sign_negative()),
            other => panic!("expected F32, got {other:?}"),
        }
    }

    #[test]
    fn f32_zero_round_trip() {
        let v = Value::F32(0.0);
        let d = value_to_dynamic(&v);
        let back = dynamic_to_value(d);
        assert_eq!(back, v);
    }

    #[test]
    fn i64_boundary_round_trip() {
        for val in [i64::MIN, i64::MAX, 0] {
            let v = Value::I64(val);
            let d = value_to_dynamic(&v);
            let back = dynamic_to_value(d);
            assert_eq!(back, v);
        }
    }

    #[test]
    fn empty_string_round_trip() {
        let v = Value::String(String::new());
        let d = value_to_dynamic(&v);
        let back = dynamic_to_value(d);
        assert_eq!(back, v);
    }

    #[test]
    fn empty_vec_round_trip() {
        let v = Value::Vec(vec![]);
        let d = value_to_dynamic(&v);
        let back = dynamic_to_value(d);
        assert_eq!(back, v);
    }

    #[test]
    fn empty_map_round_trip() {
        let v = Value::Map(BTreeMap::new());
        let d = value_to_dynamic(&v);
        let back = dynamic_to_value(d);
        assert_eq!(back, v);
    }

    #[test]
    fn deeply_nested_structure() {
        // 3 levels deep: Vec<Map<Vec<I64>>>
        let inner_vec = Value::Vec(vec![Value::I64(1), Value::I64(2)]);
        let mut map = BTreeMap::new();
        map.insert("data".into(), inner_vec);
        let outer = Value::Vec(vec![Value::Map(map.clone()), Value::Map(map)]);

        let d = value_to_dynamic(&outer);
        let back = dynamic_to_value(d);
        assert_eq!(back, outer);
    }

    #[test]
    fn mixed_type_vec() {
        let v = Value::Vec(vec![
            Value::String("hello".into()),
            Value::I64(42),
            Value::Bool(true),
            Value::Null,
        ]);
        let d = value_to_dynamic(&v);
        let back = dynamic_to_value(d);
        assert_eq!(back, v);
    }

    #[test]
    fn domain_with_nested_arrays() {
        let v = Value::Domain {
            type_name: "Complex".into(),
            data: serde_json::json!({
                "items": [1, 2, 3],
                "nested": {"key": "val"}
            }),
        };
        let d = value_to_dynamic(&v);
        assert!(d.is_map());
        // Verify nested structure survived the round trip
        let back = dynamic_to_value(d);
        match back {
            Value::Map(m) => {
                assert!(m.contains_key("items"));
                assert!(m.contains_key("nested"));
            }
            other => panic!("expected Map, got {other:?}"),
        }
    }
}
