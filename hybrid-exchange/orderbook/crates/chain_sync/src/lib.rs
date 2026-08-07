//! Chain sync (spec §5.7): ingest all package events in checkpoint order
//! with a persisted cursor, mirror escrow balances / signer sets / salt
//! watermarks into the store, apply authoritative FillEvents (including
//! external open-orderbook fills the API never saw), and broadcast updates
//! so the service layer can prune books — the direct analogue of a 0x
//! relayer watching allowance changes.

use orderbook_core::{Digest, SuiAddress};
use orderbook_store::Store;
use orderbook_suirpc::{EventCursor, SuiEvent, SuiRpcClient};
use serde_json::Value;
use std::time::Duration;
use tokio::sync::broadcast;

pub const CURSOR_NAME: &str = "package-events";

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Rpc(#[from] orderbook_suirpc::RpcError),
    #[error(transparent)]
    Store(#[from] orderbook_store::StoreError),
}

/// Typed view of the exchange package's events (spec §4.8).
#[derive(Clone, Debug)]
pub enum ExchangeEvent {
    Fill {
        registry: SuiAddress,
        digest: Digest,
        maker: SuiAddress,
        taker: SuiAddress,
        base_amount: u64,
        quote_amount: u64,
        maker_fee: u64,
        taker_fee: u64,
        maker_sold_base: bool,
        filled_total: u64,
        timestamp_ms: u64,
    },
    Cancel {
        registry: SuiAddress,
        digest: Digest,
        maker: SuiAddress,
    },
    SaltWatermark {
        registry: SuiAddress,
        maker: SuiAddress,
        min_valid_salt: u64,
    },
    Deposit {
        manager: SuiAddress,
        token: String,
        amount: u64,
    },
    Withdraw {
        manager: SuiAddress,
        token: String,
        amount: u64,
    },
    SignerAdded {
        manager: SuiAddress,
        signer: SuiAddress,
    },
    SignerRemoved {
        manager: SuiAddress,
        signer: SuiAddress,
    },
}

fn get_str<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn get_addr(v: &Value, k: &str) -> Option<SuiAddress> {
    SuiAddress::parse(get_str(v, k)?).ok()
}

fn get_u64(v: &Value, k: &str) -> Option<u64> {
    match v.get(k)? {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

fn get_digest(v: &Value, k: &str) -> Option<Digest> {
    // event JSON renders vector<u8> as an array of numbers
    let arr = v.get(k)?.as_array()?;
    let bytes: Option<Vec<u8>> =
        arr.iter().map(|b| b.as_u64().map(|x| x as u8)).collect();
    let bytes = bytes?;
    let arr32: [u8; 32] = bytes.try_into().ok()?;
    Some(Digest(arr32))
}

fn get_token(v: &Value, k: &str) -> Option<String> {
    get_str(v, k).map(str::to_string)
}

/// Parse one raw event by its type suffix.
pub fn parse_event(ev: &SuiEvent) -> Option<ExchangeEvent> {
    let j = &ev.parsed_json;
    let suffix = ev.type_.rsplit("::").next()?;
    match suffix {
        "FillEvent" => Some(ExchangeEvent::Fill {
            registry: get_addr(j, "registry")?,
            digest: get_digest(j, "digest")?,
            maker: get_addr(j, "maker")?,
            taker: get_addr(j, "taker")?,
            base_amount: get_u64(j, "base_amount")?,
            quote_amount: get_u64(j, "quote_amount")?,
            maker_fee: get_u64(j, "maker_fee")?,
            taker_fee: get_u64(j, "taker_fee")?,
            maker_sold_base: j.get("maker_sold_base")?.as_bool()?,
            filled_total: get_u64(j, "taker_token_filled_total")?,
            timestamp_ms: get_u64(j, "timestamp_ms")?,
        }),
        "CancelEvent" => Some(ExchangeEvent::Cancel {
            registry: get_addr(j, "registry")?,
            digest: get_digest(j, "digest")?,
            maker: get_addr(j, "maker")?,
        }),
        "SaltWatermarkEvent" => Some(ExchangeEvent::SaltWatermark {
            registry: get_addr(j, "registry")?,
            maker: get_addr(j, "maker")?,
            min_valid_salt: get_u64(j, "min_valid_salt")?,
        }),
        "DepositEvent" => Some(ExchangeEvent::Deposit {
            manager: get_addr(j, "manager")?,
            token: get_token(j, "token")?,
            amount: get_u64(j, "amount")?,
        }),
        "WithdrawEvent" => Some(ExchangeEvent::Withdraw {
            manager: get_addr(j, "manager")?,
            token: get_token(j, "token")?,
            amount: get_u64(j, "amount")?,
        }),
        "SignerAddedEvent" => Some(ExchangeEvent::SignerAdded {
            manager: get_addr(j, "manager")?,
            signer: get_addr(j, "signer")?,
        }),
        "SignerRemovedEvent" => Some(ExchangeEvent::SignerRemoved {
            manager: get_addr(j, "manager")?,
            signer: get_addr(j, "signer")?,
        }),
        _ => None,
    }
}

pub struct EventIngestor {
    rpc: SuiRpcClient,
    store: Store,
    package: String,
    poll_interval: Duration,
    page_size: usize,
    tx: broadcast::Sender<ExchangeEvent>,
}

impl EventIngestor {
    pub fn new(rpc: SuiRpcClient, store: Store, package: String) -> Self {
        let (tx, _) = broadcast::channel(4096);
        EventIngestor {
            rpc,
            store,
            package,
            poll_interval: Duration::from_millis(500),
            page_size: 100,
            tx,
        }
    }

    /// Subscribe to the typed event feed (book pruning, WS fanout, …).
    pub fn subscribe(&self) -> broadcast::Receiver<ExchangeEvent> {
        self.tx.subscribe()
    }

    /// Run forever: poll from the persisted cursor, apply, publish, advance.
    pub async fn run(&self) -> Result<(), SyncError> {
        let mut cursor: Option<EventCursor> = self
            .store
            .load_cursor(CURSOR_NAME)
            .await?
            .map(|(tx_digest, event_seq)| EventCursor { tx_digest, event_seq });
        loop {
            match self.poll_once(&mut cursor).await {
                Ok(progressed) if progressed => {} // keep draining
                Ok(_) => tokio::time::sleep(self.poll_interval).await,
                Err(e) => {
                    tracing::warn!(error = %e, "chain_sync poll failed; backing off");
                    tokio::time::sleep(self.poll_interval * 4).await;
                }
            }
        }
    }

    /// One page: returns whether there may be more to drain immediately.
    pub async fn poll_once(
        &self,
        cursor: &mut Option<EventCursor>,
    ) -> Result<bool, SyncError> {
        let (events, next, has_next) = self
            .rpc
            .query_package_events(&self.package, cursor.as_ref(), self.page_size)
            .await?;
        for ev in &events {
            if let Some(typed) = parse_event(ev) {
                self.apply(ev, &typed).await?;
                let _ = self.tx.send(typed);
            }
            self.store
                .save_cursor(CURSOR_NAME, &ev.id.tx_digest, &ev.id.event_seq)
                .await?;
        }
        if let Some(n) = next {
            *cursor = Some(n);
        }
        Ok(has_next)
    }

    /// Mirror one event into the store. FillEvents are the single source of
    /// truth for fill state — including external fills the API never saw.
    async fn apply(&self, raw: &SuiEvent, ev: &ExchangeEvent) -> Result<(), SyncError> {
        match ev {
            ExchangeEvent::Fill {
                registry,
                digest,
                maker,
                taker,
                base_amount,
                quote_amount,
                maker_fee,
                taker_fee,
                maker_sold_base,
                filled_total,
                timestamp_ms,
            } => {
                let inserted = self
                    .store
                    .apply_fill(
                        &raw.id.tx_digest,
                        &raw.id.event_seq,
                        digest,
                        registry,
                        maker,
                        taker,
                        *base_amount,
                        *quote_amount,
                        *maker_fee,
                        *taker_fee,
                        *maker_sold_base,
                        *filled_total,
                        *timestamp_ms,
                    )
                    .await?;
                // Mirror the maker's escrow deltas when we know the manager
                // (the stored order pins it). Net of the maker's own fee.
                if inserted {
                    if let Some(stored) = self.store.get_order(digest).await? {
                        let o = &stored.signed.order;
                        let manager = o.maker_manager_id;
                        let (out_tok, out_amt, in_tok, in_amt) = if *maker_sold_base {
                            (
                                &o.maker_token,
                                *base_amount,
                                &o.taker_token,
                                quote_amount.saturating_sub(*maker_fee),
                            )
                        } else {
                            (
                                &o.maker_token,
                                *quote_amount,
                                &o.taker_token,
                                base_amount.saturating_sub(*maker_fee),
                            )
                        };
                        self.store
                            .apply_balance_delta(&manager, out_tok, -(out_amt as i64))
                            .await?;
                        self.store
                            .apply_balance_delta(&manager, in_tok, in_amt as i64)
                            .await?;
                    }
                }
            }
            ExchangeEvent::Cancel { digest, .. } => {
                self.store
                    .set_order_status(digest, orderbook_store::OrderStatus::Cancelled)
                    .await?;
            }
            ExchangeEvent::SaltWatermark { registry, maker, min_valid_salt } => {
                self.store.set_watermark(registry, maker, *min_valid_salt).await?;
            }
            ExchangeEvent::Deposit { manager, token, amount } => {
                self.store
                    .apply_balance_delta(manager, token, *amount as i64)
                    .await?;
            }
            ExchangeEvent::Withdraw { manager, token, amount } => {
                self.store
                    .apply_balance_delta(manager, token, -(*amount as i64))
                    .await?;
            }
            ExchangeEvent::SignerAdded { manager, signer } => {
                self.store.set_signer(manager, signer, true).await?;
            }
            ExchangeEvent::SignerRemoved { manager, signer } => {
                self.store.set_signer(manager, signer, false).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(type_: &str, json: Value) -> SuiEvent {
        serde_json::from_value(serde_json::json!({
            "id": { "txDigest": "abc", "eventSeq": "0" },
            "packageId": "0x1",
            "transactionModule": "settlement",
            "type": type_,
            "parsedJson": json,
        }))
        .unwrap()
    }

    #[test]
    fn parses_fill_event() {
        let digest_bytes: Vec<u8> = (0..32).collect();
        let e = ev(
            "0xabc::settlement::FillEvent",
            serde_json::json!({
                "registry": "0x5c",
                "digest": digest_bytes,
                "maker": "0xa1",
                "taker": "0xb1",
                "base_amount": "1000",
                "quote_amount": "2000",
                "maker_fee_bps": "10",
                "taker_fee_bps": "10",
                "maker_fee": "2",
                "taker_fee": "1",
                "maker_sold_base": true,
                "taker_token_filled_total": "2000",
                "timestamp_ms": "1754330000000",
            }),
        );
        match parse_event(&e).unwrap() {
            ExchangeEvent::Fill { base_amount, quote_amount, maker_sold_base, filled_total, .. } => {
                assert_eq!(base_amount, 1000);
                assert_eq!(quote_amount, 2000);
                assert!(maker_sold_base);
                assert_eq!(filled_total, 2000);
            }
            other => panic!("wrong parse: {other:?}"),
        }
    }

    #[test]
    fn parses_deposit_and_signer_events() {
        let e = ev(
            "0xabc::balance_manager::DepositEvent",
            serde_json::json!({
                "manager": "0x71",
                "owner": "0xa1",
                "token": "0x2::sui::SUI",
                "amount": "500",
            }),
        );
        assert!(matches!(
            parse_event(&e).unwrap(),
            ExchangeEvent::Deposit { amount: 500, .. }
        ));

        let e = ev(
            "0xabc::balance_manager::SignerRemovedEvent",
            serde_json::json!({ "manager": "0x71", "owner": "0xa1", "signer": "0xcc" }),
        );
        assert!(matches!(parse_event(&e).unwrap(), ExchangeEvent::SignerRemoved { .. }));
    }

    #[test]
    fn unknown_event_ignored() {
        let e = ev("0xabc::other::SomethingElse", serde_json::json!({}));
        assert!(parse_event(&e).is_none());
    }
}
