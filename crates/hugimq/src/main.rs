use arc_swap::ArcSwap;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::collections::HashMap;
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

/// Per-subscriber bounded channel capacity (in batches).
const SUBSCRIBER_CHANNEL_CAPACITY: usize = 4096;

/// Read buffer size (64KB)
const READ_BUF_SIZE: usize = 64 * 1024;

#[repr(align(64))]
struct Topic {
    /// Lock-free subscriber list via ArcSwap — publish path does single atomic load
    /// Now sends pre-encoded Bytes batches for maximum efficiency.
    subscribers: ArcSwap<Vec<mpsc::Sender<Arc<Bytes>>>>,
    subscriber_count: AtomicUsize,
    name: String,
}

struct AppState {
    /// Lock-free topic registry — read path is 100% lock-free
    topics: ArcSwap<HashMap<String, Arc<Topic>>>,
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
    // Fast path: topic already exists (lock-free read)
    if let Some(entry) = state.topics.load().get(topic) {
        return entry.clone();
    }

    // Slow path: topic creation (CAS loop)
    loop {
        let current = state.topics.load();
        if let Some(entry) = current.get(topic) {
            return entry.clone();
        }

        let mut new_map: HashMap<String, Arc<Topic>> = current.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let new_topic = Arc::new(Topic {
            subscribers: ArcSwap::new(Arc::new(Vec::new())),
            subscriber_count: AtomicUsize::new(0),
            name: topic.to_string(),
        });
        new_map.insert(topic.to_string(), new_topic.clone());

        let old = state.topics.compare_and_swap(&current, Arc::new(new_map));
        if Arc::ptr_eq(&*old, &*current) {
            return new_topic;
        }
    }
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
    let mut pending_batch: Vec<(Arc<Topic>, Message)> = Vec::with_capacity(512);
    const BATCH_SIZE: usize = 512;
    let mut fanout_tasks = tokio::task::JoinSet::new();

    if let Some((topic, msg)) = parse_and_create_message(Bytes::copy_from_slice(first_payload), &mut cache, &state) {
        pending_batch.push((topic, msg));
    }

    if pending_batch.len() >= BATCH_SIZE {
        flush_message_batch(&mut pending_batch, &mut fanout_tasks).await;
    }

    let mut read_buf = BytesMut::with_capacity(READ_BUF_SIZE);
    loop {
        let n = stream.read_buf(&mut read_buf).await.unwrap_or(0);
        
        while read_buf.len() >= 3 {
            let total_len = u16::from_be_bytes([read_buf[0], read_buf[1]]) as usize;
            let frame_len = 2 + total_len;

            if read_buf.len() < frame_len {
                break;
            }

            let mut frame = read_buf.split_to(frame_len);
            frame.advance(2);

            if frame[0] == MSG_PUBLISH {
                let msg_frame = frame.split_off(1);
                if let Some((topic, msg)) = parse_and_create_message(msg_frame.freeze(), &mut cache, &state) {
                    pending_batch.push((topic, msg));
                    if pending_batch.len() >= BATCH_SIZE {
                        flush_message_batch(&mut pending_batch, &mut fanout_tasks).await;
                    }
                }
            }
        }

        if n == 0 {
            break;
        }
    }

    if !pending_batch.is_empty() {
        flush_message_batch(&mut pending_batch, &mut fanout_tasks).await;
    }

    // Crucially, we MUST await all spawned fan-out tasks to ensure the messages
    // reach the subscriber channels before this producer handler exits.
    while let Some(res) = fanout_tasks.join_next().await {
        if let Err(e) = res {
            tracing::error!("Fan-out task panicked: {}", e);
        }
    }
}

fn parse_and_create_message(
    payload: Bytes,
    cache: &mut Vec<(String, Arc<Topic>)>,
    state: &Arc<AppState>,
) -> Option<(Arc<Topic>, Message)> {
    if payload.len() < 2 {
        return None;
    }
    let topic_len = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    if payload.len() < 2 + topic_len {
        return None;
    }
    let topic_name = String::from_utf8_lossy(&payload[2..2 + topic_len]).into_owned();

    let topic = if let Some(found) = cache.iter().find(|(name, _)| name == &topic_name) {
        found.1.clone()
    } else {
        let topic = get_or_create_topic(state, &topic_name);
        if cache.len() >= 4 {
            cache.remove(0);
        }
        cache.push((topic_name.clone(), topic.clone()));
        topic
    };

    let payload_data = payload.slice(2 + topic_len..);
    let message = Message {
        payload: Arc::new(payload_data),
    };

    Some((topic, message))
}

