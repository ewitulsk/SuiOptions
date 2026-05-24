//! In-process Layer 1 harness.
//!
//! Spins up:
//!
//! - The real quoting service WS server bound to an ephemeral port.
//! - A fake indexer fanout WS server backed by [`indexer::Store::ingest`].
//!   Tests drive chain state by ingesting `ChainEvent`s directly into the
//!   store, bypassing the BCS / checkpoint worker / Postgres path.
//!
//! What this layer covers: RFQ routing, signature/balance/nonce validation,
//! reservation bookkeeping, sort order, deadline handling, multi-MM
//! dispatch.
//!
//! What it does NOT cover: PTB construction, on-chain reverts, the indexer
//! worker's BCS decode path, the real fanout's snapshot/resume protocol.
//! Those live in the Layer 2 chain harness (`#[cfg(feature = "chain")]`).
//!
//! ## Usage
//!
//! ```ignore
//! let h = Harness::start().await?;
//! // h.mm_account / h.mm_sk / h.bucket are the pre-seeded defaults.
//! // Add more MMs / buckets with h.add_mm(...), h.add_bucket(...).
//! let mut retail = h.connect_retail(RetailRole::Writer).await?;
//! let mut mm = h.connect_mm(h.mm_account, &h.mm_sk, vec![MmRole::TraderMm]).await?;
//! ```

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use shared::protocol_types::asset::AssetType;
use shared::protocol_types::events::{
    AccountCreated, AccountDeposit, BucketCreated, ChainEvent, WriteExecuted,
};
use shared::protocol_types::ids::{ObjectId, SuiAddress};
use shared::protocol_types::messages::{
    AuthResponsePayload, MmHelloPayload, MmQuotePayload, MmToService, RetailHelloPayload,
    RetailToService, RfqRequestPayload, ServiceToMm, ServiceToRetail,
};
use shared::protocol_types::quote::Quote;
use shared::protocol_types::sides::{MmRole, RetailRole, Side};

/// Harness: in-process indexer + quoting service + a pre-seeded default
/// MM keypair/account + a pre-seeded default bucket.
///
/// `app` is the live quoting-service state — assertions on reservations,
/// account balances, reputation, bucket views all go through here.
pub struct Harness {
    pub indexer_store: Arc<indexer::Store>,
    pub app: Arc<quoting_service::AppState>,
    pub quoting_addr: SocketAddr,

    pub mm_account: ObjectId,
    pub mm_sk: SigningKey,
    pub bucket: ObjectId,
    pub protocol_id: Vec<u8>,

    /// Monotonic counter for any new events the test ingests via
    /// [`Self::ingest`]. Pre-seeded events occupy 1..=3.
    next_sequence: AtomicU64,
}

