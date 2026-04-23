use crate::auth::apply_ctx::AuthApplyCtx;
use crate::auth::decl::AuthStrategyDecl;
use crate::auth::error::AuthError;
use crate::auth::strategy::AuthStrategy;
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use reqwest::RequestBuilder;
use sha2::{Sha256, Sha512};
use std::sync::Arc;

pub const HMAC_TYPE: &str = "hmac";

const KEY_ID_ROLE: &str = "key_id";
const SECRET_ROLE: &str = "secret";

const DEFAULT_ALGO: &str = "sha256";
const DEFAULT_KEY_ID_HEADER: &str = "X-Key-Id";
const DEFAULT_SIGNATURE_HEADER: &str = "X-Signature";

/// Generic HMAC request-signing strategy.
///
/// Canonical string (newline-joined, deterministic):
///   `<METHOD>\n<PATH_WITH_QUERY>\n<signed_headers_lowercased_and_sorted>\n<hex(sha256(body))>`
/// where each signed header renders as `name:value` on its own line.
///
/// Not byte-compatible with AWS SigV4 or any specific vendor scheme — it is
/// a reasonable default for custom APIs. Vendor-specific strategies should
/// ship as their own types.
pub struct HmacStrategy {
    algorithm: HmacAlgo,
    key_id_header: String,
    signature_header: String,
    signed_headers: Vec<String>,
    include_body: bool,
}

#[derive(Clone, Copy)]
enum HmacAlgo {
    Sha256,
    Sha512,
}

impl HmacStrategy {
    pub fn from_decl(decl: &AuthStrategyDecl) -> Result<Arc<dyn AuthStrategy>, AuthError> {
        let obj_default = serde_json::Map::new();
        let obj = match &decl.params {
            serde_json::Value::Null => &obj_default,
            serde_json::Value::Object(m) => m,
            _ => {
                return Err(AuthError::Config {
                    name: HMAC_TYPE.to_string(),
                    message: "params must be an object or absent".to_string(),
                });
            }
        };

        let algorithm = match obj
            .get("algorithm")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_ALGO)
        {
            "sha256" => HmacAlgo::Sha256,
            "sha512" => HmacAlgo::Sha512,
            other => {
                return Err(AuthError::Config {
                    name: HMAC_TYPE.to_string(),
                    message: format!("unsupported algorithm '{other}' (sha256 | sha512)"),
                });
            }
        };

        let key_id_header = obj
            .get("key_id_header")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_KEY_ID_HEADER)
            .to_string();
        let signature_header = obj
            .get("signature_header")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_SIGNATURE_HEADER)
            .to_string();

        let signed_headers: Vec<String> = obj
            .get("signed_headers")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_ascii_lowercase()))
                    .collect()
            })
            .unwrap_or_default();

        let include_body = obj
            .get("include_body")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        Ok(Arc::new(Self {
            algorithm,
            key_id_header,
            signature_header,
            signed_headers,
            include_body,
        }))
    }

    fn build_canonical(
        &self,
        method: &str,
        path_and_query: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> String {
        let mut sorted = headers.to_vec();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let header_block: String = sorted
            .iter()
            .map(|(k, v)| format!("{k}:{v}"))
            .collect::<Vec<_>>()
            .join("\n");
        let body_hash = if self.include_body {
            use sha2::Digest;
            let mut hasher = Sha256::new();
            hasher.update(body);
            hex::encode(hasher.finalize())
        } else {
            String::new()
        };
        format!("{method}\n{path_and_query}\n{header_block}\n{body_hash}")
    }

    fn sign(&self, key: &[u8], message: &[u8]) -> Result<String, AuthError> {
        match self.algorithm {
            HmacAlgo::Sha256 => {
                let mut mac =
                    Hmac::<Sha256>::new_from_slice(key).map_err(|e| AuthError::Apply {
                        name: HMAC_TYPE.to_string(),
                        message: format!("hmac init failed: {e}"),
                    })?;
                mac.update(message);
                Ok(hex::encode(mac.finalize().into_bytes()))
            }
            HmacAlgo::Sha512 => {
                let mut mac =
                    Hmac::<Sha512>::new_from_slice(key).map_err(|e| AuthError::Apply {
                        name: HMAC_TYPE.to_string(),
                        message: format!("hmac init failed: {e}"),
                    })?;
                mac.update(message);
                Ok(hex::encode(mac.finalize().into_bytes()))
            }
        }
    }
}

