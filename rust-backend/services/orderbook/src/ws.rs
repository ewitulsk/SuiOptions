//! WebSocket fanout (spec §5.3): channels `book.{market}`, `trades.{market}`,
//! `orders.{addr}`. Clients send `{"op":"subscribe","channels":[…]}` /
//! `{"op":"unsubscribe","channels":[…]}`; the per-socket task filters the
//! global broadcast stream.

use crate::state::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Deserialize)]
struct ClientMsg {
    op: String,
    #[serde(default)]
    channels: Vec<String>,
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle(socket, state))
}

async fn handle(socket: WebSocket, state: Arc<AppState>) {
    let (mut sink, mut stream) = socket.split();
    let mut rx = state.ws_tx.subscribe();
    let mut subs: HashSet<String> = HashSet::new();

    loop {
        tokio::select! {
            msg = stream.next() => {
                let Some(Ok(msg)) = msg else { break };
                if let Message::Text(text) = msg {
                    if let Ok(m) = serde_json::from_str::<ClientMsg>(&text) {
                        match m.op.as_str() {
                            "subscribe" => {
                                for c in m.channels { subs.insert(c); }
                                let ack = serde_json::json!({"type":"subscribed","channels":subs.iter().collect::<Vec<_>>()});
                                if sink.send(Message::Text(ack.to_string())).await.is_err() { break; }
                            }
                            "unsubscribe" => {
                                for c in &m.channels { subs.remove(c); }
                            }
                            _ => {}
                        }
                    }
                }
            }
            ev = rx.recv() => {
                match ev {
                    Ok(msg) if subs.contains(&msg.channel) => {
                        let frame = serde_json::json!({
                            "channel": msg.channel,
                            "data": msg.payload,
                        });
                        if sink.send(Message::Text(frame.to_string())).await.is_err() { break; }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "ws client lagged; stream resumed");
                    }
                    Err(_) => break,
                }
            }
        }
    }
}
