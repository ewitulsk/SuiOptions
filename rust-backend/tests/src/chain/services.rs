//! Boot the real indexer + real quoting-service in-process against the
//! localnet + ephemeral Postgres.
//!
//! Both services run on randomized localhost ports so several tests can
//! run sequentially without colliding (the suite is `--test-threads=1`
//! by convention, but the design doesn't depend on it).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use sui_data_ingestion_core::setup_single_workflow;
use sui_types::base_types::ObjectID;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use indexer::{establish_pool, fanout, run_migrations, ProtocolEventWorker, Repo, Store};
use quoting_service::{AppState, Config as QuotingConfig};
use shared::deployments::Deployments;

/// Booted indexer task + the handles tests need to assert against it.
pub struct IndexerHandle {
    pub fanout_addr: SocketAddr,
    pub store: Arc<Store>,
    /// Joinable handles. We don't `await` them in normal operation; the
    /// chain harness shuts down by `abort`ing on Drop.
    pub worker_task: Option<JoinHandle<anyhow::Result<()>>>,
    pub fanout_task: Option<JoinHandle<()>>,
}

impl Drop for IndexerHandle {
    fn drop(&mut self) {
        if let Some(h) = self.worker_task.take() {
            h.abort();
        }
        if let Some(h) = self.fanout_task.take() {
            h.abort();
        }
    }
}

/// Spawn the real indexer:
///
/// 1. Establish Postgres pool + apply migrations.
/// 2. Hydrate in-memory store from the (empty) DB.
/// 3. Subscribe the `ProtocolEventWorker` to the localnet's data-ingestion
///    directory (the path Sui dumped checkpoints to).
/// 4. Spawn the WS fanout on an ephemeral port.
pub async fn spawn_indexer(
    data_ingestion_dir: &std::path::Path,
    postgres_url: &str,
    deployments: &Deployments,
) -> Result<IndexerHandle> {
    let net = deployments.for_network("devnet")?;
    let package_id = net.package_info.package_id.clone();

    // Diesel: pool + migrations on a fresh DB. The migration set is
    // embedded in the indexer crate, same as production.
    let pool = Arc::new(
        establish_pool(postgres_url, 4).context("establish_pool against ephemeral postgres")?,
    );
    run_migrations(&pool).context("run_migrations on ephemeral postgres")?;
    let repo = Repo::new(Arc::clone(&pool));
    info!("indexer postgres ready");

    let store = Arc::new(Store::new(1024));

    // Empty DB → empty hydrate. Still call it so the boot path matches
    // the production binary's structure.
    let progress = repo.load_progress().context("load_progress")?;
    let recent_log = repo.recent_events(1024).context("recent_events")?;
    let views = repo.hydrate().context("hydrate views")?;
    let last_sequence = progress.as_ref().map(|p| p.last_sequence as u64).unwrap_or(0);
    store.hydrate(views, last_sequence, recent_log);

    // Checkpoint source: the localnet writes `.chk` files into
    // `data_ingestion_dir`. `sui_data_ingestion_core::setup_single_workflow`
    // accepts a local path here directly — same surface as the
    // production `remote_store_url` taking an HTTPS URL.
    let worker = ProtocolEventWorker::new(Arc::clone(&store), repo.clone(), &package_id);
    let source = data_ingestion_dir
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-utf8 data-ingestion-dir path"))?
        .to_string();
    let start = progress
        .as_ref()
        .map(|p| (p.last_checkpoint as u64) + 1)
        .unwrap_or(0);
    // `None::<ReaderOptions>` is required for type inference — the
    // production indexer infers it from a surrounding annotation, which
    // we don't have here.
    let (executor, _term) = setup_single_workflow(
        worker,
        source,
        start,
        1,
        None::<sui_data_ingestion_core::ReaderOptions>,
    )
    .await
    .context("setup_single_workflow")?;
    let worker_task: JoinHandle<anyhow::Result<()>> = tokio::spawn(async move {
        let _ = executor.await;
        Ok(())
    });

    // Fanout on an ephemeral port. We don't go through `fanout::serve`
    // because it binds internally; instead we bind here and pass a
    // pre-bound listener.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let fanout_addr = listener.local_addr()?;
    let store_for_fanout = Arc::clone(&store);
    let fanout_task = tokio::spawn(async move {
        // Re-implement the accept loop using the pre-bound listener so
        // we don't need to fork `fanout::serve` to take one.
        loop {
            let (socket, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "fanout accept failed");
                    break;
                }
            };
            let store = Arc::clone(&store_for_fanout);
            tokio::spawn(async move {
                // The crate's per-connection handler is private; instead
                // pull `fanout::serve`'s public façade and let it own
                // the WS handshake by handing it a TCP stream wrapped in
                // a one-shot listener. We can't easily do that from
                // here, so emulate the protocol inline — same shape as
                // the router-layer harness uses.
                if let Err(e) = handle_fanout_client(socket, peer, store).await {
                    tracing::debug!(?peer, error = %e, "fanout client done");
                }
            });
        }
    });
    info!(%fanout_addr, "indexer fanout listening (test mode)");

    // Suppress the unused-import warning when fanout isn't referenced.
    let _ = fanout::serve;

    Ok(IndexerHandle {
        fanout_addr,
        store,
        worker_task: Some(worker_task),
        fanout_task: Some(fanout_task),
    })
}

