//! Chain-sourced public inputs for zkLogin verification (SO-423): the
//! consensus-agreed JWK registry and the current epoch.
//!
//! Sui stores the JWK set validators accept in the `AuthenticatorState`
//! system object (`0x7` → dynamic field → `AuthenticatorStateInner
//! .active_jwks`). Sourcing from there — rather than fanning out to each
//! OIDC provider's endpoint — means we accept exactly what the chain
//! accepts, in ONE GraphQL request that also carries the epoch. Both are
//! public inputs, not delegated trust: a wrong value fails verification,
//! never passes it.
//!
//! Read-through cache with a TTL; `force_refresh` covers the rotation race
//! (a freshly rotated provider key reaches 0x7 minutes later — on a zkLogin
//! verify failure the login handler refreshes once and retries). Everything
//! fails closed: no inputs → zkLogin logins are rejected, classic wallets
//! and the static allowlist never depend on this module.

use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use fastcrypto_zkp::bn254::zk_login::{JwkId, JWK};
use im::HashMap as ImHashMap;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::info;

/// How long a fetched (JWK set, epoch) pair is trusted. Testnet/mainnet
/// epochs are 24h and JWK rotations land on-chain within minutes, so ten
/// minutes keeps the steady-state login path fetch-free without meaningful
/// staleness (the 1h JWT TTL dominates any acceptance overshoot).
const TTL: Duration = Duration::from_secs(600);

/// Floor between forced refreshes, so a burst of bad logins cannot turn the
/// retry path into a GraphQL hammer.
const FORCE_REFRESH_FLOOR: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct Inputs {
    pub jwks: ImHashMap<JwkId, JWK>,
    pub epoch: u64,
}

struct Cached {
    inputs: Inputs,
    at: Instant,
}

pub struct ZkInputs {
    http: reqwest::Client,
    graphql_url: String,
    cached: Mutex<Option<Cached>>,
}

impl ZkInputs {
    pub fn new(graphql_url: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            graphql_url: graphql_url.trim_end_matches('/').to_string(),
            cached: Mutex::new(None),
        }
    }

    /// Current inputs, fetching if the cache is absent or older than [`TTL`].
    pub async fn current(&self) -> Result<Inputs> {
        let mut guard = self.cached.lock().await;
        if let Some(c) = guard.as_ref() {
            if c.at.elapsed() < TTL {
                return Ok(c.inputs.clone());
            }
        }
        let inputs = self.fetch().await?;
        *guard = Some(Cached {
            inputs: inputs.clone(),
            at: Instant::now(),
        });
        Ok(inputs)
    }

    /// Refresh regardless of TTL (rotation-race retry), rate-floored so
    /// failed-login bursts can't hammer the endpoint. Returns the fresh (or
    /// floor-limited current) inputs.
    pub async fn force_refresh(&self) -> Result<Inputs> {
        let mut guard = self.cached.lock().await;
        if let Some(c) = guard.as_ref() {
            if c.at.elapsed() < FORCE_REFRESH_FLOOR {
                return Ok(c.inputs.clone());
            }
        }
        let inputs = self.fetch().await?;
        *guard = Some(Cached {
            inputs: inputs.clone(),
            at: Instant::now(),
        });
        Ok(inputs)
    }

    /// One aliased GraphQL request: current epoch + the 0x7 JWK registry.
    async fn fetch(&self) -> Result<Inputs> {
        const Q: &str = "query { epoch { epochId } \
 authState: object(address: \"0x7\") { dynamicFields(first: 10) {\
 nodes { value { ... on MoveValue { json } } } } } }";
        let resp = self
            .http
            .post(&self.graphql_url)
            .json(&json!({ "query": Q }))
            .send()
            .await
            .context("sending zk-inputs query")?
            .error_for_status()
            .context("zk-inputs query returned an HTTP error")?;
        let body: Value = resp.json().await.context("decoding zk-inputs response")?;
        if let Some(errs) = body.get("errors") {
            return Err(anyhow!("zk-inputs query failed: {errs}"));
        }
        let inputs = parse_inputs(&body)?;
        info!(
            epoch = inputs.epoch,
            jwks = inputs.jwks.len(),
            "refreshed zkLogin public inputs"
        );
        metrics::counter!("auth_zk_inputs_refresh_total", "outcome" => "ok").increment(1);
        Ok(inputs)
    }
}

