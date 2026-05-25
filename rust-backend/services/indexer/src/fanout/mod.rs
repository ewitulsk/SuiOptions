//! WebSocket fanout server.
//!
//! Each subscriber:
//! 1. Connects (any path) and sends a single
//!    [`IndexerSubscribe::Subscribe { after_sequence }`].
//! 2. Receives a single [`IndexerStream::Snapshot`] containing every
//!    `IndexedEvent` strictly after that sequence (or, if none yet, an empty
//!    snapshot — the consumer still gets a starting `latest_sequence`).
//! 3. Receives a live stream of [`IndexerStream::Event`] frames, plus a
//!    periodic [`IndexerStream::Heartbeat`] so an idle stream is still
//!    detectable.
//!
//! On lag (the broadcast channel overflowed), the server logs the gap and
//! sends a fresh [`Snapshot`] re-anchored at the latest sequence — the
//! consumer will see the catch-up and continue.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::interval;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use protocol_types::events::IndexedEvent;
use protocol_types::messages::{
    IndexerSnapshotPayload, IndexerStream, IndexerSubscribe,
};

use crate::store::Store;

/// Bind a TCP listener at `addr` and accept WS connections forever.
pub async fn serve(
    addr: SocketAddr,
    store: Arc<Store>,
    heartbeat_interval: Duration,
) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "indexer fanout listening");
    loop {
        let (socket, peer) = listener.accept().await?;
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(socket, peer, store, heartbeat_interval).await {
                debug!(?peer, error = %e, "indexer fanout client closed");
            }
        });
    }
}

async fn handle_connection(
    socket: TcpStream,
    peer: SocketAddr,
    store: Arc<Store>,
    heartbeat_interval: Duration,
) -> Result<()> {
    let ws = tokio_tungstenite::accept_async(socket).await?;
    let (mut sink, mut stream) = ws.split();

    // Wait for the Subscribe frame.
    let frame = stream
        .next()
        .await
        .ok_or_else(|| anyhow!("client closed before subscribe"))??;
    let after_sequence = parse_subscribe(frame)?;
    debug!(?peer, after_sequence, "subscribe");

    // Snapshot catch-up.
    let snapshot = store.snapshot_after(after_sequence);
    let initial = IndexerStream::Snapshot {
        payload: IndexerSnapshotPayload {
            latest_sequence: snapshot.latest_sequence,
            events: snapshot.events,
        },
    };
    sink.send(Message::Text(serde_json::to_string(&initial)?))
        .await?;

    // Live stream.
    let mut rx = store.subscribe();
    let mut hb = interval(heartbeat_interval);
    hb.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume the immediate first tick that `interval` fires.
    hb.tick().await;
    let mut latest_seen = snapshot.latest_sequence;
    loop {
        tokio::select! {
            biased;
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => return Ok(()),
                    Some(Ok(_)) => continue, // ignore further frames from client
                    Some(Err(e)) => return Err(e.into()),
                }
            }
            recv = rx.recv() => {
                match recv {
                    Ok(ev) => {
                        latest_seen = ev.sequence;
                        send_event(&mut sink, &ev).await?;
                    }
                    Err(RecvError::Lagged(n)) => {
                        warn!(?peer, dropped = n, "subscriber lagged, re-snapshotting");
                        let snap = store.snapshot_after(latest_seen);
                        latest_seen = snap.latest_sequence;
                        let frame = IndexerStream::Snapshot {
                            payload: IndexerSnapshotPayload {
                                latest_sequence: snap.latest_sequence,
                                events: snap.events,
                            },
                        };
                        sink.send(Message::Text(serde_json::to_string(&frame)?)).await?;
                    }
                    Err(RecvError::Closed) => return Ok(()),
                }
            }
            _ = hb.tick() => {
                let frame = IndexerStream::Heartbeat { latest_sequence: latest_seen };
                sink.send(Message::Text(serde_json::to_string(&frame)?)).await?;
            }
        }
    }
}

async fn send_event<S>(sink: &mut S, ev: &IndexedEvent) -> Result<()>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    let frame = IndexerStream::Event { payload: ev.clone() };
    sink.send(Message::Text(serde_json::to_string(&frame)?))
        .await
        .map_err(|e| anyhow!(e))?;
    Ok(())
}

fn parse_subscribe(frame: Message) -> Result<u64> {
    let text = match frame {
        Message::Text(t) => t,
        Message::Binary(b) => String::from_utf8(b)?,
        other => return Err(anyhow!("unexpected first frame: {:?}", other)),
    };
    let req: IndexerSubscribe = serde_json::from_str(&text)?;
    match req {
        IndexerSubscribe::Subscribe { after_sequence } => Ok(after_sequence),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};
    use protocol_types::asset::AssetType;
    use protocol_types::events::{BucketCreated, ChainEvent};
    use protocol_types::ids::ObjectId;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    /// End-to-end roundtrip through the fanout server: snapshot then live.
    #[tokio::test]
    async fn snapshot_then_live_event_round_trip() {
        let store = Arc::new(Store::new(16));
        store.ingest(
            ChainEvent::BucketCreated(BucketCreated {
                bucket_id: ObjectId::new([0x01; 32]),
                asset_type: AssetType::new("BTC"),
                settlement_type: AssetType::new("USDC"),
                expiry_ms: 1,
                strike: 1,
                strike_scale: 0,
            }),
            0,
        );

        let addr: SocketAddr = (IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into();
        let listener = TcpListener::bind(addr).await.unwrap();
        let local = listener.local_addr().unwrap();
        let st = Arc::clone(&store);
        tokio::spawn(async move {
            let (socket, peer) = listener.accept().await.unwrap();
            handle_connection(socket, peer, st, Duration::from_secs(3600))
                .await
                .ok();
        });

        let url = format!("ws://{}/", local);
        let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        // Subscribe from the start.
        ws.send(WsMessage::Text(
            serde_json::to_string(&IndexerSubscribe::Subscribe { after_sequence: 0 }).unwrap(),
        ))
        .await
        .unwrap();

        // `after_sequence: 0` means "everything" — the pre-ingested
        // bucket should show up in the snapshot.
        let snap = ws.next().await.unwrap().unwrap().into_text().unwrap();
        let parsed: IndexerStream = serde_json::from_str(&snap).unwrap();
        match parsed {
            IndexerStream::Snapshot { payload } => {
                assert_eq!(payload.events.len(), 1);
                assert_eq!(payload.events[0].sequence, 1);
                assert_eq!(payload.latest_sequence, 1);
            }
            other => panic!("expected snapshot, got {:?}", other),
        }

        // Ingest a fresh event and expect it on the stream.
        store.ingest(
            ChainEvent::BucketCreated(BucketCreated {
                bucket_id: ObjectId::new([0x02; 32]),
                asset_type: AssetType::new("BTC"),
                settlement_type: AssetType::new("USDC"),
                expiry_ms: 1,
                strike: 1,
                strike_scale: 0,
            }),
            1,
        );
        let next = ws.next().await.unwrap().unwrap().into_text().unwrap();
        let parsed: IndexerStream = serde_json::from_str(&next).unwrap();
        match parsed {
            IndexerStream::Event { payload } => {
                assert_eq!(payload.sequence, 2);
            }
            other => panic!("expected event, got {:?}", other),
        }
    }
}