/// Booted quoting service.
pub struct QuotingHandle {
    pub bind_addr: SocketAddr,
    pub app: Arc<AppState>,
    pub config: Arc<QuotingConfig>,
    indexer_task: Option<JoinHandle<()>>,
    ws_task: Option<JoinHandle<()>>,
}

impl Drop for QuotingHandle {
    fn drop(&mut self) {
        if let Some(h) = self.indexer_task.take() {
            h.abort();
        }
        if let Some(h) = self.ws_task.take() {
            h.abort();
        }
    }
}

/// Spawn the quoting service. Connects to `indexer_addr` over WS and
/// binds its own retail/MM WS server on a fresh ephemeral port.
pub async fn spawn_quoting_service(
    indexer_addr: SocketAddr,
    deployments: &Deployments,
) -> Result<QuotingHandle> {
    let net = deployments.for_network("devnet")?;
    // The protocol_id the chain enforces is the AdminCap's id bytes
    // (`admin.move:20`); we forward that to the quoting service so
    // off-chain validation lines up with what the chain accepts.
    let protocol_id = net.protocol_id_bytes()?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let bind_addr = listener.local_addr()?;

    let config = Arc::new(QuotingConfig {
        bind_addr,
        indexer_url: format!("ws://{}/", indexer_addr),
        rfq_window: Duration::from_millis(800),
        ping_interval: Duration::from_secs(15),
        protocol_id,
    });
    let app = Arc::new(AppState::new());

    // Indexer client subscription task.
    let indexer_url = config.indexer_url.clone();
    let app_for_indexer = Arc::clone(&app);
    let indexer_task = tokio::spawn(async move {
        if let Err(e) = quoting_service::indexer_client::run(indexer_url, app_for_indexer).await {
            warn!(error = %e, "quoting indexer client exited");
        }
    });

    // WS accept loop.
    let cfg_for_ws = Arc::clone(&config);
    let app_for_ws = Arc::clone(&app);
    let ws_task = tokio::spawn(async move {
        loop {
            let (socket, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "quoting accept failed");
                    break;
                }
            };
            let app = Arc::clone(&app_for_ws);
            let cfg = Arc::clone(&cfg_for_ws);
            tokio::spawn(async move {
                let ws = match tokio_tungstenite::accept_async(socket).await {
                    Ok(w) => w,
                    Err(e) => {
                        tracing::debug!(error = %e, "ws handshake failed");
                        return;
                    }
                };
                let _ = quoting_service::ws::accept_handshake(ws, peer, app, cfg).await;
            });
        }
    });
    info!(%bind_addr, "quoting service listening (test mode)");

    Ok(QuotingHandle {
        bind_addr,
        app,
        config,
        indexer_task: Some(indexer_task),
        ws_task: Some(ws_task),
    })
}

/// Wait until both the indexer's store and the quoting service's app
/// state have observed a given Account. Used after on-chain account
/// creation to make sure events have propagated all the way through.
pub async fn wait_for_account_propagation(
    indexer: &IndexerHandle,
    quoting: &QuotingHandle,
    account_id_bytes: [u8; 32],
    timeout: Duration,
) -> Result<()> {
    let pt = shared::protocol_types::ids::ObjectId::new(account_id_bytes);
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if indexer.store.account(&pt).is_some()
            && quoting.app.accounts.snapshot(&pt).is_some()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!(
        "account {} not observed by indexer+quoting within {:?}",
        pt,
        timeout
    )
}

/// Same idea for a bucket.
pub async fn wait_for_bucket_propagation(
    indexer: &IndexerHandle,
    quoting: &QuotingHandle,
    bucket: ObjectID,
    timeout: Duration,
) -> Result<()> {
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(bucket.into_bytes().as_ref());
    let pt = shared::protocol_types::ids::ObjectId::new(bytes);
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if indexer.store.bucket(&pt).is_some()
            && quoting.app.buckets.get(&pt).is_some()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("bucket {} not observed within {:?}", pt, timeout)
}

// -- inline fanout handler ------------------------------------------------
//
// Re-implements indexer::fanout's handle_connection — kept small so we
// don't need to fork the crate to take a pre-bound listener.
async fn handle_fanout_client(
    socket: tokio::net::TcpStream,
    _peer: SocketAddr,
    store: Arc<Store>,
) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use shared::protocol_types::messages::{
        IndexerSnapshotPayload, IndexerStream, IndexerSubscribe,
    };
    use tokio_tungstenite::tungstenite::Message;

    let ws = tokio_tungstenite::accept_async(socket).await?;
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
    sink.send(Message::Text(serde_json::to_string(&frame)?))
        .await?;

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
