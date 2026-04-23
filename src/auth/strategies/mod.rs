//! Built-in auth strategies: static_header, bearer, hmac, cookie_jar.

mod bearer;
mod cookie_jar;
mod hmac;
mod static_header;

pub use bearer::{BearerStrategy, BEARER_TYPE};
pub use cookie_jar::{CookieJarStrategy, COOKIE_JAR_TYPE};
pub use hmac::{HmacStrategy, HMAC_TYPE};
pub use static_header::{StaticHeaderStrategy, STATIC_HEADER_TYPE};

use super::registry::AuthStrategyRegistry;
use std::sync::Arc;

/// Register the four built-in factories into `reg`.
pub fn register_builtins(reg: &mut AuthStrategyRegistry) {
    reg.register_factory(
        STATIC_HEADER_TYPE,
        Arc::new(StaticHeaderStrategy::from_decl),
    );
    reg.register_factory(BEARER_TYPE, Arc::new(BearerStrategy::from_decl));
    reg.register_factory(HMAC_TYPE, Arc::new(HmacStrategy::from_decl));
    reg.register_factory(COOKIE_JAR_TYPE, Arc::new(CookieJarStrategy::from_decl));
}
