//! Reading claims out of JWTs issued to agent CLIs.
//!
//! Signatures are not verified: the tokens come from files the user's own agent
//! CLI wrote, and the claims are only used for display and identity matching.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jiff::Timestamp;
use serde_json::Value;

/// Decodes the payload of a JWT. Returns `None` if `token` is not a JWT.
pub fn claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.is_object().then_some(value)
}

/// The `exp` claim of a JWT.
pub fn expiry(token: &str) -> Option<Timestamp> {
    let exp = claims(token)?.get("exp")?.as_i64()?;
    Timestamp::from_second(exp).ok()
}

#[cfg(test)]
pub(crate) fn encode_unsigned(claims: &Value) -> String {
    format!(
        "{}.{}.sig",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_payload_and_expiry() {
        let token = encode_unsigned(&json!({"email": "a@b.c", "exp": 1_800_000_000}));
        assert_eq!(claims(&token).unwrap()["email"], "a@b.c");
        assert_eq!(expiry(&token).unwrap().as_second(), 1_800_000_000);
    }

    #[test]
    fn rejects_non_jwts() {
        assert!(claims("sk-ant-oat01-abc").is_none());
        assert!(claims("a.!!!.c").is_none());
    }
}
