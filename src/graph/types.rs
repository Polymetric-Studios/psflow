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
}
