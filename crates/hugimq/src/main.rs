//! HugiMQ UDP Server — Phase 1: Lock-free SPSC Ring Buffers
//!
//! Architecture:
//! - SO_REUSEPORT for multi-core recv distribution
//! - Per-subscriber SPSC ring buffers (lock-free, cache-line aligned)
//! - Dedicated send thread per subscriber
//! - Flow control: main loop blocks when ring buffer full → kernel buffers fill → publishers retry
//! - Sequence numbers on every datagram (Phase 2: NAK retransmit)

mod spsc_ring;

use bytes::BufMut;
use dashmap::DashMap;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use spsc_ring::SpscRingBuffer;
use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

// ─── Wire protocol ──────────────────────────────────────────────
// PUBLISH:    [1B: 0x02][2B: topic_len][topic][8B: seq][payload]
// SUBSCRIBE:  [1B: 0x01][2B: topic_len][topic]
// DATA:       [8B: seq][2B: topic_len][topic][2B: payload_len][payload]
// ─────────────────────────────────────────────────────────────────

const MSG_SUBSCRIBE: u8 = 0x01;
const MSG_PUBLISH: u8 = 0x02;
const MAX_DATAGRAM: usize = 65507;

/// Number of cores for SO_REUSEPORT socket distribution
const NUM_CORES: usize = 1;

/// Ring buffer capacity per subscriber (power of 2)
const RING_BUFFER_SIZE: usize = 1_048_576; // 1M slots — ~400MB per subscriber

#[derive(Clone)]
struct Subscriber {
    addr: SocketAddr,
    ring: Arc<SpscRingBuffer>,
}

struct TopicInner {
    subscribers: parking_lot::Mutex<Vec<Subscriber>>,
    subscriber_count: AtomicUsize,
    seq: AtomicU64,
}

struct AppState {
    topics: DashMap<String, Arc<TopicInner>>,
}

fn get_or_create_topic(state: &Arc<AppState>, topic: &str) -> Arc<TopicInner> {
    if let Some(entry) = state.topics.get(topic) {
        return entry.clone();
    }
    state.topics.entry(topic.to_string()).or_insert_with(|| {
        Arc::new(TopicInner {
            subscribers: parking_lot::Mutex::new(Vec::new()),
            subscriber_count: AtomicUsize::new(0),
            seq: AtomicU64::new(0),
        })
    }).clone()
}

/// Create a UDP socket with SO_REUSEPORT
fn create_udp_socket(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    let fd = socket.as_raw_fd();
    let reuse: libc::c_int = 1;
    unsafe {
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEPORT,
            &reuse as *const _ as *const _, std::mem::size_of::<libc::c_int>() as u32);
    }
    socket.set_nonblocking(true)?;
    const BUF: libc::c_int = 64 * 1024 * 1024;
    unsafe {
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF,
            &BUF as *const _ as *const _, std::mem::size_of::<libc::c_int>() as u32);
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_SNDBUF,
            &BUF as *const _ as *const _, std::mem::size_of::<libc::c_int>() as u32);
    }
    socket.bind(&SockAddr::from(addr))?;
    Ok(socket.into())
}

/// Subscriber send thread — pops from ring buffer and sends via UDP
fn send_loop(ring: Arc<SpscRingBuffer>, send_socket: UdpSocket, addr: SocketAddr) {
    thread::Builder::new()
        .name(format!("udp-send-{:?}", addr))
        .spawn(move || {
            loop {
                if let Some(message) = ring.try_pop() {
                    loop {
                        match send_socket.send_to(&message, addr) {
                            Ok(_) => break,
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::yield_now();
                            }
                            Err(e) => {
                                tracing::error!("[send {:?}] Send error: {}", addr, e);
                                return;
                            }
                        }
                    }
                } else {
                    std::thread::yield_now();
                }
            }
        })
        .expect("Failed to spawn send thread");
}

