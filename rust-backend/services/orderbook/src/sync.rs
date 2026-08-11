//! Chain sync (spec §5.7) over the workspace's GraphQL event reader
//! (`sui_tx::EventClient` — gRPC has no events query): ingest the exchange
//! package's events per emitting module with persisted opaque cursors,
//! mirror escrow balances / signer sets / salt watermarks into Postgres,
//! apply authoritative FillEvents (including external open-orderbook fills
//! the API never saw), and broadcast typed updates so the service layer can
//! prune books — the direct analogue of a 0x relayer watching allowance
//! changes.
//!
//! The three module streams (`settlement`, `balance_manager`, `registry`)
//! advance independent cursors, so cross-module ordering is not guaranteed;
//! every mirror write is therefore idempotent/clamped (fills keyed by event
//! id, balances clamped at zero, watermarks monotonic).

use std::time::Duration;

use exchange_types::{Digest, SuiAddress};
use serde_json::Value;
use sui_tx::events::{ChainEvent, EventClient};
use tokio::sync::broadcast;

use crate::db::{Db, NewFill, StoreError, VaultManagerRow};

/// The exchange modules that emit events.
const MODULES: [&str; 3] = ["settlement", "balance_manager", "registry"];

/// The exchange_adapter module (a different package: the trading-vault
/// adapter, SO-372). Its CustodyCreated events are the manager-mode map —
/// which BMs belong to a vault, and whether they are direct (identity-only)
/// managers whose escrow is the vault's free balances.
const ADAPTER_MODULE: &str = "exchange_adapter";

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("event query: {0}")]
    Events(String),
    #[error(transparent)]
    Store(#[from] StoreError),
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
    /// exchange_adapter::CustodyCreated (SO-372): a trading vault custodied
    /// a BalanceManager. `direct` managers settle through the adapter's
    /// fill/match entries against vault free balances.
    VaultCustody {
        vault: SuiAddress,
        custody: SuiAddress,
        manager: SuiAddress,
        direct: bool,
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

/// `vector<u8>` in event JSON: array of numbers (GraphQL MoveValue json),
/// with a base64-string fallback for robustness.
fn get_digest(v: &Value, k: &str) -> Option<Digest> {
    let bytes: Vec<u8> = match v.get(k)? {
        Value::Array(arr) => arr
            .iter()
            .map(|b| b.as_u64().map(|x| x as u8))
            .collect::<Option<Vec<u8>>>()?,
        Value::String(s) => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.decode(s).ok()?
        }
        _ => return None,
    };
    let arr32: [u8; 32] = bytes.try_into().ok()?;
    Some(Digest(arr32))
}

/// Parse one raw event by its struct name.
pub fn parse_event(ev: &ChainEvent) -> Option<ExchangeEvent> {
    let j = &ev.parsed_json;
    match ev.type_.name.as_str() {
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
            token: get_str(j, "token")?.to_owned(),
            amount: get_u64(j, "amount")?,
        }),
        "WithdrawEvent" => Some(ExchangeEvent::Withdraw {
            manager: get_addr(j, "manager")?,
            token: get_str(j, "token")?.to_owned(),
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
        // exchange_adapter (SO-372). VaultQuoteFilled is deliberately not
        // parsed: the exchange emits its normal FillEvent alongside, which
        // is already the fill's source of truth here.
        "CustodyCreated" => Some(ExchangeEvent::VaultCustody {
            vault: get_addr(j, "vault_id")?,
            custody: get_addr(j, "custody_id")?,
            manager: get_addr(j, "balance_manager_id")?,
            direct: j.get("direct")?.as_bool()?,
        }),
        _ => None,
    }
}

pub struct EventIngestor {
    events: EventClient,
    db: Db,
    /// Exchange package id, `0x`-hex.
    package: String,
    /// exchange_adapter package id (SO-372), `0x`-hex. `None` on
    /// deployments without the trading-vault adapter — the adapter stream
    /// simply doesn't subscribe.
    adapter_package: Option<String>,
    poll_interval: Duration,
    page_size: u32,
    tx: broadcast::Sender<ExchangeEvent>,
}

impl EventIngestor {
    pub fn new(
        events: EventClient,
        db: Db,
        package: String,
        adapter_package: Option<String>,
    ) -> Self {
        let (tx, _) = broadcast::channel(4096);
        EventIngestor {
            events,
            db,
            package,
            adapter_package,
            poll_interval: Duration::from_millis(500),
            // The GraphQL events query caps page size at 50 (observed live:
            // "Page size is too large: 100 > 50" — every poll failed).
            page_size: 50,
            tx,
        }
    }

    /// The module streams to poll: the exchange package's three modules
    /// plus, when deployed, the exchange_adapter's. Each advances its own
    /// persisted cursor.
    fn streams(&self) -> Vec<(String, String)> {
        let mut streams: Vec<(String, String)> = MODULES
            .iter()
            .map(|m| (format!("exchange-events:{m}"), format!("{}::{m}", self.package)))
            .collect();
        if let Some(pkg) = &self.adapter_package {
            streams.push((
                format!("exchange-adapter-events:{ADAPTER_MODULE}"),
                format!("{pkg}::{ADAPTER_MODULE}"),
            ));
        }
        streams
    }

    /// Subscribe to the typed event feed (book pruning, WS fanout, …).
    pub fn subscribe(&self) -> broadcast::Receiver<ExchangeEvent> {
        self.tx.subscribe()
    }

    /// Run forever: poll each module stream from its persisted cursor,
    /// apply, publish, advance.
    pub async fn run(&self) {
        let streams = self.streams();
        loop {
            let mut drained = true;
            for (cursor_name, module) in &streams {
                match self.poll_module(cursor_name, module).await {
                    Ok(has_more) => drained &= !has_more,
                    Err(e) => {
                        tracing::warn!(module, error = %e, "event poll failed; backing off");
                        tokio::time::sleep(self.poll_interval * 4).await;
                    }
                }
            }
            if drained {
                tokio::time::sleep(self.poll_interval).await;
            }
        }
    }

    /// One page of one module's stream; returns whether more pages remain.
    pub async fn poll_module(
        &self,
        cursor_name: &str,
        module: &str,
    ) -> Result<bool, SyncError> {
        let cursor = self.db.load_cursor(cursor_name).await?;
        let page = self
            .events
            .query_by_module(module, cursor.as_deref(), self.page_size, false)
            .await
            .map_err(|e| SyncError::Events(format!("{e:#}")))?;
        for ev in &page.data {
            if let Some(typed) = parse_event(ev) {
                self.apply(ev, &typed).await?;
                let _ = self.tx.send(typed);
            }
        }
        if let Some(next) = &page.next_cursor {
            self.db.save_cursor(cursor_name, next).await?;
        }
        Ok(page.has_next_page)
    }

    /// Mirror one event into the store. FillEvents are the single source of
    /// truth for fill state — including external fills the API never saw.
    async fn apply(&self, raw: &ChainEvent, ev: &ExchangeEvent) -> Result<(), SyncError> {
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
                let fill = NewFill {
                    tx_digest: raw.tx_digest.to_string(),
                    event_seq: raw.event_seq as i64,
                    digest: digest.to_hex(),
                    registry_id: registry.to_hex(),
                    maker: maker.to_hex(),
                    taker: taker.to_hex(),
                    base_amount: *base_amount as i64,
                    quote_amount: *quote_amount as i64,
                    maker_fee: *maker_fee as i64,
                    taker_fee: *taker_fee as i64,
                    maker_sold_base: *maker_sold_base,
                    filled_total: *filled_total as i64,
                    timestamp_ms: *timestamp_ms as i64,
                };
                let inserted = self.db.apply_fill(fill).await?;
                // Mirror the maker's escrow deltas when we know the manager
                // (the stored order pins it). Net of the maker's own fee.
                // Direct vault managers hold nothing — their escrow is the
                // vault's free balances, which the mirror does not track.
                if inserted {
                    if let Some(stored) = self.db.get_order(digest).await? {
                        let o = &stored.signed.order;
                        let manager = o.maker_manager_id;
                        if self
                            .db
                            .vault_manager(&manager)
                            .await?
                            .is_some_and(|v| v.direct)
                        {
                            return Ok(());
                        }
                        let (out_amt, in_amt) = if *maker_sold_base {
                            (*base_amount, quote_amount.saturating_sub(*maker_fee))
                        } else {
                            (*quote_amount, base_amount.saturating_sub(*maker_fee))
                        };
                        self.db
                            .apply_balance_delta(&manager, &o.maker_token, -(out_amt as i64))
                            .await?;
                        self.db
                            .apply_balance_delta(&manager, &o.taker_token, in_amt as i64)
                            .await?;
                    }
                }
            }
            ExchangeEvent::Cancel { digest, .. } => {
                self.db
                    .set_order_status(digest, crate::db::OrderStatus::Cancelled)
                    .await?;
            }
            ExchangeEvent::SaltWatermark { registry, maker, min_valid_salt } => {
                self.db.set_watermark(registry, maker, *min_valid_salt).await?;
            }
            ExchangeEvent::Deposit { manager, token, amount } => {
                self.db
                    .apply_balance_delta(manager, token, *amount as i64)
                    .await?;
            }
            ExchangeEvent::Withdraw { manager, token, amount } => {
                self.db
                    .apply_balance_delta(manager, token, -(*amount as i64))
                    .await?;
            }
            ExchangeEvent::SignerAdded { manager, signer } => {
                self.db.set_signer(manager, signer, true).await?;
            }
            ExchangeEvent::SignerRemoved { manager, signer } => {
                self.db.set_signer(manager, signer, false).await?;
            }
            ExchangeEvent::VaultCustody { vault, custody, manager, direct } => {
                self.db
                    .upsert_vault_manager(VaultManagerRow {
                        manager_id: manager.to_hex(),
                        vault_id: vault.to_hex(),
                        custody_id: custody.to_hex(),
                        direct: *direct,
                    })
                    .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use move_core_types::language_storage::StructTag;
    use std::str::FromStr;

    fn ev(type_: &str, json: Value) -> ChainEvent {
        ChainEvent {
            tx_digest: sui_types::digests::TransactionDigest::default(),
            event_seq: 0,
            type_: StructTag::from_str(type_).unwrap(),
            parsed_json: json,
            sender: sui_types::base_types::SuiAddress::ZERO,
            timestamp_ms: None,
            transaction_module: "settlement".into(),
        }
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
    fn parses_adapter_custody_created() {
        let e = ev(
            "0xea::exchange_adapter::CustodyCreated",
            serde_json::json!({
                "vault_id": "0xf0",
                "custody_id": "0xc1",
                "balance_manager_id": "0xb2",
                "direct": true,
            }),
        );
        match parse_event(&e).unwrap() {
            ExchangeEvent::VaultCustody { direct, manager, .. } => {
                assert!(direct);
                assert_eq!(manager, SuiAddress::parse("0xb2").unwrap());
            }
            other => panic!("wrong parse: {other:?}"),
        }
    }

    #[test]
    fn unknown_event_ignored() {
        let e = ev("0xabc::other::SomethingElse", serde_json::json!({}));
        assert!(parse_event(&e).is_none());
    }
}
