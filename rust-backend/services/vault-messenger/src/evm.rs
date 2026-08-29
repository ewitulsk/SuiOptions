//! EVM spoke-chain access: a thin JSON-RPC client over reqwest (mirroring
//! cctp-relay's Solana leg — no heavy provider stack) plus alloy legacy-tx
//! signing. The chain surface the rest of the service uses is the
//! [`SpokeChain`] trait so tests can mock it without a network.

use anyhow::{anyhow, bail, Context, Result};
use alloy_consensus::{SignableTransaction, TxEnvelope, TxLegacy};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{keccak256, Address, Bytes, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use async_trait::async_trait;
use serde_json::{json, Value};

/// One `OutboundMessage(bytes)` log off the spoke endpoint.
#[derive(Debug, Clone)]
pub struct SpokeLog {
    pub tx_hash: String,
    pub block: u64,
    /// The full wire message (envelope ‖ payload).
    pub message: Vec<u8>,
}

/// The spoke-chain operations the watcher/deliverer/cranks need.
#[async_trait]
pub trait SpokeChain: Send + Sync {
    async fn latest_block(&self) -> Result<u64>;
    /// `OutboundMessage(bytes)` logs from the endpoint, inclusive range.
    async fn outbound_logs(&self, from_block: u64, to_block: u64) -> Result<Vec<SpokeLog>>;
    /// Submit `RelayerEndpoint.deliver(message)`; returns the tx hash.
    async fn deliver(&self, message: &[u8]) -> Result<String>;
    /// Submit the permissionless `SpokeVault.syncState()`.
    async fn sync_state(&self) -> Result<String>;
    /// Receipt status: None = not yet mined, Some(success).
    async fn tx_status(&self, tx_hash: &str) -> Result<Option<bool>>;
    /// `SpokeVault.lastInboundSeq()` — the spoke's applied hub→spoke seq.
    async fn last_inbound_seq(&self) -> Result<u64>;
}

// ── ABI helpers (pure; unit-tested) ────────────────────────────────────

pub fn selector(signature: &str) -> [u8; 4] {
    let h = keccak256(signature.as_bytes());
    [h[0], h[1], h[2], h[3]]
}

/// `deliver(bytes)` calldata: selector ‖ head(0x20) ‖ len ‖ padded data.
pub fn encode_deliver(message: &[u8]) -> Vec<u8> {
    let mut out = selector("deliver(bytes)").to_vec();
    out.extend_from_slice(&abi_word(0x20));
    out.extend_from_slice(&abi_word(message.len() as u64));
    out.extend_from_slice(message);
    let pad = (32 - message.len() % 32) % 32;
    out.extend(std::iter::repeat(0u8).take(pad));
    out
}

pub fn encode_sync_state() -> Vec<u8> {
    selector("syncState()").to_vec()
}

fn abi_word(v: u64) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&v.to_be_bytes());
    w
}

/// Decode a single ABI-encoded dynamic `bytes` value (the non-indexed data
/// of `OutboundMessage(bytes)`).
pub fn decode_abi_bytes(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 64 {
        bail!("abi bytes too short: {}", data.len());
    }
    let offset = abi_u64(&data[..32])? as usize;
    if offset + 32 > data.len() {
        bail!("abi bytes offset {offset} out of range");
    }
    let len = abi_u64(&data[offset..offset + 32])? as usize;
    let start = offset + 32;
    if start + len > data.len() {
        bail!("abi bytes length {len} out of range");
    }
    Ok(data[start..start + len].to_vec())
}

/// A 32-byte ABI word that must fit u64 (offsets, lengths, uint64 returns).
pub fn abi_u64(word: &[u8]) -> Result<u64> {
    if word.len() < 32 || word[..24].iter().any(|b| *b != 0) {
        bail!("abi word does not fit u64");
    }
    Ok(u64::from_be_bytes(word[24..32].try_into().unwrap()))
}

fn hex_bytes(s: &str) -> Result<Vec<u8>> {
    hex::decode(s.trim_start_matches("0x")).with_context(|| format!("decoding hex {s}"))
}

// ── the live client ────────────────────────────────────────────────────

pub struct EvmClient {
    http: reqwest::Client,
    rpc_url: String,
    chain_id: u64,
    signer: PrivateKeySigner,
    vault: Address,
    endpoint: Address,
    gas_limit: u64,
}

impl EvmClient {
    pub fn new(cfg: &crate::config::SpokeConfig, private_key: &str) -> Result<Self> {
        let signer: PrivateKeySigner = private_key
            .trim()
            .parse()
            .map_err(|e| anyhow!("decoding EVM private key: {e}"))?;
        Ok(Self {
            http: reqwest::Client::new(),
            rpc_url: cfg.rpc_url.clone(),
            chain_id: cfg.chain_id,
            signer,
            vault: cfg
                .spoke_vault_address
                .parse()
                .map_err(|e| anyhow!("bad spoke_vault_address: {e}"))?,
            endpoint: cfg
                .relayer_endpoint_address
                .parse()
                .map_err(|e| anyhow!("bad relayer_endpoint_address: {e}"))?,
            gas_limit: cfg.gas_limit,
        })
    }

