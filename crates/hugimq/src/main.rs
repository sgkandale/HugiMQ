pub mod hugimq {
    tonic::include_proto!("hugimq");
}

use axum::{
    extract::{Query, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
    Router,
};
use axum::extract::ws::{Message as WsMessage, WebSocket};
use dashmap::DashMap;
use futures::StreamExt;
use futures_util::SinkExt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Shared message payload — reference-counted to avoid copying the underlying
/// byte buffer when fanning out to multiple subscribers.
#[derive(Debug, Clone)]
struct Message {
    payload: Arc<bytes::Bytes>,
}

/// Per-subscriber bounded channel capacity.
const SUBSCRIBER_CHANNEL_CAPACITY: usize = 4096;

/// A single topic with per-subscriber mpsc channels.
struct Topic {
    subscribers: tokio::sync::RwLock<Vec<mpsc::Sender<Message>>>,
    subscriber_count: AtomicUsize,
}

struct AppState {
    topics: DashMap<String, Arc<Topic>>,
}

#[derive(serde::Deserialize)]
struct PublishRequest {
    topic: String,
    payload: String,
}

#[derive(serde::Deserialize)]
struct SubscribeQuery {
    topic: String,
}

async fn ws_publish_handler(
    ws: WebSocketUpgrade,
    state: axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    let state = state.0;
    ws.on_upgrade(move |socket| handle_publish_socket(socket, state))
}

async fn handle_publish_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut tx, mut rx) = socket.split();
    let mut cache: Vec<(String, Arc<Topic>)> = Vec::with_capacity(4);

    while let Some(Ok(msg)) = rx.next().await {
        // Binary format: [2 bytes topic_len][topic bytes][payload bytes]
        let (topic_name, payload_bytes) = match &msg {
            WsMessage::Binary(data) => {
                if data.len() < 2 {
                    continue;
                }
                let topic_len = u16::from_be_bytes([data[0], data[1]]) as usize;
                if data.len() < 2 + topic_len {
                    continue;
                }
                let topic = String::from_utf8_lossy(&data[2..2 + topic_len]).into_owned();
                let payload = data[2 + topic_len..].to_vec();
                (topic, payload)
            }
            WsMessage::Text(text) => {
                let req: PublishRequest = match serde_json::from_str(text) {
                    Ok(req) => req,
                    Err(_) => continue,
                };
                (req.topic, req.payload.into_bytes())
            }
            _ => continue,
        };

        let topic = if let Some(found) = cache.iter().find(|(name, _)| name == &topic_name) {
            found.1.clone()
        } else {
            let topic = get_or_create_topic(&state, &topic_name);
            if cache.len() >= 4 {
                cache.remove(0);
            }
            cache.push((topic_name.clone(), topic.clone()));
            topic
        };

        let message = Message {
            payload: Arc::new(bytes::Bytes::from(payload_bytes)),
        };

        // Clone subscribers for async delivery — do NOT await on send here.
        // Spawning a delivery task prevents blocking the ACK response.
        let subs: Vec<mpsc::Sender<Message>> = {
            let subs = topic.subscribers.read().await;
            subs.iter().cloned().collect()
        };

        if !subs.is_empty() {
            let topic_for_cleanup = topic.clone();
            tokio::spawn(async move {
                let mut dead_indices = Vec::new();
                for (i, sub) in subs.iter().enumerate() {
                    if sub.send(message.clone()).await.is_err() {
                        dead_indices.push(i);
                    }
                }

                if !dead_indices.is_empty() {
                    let mut subs_mut = topic_for_cleanup.subscribers.write().await;
                    for i in dead_indices.into_iter().rev() {
                        subs_mut.swap_remove(i);
                    }
                    topic_for_cleanup.subscriber_count.store(subs_mut.len(), Ordering::Relaxed);
                }
            });
        }

        // Send ACK immediately — don't wait for delivery to complete
        let _ = tx.send(WsMessage::Binary(vec![0x01])).await;
    }
}

async fn ws_subscribe_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<SubscribeQuery>,
    state: axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    let state = state.0;
    ws.on_upgrade(move |socket| handle_subscribe_socket(socket, query.topic, state))
}

async fn handle_subscribe_socket(socket: WebSocket, topic_name: String, state: Arc<AppState>) {
    let topic = get_or_create_topic(&state, &topic_name);
    let (tx, rx) = mpsc::channel(SUBSCRIBER_CHANNEL_CAPACITY);

    {
        let mut subs = topic.subscribers.write().await;
        subs.push(tx);
        topic.subscriber_count.fetch_add(1, Ordering::Relaxed);
    }

    let (mut ws_tx, mut ws_rx) = socket.split();
    let mut rx = rx;

    let read_task = async {
        while let Some(Ok(_)) = ws_rx.next().await {}
    };

    let write_task = async {
        while let Some(msg) = rx.recv().await {
            // Binary: [2 bytes topic_len][topic bytes][payload bytes]
            let topic_bytes = topic_name.as_bytes();
            let topic_len = topic_bytes.len() as u16;
            let mut frame = Vec::with_capacity(2 + topic_bytes.len() + msg.payload.len());
            frame.extend_from_slice(&topic_len.to_be_bytes());
            frame.extend_from_slice(topic_bytes);
            frame.extend_from_slice(&msg.payload);
            if ws_tx.send(WsMessage::Binary(frame)).await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        _ = read_task => {}
        _ = write_task => {}
    }
}

fn get_or_create_topic(state: &Arc<AppState>, topic: &str) -> Arc<Topic> {
    if let Some(entry) = state.topics.get(topic) {
        return entry.clone();
    }

    state
        .topics
        .entry(topic.to_string())
        .or_insert_with(|| {
            Arc::new(Topic {
                subscribers: tokio::sync::RwLock::new(Vec::new()),
                subscriber_count: AtomicUsize::new(0),
            })
        })
        .clone()
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let state = Arc::new(AppState {
        topics: DashMap::new(),
    });

    let ws_addr: SocketAddr = "0.0.0.0:6379".parse().unwrap();
    tracing::info!("WebSocket server listening on {}", ws_addr);

    let app = Router::new()
        .route("/ws/publish", get(ws_publish_handler))
        .route("/ws/subscribe", get(ws_subscribe_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(ws_addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
