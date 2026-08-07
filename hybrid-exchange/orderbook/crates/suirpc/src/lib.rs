//! Minimal Sui JSON-RPC client for the orderbook service.
//!
//! Deliberately thin (reqwest + serde_json) instead of pulling the full Sui
//! SDK dependency tree: the service only needs event queries, object reads,
//! server-side transaction building (`unsafe_moveCall`) and execution.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("rpc error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("malformed response: {0}")]
    Malformed(String),
}

/// Cursor into the event stream (also the ID of an event).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventCursor {
    pub tx_digest: String,
    pub event_seq: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuiEvent {
    pub id: EventCursor,
    pub package_id: String,
    pub transaction_module: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub parsed_json: Value,
    #[serde(default)]
    pub timestamp_ms: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ExecutionResult {
    pub tx_digest: String,
    pub success: bool,
    /// Raw effects status error string when failed, e.g.
    /// `MoveAbort(MoveLocation { … name: Identifier("settlement") … }, 7) in command 0`.
    pub error: Option<String>,
}

/// A decoded Move abort: (module name, abort code).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveAbort {
    pub module: String,
    pub code: u64,
}

/// Best-effort parse of a MoveAbort out of an effects error string.
pub fn parse_move_abort(error: &str) -> Option<MoveAbort> {
    let idx = error.find("MoveAbort(")?;
    let rest = &error[idx..];
    let name_key = "Identifier(\"";
    let n = rest.find(name_key)? + name_key.len();
    let name_end = rest[n..].find('"')? + n;
    let module = rest[n..name_end].to_string();
    // abort code: the number after the last "}, " (closing the MoveLocation)
    let code_start = rest.rfind("}, ")? + 3;
    let code_str: String = rest[code_start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let code = code_str.parse().ok()?;
    Some(MoveAbort { module, code })
}

pub struct SuiRpcClient {
    http: reqwest::Client,
    url: String,
}

impl SuiRpcClient {
    pub fn new(url: impl Into<String>) -> Self {
        SuiRpcClient { http: reqwest::Client::new(), url: url.into() }
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let resp: Value = self.http.post(&self.url).json(&body).send().await?.json().await?;
        if let Some(err) = resp.get("error") {
            return Err(RpcError::Rpc {
                code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        resp.get("result")
            .cloned()
            .ok_or_else(|| RpcError::Malformed("missing result".into()))
    }

    /// All events emitted by a package, in checkpoint order, from `cursor`.
    pub async fn query_package_events(
        &self,
        package: &str,
        cursor: Option<&EventCursor>,
        limit: usize,
    ) -> Result<(Vec<SuiEvent>, Option<EventCursor>, bool), RpcError> {
        let params = json!([
            { "Package": package },
            cursor,
            limit,
            false, // ascending
        ]);
        let res = self.call("suix_queryEvents", params).await?;
        let data: Vec<SuiEvent> = serde_json::from_value(
            res.get("data").cloned().unwrap_or(Value::Array(vec![])),
        )
        .map_err(|e| RpcError::Malformed(e.to_string()))?;
        let next: Option<EventCursor> = res
            .get("nextCursor")
            .and_then(|c| serde_json::from_value(c.clone()).ok());
        let has_next = res
            .get("hasNextPage")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok((data, next, has_next))
    }

    pub async fn get_object(&self, object_id: &str) -> Result<Value, RpcError> {
        self.call(
            "sui_getObject",
            json!([object_id, { "showContent": true, "showOwner": true }]),
        )
        .await
    }

    /// Build a Move-call transaction server-side; returns base64 tx bytes.
    #[allow(clippy::too_many_arguments)]
    pub async fn unsafe_move_call(
        &self,
        signer: &str,
        package: &str,
        module: &str,
        function: &str,
        type_args: &[String],
        args: Vec<Value>,
        gas_budget: u64,
    ) -> Result<String, RpcError> {
        let res = self
            .call(
                "unsafe_moveCall",
                json!([
                    signer,
                    package,
                    module,
                    function,
                    type_args,
                    args,
                    Value::Null, // gas object: node picks
                    gas_budget.to_string(),
                    Value::Null, // execution mode
                ]),
            )
            .await?;
        res.get("txBytes")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| RpcError::Malformed("missing txBytes".into()))
    }

    /// Execute a signed transaction and wait for local execution; `signatures`
    /// are base64 serialized (`flag ‖ sig ‖ pk`).
    pub async fn execute_tx(
        &self,
        tx_bytes_b64: &str,
        signatures_b64: &[String],
    ) -> Result<ExecutionResult, RpcError> {
        let res = self
            .call(
                "sui_executeTransactionBlock",
                json!([
                    tx_bytes_b64,
                    signatures_b64,
                    { "showEffects": true, "showEvents": false },
                    "WaitForLocalExecution",
                ]),
            )
            .await?;
        let digest = res
            .get("digest")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let status = res
            .pointer("/effects/status/status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let error = res
            .pointer("/effects/status/error")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(ExecutionResult { tx_digest: digest, success: status == "success", error })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abort_parsing() {
        let s = "MoveAbort(MoveLocation { module: ModuleId { address: abc123, name: Identifier(\"settlement\") }, function: 12, instruction: 5, function_name: Some(\"fill_limit_order\") }, 7) in command 0";
        assert_eq!(
            parse_move_abort(s),
            Some(MoveAbort { module: "settlement".into(), code: 7 })
        );
        assert_eq!(parse_move_abort("InsufficientGas"), None);
    }
}
