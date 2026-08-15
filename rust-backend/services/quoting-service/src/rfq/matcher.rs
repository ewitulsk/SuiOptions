//! Deadline-bounded collection of MM responses.
//!
//! [`channel`] makes a pair: an [`MatcherInput`] (every connected MM gets a
//! clone of its `tx`; the orchestrator uses `expect()` to declare who it's
//! waiting for) and an [`MatcherOutput`] receiver that yields a final
//! `responses` vec once either the deadline elapses or all expected MMs
//! have answered.

use std::collections::HashSet;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, trace};

use protocol_types::ids::ObjectId;
use protocol_types::messages::MmQuotePayload;

pub use crate::state::MmResponse;

pub struct MatcherInput {
    pub tx: mpsc::Sender<MmResponse>,
    expected: HashSet<ObjectId>,
}

impl MatcherInput {
    pub fn expect(&mut self, mm: ObjectId) {
        self.expected.insert(mm);
    }

    /// Undo an earlier `expect` — used when the broadcast send to that MM
    /// fails, so the matcher doesn't keep waiting for a response that will
    /// never come.
    pub fn unexpect(&mut self, mm: ObjectId) {
        self.expected.remove(&mm);
    }
}

#[derive(Debug, Default)]
pub struct MatcherOutput {
    pub responses: Vec<(ObjectId, MmQuotePayload)>,
    pub declines: Vec<ObjectId>,
}

pub fn channel(capacity: usize) -> (MatcherInput, mpsc::Receiver<MmResponse>) {
    let (tx, rx) = mpsc::channel(capacity.max(1));
    (
        MatcherInput {
            tx,
            expected: HashSet::new(),
        },
        rx,
    )
}

/// Drain `rx` until either: every expected MM has answered, the sender
/// halves all drop (every MM disconnected mid-RFQ), or `window` elapses.
pub async fn collect_with_deadline(
    mut rx: mpsc::Receiver<MmResponse>,
    window: Duration,
) -> MatcherOutput {
    debug!(window_ms = window.as_millis() as u64, "starting rfq collection");
    let mut out = MatcherOutput::default();
    let _ = timeout(window, async {
        while let Some(r) = rx.recv().await {
            match r {
                MmResponse::Quote(mm, q) => {
                    trace!(mm = %mm, premium = q.quote.premium, "received mm quote");
                    out.responses.push((mm, q));
                }
                MmResponse::Decline(mm) => {
                    trace!(mm = %mm, "mm declined rfq");
                    out.declines.push(mm);
                }
                // Bulk-view responses are collected by `bulk_view.rs` on its
                // own channel; one arriving here means a stray late frame on a
                // reused request_id — ignore it.
                MmResponse::BulkView(mm, _) => {
                    trace!(mm = %mm, "ignoring bulk-view response on signed-rfq matcher");
                }
            }
        }
    })
    .await;
    debug!(quotes = out.responses.len(), declines = out.declines.len(), "rfq collection finished");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol_types::ids::SuiAddress;
    use protocol_types::quote::Quote;

    fn fake_payload(nonce: u64, premium: u64) -> MmQuotePayload {
        MmQuotePayload {
            quote: Quote {
                protocol_id: vec![],
                signer_id: ObjectId::ZERO,
                collateral_source: ObjectId::ZERO,
                release_package: SuiAddress::ZERO,
                release_module: "mm_collateral".into(),
                signer_token_recipient: SuiAddress::ZERO,
                spec: protocol_types::bucket_spec::BucketSpec::new(
                    "0x9::a::A", "0x9::b::B", 60_000, 1, 0, false,
                )
                .unwrap(),
                max_total_written: u128::MAX,
                write_amount: 1,
                premium,
                valid_until_ms: 999,
                nonce,
            },
            signature: vec![],
        }
    }

    #[tokio::test]
    async fn collects_until_senders_drop() {
        let (input, rx) = channel(4);
        let tx = input.tx.clone();
        let h = tokio::spawn(collect_with_deadline(rx, Duration::from_secs(10)));
        tx.send(MmResponse::Quote(ObjectId::new([0x01; 32]), fake_payload(1, 100)))
            .await
            .unwrap();
        tx.send(MmResponse::Decline(ObjectId::new([0x02; 32])))
            .await
            .unwrap();
        drop(tx);
        drop(input);
        let out = h.await.unwrap();
        assert_eq!(out.responses.len(), 1);
        assert_eq!(out.declines.len(), 1);
    }

    #[tokio::test]
    async fn returns_on_deadline_even_if_senders_open() {
        let (input, rx) = channel(4);
        let _input_keepalive = input; // keep the sender alive so the channel doesn't close
        let out = collect_with_deadline(rx, Duration::from_millis(50)).await;
        assert!(out.responses.is_empty());
        assert!(out.declines.is_empty());
    }
}
