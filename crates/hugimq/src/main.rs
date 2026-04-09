use arc_swap::ArcSwap;
use bytes::BufMut;
use dashmap::DashMap;
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

// ─── Wire protocol ──────────────────────────────────────────────
// UDP is connectionless — each datagram is self-contained.
//
// Client → Server (PUBLISH):
//   [1 byte: 0x02][2 bytes: topic_len][topic][message payload]
//
// Client → Server (SUBSCRIBE):
//   [1 byte: 0x01][2 bytes: topic_len][topic]
//
// Server → Client (SUBSCRIBE_DATA):
//   [2 bytes: topic_len][topic][2 bytes: payload_len][message payload]
// ─────────────────────────────────────────────────────────────────

const MSG_SUBSCRIBE: u8 = 0x01;
const MSG_PUBLISH: u8 = 0x02;

/// Max UDP datagram payload (65535 - 8 UDP header - 20 IP header)
const MAX_DATAGRAM_SIZE: usize = 65507;

#[derive(Clone)]
struct Subscriber {
    addr: SocketAddr,
}

struct Topic {
    /// Lock-free subscriber list via ArcSwap — single atomic load on publish path
    subscribers: ArcSwap<Vec<Subscriber>>,
    subscriber_count: AtomicUsize,
}

struct AppState {
    topics: DashMap<String, Arc<Topic>>,
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
                subscribers: ArcSwap::new(Arc::new(Vec::new())),
                subscriber_count: AtomicUsize::new(0),
            })
        })
        .clone()
}

// ─── Subscribe handler ──────────────────────────────────────────
// Registers subscriber address and spawns dispatcher task.

async fn handle_subscribe(
    payload: &[u8],
    client_addr: SocketAddr,
    state: &Arc<AppState>,
) {
    if payload.len() < 2 {
        return;
    }
    let topic_len = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    if payload.len() < 2 + topic_len {
        return;
    }
    let topic_name = String::from_utf8_lossy(&payload[2..2 + topic_len]).into_owned();

    let topic = get_or_create_topic(state, &topic_name);

    let subscriber = Subscriber {
        addr: client_addr,
    };

    // ArcSwap CAS loop — no write-lock needed
    loop {
        let current = topic.subscribers.load();
        let mut new_subs: Vec<_> = current.iter().cloned().collect();
        new_subs.push(subscriber.clone());
        let new_arc = Arc::new(new_subs);
        let old = topic.subscribers.compare_and_swap(&current, new_arc);
        if Arc::ptr_eq(&*old, &*current) {
            break;
        }
    }
    topic.subscriber_count.fetch_add(1, Ordering::Relaxed);

    tracing::info!("Subscribed {} to topic '{}'", client_addr, topic_name);
}

// ─── Publish handler ────────────────────────────────────────────
// Encodes frame once, sends directly via UDP to each subscriber.
// Uses blocking std::net::UdpSocket to avoid yielding to tokio runtime —
// each send_to copies to kernel buffer and returns immediately.

fn handle_publish(
    payload: &[u8],
    cache: &mut Vec<(String, Arc<Topic>)>,
    state: &Arc<AppState>,
    std_socket: &std::net::UdpSocket,
) {
    if payload.len() < 2 {
        return;
    }
    let topic_len = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    if payload.len() < 2 + topic_len {
        return;
    }
    let topic_name_cow = String::from_utf8_lossy(&payload[2..2 + topic_len]);

    let (topic_name, topic) = if let Some(found) = cache.iter().find(|(n, _)| n == topic_name_cow.as_ref()) {
        (found.0.clone(), found.1.clone())
    } else {
        let t = get_or_create_topic(state, topic_name_cow.as_ref());
        if cache.len() >= 4 {
            cache.remove(0);
        }
        let owned = topic_name_cow.into_owned();
        cache.push((owned.clone(), t.clone()));
        (owned, t)
    };

    let msg_payload = payload[2 + topic_len..].to_vec();

    let subs = topic.subscribers.load();
    if subs.is_empty() {
        return;
    }

    // Encode frame once: [topic_len][topic][payload_len][payload]
    let topic_bytes = topic_name.as_bytes();
    let topic_len_u16 = topic_bytes.len() as u16;
    let payload_len = msg_payload.len() as u16;

    let mut frame = Vec::with_capacity(2 + topic_bytes.len() + 2 + msg_payload.len());
    frame.put_u16(topic_len_u16);
    frame.extend_from_slice(topic_bytes);
    frame.put_u16(payload_len);
    frame.extend_from_slice(&msg_payload);

    // Blocking send_to each subscriber — copies to kernel buffer, no yield
    for sub in subs.iter() {
        match std_socket.send_to(&frame, sub.addr) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Kernel buffer full — drop message (UDP semantics)
            }
            Err(e) => {
                tracing::debug!("UDP send error to {}: {}", sub.addr, e);
            }
        }
    }
}

// ─── Main ───────────────────────────────────────────────────────
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

    let port = std::env::var("HUGIMQ_PORT").unwrap_or_else(|_| "6379".to_string());
    let addr: SocketAddr = format!("0.0.0.0:{}", port).parse().unwrap();

    // Std socket for blocking sends (no tokio yielding)
    let std_socket = std::net::UdpSocket::bind(addr).unwrap();
    std_socket.set_nonblocking(true).unwrap();
    // Large socket buffers for high-throughput UDP via libc setsockopt
    const SOCKET_BUF_SIZE: libc::c_int = 64 * 1024 * 1024; // 64MB
    let fd = std_socket.as_raw_fd();
    unsafe {
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF,
            &SOCKET_BUF_SIZE as *const _ as *const _, std::mem::size_of::<libc::c_int>() as u32);
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_SNDBUF,
            &SOCKET_BUF_SIZE as *const _ as *const _, std::mem::size_of::<libc::c_int>() as u32);
    }

    // Tokio socket for async recv
    let socket = UdpSocket::from_std(std_socket.try_clone().unwrap()).unwrap();
    tracing::info!("HugiMQ UDP server listening on {}", addr);

    let mut buf = [0u8; MAX_DATAGRAM_SIZE];
    let mut cache: Vec<(String, Arc<Topic>)> = Vec::with_capacity(4);

    loop {
        let (len, client_addr) = match socket.recv_from(&mut buf).await {
            Ok(result) => result,
            Err(e) => {
                tracing::error!("Failed to receive: {}", e);
                continue;
            }
        };

        if len < 2 {
            continue;
        }

        let msg_type = buf[0];
        let payload = &buf[1..len];

        match msg_type {
            MSG_PUBLISH => {
                handle_publish(payload, &mut cache, &state, &std_socket);
            }
            MSG_SUBSCRIBE => {
                handle_subscribe(payload, client_addr, &state).await;
            }
            _ => {
                tracing::warn!("Unknown message type: {} from {}", msg_type, client_addr);
            }
        }
    }
}
