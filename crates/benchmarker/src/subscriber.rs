/// Subscriber logic for the benchmarker.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use hdrhistogram::Histogram;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

use crate::protocol::{build_nack_packet, parse_packet, MSG_DATA, MSG_RETRANSMIT};

pub struct SubscriberConfig {
    pub subscriber_id: u32,
    pub topics: Vec<u32>,
    pub server_addr: SocketAddr,
    pub listen_port: u16,
    pub expected_messages_per_topic: u64,
    pub done_flag: Arc<Mutex<bool>>,
}

pub struct SubscriberResult {
    pub subscriber_id: u32,
    pub total_received: u64,
    pub total_nacks_sent: u64,
    pub total_retransmissions: u64,
    pub messages_lost: u64,
    pub latency_histogram: Histogram<u64>,
    pub elapsed: std::time::Duration,
}

pub async fn run_subscriber(config: SubscriberConfig) -> SubscriberResult {
    let socket = UdpSocket::bind(format!("0.0.0.0:{}", config.listen_port))
        .await
        .expect("Failed to bind subscriber socket");

    println!("[Subscriber {}] Sending SUBSCRIBE packets to {} for topics {:?}", 
        config.subscriber_id, config.server_addr, config.topics);
    
    for &topic_id in &config.topics {
        let sub_packet = crate::protocol::build_subscribe_packet(topic_id);
        eprintln!("[Subscriber {}] Sending SUB for topic {} to {}", config.subscriber_id, topic_id, config.server_addr);
        let _ = socket.send_to(&sub_packet, config.server_addr).await;
    }

    let mut expected_seq: HashMap<u32, u64> = HashMap::new();
    for &topic_id in &config.topics {
        expected_seq.insert(topic_id, 0);
    }

    let mut histogram = Histogram::<u64>::new_with_bounds(1, 60_000_000_000, 3)
        .expect("Failed to create histogram");

    let mut total_received = 0u64;
    let mut total_nacks_sent = 0u64;
    let mut total_retransmissions = 0u64;
    let mut messages_lost = 0u64;
    let mut buf = [0u8; 4096];

    let start = std::time::Instant::now();
    let total_expected = config.topics.len() as u64 * config.expected_messages_per_topic;

    let mut grace_deadline = None;

    loop {
        {
            let done = *config.done_flag.lock().await;
            if done && total_received >= total_expected {
                break;
            }
            if done && grace_deadline.is_none() {
                grace_deadline = Some(start.elapsed() + std::time::Duration::from_secs(10));
            }
            if let Some(deadline) = grace_deadline {
                if start.elapsed() > deadline {
                    break;
                }
            }
        }

        let timeout = std::time::Duration::from_millis(50);

        tokio::select! {
            result = socket.recv_from(&mut buf) => {
                let (len, _from_addr) = match result {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                let packet = &buf[..len];
                if let Some((msg_type, topic_id, sequence, _timestamp, payload_opt)) = parse_packet(packet) {
                    if msg_type == MSG_DATA || msg_type == MSG_RETRANSMIT {
                        if msg_type == MSG_RETRANSMIT {
                            total_retransmissions += 1;
                        }

                        let expected = expected_seq.entry(topic_id).or_insert(0);

                        if sequence == *expected {
                            if let Some(ref payload) = payload_opt {
                                let now = timestamp_ns();
                                let latency = now.saturating_sub(payload.send_timestamp_ns);
                                if latency > 0 && latency < 60_000_000_000 {
                                    let _ = histogram.record(latency);
                                }
                            }
                            *expected += 1;
                            total_received += 1;
                        } else if sequence > *expected {
                            let gap = sequence - *expected;
                            messages_lost += gap;

                            let nack = build_nack_packet(topic_id, *expected, sequence - 1);
                            let _ = socket.send_to(&nack, config.server_addr).await;
                            total_nacks_sent += 1;

                            if let Some(ref payload) = payload_opt {
                                let now = timestamp_ns();
                                let latency = now.saturating_sub(payload.send_timestamp_ns);
                                if latency > 0 && latency < 60_000_000_000 {
                                    let _ = histogram.record(latency);
                                }
                            }
                            *expected = sequence + 1;
                            total_received += 1;
                        }
                    }
                }
            }
            _ = tokio::time::sleep(timeout) => {
                continue;
            }
        }

        if total_received >= total_expected {
            break;
        }
    }

    let elapsed = start.elapsed();

    SubscriberResult {
        subscriber_id: config.subscriber_id,
        total_received,
        total_nacks_sent,
        total_retransmissions,
        messages_lost,
        latency_histogram: histogram,
        elapsed,
    }
}

fn timestamp_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}