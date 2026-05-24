//! Cross-crate integration tests. The actual scenarios live under `tests/`.
//!
//! Shared scaffolding for spinning up an in-process indexer + quoting
//! service, plus mock retail and MM WS clients, lives in this lib so each
//! integration test stays focused on the scenario it's exercising.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use protocol_types::asset::AssetType;
use protocol_types::events::{AccountCreated, AccountDeposit, BucketCreated, ChainEvent};
use protocol_types::ids::{ObjectId, SuiAddress};
use protocol_types::messages::{
    AuthResponsePayload, MmHelloPayload, MmQuotePayload, MmToService, RetailHelloPayload,
    RetailToService, RfqRequestPayload, ServiceToMm, ServiceToRetail,
};
use protocol_types::quote::Quote;
use protocol_types::sides::{MmRole, RetailRole, Side};

/// Test harness: in-process indexer + quoting service + a known MM
/// keypair/account, all pre-seeded with the test bucket.
pub struct Harness {
    pub indexer_store: Arc<indexer::Store>,
    pub quoting_addr: SocketAddr,
    pub mm_account: ObjectId,
    pub mm_sk: SigningKey,
    pub bucket: ObjectId,
    pub protocol_id: Vec<u8>,
}

impl Harness {
    pub async fn start() -> Result<Self> {
        let _ = tracing_subscriber::fmt::try_init();

        let store = Arc::new(indexer::Store::new(64));
        let mm_account = ObjectId::new([0x11; 32]);
        let mm_sk = SigningKey::generate(&mut rand::rngs::OsRng);
        let bucket = ObjectId::new([0x22; 32]);

        // Pre-seed: MM account with USDC balance, plus the bucket.
        store.ingest(
            ChainEvent::AccountCreated(AccountCreated {
                account_id: mm_account,
                owner: SuiAddress::new([0x33; 32]),
                signing_scheme: protocol_types::SigningScheme::Ed25519,
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

        // Bring up indexer fanout on an ephemeral port.
        let indexer_listener = TcpListener::bind("127.0.0.1:0").await?;
        let indexer_addr = indexer_listener.local_addr()?;
        let store_for_indexer = Arc::clone(&store);
        tokio::spawn(async move {
            // Reimplement the accept loop here so we can use the
            // already-bound listener (fanout::serve binds internally).
            loop {
                let (socket, peer) = match indexer_listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let st = Arc::clone(&store_for_indexer);
                tokio::spawn(async move {
                    // The fanout module's connection handler is private —
                    // call serve with our own listener via an inline copy
                    // would be ugly. Instead, run the WS handshake here
                    // and re-use the indexer's snapshot machinery.
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
            let _ = indexer_client::run(url, app_clone).await;
        });

        // Quoting WS server: rebind on ephemeral.
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

        // Wait until the quoting service's indexer client has caught up on
        // the MM account and bucket. Poll for up to ~2s.
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
            quoting_addr,
            mm_account,
            mm_sk,
            bucket,
            protocol_id: cfg.protocol_id.clone(),
        })
    }
}

// -- Connection helpers --------------------------------------------------

async fn handle_indexer_connection<S>(
    ws: WebSocketStream<S>,
    _peer: SocketAddr,
    _store: Arc<indexer::Store>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // Forward to the indexer's own connection handler logic by re-implementing
    // the small Snapshot+stream protocol. Kept tiny to avoid pulling the
    // indexer's internal `handle_connection`.
    use protocol_types::messages::{IndexerSnapshotPayload, IndexerStream, IndexerSubscribe};
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
    let snap = _store.snapshot_after(after);
    let frame = IndexerStream::Snapshot {
        payload: IndexerSnapshotPayload {
            latest_sequence: snap.latest_sequence,
            events: snap.events,
        },
    };
    sink.send(Message::Text(serde_json::to_string(&frame)?)).await?;

    let mut rx = _store.subscribe();
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
    // The crate's public `serve` binds its own listener; we want to use a
    // pre-bound one. The handler logic is reachable via accept_async →
    // dispatch indirectly: call serve doesn't expose the per-conn fn.
    // Inline the dispatch using public state to stay testable.
    let ws = tokio_tungstenite::accept_async(socket).await?;
    quoting_service::ws::accept_handshake(ws, peer, state, cfg).await
}

// -- Test client helpers -------------------------------------------------

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
            signing_scheme: protocol_types::SigningScheme::Ed25519,
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
