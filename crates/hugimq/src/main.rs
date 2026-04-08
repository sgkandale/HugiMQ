use bytes::{Buf, BufMut, BytesMut};
use dashmap::DashMap;
use futures::SinkExt;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::codec::{Decoder, Encoder, Framed};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Message types
const MSG_SUBSCRIBE: u8 = 0x01;
const MSG_PUBLISH: u8 = 0x02;
const MSG_SUBSCRIBE_DATA: u8 = 0x03;

#[derive(Debug, Clone)]
struct Message {
    payload: Arc<bytes::Bytes>,
}

/// Per-subscriber bounded channel capacity.
const SUBSCRIBER_CHANNEL_CAPACITY: usize = 1_048_576;

struct Topic {
    subscribers: tokio::sync::RwLock<Vec<mpsc::Sender<Message>>>,
    subscriber_count: AtomicUsize,
}

struct AppState {
    topics: DashMap<String, Arc<Topic>>,
}

// ─── Wire protocol ──────────────────────────────────────────────
//
// All frames: [2 bytes: total payload length (big-endian u16)]
//             [1 byte: message type]
//             [N bytes: type-specific payload]
//
// PUBLISH:    [1 byte: 0x02][2 bytes: topic_len][topic][message payload]
// SUBSCRIBE:  [1 byte: 0x01][2 bytes: topic_len][topic]
// SUBSCRIBE_DATA: [1 byte: 0x03][2 bytes: topic_len][topic][2 bytes: payload_len][message payload]
// ─────────────────────────────────────────────────────────────────

/// Codec for the wire protocol: [2 bytes len][type byte][payload]
struct FrameCodec;

impl Decoder for FrameCodec {
    type Item = (u8, Vec<u8>);
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // Need at least 3 bytes: 2 length + 1 type
        if src.len() < 3 {
            return Ok(None);
        }
        let total_len = u16::from_be_bytes([src[0], src[1]]) as usize;
        if src.len() < 2 + total_len {
            return Ok(None);
        }
        src.advance(2); // skip length field
        let msg_type = src.get_u8();
        let payload = src.split_to(total_len - 1).to_vec();
        Ok(Some((msg_type, payload)))
    }
}

impl Encoder<(u8, Vec<u8>)> for FrameCodec {
    type Error = io::Error;