/// The slice of `AuthenticatorStateInner` we consume. `epoch` (a u64 the
/// GraphQL JSON renders as a string) is deliberately not modeled — serde
/// skips unknown/extra fields, and `JwkId`/`JWK` are all-string structs that
/// deserialize from the rendered JSON as-is.
#[derive(Deserialize)]
struct ActiveJwkJson {
    jwk_id: JwkId,
    jwk: JWK,
}

#[derive(Deserialize)]
struct AuthStateInnerJson {
    active_jwks: Vec<ActiveJwkJson>,
}

fn parse_inputs(body: &Value) -> Result<Inputs> {
    let epoch = body
        .pointer("/data/epoch/epochId")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("zk-inputs response missing epoch"))?;

    let nodes = body
        .pointer("/data/authState/dynamicFields/nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("zk-inputs response missing 0x7 dynamic fields"))?;

    let mut jwks = ImHashMap::new();
    for node in nodes {
        let Some(inner) = node.pointer("/value/json") else {
            continue;
        };
        let inner: AuthStateInnerJson = serde_json::from_value(inner.clone())
            .context("parsing AuthenticatorStateInner json")?;
        for a in inner.active_jwks {
            jwks.insert(a.jwk_id, a.jwk);
        }
    }
    if jwks.is_empty() {
        return Err(anyhow!("0x7 carried no active JWKs"));
    }
    Ok(Inputs { jwks, epoch })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_live_response_shape() {
        // Trimmed from a real graphql.testnet.sui.io response (2026-08-22).
        let body = serde_json::json!({
            "data": {
                "epoch": { "epochId": 1199 },
                "authState": { "dynamicFields": { "nodes": [ { "value": { "json": {
                    "version": "1",
                    "active_jwks": [
                        {
                            "jwk_id": { "iss": "https://accounts.google.com", "kid": "8ff13a6f" },
                            "jwk": { "kty": "RSA", "e": "AQAB", "n": "xm1S...", "alg": "RS256" },
                            "epoch": "1198"
                        },
                        {
                            "jwk_id": { "iss": "https://id.twitch.tv/oauth2", "kid": "1" },
                            "jwk": { "kty": "RSA", "e": "AQAB", "n": "6lq9...", "alg": "RS256" },
                            "epoch": "1198"
                        }
                    ]
                } } } ] } }
            }
        });
        let inputs = parse_inputs(&body).unwrap();
        assert_eq!(inputs.epoch, 1199);
        assert_eq!(inputs.jwks.len(), 2);
        let google = JwkId::new("https://accounts.google.com".into(), "8ff13a6f".into());
        assert_eq!(inputs.jwks.get(&google).unwrap().e, "AQAB");
    }

    #[test]
    fn empty_registry_is_an_error() {
        let body = serde_json::json!({
            "data": { "epoch": { "epochId": 1 },
                      "authState": { "dynamicFields": { "nodes": [] } } }
        });
        assert!(parse_inputs(&body).is_err());
    }

    /// Live fetch against public testnet GraphQL. Network-bound, so ignored
    /// by default: `cargo test -p auth-service -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_testnet_inputs_fetch() {
        let zk = ZkInputs::new("https://graphql.testnet.sui.io/graphql");
        let inputs = zk.current().await.unwrap();
        assert!(inputs.epoch > 0);
        assert!(!inputs.jwks.is_empty());
        // Google is always registered on testnet.
        assert!(inputs.jwks.keys().any(|k| k.iss.contains("google")));
    }
}
