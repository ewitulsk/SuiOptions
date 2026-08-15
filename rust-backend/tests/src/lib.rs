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

use protocol_types::BucketSpec;
use protocol_types::asset::AssetType;
use protocol_types::events::{BucketCreated, ChainEvent, IndexedEvent, SignerCreated};
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
    /// The economics the RFQ names. Under spec-bound quoting this — not
    /// `bucket` — is what travels and what the MM signs.
    pub spec: BucketSpec,
    pub protocol_id: Vec<u8>,
}

impl Harness {
    pub async fn start() -> Result<Self> {
        let _ = tracing_subscriber::fmt::try_init();

        let store = Arc::new(indexer::Store::new());
        let mm_account = ObjectId::new([0x11; 32]);
        let mm_sk = SigningKey::generate(&mut rand::rngs::OsRng);
        let bucket = ObjectId::new([0x22; 32]);
        // Minute-aligned: creation requires it, so a spec that isn't is
        // refused before it ever reaches an MM.
        let expiry_ms: u64 = 9_999_999_960_000;
        let spec = BucketSpec::new("BTC", "USDC", expiry_ms, 50_000_000, 0, false)
            .expect("harness spec");

        // Pre-seed: the MM's QuoteSigner registration, plus the bucket
        // (there is no balance state to seed — collateral custody lives
        // outside core). The store no longer keeps an event log (that was
        // fanout-only), so we capture the ingested events to serve the mock
        // `events(...)` query from.
        let seed_events = vec![
            store.ingest(
                ChainEvent::SignerCreated(SignerCreated {
                    signer_id: mm_account,
                    owner: SuiAddress::new([0x33; 32]),
                    signing_scheme: protocol_types::SigningScheme::Ed25519,
                    signing_pubkey: mm_sk.verifying_key().to_bytes().to_vec(),
                }),
                1,
            ),
            store.ingest(
                ChainEvent::BucketCreated(BucketCreated {
                    bucket_id: bucket,
                    asset_type: AssetType::new("BTC"),
                    settlement_type: AssetType::new("USDC"),
                    call_type: AssetType::new("0x9::call_0::CALL_0"),
                    expiry_ms,
                    strike: 50_000_000,
                    strike_scale: 0,
                }),
                3,
            ),
        ];

        // Stand up a mock indexer GraphQL server backed by the in-memory
        // store + captured events. JIT: the quoting service reads
        // account/bucket/events on demand from here.
        let graphql_addr =
            spawn_mock_indexer_graphql(Arc::clone(&store), Arc::new(seed_events)).await?;

        // Quoting service.
        let cfg = Arc::new(quoting_service::Config {
            max_bulk_view_specs: 120,
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            indexer_graphql_url: format!("http://{}/graphql", graphql_addr),
            rfq_window: Duration::from_millis(400),
            bulk_view_cache_ttl: Duration::from_secs(30),
            ping_interval: Duration::from_secs(10),
            protocol_id: b"test-protocol".to_vec(),
            token_info_url: String::new(),
            max_inflight_rfqs_per_session: 16,
            max_inflight_rfqs_global: 256,
        });
        let app = Arc::new(quoting_service::AppState::with_global_rfq_cap(
            cfg.max_inflight_rfqs_global,
            cfg.indexer_graphql_url.clone(),
        ));

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

        Ok(Self {
            indexer_store: store,
            quoting_addr,
            mm_account,
            mm_sk,
            bucket,
            spec,
            protocol_id: cfg.protocol_id.clone(),
        })
    }
}

// -- Connection helpers --------------------------------------------------

/// Minimal mock of the indexer's GraphQL API, backed by the in-memory
/// `Store`. Answers the three query shapes the quoting service issues —
/// `account(id)`, `bucket(id)`, and `events(...)` — dispatching on which root
/// field the query string mentions. Field encodings (camelCase keys, decimal
/// strings, hex pubkey) mirror the real resolvers in `indexer::graphql`.
async fn spawn_mock_indexer_graphql(
    store: Arc<indexer::Store>,
    events: Arc<Vec<IndexedEvent>>,
) -> Result<SocketAddr> {
    use axum::{extract::State, routing::post, Json, Router};
    use serde_json::{json, Value};

    #[derive(Clone)]
    struct MockState {
        store: Arc<indexer::Store>,
        events: Arc<Vec<IndexedEvent>>,
    }

    async fn handler(State(st): State<MockState>, Json(body): Json<Value>) -> Json<Value> {
        let query = body.get("query").and_then(|q| q.as_str()).unwrap_or("");
        let vars = body.get("variables").cloned().unwrap_or_else(|| json!({}));

        if query.contains("account(") {
            let id = vars.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let acct = ObjectId::from_hex(id).ok().and_then(|o| st.store.account(&o));
            let node = acct.map(|a| account_json(id, &a)).unwrap_or(Value::Null);
            return Json(json!({ "data": { "account": node } }));
        }
        // Filtered bucket list — how the quoting service resolves an RFQ spec
        // to a bucket (or to "no bucket yet, which is fine"). Must be checked
        // before the singular `bucket(` branch.
        if query.contains("buckets(") {
            let want = |k: &str| vars.get(k).and_then(|v| v.as_str()).map(str::to_string);
            let (asset, settlement) = (want("u"), want("s"));
            let expiry = vars.get("e").and_then(|v| v.as_str()).map(str::to_string);
            let nodes: Vec<Value> = st
                .store
                .all_buckets()
                .into_iter()
                .filter(|(_, b)| {
                    asset.as_deref().is_none_or(|a| b.asset_type.as_str() == a)
                        && settlement
                            .as_deref()
                            .is_none_or(|x| b.settlement_type.as_str() == x)
                        && expiry
                            .as_deref()
                            .is_none_or(|e| b.expiry_ms.to_string() == e)
                })
                .map(|(id, b)| bucket_json(&id.to_hex(), &b))
                .collect();
            return Json(json!({ "data": { "buckets": nodes } }));
        }
        if query.contains("bucket(") {
            let id = vars.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let bkt = ObjectId::from_hex(id).ok().and_then(|o| st.store.bucket(&o));
            let node = bkt.map(|b| bucket_json(id, &b)).unwrap_or(Value::Null);
            return Json(json!({ "data": { "bucket": node } }));
        }
        if query.contains("events(") {
            return Json(json!({ "data": { "events": events_json(&st.events, &vars) } }));
        }
        Json(json!({ "data": Value::Null }))
    }

    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(MockState { store, events });
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(addr)
}