impl Harness {
    pub async fn start() -> Result<Self> {
        let _ = tracing_subscriber::fmt::try_init();

        let store = Arc::new(indexer::Store::new(64));
        let mm_account = ObjectId::new([0x11; 32]);
        let mm_sk = SigningKey::generate(&mut rand::rngs::OsRng);
        let bucket = ObjectId::new([0x22; 32]);

        // Pre-seed: MM account with USDC balance, plus the default bucket.
        store.ingest(
            ChainEvent::AccountCreated(AccountCreated {
                account_id: mm_account,
                owner: SuiAddress::new([0x33; 32]),
                signing_scheme: shared::protocol_types::SigningScheme::Ed25519,
                signing_pubkey: mm_sk.verifying_key().to_bytes().to_vec(),
            }),
            1,
        );
        store.ingest(
            ChainEvent::AccountDeposit(AccountDeposit {
                account_id: mm_account,
                asset_type: AssetType::new("USDC"),
                amount: 1_000_000_000,
            }),
            2,
        );
        store.ingest(
            ChainEvent::BucketCreated(BucketCreated {
                bucket_id: bucket,
                asset_type: AssetType::new("BTC"),
                settlement_type: AssetType::new("USDC"),
                expiry_ms: 9_999_999_999_999,
                strike: 50_000_000,
            }),
            3,
        );

        // Indexer fanout on an ephemeral port.
        let indexer_listener = TcpListener::bind("127.0.0.1:0").await?;
        let indexer_addr = indexer_listener.local_addr()?;
        let store_for_indexer = Arc::clone(&store);
        tokio::spawn(async move {
            loop {
                let (socket, peer) = match indexer_listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let st = Arc::clone(&store_for_indexer);
                tokio::spawn(async move {
                    let ws = match tokio_tungstenite::accept_async(socket).await {
                        Ok(w) => w,
                        Err(_) => return,
                    };
                    drop(handle_indexer_connection(ws, peer, st).await);
                });
            }
        });

        // Quoting service.
        let cfg = Arc::new(quoting_service::Config {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            indexer_url: format!("ws://{}/", indexer_addr),
            rfq_window: Duration::from_millis(400),
            ping_interval: Duration::from_secs(10),
            protocol_id: b"test-protocol".to_vec(),
        });
        let app = Arc::new(quoting_service::AppState::new());

        // Subscribe the quoting service to the indexer.
        let app_clone = Arc::clone(&app);
        let url = cfg.indexer_url.clone();
        tokio::spawn(async move {
            let _ = quoting_service::indexer_client::run(url, app_clone).await;
        });

        // Quoting WS server, ephemeral port.
        let quoting_listener = TcpListener::bind("127.0.0.1:0").await?;
        let quoting_addr = quoting_listener.local_addr()?;
        let cfg_for_ws = Arc::clone(&cfg);
        let app_for_ws = Arc::clone(&app);
        tokio::spawn(async move {
            loop {
                let (socket, peer) = match quoting_listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let app = Arc::clone(&app_for_ws);
                let cfg = Arc::clone(&cfg_for_ws);
                tokio::spawn(async move {
                    let _ = handle_quoting_connection(socket, peer, app, cfg).await;
                });
            }
        });

        // Wait until the quoting service's indexer client has propagated
        // the MM account + bucket. Poll for up to ~2s.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if app.accounts.snapshot(&mm_account).is_some()
                && app.buckets.get(&bucket).is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(app.accounts.snapshot(&mm_account).is_some(), "mm account didn't propagate");
        assert!(app.buckets.get(&bucket).is_some(), "bucket didn't propagate");

        Ok(Self {
            indexer_store: store,
            app,
            quoting_addr,
            mm_account,
            mm_sk,
            bucket,
            protocol_id: cfg.protocol_id.clone(),
            next_sequence: AtomicU64::new(4),
        })
    }

    // -- Event ingestion helpers ------------------------------------------

    /// Ingest one event into the indexer store at the next monotonic
    /// sequence and wait briefly for the quoting service's indexer client
    /// to apply it. Convenience wrapper around `indexer_store.ingest`.
    pub async fn ingest(&self, event: ChainEvent) {
        let seq = self.next_sequence.fetch_add(1, Ordering::SeqCst);
        self.indexer_store.ingest(event, seq);
        // Give the indexer client task a tick to drain the broadcast.
        tokio::time::sleep(Duration::from_millis(40)).await;
    }