async fn flush_message_batch(batch: &mut Vec<(Arc<Topic>, Message)>, fanout_tasks: &mut tokio::task::JoinSet<()>) {
    if batch.is_empty() {
        return;
    }

    let mut groups: Vec<(Arc<Topic>, Vec<Message>)> = Vec::new();
    for (topic, msg) in batch.drain(..) {
        // Fast path: check if it belongs to the last group (common in 1-topic benchmarks)
        if let Some(last) = groups.last_mut() {
            if Arc::ptr_eq(&last.0, &topic) {
                last.1.push(msg);
                continue;
            }
        }
        
        // Slow path: find existing group
        if let Some(existing) = groups.iter_mut().find(|(t, _)| Arc::ptr_eq(&t, &topic)) {
            existing.1.push(msg);
        } else {
            groups.push((topic, vec![msg]));
        }
    }

    for (topic, messages) in groups {
        let subs = topic.subscribers.load().clone();
        let topic_clone = topic.clone();
        
        fanout_tasks.spawn(async move {
            // Encode ONCE per topic
            let mut encoded_batch = BytesMut::with_capacity(messages.len() * 150);
            let topic_bytes = topic_clone.name.as_bytes();
            let topic_len = topic_bytes.len() as u16;

            for msg in messages {
                let payload_len = msg.payload.len() as u16;
                let total_len = 1 + 2 + topic_bytes.len() + 2 + msg.payload.len();

                encoded_batch.put_u16(total_len as u16);
                encoded_batch.put_u8(MSG_SUBSCRIBE_DATA);
                encoded_batch.put_u16(topic_len);
                encoded_batch.put_slice(topic_bytes);
                encoded_batch.put_u16(payload_len);
                encoded_batch.put_slice(&msg.payload);
            }
            
            let final_batch = Arc::new(encoded_batch.freeze());
            let dead_count = broadcast_to_subscribers(&subs, final_batch).await;
            
            if dead_count > 0 {
                remove_dead_subscribers(&topic_clone, &subs, dead_count).await;
            }
        });
    }
}

async fn broadcast_to_subscribers(subs: &Vec<mpsc::Sender<Arc<Bytes>>>, batch: Arc<Bytes>) -> usize {
    let mut dead_count = 0;
    for sub in subs.iter() {
        if sub.send(batch.clone()).await.is_err() {
            dead_count += 1;
        }
    }
    dead_count
}

async fn remove_dead_subscribers(topic: &Arc<Topic>, subs_snapshot: &Vec<mpsc::Sender<Arc<Bytes>>>, dead_count: usize) {
    if dead_count == 0 { return; }
    
    // Use a CAS loop to ensures we don't overwrite other concurrent subscriber changes
    loop {
        let current = topic.subscribers.load();
        // Remove any senders that are closed. We check the current list, not the snapshot.
        let new_subs: Vec<_> = current.iter().cloned().filter(|s| !s.is_closed()).collect();
        let removed_this_time = current.len().saturating_sub(new_subs.len());
        
        let new_arc = Arc::new(new_subs);
        let old = topic.subscribers.compare_and_swap(&current, new_arc);
        if Arc::ptr_eq(&*old, &*current) {
            if removed_this_time > 0 {
                topic.subscriber_count.fetch_sub(removed_this_time, Ordering::Relaxed);
            }
            break;
        }
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

    loop {
        let encoded_batch = match rx.recv().await {
            Some(batch) => batch,
            None => break,
        };

        if stream.write_all(&encoded_batch).await.is_err() {
            break;
        }
        
        // Only flush if no more batches are pending in the channel
        if rx.is_empty() {
            if stream.flush().await.is_err() {
                break;
            }
        }
    }
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

    let port = std::env::args().nth(1).unwrap_or_else(|| "6379".to_string());
    let addr: SocketAddr = format!("0.0.0.0:{}", port).parse().unwrap();
    tracing::info!("HugiMQ TCP server listening on {}", addr);

    let state = Arc::new(AppState {
        topics: ArcSwap::new(Arc::new(HashMap::new())),
    });

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