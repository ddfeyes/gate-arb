//! Gate.io WS v4 HMAC-SHA512 authentication.
//!
//! Gate.io private WS auth format:
//!   sign_string = "channel=\nevent=\nts=<unix_seconds>"
//!   signature   = hex(HMAC-SHA512(secret, sign_string))
//!   payload     = { method: "api_key", KEY: key, SIGN: signature, TIME: ts }

use hmac::{Hmac, Mac};
use sha2::Sha512;

type HmacSha512 = Hmac<Sha512>;

/// Auth header sent as the `auth` field in the Gate.io WS login message.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuthHeader {
    pub method: String,
    pub key: String,
    pub signature: String,
    pub timestamp: u64,
}

/// Sign a message with HMAC-SHA512. Returns hex-encoded signature.
///
/// All Gate.io WS v4 signing uses this primitive.
pub fn hmac_sign(secret: &str, message: &str) -> String {
    let mut mac = HmacSha512::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
    mac.update(message.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Build a Gate.io WS v4 auth header.
///
/// `ts` is Unix epoch seconds (not milliseconds).
pub fn build_auth_header(api_key: &str, api_secret: &str, ts: u64) -> AuthHeader {
    // Gate.io sign string (channel and event are empty for login)
    let sign_string = format!("channel=\nevent=\nts={ts}");
    let signature = hmac_sign(api_secret, &sign_string);

    AuthHeader {
        method: "api_key".to_string(),
        key: api_key.to_string(),
        signature,
        timestamp: ts,
    }
}