    /// Provision a fresh MM Account with `balance` of `asset`. Returns the
    /// account id and signing key the test should use to sign quotes /
    /// authenticate. Account id is derived from a per-test counter so
    /// concurrent additions don't collide.
    pub async fn add_mm(
        &self,
        asset: &AssetType,
        balance: u64,
    ) -> (ObjectId, SigningKey) {
        let sk = SigningKey::generate(&mut rand::rngs::OsRng);
        let salt = self.next_sequence.load(Ordering::SeqCst) as u8;
        let mut bytes = [0u8; 32];
        bytes[0] = 0xa0 | (salt & 0x0F);
        bytes[1] = salt;
        for (i, b) in bytes.iter_mut().enumerate().skip(2) {
            *b = i as u8;
        }
        let account_id = ObjectId::new(bytes);
        self.ingest(ChainEvent::AccountCreated(AccountCreated {
            account_id,
            owner: SuiAddress::ZERO,
            signing_scheme: shared::protocol_types::SigningScheme::Ed25519,
            signing_pubkey: sk.verifying_key().to_bytes().to_vec(),
        }))
        .await;
        if balance > 0 {
            self.ingest(ChainEvent::AccountDeposit(AccountDeposit {
                account_id,
                asset_type: asset.clone(),
                amount: balance,
            }))
            .await;
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while std::time::Instant::now() < deadline {
            if self.app.accounts.snapshot(&account_id).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        (account_id, sk)
    }

    /// Provision a fresh bucket with the given (asset, settlement, strike,
    /// expiry_ms). Returns the bucket id.
    pub async fn add_bucket(
        &self,
        asset: AssetType,
        settlement: AssetType,
        strike: u64,
        expiry_ms: u64,
    ) -> ObjectId {
        let salt = self.next_sequence.load(Ordering::SeqCst) as u8;
        let mut bytes = [0u8; 32];
        bytes[0] = 0xb0 | (salt & 0x0F);
        bytes[1] = salt;
        for (i, b) in bytes.iter_mut().enumerate().skip(2) {
            *b = (i ^ 0xff) as u8;
        }
        let bucket_id = ObjectId::new(bytes);
        self.ingest(ChainEvent::BucketCreated(BucketCreated {
            bucket_id,
            asset_type: asset,
            settlement_type: settlement,
            expiry_ms,
            strike,
        }))
        .await;
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while std::time::Instant::now() < deadline {
            if self.app.buckets.get(&bucket_id).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        bucket_id
    }

    /// Drive an indexer `WriteExecuted` event into the store. Useful for
    /// the "reservation released on execution" scenario without going
    /// through the full PTB path.
    pub async fn ingest_write_executed(
        &self,
        bucket_id: ObjectId,
        signer_account_id: ObjectId,
        write_amount: u64,
        premium: u64,
        nonce: u64,
        range_start: u128,
    ) {
        self.ingest(ChainEvent::WriteExecuted(WriteExecuted {
            bucket_id,
            signer_account_id,
            signer_token_recipient: SuiAddress::ZERO,
            executor: SuiAddress::ZERO,
            position_nft_recipient: SuiAddress::ZERO,
            call_token_recipient: SuiAddress::ZERO,
            write_amount,
            gross_premium: premium,
            fee: 0,
            net_premium: premium,
            range_start,
            range_end: range_start + write_amount as u128,
            nonce,
        }))
        .await;
    }

    // -- WS client conveniences ------------------------------------------

    pub async fn connect_retail(&self, role: RetailRole) -> Result<WsClient> {
        connect_retail(self.quoting_addr, role).await
    }

    pub async fn connect_mm(
        &self,
        account_id: ObjectId,
        sk: &SigningKey,
        roles: Vec<MmRole>,
    ) -> Result<WsClient> {
        connect_mm(self.quoting_addr, account_id, sk, roles).await
    }
}

// -- Connection helpers --------------------------------------------------

async fn handle_indexer_connection<S>(
    ws: WebSocketStream<S>,
    _peer: SocketAddr,
    store: Arc<indexer::Store>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // Re-implement the indexer's small Snapshot+stream protocol so the
    // crate's private connection handler isn't a dependency for tests.
    use shared::protocol_types::messages::{
        IndexerSnapshotPayload, IndexerStream, IndexerSubscribe,
    };
    let (mut sink, mut stream) = ws.split();
    let first = match stream.next().await {
        Some(Ok(Message::Text(t))) => t,
        Some(Ok(Message::Binary(b))) => String::from_utf8(b)?,
        _ => return Ok(()),
    };
    let sub: IndexerSubscribe = serde_json::from_str(&first)?;
    let after = match sub {
        IndexerSubscribe::Subscribe { after_sequence } => after_sequence,
    };
    let snap = store.snapshot_after(after);
    let frame = IndexerStream::Snapshot {
        payload: IndexerSnapshotPayload {
            latest_sequence: snap.latest_sequence,
            events: snap.events,
        },
    };
    sink.send(Message::Text(serde_json::to_string(&frame)?)).await?;

    let mut rx = store.subscribe();
    loop {
        tokio::select! {
            ev = rx.recv() => {
                match ev {
                    Ok(ev) => {
                        let frame = IndexerStream::Event { payload: ev };
                        if sink.send(Message::Text(serde_json::to_string(&frame)?)).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => continue,
                }
            }
        }
    }
    Ok(())
}

async fn handle_quoting_connection(
    socket: tokio::net::TcpStream,
    peer: SocketAddr,
    state: Arc<quoting_service::AppState>,
    cfg: Arc<quoting_service::Config>,
) -> Result<()> {
    let ws = tokio_tungstenite::accept_async(socket).await?;
    quoting_service::ws::accept_handshake(ws, peer, state, cfg).await
}

// -- Standalone WS client helpers (kept module-level so tests can use them
// without holding a `Harness` reference) ---------------------------------

pub type WsClient = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

pub async fn connect_retail(addr: SocketAddr, role: RetailRole) -> Result<WsClient> {
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://{}/", addr)).await?;
    let hello = RetailToService::Hello {
        payload: RetailHelloPayload {
            role,
            version: "test".into(),
        },
    };
    ws.send(Message::Text(serde_json::to_string(&hello)?)).await?;
    let ack = next_text(&mut ws).await?;
    let parsed: ServiceToRetail = serde_json::from_str(&ack)?;
    match parsed {
        ServiceToRetail::HelloAck { .. } => {}
        other => anyhow::bail!("expected HelloAck, got {:?}", other),
    }
    Ok(ws)
}

pub async fn connect_mm(
    addr: SocketAddr,
    account_id: ObjectId,
    sk: &SigningKey,
    roles: Vec<MmRole>,
) -> Result<WsClient> {
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://{}/", addr)).await?;
    let hello = MmToService::Hello {
        payload: MmHelloPayload {
            roles,
            account_id,
            signing_scheme: shared::protocol_types::SigningScheme::Ed25519,
            signing_pubkey: sk.verifying_key().to_bytes().to_vec(),
        },
    };
    ws.send(Message::Text(serde_json::to_string(&hello)?)).await?;

    let challenge_frame = next_text(&mut ws).await?;
    let parsed: ServiceToMm = serde_json::from_str(&challenge_frame)?;
    let challenge = match parsed {
        ServiceToMm::AuthChallenge { payload } => payload.challenge,
        other => anyhow::bail!("expected AuthChallenge, got {:?}", other),
    };
    let sig = sk.sign(&challenge).to_bytes().to_vec();
    let resp = MmToService::AuthResponse {
        payload: AuthResponsePayload { signature: sig },
    };
    ws.send(Message::Text(serde_json::to_string(&resp)?)).await?;
    let ack = next_text(&mut ws).await?;
    let parsed: ServiceToMm = serde_json::from_str(&ack)?;
    match parsed {
        ServiceToMm::AuthAck { .. } => {}
        other => anyhow::bail!("expected AuthAck, got {:?}", other),
    }
    Ok(ws)
}

pub async fn next_text(ws: &mut WsClient) -> Result<String> {
    loop {
        let frame = ws
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("ws closed"))??;
        match frame {
            Message::Text(t) => return Ok(t),
            Message::Binary(b) => return Ok(String::from_utf8(b)?),
            Message::Close(_) => anyhow::bail!("ws closed"),
            _ => continue,
        }
    }
}

/// Read the next text frame on `ws`, returning `Ok(None)` if no frame
/// arrives within `timeout` (instead of waiting forever). Used in
/// negative tests where we assert "nothing more arrives."
pub async fn next_text_timeout(
    ws: &mut WsClient,
    timeout: Duration,
) -> Result<Option<String>> {
    match tokio::time::timeout(timeout, next_text(ws)).await {
        Ok(r) => r.map(Some),
        Err(_) => Ok(None),
    }
}

pub async fn send_rfq(
    ws: &mut WsClient,
    request_id: &str,
    bucket: ObjectId,
    write_amount: u64,
    side: Side,
) -> Result<()> {
    let req = RetailToService::RFQRequest {
        request_id: request_id.into(),
        payload: RfqRequestPayload {
            bucket_id: bucket,
            write_amount,
            side,
        },
    };
    ws.send(Message::Text(serde_json::to_string(&req)?)).await?;
    Ok(())
}

/// Build an Ed25519-signed quote payload over canonical BCS bytes. Mirror
/// of what an honest MM bot produces.
pub fn build_signed_quote(
    sk: &SigningKey,
    protocol_id: Vec<u8>,
    mm_account: ObjectId,
    bucket: ObjectId,
    write_amount: u64,
    premium: u64,
    nonce: u64,
    valid_until_ms: u64,
) -> MmQuotePayload {
    let q = Quote {
        protocol_id,
        signer_account_id: mm_account,
        signer_token_recipient: SuiAddress::ZERO,
        bucket_id: bucket,
        write_amount,
        premium,
        valid_until_ms,
        nonce,
    };
    let sig = sk.sign(&q.to_bcs_bytes().unwrap()).to_bytes().to_vec();
    MmQuotePayload { quote: q, signature: sig }
}

pub fn now_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