    pub fn address(&self) -> Address {
        self.signer.address()
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let resp: Value = self
            .http
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("{method} request"))?
            .json()
            .await
            .with_context(|| format!("{method} response body"))?;
        if let Some(err) = resp.get("error").filter(|e| !e.is_null()) {
            bail!("{method} error: {err}");
        }
        resp.get("result")
            .cloned()
            .ok_or_else(|| anyhow!("{method}: no result"))
    }

    async fn rpc_u64(&self, method: &str, params: Value) -> Result<u64> {
        let v = self.rpc(method, params).await?;
        let s = v.as_str().ok_or_else(|| anyhow!("{method}: non-string result"))?;
        u64::from_str_radix(s.trim_start_matches("0x"), 16)
            .with_context(|| format!("{method}: bad hex {s}"))
    }

    /// Sign + submit a legacy (EIP-155) call — Orbit chains accept these
    /// everywhere and the fee logic stays trivial.
    async fn submit_call(&self, to: Address, data: Vec<u8>) -> Result<String> {
        let nonce = self
            .rpc_u64(
                "eth_getTransactionCount",
                json!([format!("{:#x}", self.address()), "pending"]),
            )
            .await?;
        let gas_price = self.rpc_u64("eth_gasPrice", json!([])).await?;
        let tx = TxLegacy {
            chain_id: Some(self.chain_id),
            nonce,
            // 2x headroom over the node quote; the excess is not charged.
            gas_price: (gas_price as u128).saturating_mul(2),
            gas_limit: self.gas_limit,
            to: TxKind::Call(to),
            value: U256::ZERO,
            input: Bytes::from(data),
        };
        let sig = self
            .signer
            .sign_hash_sync(&tx.signature_hash())
            .context("signing spoke tx")?;
        let raw = TxEnvelope::Legacy(tx.into_signed(sig)).encoded_2718();
        let hash = self
            .rpc("eth_sendRawTransaction", json!([format!("0x{}", hex::encode(&raw))]))
            .await?;
        hash.as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("eth_sendRawTransaction: non-string hash"))
    }

    async fn call_view(&self, to: Address, data: Vec<u8>) -> Result<Vec<u8>> {
        let ret = self
            .rpc(
                "eth_call",
                json!([{ "to": format!("{to:#x}"), "data": format!("0x{}", hex::encode(data)) }, "latest"]),
            )
            .await?;
        hex_bytes(ret.as_str().ok_or_else(|| anyhow!("eth_call: non-string result"))?)
    }
}

#[async_trait]
impl SpokeChain for EvmClient {
    async fn latest_block(&self) -> Result<u64> {
        self.rpc_u64("eth_blockNumber", json!([])).await
    }

    async fn outbound_logs(&self, from_block: u64, to_block: u64) -> Result<Vec<SpokeLog>> {
        let topic = format!("0x{}", hex::encode(keccak256("OutboundMessage(bytes)".as_bytes())));
        let logs = self
            .rpc(
                "eth_getLogs",
                json!([{
                    "address": format!("{:#x}", self.endpoint),
                    "topics": [topic],
                    "fromBlock": format!("{from_block:#x}"),
                    "toBlock": format!("{to_block:#x}"),
                }]),
            )
            .await?;
        let mut out = Vec::new();
        for log in logs.as_array().cloned().unwrap_or_default() {
            let data = hex_bytes(
                log.get("data")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("log missing data"))?,
            )?;
            let block = u64::from_str_radix(
                log.get("blockNumber")
                    .and_then(Value::as_str)
                    .unwrap_or("0x0")
                    .trim_start_matches("0x"),
                16,
            )
            .unwrap_or(0);
            out.push(SpokeLog {
                tx_hash: log
                    .get("transactionHash")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                block,
                message: decode_abi_bytes(&data)?,
            });
        }
        Ok(out)
    }

    async fn deliver(&self, message: &[u8]) -> Result<String> {
        self.submit_call(self.endpoint, encode_deliver(message)).await
    }

    async fn sync_state(&self) -> Result<String> {
        self.submit_call(self.vault, encode_sync_state()).await
    }

    async fn tx_status(&self, tx_hash: &str) -> Result<Option<bool>> {
        let receipt = self.rpc("eth_getTransactionReceipt", json!([tx_hash])).await?;
        if receipt.is_null() {
            return Ok(None);
        }
        Ok(Some(receipt.get("status").and_then(Value::as_str) == Some("0x1")))
    }

    async fn last_inbound_seq(&self) -> Result<u64> {
        let ret = self
            .call_view(self.vault, selector("lastInboundSeq()").to_vec())
            .await?;
        abi_u64(&ret).context("decoding lastInboundSeq()")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deliver_calldata_shape() {
        let msg = vec![0xde, 0xad, 0xbe, 0xef, 0x01];
        let data = encode_deliver(&msg);
        assert_eq!(&data[..4], &selector("deliver(bytes)"));
        // head word: offset 0x20
        assert_eq!(abi_u64(&data[4..36]).unwrap(), 0x20);
        // length word
        assert_eq!(abi_u64(&data[36..68]).unwrap(), 5);
        // payload, zero-padded to a word
        assert_eq!(&data[68..73], &msg[..]);
        assert_eq!(data.len(), 4 + 32 + 32 + 32);
        assert!(data[73..].iter().all(|b| *b == 0));
    }

    #[test]
    fn abi_bytes_round_trip() {
        let msg: Vec<u8> = (0..90u8).collect();
        // encode_deliver's tail IS abi.encode(bytes): reuse it.
        let data = encode_deliver(&msg)[4..].to_vec();
        assert_eq!(decode_abi_bytes(&data).unwrap(), msg);

        assert!(decode_abi_bytes(&[0u8; 8]).is_err());
        // Length overruns the buffer.
        let mut bad = data.clone();
        bad[63] = 0xff;
        assert!(decode_abi_bytes(&bad).is_err());
    }

    #[test]
    fn abi_u64_rejects_wide_words() {
        let mut w = [0u8; 32];
        w[31] = 7;
        assert_eq!(abi_u64(&w).unwrap(), 7);
        w[0] = 1;
        assert!(abi_u64(&w).is_err());
    }
}