/// Handle incoming datagrams on a single REUSEPORT socket
fn recv_loop(
    socket: UdpSocket,
    state: Arc<AppState>,
    core_id: usize,
) {
    let mut buf = [0u8; MAX_DATAGRAM];
    let mut cache: Vec<(String, Arc<TopicInner>)> = Vec::with_capacity(4);

    loop {
        match socket.recv_from(&mut buf) {
            Ok((len, client_addr)) if len >= 2 => {
                let msg_type = buf[0];
                let payload = &buf[1..len];

                match msg_type {
                    MSG_PUBLISH => {
                        if payload.len() < 2 { continue; }
                        let topic_len = u16::from_be_bytes([payload[0], payload[1]]) as usize;
                        if payload.len() < 2 + topic_len { continue; }
                        let topic_name = String::from_utf8_lossy(&payload[2..2 + topic_len]);

                        let topic = if let Some(found) = cache.iter().find(|(n, _)| n == topic_name.as_ref()) {
                            found.1.clone()
                        } else {
                            let t = get_or_create_topic(&state, topic_name.as_ref());
                            if cache.len() >= 4 { cache.remove(0); }
                            cache.push((topic_name.to_string(), t.clone()));
                            t
                        };

                        let subs = topic.subscribers.lock().clone();
                        if subs.is_empty() { continue; }

                        // Allocate sequence number
                        static PUBLISH_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                        let seq = topic.seq.fetch_add(1, Ordering::Relaxed);
                        let total_pub = PUBLISH_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
                        if total_pub % 10000 == 0 {
                            tracing::debug!("[core {}] Publish #{}: topic={}, subs={}",
                                core_id, total_pub, topic_name, subs.len());
                        }

                        let topic_bytes = topic_name.as_bytes();
                        let topic_len_u16 = topic_bytes.len() as u16;
                        let msg_payload = &payload[2 + topic_len..];
                        let payload_len = msg_payload.len() as u16;

                        // Build frame: [8B seq][2B topic_len][topic][2B payload_len][payload]
                        let mut frame = Vec::with_capacity(8 + 2 + topic_bytes.len() + 2 + msg_payload.len());
                        frame.put_u64(seq);
                        frame.put_u16(topic_len_u16);
                        frame.extend_from_slice(topic_bytes);
                        frame.put_u16(payload_len);
                        frame.extend_from_slice(msg_payload);
                        let frame = Arc::new(frame);

                        // Push to all subscriber ring buffers — BLOCKS if full (proper backpressure)
                        // Uses condvar to park the producer thread until the send thread drains.
                        for sub in &subs {
                            sub.ring.push(Arc::clone(&frame));
                        }
                    }
                    MSG_SUBSCRIBE => {
                        if payload.len() < 2 { continue; }
                        let topic_len = u16::from_be_bytes([payload[0], payload[1]]) as usize;
                        if payload.len() < 2 + topic_len { continue; }
                        let topic_name = String::from_utf8_lossy(&payload[2..2 + topic_len]).into_owned();

                        let topic = get_or_create_topic(&state, &topic_name);

                        // Create dedicated send socket for this subscriber
                        let send_socket = match create_udp_socket("0.0.0.0:0".parse().unwrap()) {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::error!("Failed to create send socket: {}", e);
                                continue;
                            }
                        };
                        let ring = Arc::new(SpscRingBuffer::new(RING_BUFFER_SIZE));

                        send_loop(Arc::clone(&ring), send_socket.try_clone().unwrap(), client_addr);

                        let subscriber = Subscriber { addr: client_addr, ring };
                        topic.subscribers.lock().push(subscriber);
                        topic.subscriber_count.fetch_add(1, Ordering::Relaxed);

                        // Send ACK to client so benchmarker knows subscription is ready
                        if create_udp_socket("0.0.0.0:0".parse().unwrap())
                            .and_then(|s| s.send_to(&[0x00], client_addr))
                            .is_err() {
                            tracing::warn!("[core {}] Failed to send subscribe ACK to {}", core_id, client_addr);
                        }

                        tracing::info!("[core {}] Subscribed {} to '{}' (total: {})",
                            core_id, client_addr, topic_name,
                            topic.subscriber_count.load(Ordering::Relaxed));
                    }
                    _ => {}
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::yield_now();
            }
            Err(e) => {
                tracing::error!("[core {}] Recv error: {}", core_id, e);
            }
            _ => {}
        }
    }
}

fn main() {
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

    tracing::info!("HugiMQ UDP (Aeron-style) server listening on {}", addr);
    tracing::info!("Spawning {} recv threads (SO_REUSEPORT)", NUM_CORES);
    tracing::info!("Ring buffer size: {} messages per subscriber", RING_BUFFER_SIZE);

    // Spawn NUM_CORES recv threads, each with its own SO_REUSEPORT socket
    let mut threads = Vec::new();
    for core_id in 0..NUM_CORES {
        let socket = match create_udp_socket(addr) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to create REUSEPORT socket on core {}: {}", core_id, e);
                continue;
            }
        };
        let state = Arc::clone(&state);
        let handle = thread::Builder::new()
            .name(format!("udp-recv-{}", core_id))
            .spawn(move || {
                // Pin thread to CPU core
                let mut cpuset: libc::cpu_set_t = unsafe { std::mem::zeroed() };
                unsafe { libc::CPU_SET(core_id, &mut cpuset) };
                let tid = unsafe { libc::pthread_self() } as libc::pid_t;
                unsafe {
                    libc::sched_setaffinity(tid, std::mem::size_of::<libc::cpu_set_t>() as libc::size_t, &cpuset as *const _);
                }
                recv_loop(socket, state, core_id);
            })
            .expect("Failed to spawn recv thread");
        threads.push(handle);
    }

    // Wait for all threads (they run forever)
    for t in threads {
        let _ = t.join();
    }
}
