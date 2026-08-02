//! Minimal HS256 JWT sign/verify.
//!
//! Hand-rolled rather than pulling a JWT crate: the surface is tiny (one alg,
//! a fixed claim set) and this keeps the dependency footprint to `hmac` +
//! `sha2` + `base64`, all already in the workspace.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Token claims.
///
/// `sub` is the account uuid, not a wallet address. An account may hold several
/// login methods, so an address is one identity among many rather than the
/// identity itself. `address` still carries the Sui address when the session
/// was opened by wallet signature, because callers display it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Subject: the account uuid, hyphenated.
    pub sub: String,
    /// `admin` | `business` | `individual`.
    pub role: String,
    /// Opaque authorization scope — dakota-service reads it as a Dakota
    /// customer id. Absent for admins, who are unscoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Sui address, present only for wallet-opened sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// Client IP the token was issued to. Refresh is gated on this matching.
    pub ip: String,
    /// Issued-at, unix seconds.
    pub iat: u64,
    /// Expiry, unix seconds.
    pub exp: u64,
}

#[derive(Serialize)]
struct Header {
    alg: &'static str,
    typ: &'static str,
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn mac(secret: &str, signing_input: &str) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts keys of any size");
    m.update(signing_input.as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// Sign a token.
pub fn sign(claims: &Claims, secret: &str) -> Result<String> {
    let header = Header { alg: "HS256", typ: "JWT" };
    let h = B64.encode(serde_json::to_vec(&header)?);
    let p = B64.encode(serde_json::to_vec(claims)?);
    let signing_input = format!("{h}.{p}");
    let sig = B64.encode(mac(secret, &signing_input));
    Ok(format!("{signing_input}.{sig}"))
}

/// Verify a token's signature and decode its claims. When `check_exp` is true,
/// also rejects expired tokens (refresh passes `false` to accept a still-fresh
/// but expired access token within the refresh window).
pub fn verify(token: &str, secret: &str, check_exp: bool) -> Result<Claims> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        bail!("malformed jwt");
    }
    let signing_input = format!("{}.{}", parts[0], parts[1]);

    let mut m = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts keys of any size");
    m.update(signing_input.as_bytes());
    let provided = B64
        .decode(parts[2])
        .map_err(|_| anyhow!("jwt signature not base64url"))?;
    m.verify_slice(&provided)
        .map_err(|_| anyhow!("jwt signature mismatch"))?;

    let payload = B64
        .decode(parts[1])
        .map_err(|_| anyhow!("jwt payload not base64url"))?;
    let claims: Claims = serde_json::from_slice(&payload)?;

    if check_exp && now_secs() >= claims.exp {
        bail!("token expired");
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(exp: u64) -> Claims {
        Claims {
            sub: "3f1a…".into(),
            role: "admin".into(),
            scope: None,
            address: Some("0xabc".into()),
            ip: "1.2.3.4".into(),
            iat: now_secs(),
            exp,
        }
    }

    #[test]
    fn sign_then_verify() {
        let t = sign(&claims(now_secs() + 100), "secret").unwrap();
        let c = verify(&t, "secret", true).unwrap();
        assert_eq!(c.sub, "3f1a…");
        assert_eq!(c.ip, "1.2.3.4");
        assert_eq!(c.role, "admin");
        assert_eq!(c.address.as_deref(), Some("0xabc"));
    }

    #[test]
    fn scoped_claims_round_trip() {
        let mut c = claims(now_secs() + 100);
        c.role = "business".into();
        c.scope = Some("3HNCB4vp2zWMwdfoY33qKK11iOJ".into());
        c.address = None;
        let decoded = verify(&sign(&c, "secret").unwrap(), "secret", true).unwrap();
        assert_eq!(decoded.role, "business");
        assert_eq!(decoded.scope.as_deref(), Some("3HNCB4vp2zWMwdfoY33qKK11iOJ"));
        // Omitted rather than serialized as null, so the token stays compact.
        assert!(decoded.address.is_none());
    }

    #[test]
    fn wrong_secret_fails() {
        let t = sign(&claims(now_secs() + 100), "secret").unwrap();
        assert!(verify(&t, "other", true).is_err());
    }

    #[test]
    fn expired_rejected_only_when_checked() {
        let t = sign(&claims(now_secs() - 1), "secret").unwrap();
        assert!(verify(&t, "secret", true).is_err());
        // refresh path: expiry not enforced
        assert!(verify(&t, "secret", false).is_ok());
    }
}