    fn encode(&mut self, item: (u8, Vec<u8>), dst: &mut BytesMut) -> Result<(), Self::Error> {
        let total_len = 1 + item.1.len();
        dst.put_u16(total_len as u16);
        dst.put_u8(item.0);
        dst.put_slice(&item.1);
        Ok(())
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

// ─── Connection handler ──────────────────────────────────────────────

async fn handle_raw_tcp_connection(mut stream: tokio::net::TcpStream, state: Arc<AppState>) {
    // Read the first frame header to determine connection type
    // Format: [2 bytes total_len][1 byte type][...]
    let mut header = [0u8; 3];
    if stream.read_exact(&mut header).await.is_err() {
        return;
    }
    let total_len = u16::from_be_bytes([header[0], header[1]]) as usize;
    let msg_type = header[2];

    // Read the rest of the first frame's payload
    let remaining = total_len - 1; // subtract the type byte
    let mut first_payload = vec![0u8; remaining];
    if stream.read_exact(&mut first_payload).await.is_err() {
        return;
    }

    match msg_type {
        MSG_PUBLISH => {
            handle_publish_connection(stream, &first_payload, state).await;
        }
        MSG_SUBSCRIBE => {
            // Parse topic from first frame payload: [2 bytes topic_len][topic]
            if first_payload.len() < 2 {
                return;
            }
            let topic_len = u16::from_be_bytes([first_payload[0], first_payload[1]]) as usize;
            if first_payload.len() < 2 + topic_len {
                return;
            }
            let topic_name = String::from_utf8_lossy(&first_payload[2..2 + topic_len]).into_owned();
            handle_subscribe_connection(stream, topic_name, state).await;
        }
        _ => {
            tracing::warn!("Unknown message type: {}", msg_type);
        }
    }
}

async fn handle_publish_connection(
    stream: tokio::net::TcpStream,
    first_payload: &[u8],
    state: Arc<AppState>,
) {
    let mut cache: Vec<(String, Arc<Topic>)> = Vec::with_capacity(4);
    let mut stream = stream;

    // Process first message
    process_publish_payload(first_payload, &mut cache, &state).await;

    // Continue reading subsequent frames
    // Each frame: [2 bytes total_len][1 byte type][payload]
    let mut len_buf = [0u8; 2];
    loop {
        if stream.read_exact(&mut len_buf).await.is_err() {
            break;
        }
        let total_len = u16::from_be_bytes(len_buf) as usize;
        let mut frame = vec![0u8; total_len];
        if stream.read_exact(&mut frame).await.is_err() {
            break;
        }
        // frame[0] = type, frame[1..] = payload
        if frame[0] != MSG_PUBLISH {
            continue;
        }
        process_publish_payload(&frame[1..], &mut cache, &state).await;
    }
}

async fn process_publish_payload(
    payload: &[u8],
    cache: &mut Vec<(String, Arc<Topic>)>,
    state: &Arc<AppState>,
) {
    // Format: [2 bytes topic_len][topic][message payload]
    if payload.len() < 2 {
        return;
    }
    let topic_len = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    if payload.len() < 2 + topic_len {
        return;
    }
    let topic_name = String::from_utf8_lossy(&payload[2..2 + topic_len]);

    let topic = if let Some(found) = cache.iter().find(|(name, _)| name == topic_name.as_ref()) {
        found.1.clone()
    } else {
        let topic = get_or_create_topic(state, topic_name.as_ref());
        if cache.len() >= 4 {
            cache.remove(0);
        }
        cache.push((topic_name.into_owned(), topic.clone()));
        topic
    };

    let msg_payload = payload[2 + topic_len..].to_vec();
    let message = Message {
        payload: Arc::new(bytes::Bytes::from(msg_payload)),
    };

    let subs: Vec<mpsc::Sender<Message>> = {
        let subs = topic.subscribers.read().await;
        subs.iter().cloned().collect()
    };

    for sub in &subs {
        let _ = sub.try_send(message.clone());
    }
}

async fn handle_subscribe_connection(
    stream: tokio::net::TcpStream,
    topic_name: String,
    state: Arc<AppState>,
) {
    let topic = get_or_create_topic(&state, &topic_name);
    let (tx, mut rx) = mpsc::channel(SUBSCRIBER_CHANNEL_CAPACITY);

    {
        let mut subs = topic.subscribers.write().await;
        subs.push(tx);
        topic.subscriber_count.fetch_add(1, Ordering::Relaxed);
    }

    let mut framed = Framed::new(stream, FrameCodec);

    while let Some(msg) = rx.recv().await {
        // Format: [2 bytes topic_len][topic][2 bytes payload_len][payload]
        let topic_bytes = topic_name.as_bytes();
        let topic_len = topic_bytes.len() as u16;
        let payload_len = msg.payload.len() as u16;
        let mut frame_payload =
            Vec::with_capacity(2 + topic_bytes.len() + 2 + msg.payload.len());
        frame_payload.put_u16(topic_len);
        frame_payload.extend_from_slice(topic_bytes);
        frame_payload.put_u16(payload_len);
        frame_payload.extend_from_slice(&msg.payload);

        if framed.send((MSG_SUBSCRIBE_DATA, frame_payload)).await.is_err() {
            break;
        }
    }
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

    let addr: SocketAddr = "0.0.0.0:6379".parse().unwrap();
    tracing::info!("HugiMQ TCP server listening on {}", addr);

    let listener = TcpListener::bind(addr).await.unwrap();
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let _ = stream.set_nodelay(true);
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    handle_raw_tcp_connection(stream, state).await;
                });
            }
            Err(e) => {
                tracing::error!("Failed to accept connection: {}", e);
            }
        }
    }
}
