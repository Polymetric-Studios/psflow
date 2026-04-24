use crate::graph::metadata::GraphMetadata;
use crate::graph::node::{Node, NodeId};
use crate::graph::port::Port;
use crate::graph::types::PortType;
use crate::graph::Graph;
use crate::mermaid::parse::ParsedAnnotation;
use crate::mermaid::MermaidError;

/// Parse a raw annotation value string into a serde_json::Value.
///
/// Tries JSON parsing first (strings, numbers, bools, null, arrays).
/// Falls back to treating the raw text as an unquoted string.
pub fn parse_value(raw: &str) -> serde_json::Value {
    let raw = raw.trim();
    if raw.is_empty() {
        return serde_json::Value::Null;
    }
    // serde_json handles: "quoted strings", 42, 3.14, true, false, null, [arrays], {objects}
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(raw) {
        return val;
    }
    // Unquoted string (e.g. handler names like `fetch_data`)
    serde_json::Value::String(raw.to_string())
}

/// Apply all parsed annotations to the graph's nodes and metadata.
pub fn apply_annotations(
    graph: &mut Graph,
    annotations: &[ParsedAnnotation],
) -> Result<(), Vec<MermaidError>> {
    let mut errors: Vec<MermaidError> = Vec::new();

    for ann in annotations {
        if ann.target_id == "graph" {
            apply_graph_annotation(graph.metadata_mut(), ann);
            continue;
        }

        let node_id = NodeId::new(&ann.target_id);
        let Some(node) = graph.node_mut(&node_id) else {
            errors.push(MermaidError::Annotation {
                node_id: ann.target_id.clone(),
                message: "node not found in graph".into(),
            });
            continue;
        };

        let value = parse_value(&ann.raw_value);
        if let Err(msg) = apply_node_annotation(node, &ann.key, value) {
            errors.push(MermaidError::Annotation {
                node_id: ann.target_id.clone(),
                message: msg,
            });
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn apply_graph_annotation(meta: &mut GraphMetadata, ann: &ParsedAnnotation) {
    let value = parse_value(&ann.raw_value);
    let s = value.as_str().map(String::from);
    match ann.key.as_str() {
        "name" => meta.name = s,
        "version" => meta.version = s,
        "description" => meta.description = s,
        "default_executor" => meta.default_executor = s,
        "required_adapter" => meta.required_adapter = s,
        "author" => meta.author = s,
        key if key.starts_with("auth.") => {
            apply_auth_annotation(meta, &key[5..], value);
        }
        key => {
            // Unknown keys go into extras with dot-path expansion
            let mut wrapper = serde_json::Value::Object(std::mem::take(&mut meta.extras));
            set_nested(&mut wrapper, key, value);
            // Safe: set_nested never replaces the root object
            meta.extras = match wrapper {
                serde_json::Value::Object(map) => map,
                _ => unreachable!(),
            };
        }
    }
}

/// Handle `%% @graph auth.<name>.<field>: <value>` annotations.
///
/// Recognised fields per strategy:
///   - `type`        — discriminator string (e.g. `"bearer"`)
///   - `params`      — JSON value (object); whole-object set per annotation
///   - `params.<path>` — nested assignment inside params (dot-path expansion)
///   - `secrets.<role>` — logical name bound to the given role
fn apply_auth_annotation(
    meta: &mut crate::graph::metadata::GraphMetadata,
    rest: &str,
    value: serde_json::Value,
) {
    let (name, tail) = match rest.split_once('.') {
        Some((n, t)) => (n.to_string(), t),
        None => return,
    };
    let entry = meta
        .auth
        .entry(name)
        .or_insert_with(|| crate::graph::auth_decl::AuthStrategyDecl::new(""));

    if tail == "type" {
        entry.type_ = match value {
            serde_json::Value::String(s) => s,
            other => other.to_string(),
        };
    } else if tail == "params" {
        entry.params = value;
    } else if let Some(path) = tail.strip_prefix("params.") {
        // Ensure params is an object before nesting
        if !entry.params.is_object() {
            entry.params = serde_json::Value::Object(serde_json::Map::new());
        }
        set_nested(&mut entry.params, path, value);
    } else if let Some(role) = tail.strip_prefix("secrets.") {
        let logical = match value {
            serde_json::Value::String(s) => s,
            other => other.to_string(),
        };
        entry.secrets.insert(role.to_string(), logical);
    }
}

fn apply_node_annotation(
    node: &mut Node,
    key: &str,
    value: serde_json::Value,
) -> Result<(), String> {
    // handler
    if key == "handler" {
        node.handler = Some(value_as_string(&value));
        return Ok(());
    }

    // inputs.<port_name>: "<TypeName>"
    if let Some(port_name) = key.strip_prefix("inputs.") {
        let type_str = value_as_string(&value);
        let port_type = type_str
            .parse::<PortType>()
            .map_err(|e| format!("invalid port type for '{port_name}': {e}"))?;
        if node.input_port(port_name).is_none() {
            node.inputs.push(Port::new(port_name, port_type));
        }
        return Ok(());
    }

    // outputs.<port_name>: "<TypeName>"
    if let Some(port_name) = key.strip_prefix("outputs.") {
        let type_str = value_as_string(&value);
        let port_type = type_str
            .parse::<PortType>()
            .map_err(|e| format!("invalid port type for '{port_name}': {e}"))?;
        if node.output_port(port_name).is_none() {
            node.outputs.push(Port::new(port_name, port_type));
        }
        return Ok(());
    }

    // config.<dot.path>: value
    if let Some(path) = key.strip_prefix("config.") {
        set_nested(&mut node.config, path, value);
        return Ok(());
    }

    // exec.<dot.path>: value
    if let Some(path) = key.strip_prefix("exec.") {
        set_nested(&mut node.exec, path, value);
        return Ok(());
    }

    // Unknown top-level key — store in config as fallback
    set_nested(&mut node.config, key, value);
    Ok(())
}

fn value_as_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Set a value at a dot-separated path within a JSON object.
///
/// Creates intermediate objects as needed:
/// `set_nested(target, "a.b.c", 42)` → `{"a": {"b": {"c": 42}}}`
///
/// Note: relies on serde_json's `IndexMut` which auto-converts `Null` to `Object`
/// when indexed with a string key.
fn set_nested(target: &mut serde_json::Value, path: &str, value: serde_json::Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = target;

    for (i, &part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            current[part] = value;
            return;
        }
        // Ensure intermediate path is an object
        let is_obj = current.get(part).is_some_and(|v| v.is_object());
        if !is_obj {
            current[part] = serde_json::json!({});
        }
        current = &mut current[part];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_value_types() {
        assert_eq!(parse_value("\"hello\""), serde_json::json!("hello"));
        assert_eq!(parse_value("42"), serde_json::json!(42));
        assert_eq!(parse_value("2.5"), serde_json::json!(2.5));
        assert_eq!(parse_value("true"), serde_json::json!(true));
        assert_eq!(parse_value("false"), serde_json::json!(false));
        assert_eq!(parse_value("null"), serde_json::Value::Null);
        assert_eq!(parse_value("[\"a\", \"b\"]"), serde_json::json!(["a", "b"]));
        assert_eq!(parse_value("fetch_data"), serde_json::json!("fetch_data"));
        assert_eq!(parse_value(""), serde_json::Value::Null);
    }

    #[test]
    fn dot_path_expansion() {
        let mut target = serde_json::json!({});
        set_nested(&mut target, "a.b.c", serde_json::json!(42));
        assert_eq!(target, serde_json::json!({"a": {"b": {"c": 42}}}));
    }

    #[test]
    fn dot_path_merge() {
        let mut target = serde_json::json!({});
        set_nested(&mut target, "config.url", serde_json::json!("http://x"));
        set_nested(&mut target, "config.timeout", serde_json::json!(5000));
        assert_eq!(
            target,
            serde_json::json!({"config": {"url": "http://x", "timeout": 5000}})
        );
    }

    #[test]
    fn single_level_path() {
        let mut target = serde_json::json!({});
        set_nested(&mut target, "key", serde_json::json!("value"));
        assert_eq!(target, serde_json::json!({"key": "value"}));
    }

    #[test]
    fn auth_annotations_populate_metadata() {
        let input = r#"
graph TD
    A --> B

    %% @graph auth.api.type: "bearer"
    %% @graph auth.api.secrets.token: "my_api_key"
    %% @graph auth.signer.type: "hmac"
    %% @graph auth.signer.params.algorithm: "sha256"
    %% @graph auth.signer.secrets.key_id: "kid_name"
    %% @graph auth.signer.secrets.secret: "secret_name"
"#;
        let graph = crate::mermaid::load_mermaid(input).unwrap();
        let auth = &graph.metadata().auth;
        let api = auth.get("api").expect("api strategy declared");
        assert_eq!(api.type_, "bearer");
        assert_eq!(api.secrets.get("token").unwrap(), "my_api_key");

        let signer = auth.get("signer").expect("signer strategy declared");
        assert_eq!(signer.type_, "hmac");
        assert_eq!(signer.params["algorithm"], "sha256");
        assert_eq!(signer.secrets.get("key_id").unwrap(), "kid_name");
        assert_eq!(signer.secrets.get("secret").unwrap(), "secret_name");
    }
}
