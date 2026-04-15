/// Publisher logic for the benchmarker.
///
/// Each publisher is assigned a set of topics and sends messages to the HugiMQ server
/// over UDP. Messages are sent in batches for throughput efficiency.

use std::net::SocketAddr;
use std::time::Instant;

use tokio::net::UdpSocket;

use crate::protocol::build_data_packet;

pub struct PublisherConfig {
    pub publisher_id: u32,
    pub topics: Vec<u32>,
    pub server_addr: SocketAddr,
    pub messages_per_topic: u64,
}

pub async fn run_publisher(config: PublisherConfig) -> PublisherResult {
    let socket = UdpSocket::bind("127.0.0.1:0").await.expect("Failed to bind publisher socket");

    let topic_count = config.topics.len();
    let msg_per_topic = config.messages_per_topic;
    let total_messages = topic_count as u64 * msg_per_topic;

    println!(
        "[Publisher {}] Sending {} messages across {} topics ({} per topic) to {}",
        config.publisher_id, total_messages, topic_count, msg_per_topic, config.server_addr
    );

    let start = Instant::now();
    let mut sent = 0u64;

    // Pre-build all packets to avoid allocation on the hot path.
    // For large message counts, we build and send in batches.
    let batch_size = 1024;

    for &topic_id in &config.topics {
        for seq in 0..msg_per_topic {
            let ts = timestamp_ns();
            let packet = build_data_packet(topic_id, seq, ts);
            let _ = socket.send_to(&packet, config.server_addr).await;
            sent += 1;

            // Flush in batches to avoid socket buffer overflow
            if sent % batch_size == 0 {
                // Small yield to let the OS drain buffers
                tokio::task::yield_now().await;
            }
        }
    }

    let elapsed = start.elapsed();
    let throughput = (sent as f64 / elapsed.as_secs_f64()) as u64;

    println!(
        "[Publisher {}] Done. Sent {} messages in {:.3}s ({:.0} msg/s)",
        config.publisher_id, sent, elapsed.as_secs_f64(), throughput as f64
    );

    PublisherResult {
        publisher_id: config.publisher_id,
        total_sent: sent,
        elapsed,
        throughput,
    }
}

#[derive(Clone, Debug)]
pub struct PublisherResult {
    pub publisher_id: u32,
    pub total_sent: u64,
    pub elapsed: std::time::Duration,
    pub throughput: u64,
}

fn timestamp_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}
