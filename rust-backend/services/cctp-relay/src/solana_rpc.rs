//! Thin Solana JSON-RPC client over reqwest. The full solana-client crate
//! stays out of this workspace (Sui crates pin the framework/mainnet
//! branch); the relayer only needs four calls.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

#[derive(Clone)]
pub struct SolanaRpc {
    http: reqwest::Client,
    url: String,
}

impl SolanaRpc {
    pub fn new(url: &str) -> Self {
        Self { http: reqwest::Client::new(), url: url.to_string() }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let resp: Value = self
            .http
            .post(&self.url)
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
            .send()
            .await
            .with_context(|| format!("calling solana rpc {method}"))?
            .json()
            .await
            .with_context(|| format!("parsing solana rpc {method} response"))?;
        if let Some(err) = resp.get("error") {
            return Err(anyhow!("solana rpc {method} error: {err}"));
        }
        Ok(resp["result"].clone())
    }

    pub async fn latest_blockhash(&self) -> Result<String> {
        let r = self
            .call("getLatestBlockhash", json!([{"commitment": "confirmed"}]))
            .await?;
        r["value"]["blockhash"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| anyhow!("no blockhash in response"))
    }

    /// Submit a base64 bincode-serialized transaction; returns the signature.
    pub async fn send_transaction(&self, tx_base64: &str) -> Result<String> {
        let r = self
            .call(
                "sendTransaction",
                json!([tx_base64, {"encoding": "base64", "preflightCommitment": "confirmed"}]),
            )
            .await?;
        r.as_str()
            .map(String::from)
            .ok_or_else(|| anyhow!("sendTransaction returned no signature"))
    }

    /// Confirmed transaction lookup: Ok(Some((block_time, success))) once the
    /// tx is on chain, Ok(None) while unknown.
    pub async fn transaction_status(&self, signature: &str) -> Result<Option<(i64, bool)>> {
        let r = self
            .call(
                "getTransaction",
                json!([signature, {"commitment": "confirmed", "encoding": "json", "maxSupportedTransactionVersion": 0}]),
            )
            .await?;
        if r.is_null() {
            return Ok(None);
        }
        let block_time = r["blockTime"].as_i64().unwrap_or(0);
        let success = r["meta"]["err"].is_null();
        Ok(Some((block_time, success)))
    }

    pub async fn account_exists(&self, address: &str) -> Result<bool> {
        let r = self
            .call("getAccountInfo", json!([address, {"encoding": "base64"}]))
            .await?;
        Ok(!r["value"].is_null())
    }
}
