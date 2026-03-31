use serde::{Deserialize, Serialize};

/// Graph-level metadata parsed from `%% @graph` annotations.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GraphMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_executor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_adapter: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_capabilities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_metadata_is_empty() {
        let m = GraphMetadata::default();
        assert!(m.name.is_none());
        assert!(m.version.is_none());
        assert!(m.description.is_none());
        assert!(m.required_adapter.is_none());
        assert!(m.required_capabilities.is_empty());
        assert!(m.tags.is_empty());
    }

    #[test]
    fn metadata_serde_round_trip() {
        let m = GraphMetadata {
            name: Some("Test Pipeline".into()),
            version: Some("2.0".into()),
            description: Some("A test graph".into()),
            direction: Some("TD".into()),
            default_executor: Some("topological".into()),
            required_adapter: Some("claude_cli".into()),
            required_capabilities: vec!["tool_use".into(), "structured_output".into()],
            author: Some("Tester".into()),
            tags: vec!["test".into(), "example".into()],
        };

        let json = serde_json::to_string(&m).unwrap();
        let parsed: GraphMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, m);
    }

    #[test]
    fn metadata_skips_none_fields_in_json() {
        let m = GraphMetadata {
            name: Some("Only Name".into()),
            ..Default::default()
        };

        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("Only Name"));
        assert!(!json.contains("version"));
        assert!(!json.contains("description"));
        assert!(!json.contains("tags"));
    }

    #[test]
    fn metadata_parsed_from_mermaid() {
        let input = "\
graph TD
    A --> B

    %% @graph name: \"My Pipeline\"
    %% @graph version: \"1.0\"
    %% @graph description: \"Does things\"
    %% @graph author: \"Dev\"
    %% @graph required_adapter: \"claude_cli\"
";
        let graph = crate::mermaid::load_mermaid(input).unwrap();
        let m = graph.metadata();
        assert_eq!(m.name, Some("My Pipeline".into()));
        assert_eq!(m.version, Some("1.0".into()));
        assert_eq!(m.description, Some("Does things".into()));
        assert_eq!(m.author, Some("Dev".into()));
        assert_eq!(m.required_adapter, Some("claude_cli".into()));
    }

    #[test]
    fn metadata_empty_when_no_graph_annotations() {
        let input = "\
graph TD
    A --> B
";
        let graph = crate::mermaid::load_mermaid(input).unwrap();
        let m = graph.metadata();
        assert!(m.name.is_none());
        assert!(m.version.is_none());
    }

    #[test]
    fn metadata_serde_round_trip_with_all_fields() {
        // Verify the full GraphMetadata struct survives JSON round-trip
        // when every field is populated
        let m = GraphMetadata {
            name: Some("Full".into()),
            version: Some("3.0".into()),
            description: Some("All fields set".into()),
            direction: Some("LR".into()),
            default_executor: Some("reactive".into()),
            required_adapter: Some("anthropic_api".into()),
            required_capabilities: vec!["tool_use".into(), "vision".into()],
            author: Some("Test".into()),
            tags: vec!["workflow".into(), "automation".into()],
        };

        let json = serde_json::to_string_pretty(&m).unwrap();
        let parsed: GraphMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, m);
        assert_eq!(parsed.required_capabilities.len(), 2);
        assert_eq!(parsed.tags.len(), 2);
    }
}
