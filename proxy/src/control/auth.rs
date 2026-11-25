//! Who a request is from: the operator (its token, checked with an HMAC) or a user of an org (a token's
//! SHA-256 looked up in `users`).

use super::model::App;
use crate::random;

/// The operator's token, kept as an HMAC tag under a key made at start: a presented token is checked with
/// `hmac::verify`, in constant time, so it can't be found byte by byte from response times.
pub(crate) struct OperatorToken {
    pub(crate) key: ring::hmac::Key,
    pub(crate) tag: ring::hmac::Tag,
}

impl OperatorToken {
    pub(crate) fn new(token: &str) -> Result<Self, String> {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &random::bytes(32)?);
        let tag = ring::hmac::sign(&key, token.as_bytes());
        Ok(OperatorToken { key, tag })
    }

    pub(crate) fn matches(&self, token: &str) -> bool {
        ring::hmac::verify(&self.key, token.as_bytes(), self.tag.as_ref()).is_ok()
    }
}

/// Who a request is from: the operator (the control plane's own token) or a user of an org.
#[derive(Debug, PartialEq, Clone)]
pub(crate) enum Caller {
    Operator,
    User { name: String, org: String },
}

impl Caller {
    /// A user sees only its org's apps; the operator sees every app.
    pub(crate) fn sees(&self, app: &App) -> bool {
        match self {
            Caller::Operator => true,
            Caller::User { org, .. } => &app.org == org,
        }
    }
}

/// What `users.token_sha256` holds: the token itself never leaves the control plane (Corrosion copies
/// the table to every node).
pub(crate) fn token_hash(token: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, token.as_bytes()).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// A new user's API token: 32 random bytes, hex, behind a prefix that says what it is.
pub(crate) fn new_token() -> Result<String, String> {
    Ok(format!("flyyy_{}", random::hex(32)?))
}
