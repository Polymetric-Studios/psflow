//! Re-export of the shared [`AuthStrategyDecl`] from the always-available
//! graph slice.
//!
//! See [`crate::graph::auth_decl`] for the serializable declaration shape.

pub use crate::graph::auth_decl::AuthStrategyDecl;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decl_serde_round_trip() {
        let d = AuthStrategyDecl::new("bearer")
            .with_params(serde_json::json!({"scheme": "Bearer"}))
            .with_secret("token", "my_api_key");
        let json = serde_json::to_string(&d).unwrap();
        let parsed: AuthStrategyDecl = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, d);
    }
}
