use crate::execute::Outputs;
use crate::graph::types::Value;

/// Simple `{key}` template interpolation from inputs.
///
/// Replaces `{key}` placeholders with the stringified value from inputs.
/// Scalar types (String, I64, F32, Bool) are interpolated; complex types
/// (Vec, Map, Domain, Null) are skipped. Missing keys are left as-is.
pub(crate) fn interpolate(template: &str, inputs: &Outputs) -> String {
    let mut result = template.to_string();
    for (key, value) in inputs {
        let placeholder = format!("{{{key}}}");
        let replacement = match value {
            Value::String(s) => s.clone(),
            Value::I64(n) => n.to_string(),
            Value::F32(f) => f.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => continue,
        };
        result = result.replace(&placeholder, &replacement);
    }
    result
}

/// Convert a graph `Value` to a `serde_json::Value`.
pub(crate) fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::I64(n) => serde_json::json!(*n),
        Value::F32(f) => serde_json::json!(*f),
        Value::Vec(items) => serde_json::Value::Array(items.iter().map(value_to_json).collect()),
        Value::Map(map) => {
            let obj: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect();
            serde_json::Value::Object(obj)
        }
        Value::Domain { data, .. } => data.clone(),
        Value::Null => serde_json::Value::Null,
    }
}

/// Validate that a resolved path stays within a base directory.
///
/// Returns the canonicalized path if it is contained within `base_dir`,
/// or an error message if it escapes.
pub(crate) fn validate_path_containment(
    resolved_path: &str,
    base_dir: &str,
) -> Result<std::path::PathBuf, String> {
    let base = std::path::Path::new(base_dir);
    let target = if std::path::Path::new(resolved_path).is_absolute() {
        std::path::PathBuf::from(resolved_path)
    } else {
        base.join(resolved_path)
    };

    // Normalize by resolving `.` and `..` without requiring the path to exist.
    // We use a manual normalization since canonicalize() requires the path to exist.
    let normalized = normalize_path(&target);
    let base_normalized = normalize_path(base);

    if normalized.starts_with(&base_normalized) {
        Ok(normalized)
    } else {
        Err(format!(
            "path '{}' escapes base directory '{}'",
            resolved_path, base_dir
        ))
    }
}

/// Normalize a path by resolving `.` and `..` components without filesystem access.
fn normalize_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                // Only pop if we have a normal component to pop
                if matches!(components.last(), Some(std::path::Component::Normal(_))) {
                    components.pop();
                } else {
                    components.push(component);
                }
            }
            std::path::Component::CurDir => {} // skip
            _ => components.push(component),
        }
    }
    components.iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolate_simple() {
        let mut inputs = Outputs::new();
        inputs.insert("id".into(), Value::I64(42));
        inputs.insert("name".into(), Value::String("test".into()));

        let result = interpolate("https://api.example.com/{name}/{id}", &inputs);
        assert_eq!(result, "https://api.example.com/test/42");
    }

    #[test]
    fn interpolate_no_placeholders() {
        let result = interpolate("https://example.com", &Outputs::new());
        assert_eq!(result, "https://example.com");
    }

    #[test]
    fn interpolate_missing_key_left_as_is() {
        let result = interpolate("https://example.com/{missing}", &Outputs::new());
        assert_eq!(result, "https://example.com/{missing}");
    }

    #[test]
    fn value_to_json_scalars() {
        assert_eq!(value_to_json(&Value::String("hi".into())), serde_json::json!("hi"));
        assert_eq!(value_to_json(&Value::I64(42)), serde_json::json!(42));
        assert_eq!(value_to_json(&Value::Bool(true)), serde_json::json!(true));
        assert_eq!(value_to_json(&Value::Null), serde_json::Value::Null);
    }

    #[test]
    fn value_to_json_collections() {
        let vec_val = Value::Vec(vec![Value::I64(1), Value::I64(2)]);
        assert_eq!(value_to_json(&vec_val), serde_json::json!([1, 2]));

        let mut map = std::collections::BTreeMap::new();
        map.insert("a".into(), Value::Bool(true));
        let map_val = Value::Map(map);
        assert_eq!(value_to_json(&map_val), serde_json::json!({"a": true}));
    }

    #[test]
    fn path_containment_valid() {
        let result = validate_path_containment("subdir/file.txt", "/base");
        assert!(result.is_ok());
        assert!(result.unwrap().starts_with("/base"));
    }

    #[test]
    fn path_containment_rejects_traversal() {
        let result = validate_path_containment("../etc/passwd", "/base");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("escapes base directory"));
    }

    #[test]
    fn path_containment_rejects_absolute_escape() {
        let result = validate_path_containment("/etc/passwd", "/base");
        assert!(result.is_err());
    }

    #[test]
    fn path_containment_allows_absolute_within_base() {
        let result = validate_path_containment("/base/subdir/file.txt", "/base");
        assert!(result.is_ok());
    }

    #[test]
    fn path_containment_normalizes_dots() {
        let result = validate_path_containment("subdir/../subdir/file.txt", "/base");
        assert!(result.is_ok());
    }
}