fn account_json(id: &str, a: &indexer::AccountState) -> serde_json::Value {
    serde_json::json!({
        "accountId": id,
        "owner": a.owner.as_ref().map(|o| o.to_hex()),
        "signingScheme": a.signing_scheme.map(|s| s.as_u8() as i64),
        "signingPubkeyHex": hex::encode(&a.signing_pubkey),
    })
}

fn bucket_json(id: &str, b: &indexer::BucketState) -> serde_json::Value {
    serde_json::json!({
        "bucketId": id,
        "assetType": b.asset_type.as_str(),
        "settlementType": b.settlement_type.as_str(),
        "callType": b.call_type.as_str(),
        "strikeRaw": b.strike.to_string(),
        "strikeScale": b.strike_scale as i64,
        "expiryMs": b.expiry_ms.to_string(),
        "totalWrittenRaw": b.total_written.to_string(),
        "exerciseCursorRaw": b.exercise_cursor.to_string(),
        "cleaned": b.cleaned,
        "invalidated": b.invalidated,
    })
}

/// Serve the `events(...)` query from the captured seed events, applying the
/// `sequence_gt` (`after`) + `eventType` + `payloadContains.signer_id`
/// filters the quoting service uses for reputation fill reconciliation.
fn events_json(events: &[IndexedEvent], vars: &serde_json::Value) -> serde_json::Value {
    let after: u64 = vars
        .get("after")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let filter = vars.get("f");
    let want_types: Vec<String> = filter
        .and_then(|f| f.get("eventType"))
        .and_then(|t| serde_json::from_value(t.clone()).ok())
        .unwrap_or_default();
    let want_signer = filter
        .and_then(|f| f.get("payloadContains"))
        .and_then(|p| p.get("payload"))
        .and_then(|p| p.get("signer_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let nodes: Vec<serde_json::Value> = events
        .iter()
        .filter(|ev| ev.sequence > after)
        .filter(|ev| {
            let tag = indexer::db::models::event_type_tag(&ev.event);
            if !want_types.is_empty() && !want_types.iter().any(|t| t == tag) {
                return false;
            }
            match (&want_signer, &ev.event) {
                (Some(want), ChainEvent::WriteExecuted(w)) => w.signer_id.to_hex() == *want,
                (Some(_), _) => false,
                (None, _) => true,
            }
        })
        .map(|ev| {
            serde_json::json!({
                "sequence": ev.sequence.to_string(),
                "timestampMs": ev.timestamp_ms.to_string(),
                "payload": serde_json::to_value(&ev.event).unwrap(),
            })
        })
        .collect();
    serde_json::json!({ "nodes": nodes, "nextCursor": serde_json::Value::Null })
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
            bulk_view: false,
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

/// How long a test waits for a frame before declaring the flow broken.
///
/// Without a bound, a service that decides not to answer — an RFQ rejected
/// before the broadcast, say — hangs the suite instead of failing it, and a
/// hang tells you nothing about where it stopped.
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn next_text(ws: &mut WsClient) -> Result<String> {
    tokio::time::timeout(FRAME_TIMEOUT, next_text_inner(ws))
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {FRAME_TIMEOUT:?} waiting for a ws frame"))?
}

async fn next_text_inner(ws: &mut WsClient) -> Result<String> {
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
    spec: BucketSpec,
    write_amount: u64,
    side: Side,
) -> Result<()> {
    let req = RetailToService::RFQRequest {
        request_id: request_id.into(),
        payload: RfqRequestPayload {
            spec,
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
    spec: BucketSpec,
    write_amount: u64,
    premium: u64,
    nonce: u64,
    valid_until_ms: u64,
) -> MmQuotePayload {
    let q = Quote {
        protocol_id,
        signer_id: mm_account,
        collateral_source: ObjectId::new([0xc0; 32]),
        release_package: SuiAddress::new([0xd0; 32]),
        release_module: "mm_collateral".into(),
        signer_token_recipient: SuiAddress::ZERO,
        spec,
        max_total_written: u128::MAX,
        write_amount,
        premium,
        valid_until_ms,
        nonce,
    };
    let sig = sk.sign(&q.to_bcs_bytes().unwrap()).to_bytes().to_vec();
    MmQuotePayload { quote: q, signature: sig }
}
