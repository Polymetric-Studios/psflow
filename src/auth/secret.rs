use std::fmt;
use zeroize::{Zeroize, Zeroizing};

/// An opaque, zeroize-on-drop secret value returned from a [`super::SecretResolver`].
///
/// The inner bytes are wiped on drop via the `zeroize` crate. The type
/// intentionally does not implement `Display` or `serde::Serialize` to make
/// it a compile error to accidentally log or serialise a secret. `Debug`
/// prints a fixed redacted marker.
pub struct SecretValue {
    bytes: Zeroizing<Vec<u8>>,
}

impl SecretValue {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Zeroizing::new(bytes),
        }
    }

    pub fn from_string(s: String) -> Self {
        Self::new(s.into_bytes())
    }

    pub fn from_static(s: &'static str) -> Self {
        Self::new(s.as_bytes().to_vec())
    }

    /// Borrow the raw bytes. Callers must not copy these into an
    /// unprotected allocation for longer than strictly necessary.
    pub fn expose_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Borrow as `&str`. Returns `None` if the bytes are not valid UTF-8.
    ///
    /// Named `reveal_str` rather than `as_str` to make the privacy crossing
    /// explicit at call sites.
    pub fn reveal_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.bytes).ok()
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(***)")
    }
}

impl Clone for SecretValue {
    fn clone(&self) -> Self {
        Self::new(self.bytes.to_vec())
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        // Zeroizing<Vec<u8>> already zeros on drop, but this belt-and-braces
        // call documents intent and protects against future type changes.
        self.bytes.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted() {
        let s = SecretValue::from_static("super-secret-token");
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("super-secret-token"));
        assert!(dbg.contains("***"));
    }

    #[test]
    fn reveal_str_returns_utf8() {
        let s = SecretValue::from_string("abc".into());
        assert_eq!(s.reveal_str(), Some("abc"));
    }

    #[test]
    fn reveal_str_none_on_invalid_utf8() {
        let s = SecretValue::new(vec![0xff, 0xfe]);
        assert_eq!(s.reveal_str(), None);
    }

    #[test]
    fn clone_preserves_contents() {
        let a = SecretValue::from_static("x");
        let b = a.clone();
        assert_eq!(a.expose_bytes(), b.expose_bytes());
    }
}
