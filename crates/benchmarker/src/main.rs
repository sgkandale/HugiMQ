use bytes::BufMut;
use clap::{Parser, ValueEnum};
use hdrhistogram::Histogram;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::net::UdpSocket;
use tokio::sync::{Barrier, Mutex};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "High-performance pub/sub benchmarker",
)]
struct Args {
    #[arg(help = "The system to benchmark")]
    target: Target,

    #[arg(short, long, default_value_t = 20)]
    connections: usize,

    #[arg(short, long, default_value_t = 50000)]
    messages_per_conn: usize,

    #[arg(short, long, default_value_t = 128)]
    payload_size: usize,

    #[arg(long, default_value = "udp://127.0.0.1:6379")]
    url: String,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Debug)]
#[value(rename_all = "lowercase")]
enum Target {
    Udp,
}

/// Max UDP datagram size
const MAX_DATAGRAM_SIZE: usize = 65507;

#[tokio::main(worker_threads = 32)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    if args.connections < 2 {
        eprintln!("Error: Connections must be at least 2 (1 producer, 1 consumer)");
        std::process::exit(1);
    }

    let num_producers = args.connections / 2;
    let num_consumers = args.connections - num_producers;

    // Each producer publishes to its own topic (producer 0 → topic_0, etc.)
    // Each consumer subscribes to its own topic (consumer 0 → topic_0, etc.)
    // So consumer i receives messages only from producer i.
    // Total expected: num_producers × messages_per_conn
    let total_expected_deliveries = (num_producers as u64) * (args.messages_per_conn as u64);

    println!("Target: {:?} | Connections: {} | Topics: {} | Msg/Conn: {} | Payload: {} bytes",
             args.target, args.connections, num_producers, args.messages_per_conn, args.payload_size);
    println!("Expected deliveries: {} ({} producers × {} msgs each)", total_expected_deliveries, num_producers, args.messages_per_conn);

    let total_received = Arc::new(AtomicU64::new(0));
    let barrier = Arc::new(Barrier::new(args.connections));
    let start_barrier = Arc::new(Barrier::new(args.connections + 1));

    let pub_ack_hist = Arc::new(Mutex::new(Histogram::<u64>::new_with_bounds(1, 300_000_000_000, 3).unwrap()));
    let e2e_hist = Arc::new(Mutex::new(Histogram::<u64>::new_with_bounds(1, 10_000_000_000, 3).unwrap()));

    let mut handles = Vec::new();
    let payload = Arc::new("a".repeat(args.payload_size));

    println!("Spawning {} consumers and {} producers...", num_consumers, num_producers);

    let server_addr = args.url.replace("udp://", "");

    // ─── Consumers ──────────────────────────────────────────────
    for consumer_id in 0..num_consumers {
        let total_received = Arc::clone(&total_received);
        let b = Arc::clone(&barrier);
        let sb = Arc::clone(&start_barrier);
        let target = args.target;
        let e2e_hist = Arc::clone(&e2e_hist);
        let server_addr = server_addr.clone();
        let topic_name = format!("topic_{}", consumer_id);
        let expected_for_this_consumer = args.messages_per_conn as u64;

        handles.push(tokio::spawn(async move {
            let mut received_by_this_consumer = 0u64;
            let mut local_e2e = Histogram::<u64>::new_with_bounds(1, 10_000_000_000, 3).unwrap();

            match target {
                Target::Udp => {
                    let std_socket = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
                    const SOCKET_BUF_SIZE: libc::c_int = 128 * 1024 * 1024;
                    let fd = std_socket.as_raw_fd();
                    unsafe {
                        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF,
                            &SOCKET_BUF_SIZE as *const _ as *const _, std::mem::size_of::<libc::c_int>() as u32);
                    }
                    let socket = UdpSocket::from_std(std_socket).unwrap();

                    // Send SUBSCRIBE
                    let topic_bytes = topic_name.as_bytes();
                    let topic_len = topic_bytes.len() as u16;
                    let mut subscribe_msg = Vec::with_capacity(1 + 2 + topic_bytes.len());
                    subscribe_msg.push(0x01); // MSG_SUBSCRIBE
                    subscribe_msg.put_u16(topic_len);
                    subscribe_msg.extend_from_slice(topic_bytes);
                    socket.send_to(&subscribe_msg, &server_addr).await.unwrap();

                    // Wait for server ACK confirming subscription
                    let mut ack_buf = [0u8; 1];
                    socket.recv_from(&mut ack_buf).await.unwrap();

                    b.wait().await;
                    sb.wait().await;

                    // Receive loop — parse: [8B seq][2B topic_len][topic][2B payload_len][payload]
                    let mut buf = [0u8; MAX_DATAGRAM_SIZE];
                    let mut raw_received: u64 = 0;
                    let mut parse_errors: u64 = 0;
                    let mut gaps: u64 = 0;
                    let mut prev_seq: u64 = 0;
                    while received_by_this_consumer < expected_for_this_consumer {
                        match tokio::time::timeout(
                            Duration::from_secs(60),
                            socket.recv_from(&mut buf),
                        ).await {
                            Ok(Ok((len, _))) => {
                                raw_received += 1;
                                let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();

                                if len < 8 + 4 {
                                    parse_errors += 1;
                                    continue;
                                }
                                let seq = u64::from_be_bytes([buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7]]);
                                if prev_seq > 0 && seq != prev_seq + 1 {
                                    gaps += 1;
                                }
                                prev_seq = seq;

                                let t_len = u16::from_be_bytes([buf[8], buf[9]]) as usize;
                                if len < 8 + 2 + t_len + 2 {
                                    parse_errors += 1;
                                    continue;
                                }
                                let received_topic = String::from_utf8_lossy(&buf[10..10 + t_len]);
                                if received_topic != topic_name {
                                    eprintln!("CRITICAL ERROR: topic '{}' expected '{}'", received_topic, topic_name);
                                    std::process::exit(1);
                                }
                                let p_offset = 10 + t_len + 2;
                                let payload_bytes = &buf[p_offset..len];
                                let payload_str = String::from_utf8_lossy(payload_bytes);
                                let parts: Vec<&str> = payload_str.splitn(5, ':').collect();
                                if parts.len() >= 3 {
                                    if let Ok(sent_at) = parts[2].parse::<u128>() {
                                        let latency = (now.saturating_sub(sent_at)) as u64;
                                        let _ = local_e2e.record(latency);
                                    }
                                }
                                total_received.fetch_add(1, Ordering::Relaxed);
                                received_by_this_consumer += 1;
                            }
                            _ => break,
                        }
                    }
                    eprintln!("Consumer {} (topic={}): raw={} parsed={} errors={} gaps={} expected={}",
                        consumer_id, topic_name, raw_received, received_by_this_consumer, parse_errors, gaps, expected_for_this_consumer);
                }
            }
            let mut global_e2e = e2e_hist.lock().await;
            global_e2e.add(local_e2e).unwrap();
        }));
    }

    // ─── Producers ──────────────────────────────────────────────
    for prod_id in 0..num_producers {
        let b = Arc::clone(&barrier);
        let sb = Arc::clone(&start_barrier);
        let target = args.target;
        let msg_count = args.messages_per_conn;
        let payload = Arc::clone(&payload);
        let pub_ack_hist = Arc::clone(&pub_ack_hist);
        let server_addr = server_addr.clone();

        handles.push(tokio::spawn(async move {
            let mut local_pub_ack = Histogram::<u64>::new_with_bounds(1, 300_000_000_000, 3).unwrap();
            let topic_name = format!("topic_{}", prod_id);

            match target {
                Target::Udp => {
                    let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();

                    b.wait().await;
                    sb.wait().await;

                    let publish_start = Instant::now();

                    for seq in 0..msg_count {
                        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
                        let msg = format!("{}:{}:{}:{}:{}", prod_id, seq, now, topic_name, payload);

                        // PUBLISH: [1B: 0x02][2B: topic_len][topic][8B: seq][payload]
                        let topic_bytes = topic_name.as_bytes();
                        let topic_len = topic_bytes.len() as u16;
                        let payload_bytes = msg.as_bytes();
                        let mut frame = Vec::with_capacity(1 + 2 + topic_bytes.len() + 8 + payload_bytes.len());
                        frame.push(0x02); // MSG_PUBLISH
                        frame.put_u16(topic_len);
                        frame.extend_from_slice(topic_bytes);
                        frame.put_u64(seq as u64);
                        frame.extend_from_slice(payload_bytes);

                        loop {
                            match socket.send_to(&frame, &server_addr).await {
                                Ok(_) => break,
                                Err(_) => tokio::task::yield_now().await,
                            }
                        }
                    }

                    let total_duration = publish_start.elapsed().as_nanos() as u64;
                    let _ = local_pub_ack.record(total_duration);
                }
            }
            let mut global_pub_ack = pub_ack_hist.lock().await;
            global_pub_ack.add(local_pub_ack).unwrap();
        }));
    }

    println!("All connections ready. Starting benchmark...");
    let start_time = Instant::now();
    start_barrier.wait().await;

    let total_received_monitor = Arc::clone(&total_received);
    let monitor_handle = tokio::spawn(async move {
        let mut last_count = 0;
        let start_time = Instant::now();
        while start_time.elapsed() < Duration::from_secs(300) {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let current = total_received_monitor.load(Ordering::Relaxed);
            let delta = current - last_count;
            last_count = current;
            println!("Current Throughput: {} msg/s | Total Received: {}/{}", delta, current, total_expected_deliveries);
            if current >= total_expected_deliveries {
                break;
            }
        }
    });

    for handle in handles {
        let _ = handle.await;
    }
    monitor_handle.abort();

    let duration = start_time.elapsed();
    let final_received = total_received.load(Ordering::Relaxed);
    let throughput = final_received as f64 / duration.as_secs_f64();

    println!("\nBenchmark Results:");
    println!("Duration: {:?}", duration);
    println!("Total Expected Deliveries: {}", total_expected_deliveries);
    println!("Total Received: {}", final_received);
    println!("Messages Lost: {}", (total_expected_deliveries as i128) - (final_received as i128));
    println!("Average Throughput: {:.2} msg/s", throughput);

    let pub_ack = pub_ack_hist.lock().await;
    let e2e = e2e_hist.lock().await;

    println!("\nProducer ACK Latency (ns):");
    println!("  Min:    {}", pub_ack.min());
    println!("  P50:    {}", pub_ack.value_at_quantile(0.5));
    println!("  P90:    {}", pub_ack.value_at_quantile(0.90));
    println!("  P99:    {}", pub_ack.value_at_quantile(0.99));
    println!("  Max:    {}", pub_ack.max());

    println!("\nEnd-to-End Latency (ns):");
    println!("  Min:    {}", e2e.min());
    println!("  P50:    {}", e2e.value_at_quantile(0.5));
    println!("  P90:    {}", e2e.value_at_quantile(0.90));
    println!("  P99:    {}", e2e.value_at_quantile(0.99));
    println!("  Max:    {}", e2e.max());

    std::process::exit(0);
}
