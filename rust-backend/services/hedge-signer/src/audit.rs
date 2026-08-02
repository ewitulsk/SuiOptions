//! Append-only JSONL audit log — one line per `/sign` request.
//!
//! Best-effort durability: writes go through a tokio-mutexed file handle
//! and are flushed, not fsynced. Open failure at boot is fatal (main
//! propagates the error).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::sync::Mutex;
use tracing::error;

/// One audit line.
#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    /// Unix millis at decision time.
    pub ts_ms: u64,
    pub vault_id: String,
    /// Transaction sender (empty when the request never decoded).
    pub sender: String,
    /// `TransactionData` digest (empty when the request never decoded).
    pub tx_digest: String,
    /// `"approved"` | `"denied"`.
    pub decision: String,
    /// Approval tier (`strict`, or `frost:<kind>`); absent on denial.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// Denial reason; absent on approval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `describe_ptb` command summary.
    pub ptb_summary: String,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub struct AuditLog {
    file: Mutex<File>,
}

impl AuditLog {
    /// Open (append/create) the audit log. Fatal at boot on failure.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating audit log dir {}", parent.display()))?;
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening audit log {}", path.display()))?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    /// Append one line. Failures are logged, never propagated — an audit
    /// write must not turn a policy decision into a 500.
    pub async fn record(&self, entry: &AuditEntry) {
        let line = match serde_json::to_string(entry) {
            Ok(l) => l,
            Err(e) => {
                error!(error = %e, "serializing audit entry");
                return;
            }
        };
        let mut file = self.file.lock().await;
        if let Err(e) = writeln!(file, "{line}").and_then(|_| file.flush()) {
            error!(error = %e, "writing audit entry");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_line_serializes_expected_shape() {
        let entry = AuditEntry {
            ts_ms: 1_753_000_000_000,
            vault_id: "0xaa".into(),
            sender: "0xee".into(),
            tx_digest: "9pDigest".into(),
            decision: "denied".into(),
            tier: None,
            reason: Some("shared object 0xf00d is not in the allowlist".into()),
            ptb_summary: "0xdb::pool_proxy::place_limit_order<0>".into(),
        };
        let line = serde_json::to_string(&entry).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["ts_ms"], 1_753_000_000_000u64);
        assert_eq!(v["vault_id"], "0xaa");
        assert_eq!(v["decision"], "denied");
        assert!(v.get("tier").is_none(), "tier must be omitted on denial");
        assert_eq!(v["reason"], "shared object 0xf00d is not in the allowlist");

        let approved = AuditEntry {
            decision: "approved".into(),
            tier: Some("strict".into()),
            reason: None,
            ..entry
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&approved).unwrap()).unwrap();
        assert_eq!(v["tier"], "strict");
        assert!(v.get("reason").is_none(), "reason must be omitted on approval");
    }
}