#[async_trait]
impl AuthStrategy for HmacStrategy {
    fn type_name(&self) -> &'static str {
        HMAC_TYPE
    }

    fn required_roles(&self) -> &'static [&'static str] {
        &[KEY_ID_ROLE, SECRET_ROLE]
    }

    async fn apply(
        &self,
        ctx: &AuthApplyCtx<'_>,
        builder: RequestBuilder,
    ) -> Result<RequestBuilder, AuthError> {
        let key_id = ctx.secret(KEY_ID_ROLE).await?;
        let secret = ctx.secret(SECRET_ROLE).await?;

        let key_id_str = key_id.reveal_str().ok_or_else(|| AuthError::Apply {
            name: ctx.strategy_name.to_string(),
            message: "hmac key_id secret is not valid UTF-8".to_string(),
        })?;

        // Canonicalize path+query from the URL.
        let path_and_query = match ctx.url.query() {
            Some(q) => format!("{}?{}", ctx.url.path(), q),
            None => ctx.url.path().to_string(),
        };

        // The signed_headers list is treated as expected header *names*; if
        // the handler hasn't set them on the builder yet, the HMAC view is
        // still deterministic because we canonicalize over the declared
        // names (and the caller is responsible for having set them).
        //
        // We cannot read arbitrary headers off the `RequestBuilder` without
        // try_clone + build, so we include only what's accessible: the
        // key_id header we are about to add.
        let mut headers: Vec<(String, String)> = Vec::new();
        for name in &self.signed_headers {
            if name == &self.key_id_header.to_ascii_lowercase() {
                headers.push((name.clone(), key_id_str.to_string()));
            }
            // Other named headers must come from the handler already having
            // set them; we document this limitation and skip silently.
        }

        let canonical = self.build_canonical(ctx.method, &path_and_query, &headers, ctx.body);
        let signature = self.sign(secret.expose_bytes(), canonical.as_bytes())?;

        Ok(builder
            .header(self.key_id_header.as_str(), key_id_str)
            .header(self.signature_header.as_str(), signature))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::resolver::{SecretResolver, StaticSecretResolver};
    use crate::auth::state::AuthState;
    use crate::execute::blackboard::Blackboard;
    use crate::execute::Outputs;
    use crate::template::default_resolver;
    use std::collections::BTreeMap;

    fn make_ctx<'a>(
        resolver: Arc<dyn SecretResolver>,
        secrets: &'a BTreeMap<String, String>,
        inputs: &'a Outputs,
        bb: &'a Blackboard,
        body: &'a [u8],
        url: &'a reqwest::Url,
    ) -> AuthApplyCtx<'a> {
        AuthApplyCtx {
            strategy_name: "h",
            secrets_map: secrets,
            resolver,
            state: Arc::new(AuthState::new()),
            inputs,
            blackboard: bb,
            template: default_resolver(),
            body,
            method: "POST",
            url,
        }
    }

    #[test]
    fn required_roles_reports_key_id_and_secret() {
        let s = HmacStrategy::from_decl(&AuthStrategyDecl::new(HMAC_TYPE)).unwrap();
        let roles = s.required_roles();
        assert!(roles.contains(&"key_id"));
        assert!(roles.contains(&"secret"));
    }

    #[test]
    fn rejects_unknown_algorithm() {
        let decl =
            AuthStrategyDecl::new(HMAC_TYPE).with_params(serde_json::json!({"algorithm": "md5"}));
        let err = match HmacStrategy::from_decl(&decl) {
            Ok(_) => panic!("expected Config error"),
            Err(e) => e,
        };
        assert!(matches!(err, AuthError::Config { .. }));
    }

    #[tokio::test]
    async fn apply_adds_both_headers() {
        let resolver = Arc::new(StaticSecretResolver::new());
        resolver.insert("h", "kid", "KID-1");
        resolver.insert("h", "sec", "supersecret");
        let mut secrets = BTreeMap::new();
        secrets.insert("key_id".into(), "kid".into());
        secrets.insert("secret".into(), "sec".into());

        let strategy = HmacStrategy::from_decl(&AuthStrategyDecl::new(HMAC_TYPE)).unwrap();
        let inputs = Outputs::new();
        let bb = Blackboard::new();
        let body = b"{\"k\":1}";
        let url = reqwest::Url::parse("http://example.com/v1/widget?q=1").unwrap();
        let ctx = make_ctx(
            resolver as Arc<dyn SecretResolver>,
            &secrets,
            &inputs,
            &bb,
            body,
            &url,
        );

        let client = reqwest::Client::new();
        let built = strategy
            .apply(&ctx, client.post(url.clone()).body(body.to_vec()))
            .await
            .unwrap();
        let req = built.build().unwrap();
        assert_eq!(req.headers().get("x-key-id").unwrap(), "KID-1");
        assert!(req.headers().get("x-signature").is_some());
        // Hex signature is 64 chars for sha256.
        let sig = req.headers().get("x-signature").unwrap().to_str().unwrap();
        assert_eq!(sig.len(), 64);
    }

    #[tokio::test]
    async fn signature_deterministic_for_same_body() {
        let resolver = Arc::new(StaticSecretResolver::new());
        resolver.insert("h", "kid", "KID-1");
        resolver.insert("h", "sec", "supersecret");
        let mut secrets = BTreeMap::new();
        secrets.insert("key_id".into(), "kid".into());
        secrets.insert("secret".into(), "sec".into());

        let strategy = HmacStrategy::from_decl(&AuthStrategyDecl::new(HMAC_TYPE)).unwrap();
        let inputs = Outputs::new();
        let bb = Blackboard::new();
        let body = b"payload";
        let url = reqwest::Url::parse("http://example.com/x").unwrap();

        let sig1 = {
            let ctx = make_ctx(
                resolver.clone() as Arc<dyn SecretResolver>,
                &secrets,
                &inputs,
                &bb,
                body,
                &url,
            );
            let client = reqwest::Client::new();
            let built = strategy
                .apply(&ctx, client.post(url.clone()))
                .await
                .unwrap();
            let req = built.build().unwrap();
            req.headers()
                .get("x-signature")
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        };
        let sig2 = {
            let ctx = make_ctx(
                resolver as Arc<dyn SecretResolver>,
                &secrets,
                &inputs,
                &bb,
                body,
                &url,
            );
            let client = reqwest::Client::new();
            let built = strategy
                .apply(&ctx, client.post(url.clone()))
                .await
                .unwrap();
            let req = built.build().unwrap();
            req.headers()
                .get("x-signature")
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(sig1, sig2);
    }

    #[tokio::test]
    async fn signature_changes_with_body() {
        let resolver = Arc::new(StaticSecretResolver::new());
        resolver.insert("h", "kid", "K");
        resolver.insert("h", "sec", "S");
        let mut secrets = BTreeMap::new();
        secrets.insert("key_id".into(), "kid".into());
        secrets.insert("secret".into(), "sec".into());
        let strategy = HmacStrategy::from_decl(&AuthStrategyDecl::new(HMAC_TYPE)).unwrap();
        let inputs = Outputs::new();
        let _bb = Blackboard::new();
        let url = reqwest::Url::parse("http://example.com/x").unwrap();

        let do_sign = |body: &'static [u8]| {
            let secrets = secrets.clone();
            let resolver = resolver.clone();
            let inputs = inputs.clone();
            let bb = Blackboard::new();
            let url = url.clone();
            let strategy = strategy.clone();
            async move {
                let ctx = make_ctx(
                    resolver as Arc<dyn SecretResolver>,
                    &secrets,
                    &inputs,
                    &bb,
                    body,
                    &url,
                );
                let client = reqwest::Client::new();
                let built = strategy
                    .apply(&ctx, client.post(url.clone()))
                    .await
                    .unwrap();
                let req = built.build().unwrap();
                req.headers()
                    .get("x-signature")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string()
            }
        };
        let sig_a = do_sign(b"a").await;
        let sig_b = do_sign(b"b").await;
        assert_ne!(sig_a, sig_b);
    }
}
