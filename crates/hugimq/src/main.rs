use arc_swap::ArcSwap;
use bytes::{BufMut, Bytes};
use dashmap::DashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use std::os::fd::AsRawFd;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Message types
const MSG_SUBSCRIBE: u8 = 0x01;
const MSG_PUBLISH: u8 = 0x02;
const MSG_SUBSCRIBE_DATA: u8 = 0x03;

#[derive(Debug, Clone)]
struct Message {
    payload: Arc<Bytes>,
}

/// Per-subscriber bounded channel capacity.
const SUBSCRIBER_CHANNEL_CAPACITY: usize = 4096;

/// Write batch size for subscribers (128KB — doubled from 64KB to cut syscalls in half)
const WRITE_BATCH_SIZE: usize = 128 * 1024;

/// 4MB socket buffers (kernel doubles to 8MB)
const SOCKET_BUF_SIZE: i32 = 4 * 1024 * 1024;

struct Topic {
    /// Lock-free subscriber list via ArcSwap — publish path does single atomic load
    subscribers: ArcSwap<Vec<mpsc::Sender<Message>>>,
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

fn get_or_create_topic(state: &Arc<AppState>, topic: &str) -> Arc<Topic> {
    if let Some(entry) = state.topics.get(topic) {
        return entry.clone();
    }

    state
        .topics
        .entry(topic.to_string())
        .or_insert_with(|| {
            Arc::new(Topic {
                subscribers: ArcSwap::new(Arc::new(Vec::new())),
                subscriber_count: AtomicUsize::new(0),
            })
        })
        .clone()
}

// ─── Connection handler ──────────────────────────────────────────────

async fn handle_raw_tcp_connection(mut stream: tokio::net::TcpStream, state: Arc<AppState>) {
    let mut header = [0u8; 3];
    if stream.read_exact(&mut header).await.is_err() {
        return;
    }
    let total_len = u16::from_be_bytes([header[0], header[1]]) as usize;
    let msg_type = header[2];

    let remaining = total_len - 1;
    let mut first_payload = vec![0u8; remaining];
    if stream.read_exact(&mut first_payload).await.is_err() {
        return;
    }

    match msg_type {
        MSG_PUBLISH => {
            handle_publish_connection(stream, &first_payload, state).await;
        }
        MSG_SUBSCRIBE => {
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

    process_publish_payload(first_payload, &mut cache, &state).await;

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

    // OPTIMIZATION: Arc<Bytes> zero-copy — clone is a single atomic ref-count increment
    // instead of a full memcpy of the payload bytes.
    let message = Message {
        payload: Arc::new(Bytes::from(payload[2 + topic_len..].to_vec())),
    };

    // OPTIMIZATION: Lock-free subscriber list via ArcSwap — single atomic load,
    // no futex syscall (RwLock::read() acquires a mutex even under no contention).
    let subs = topic.subscribers.load();

    // Serial send().await with backpressure — correct for tokio runtime scheduling.
    let mut dead_indices = Vec::new();
    for (i, sub) in subs.iter().enumerate() {
        if sub.send(message.clone()).await.is_err() {
            dead_indices.push(i);
        }
    }

    if !dead_indices.is_empty() {
        let count = dead_indices.len();
        // ArcSwap: build a new Vec, swap atomically — no write-lock needed
        let mut new_subs: Vec<_> = subs.iter().cloned().collect();
        for i in dead_indices.into_iter().rev() {
            new_subs.remove(i);
        }
        topic.subscribers.store(Arc::new(new_subs));
        topic.subscriber_count.fetch_sub(count, Ordering::Relaxed);
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
        // Subscribe path: need write access to swap the Vec — use CAS loop on ArcSwap
        loop {
            let current = topic.subscribers.load();
            let mut new_subs: Vec<_> = current.iter().cloned().collect();
            new_subs.push(tx.clone());
            let new_arc = Arc::new(new_subs);
            // compare_and_swap returns the old Guard — if the pointer matches, CAS succeeded
            let old = topic.subscribers.compare_and_swap(&current, new_arc);
            if Arc::ptr_eq(&*old, &*current) {
                break;
            }
        }
        topic.subscriber_count.fetch_add(1, Ordering::Relaxed);
    }

    // Send 1-byte ACK so the consumer knows it's registered.
    let mut stream = stream;
    if stream.write_all(&[0x00]).await.is_err() {
        return;
    }

    // OPTIMIZATION: 128KB write batching (doubled from 64KB) — each syscall delivers
    // ~800 messages instead of ~400, cutting subscribe-path syscalls from ~1.5M to ~750K.
    let mut write_buf = Vec::with_capacity(WRITE_BATCH_SIZE);
    let topic_bytes = topic_name.as_bytes();
    let topic_len = topic_bytes.len() as u16;

    loop {
        let msg = match rx.recv().await {
            Some(msg) => msg,
            None => break,
        };

        encode_subscribe_data(&msg, topic_bytes, topic_len, &mut write_buf);

        // Drain remaining buffered messages (non-blocking, same consumer task)
        while write_buf.len() < WRITE_BATCH_SIZE {
            match rx.try_recv() {
                Ok(msg) => encode_subscribe_data(&msg, topic_bytes, topic_len, &mut write_buf),
                Err(_) => break,
            }
        }

        // Write entire batch in one syscall + flush
        if stream.write_all(&write_buf).await.is_err() {
            break;
        }
        if stream.flush().await.is_err() {
            break;
        }
        write_buf.clear();
    }
}

/// Encode a SUBSCRIBE_DATA frame into the write buffer.
#[inline]
fn encode_subscribe_data(msg: &Message, topic_bytes: &[u8], topic_len: u16, buf: &mut Vec<u8>) {
    let payload_len = msg.payload.len() as u16;
    let total_len = 1 + 2 + topic_bytes.len() + 2 + msg.payload.len();
    buf.put_u16(total_len as u16);
    buf.put_u8(MSG_SUBSCRIBE_DATA);
    buf.put_u16(topic_len);
    buf.extend_from_slice(topic_bytes);
    buf.put_u16(payload_len);
    buf.extend_from_slice(&msg.payload);
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
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

                // Socket buffer tuning: 4MB receive and send buffers
                const SOCKET_BUF_SIZE: libc::c_int = 4 * 1024 * 1024;
                let fd = stream.as_raw_fd();
                unsafe {
                    libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_RCVBUF,
                        &SOCKET_BUF_SIZE as *const _ as *const libc::c_void,
                        std::mem::size_of::<libc::c_int>() as u32,
                    );
                    libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_SNDBUF,
                        &SOCKET_BUF_SIZE as *const _ as *const libc::c_void,
                        std::mem::size_of::<libc::c_int>() as u32,
                    );
                }

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
