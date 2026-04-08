use clap::{Parser, ValueEnum};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Barrier, Mutex};
use tokio::time::{Duration, Instant};
use std::sync::atomic::{AtomicU64, Ordering};
use hdrhistogram::Histogram;
use std::time::SystemTime;

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "High-performance pub/sub benchmarker",
    after_help = "EXAMPLES:\n  Run TCP benchmark:\n    cargo run --release -p benchmarker -- tcp --connections 20 --messages-per-conn 50000\n\n  Test with 5KB payloads:\n    cargo run --release -p benchmarker -- tcp --payload-size 5120"
)]
struct Args {
    #[arg(help = "The system to benchmark")]
    target: Target,

    #[arg(short, long, default_value_t = 10)]
    connections: usize,

    #[arg(short, long, default_value_t = 100000)]
    messages_per_conn: usize,

    #[arg(short, long, default_value_t = 128)]
    payload_size: usize,

    #[arg(long, default_value = "tcp://127.0.0.1:6379")]
    url: String,

    #[arg(long, default_value_t = 1)]
    topics: usize,

    #[arg(long, default_value_t = 1)]
    topics_per_producer: usize,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Debug)]
#[value(rename_all = "lowercase")]
enum Target {
    Tcp,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    if args.connections < 2 {
        eprintln!("Error: Connections must be at least 2 (1 producer, 1 consumer)");
        std::process::exit(1);
    }

    if args.topics < 1 {
        eprintln!("Error: Topics must be at least 1");
        std::process::exit(1);
    }

    if args.topics_per_producer < 1 || args.topics_per_producer > args.topics {
        eprintln!("Error: topics_per_producer must be between 1 and topics ({}).", args.topics);
        std::process::exit(1);
    }

    if args.messages_per_conn % args.topics_per_producer != 0 {
        eprintln!(
            "Warning: messages_per_conn ({}) is not evenly divisible by topics_per_producer ({}). Some messages will be dropped.",
            args.messages_per_conn, args.topics_per_producer
        );
    }

    println!("Target: {:?} | Connections: {} | Topics: {} | Msg/Conn: {} | Payload: {} bytes",
             args.target, args.connections, args.topics, args.messages_per_conn, args.payload_size);

    let num_producers = args.connections / 2;
    let num_consumers = args.connections - num_producers;

    // Calculate total expected deliveries based on how topics are distributed.
    // Each producer publishes to topics_per_producer topics. Each consumer subscribes to ONE topic.
    // A message is delivered to all consumers on that topic.
    let mut total_expected_deliveries = 0u64;
    for p_id in 0..num_producers {
        for i in 0..args.topics_per_producer {
            let topic_idx = (p_id + i) % args.topics;
            let consumers_on_topic = (0..num_consumers).filter(|&c_id| c_id % args.topics == topic_idx).count();
            // We distribute the messages_per_conn across the chosen topics
            let msgs_on_this_topic = args.messages_per_conn / args.topics_per_producer;
            total_expected_deliveries += (msgs_on_this_topic * consumers_on_topic) as u64;
        }
    }

    let total_received = Arc::new(AtomicU64::new(0));

    let barrier = Arc::new(Barrier::new(args.connections));
    let start_barrier = Arc::new(Barrier::new(args.connections + 1));

    let pub_ack_hist = Arc::new(Mutex::new(Histogram::<u64>::new_with_bounds(1, 300_000_000_000, 3).unwrap()));
    let e2e_hist = Arc::new(Mutex::new(Histogram::<u64>::new_with_bounds(1, 10_000_000_000, 3).unwrap()));

    let mut handles = Vec::new();
    let payload = Arc::new("a".repeat(args.payload_size));

    println!("Spawning {} consumers and {} producers...", num_consumers, num_producers);

    // ─── Consumers ──────────────────────────────────────────────
    for consumer_id in 0..num_consumers {
        let total_received = Arc::clone(&total_received);
        let b = Arc::clone(&barrier);
        let sb = Arc::clone(&start_barrier);
        let target = args.target;
        let e2e_hist = Arc::clone(&e2e_hist);
        let url = args.url.clone();
        let topic_idx = consumer_id % args.topics;
        let topic_name = format!("benchmark_topic_{}", topic_idx);

        // Each consumer expects messages from all producers on the SAME topic.
        // Each producer splits its messages across topics_per_producer topics,
        // so we divide messages_per_conn accordingly.
        let producers_on_topic = (0..num_producers).filter(|&p_id| {
            // A producer publishes to topic_idx if topic_idx is in its range [p_id, p_id + topics_per_producer)
            (0..args.topics_per_producer).any(|i| (p_id + i) % args.topics == topic_idx)
        }).count();
        let expected_for_this_consumer = (args.messages_per_conn / args.topics_per_producer * producers_on_topic) as u64;

        handles.push(tokio::spawn(async move {
            let mut received_by_this_consumer = 0u64;
            let mut local_e2e = Histogram::<u64>::new_with_bounds(1, 10_000_000_000, 3).unwrap();

            match target {
                Target::Tcp => {
                    let stream = tokio::net::TcpStream::connect(url.replace("tcp://", ""))
                        .await
                        .unwrap();
                    let (mut reader, mut writer) = tokio::io::split(stream);

                    // Send subscribe message: [2 bytes total_len][1 byte type][2 bytes topic_len][topic]
                    let topic_bytes = topic_name.as_bytes();
                    let topic_len = topic_bytes.len() as u16;
                    let total_len = 1 + 2 + topic_bytes.len();
                    let mut frame = Vec::with_capacity(2 + total_len);
                    frame.extend_from_slice(&(total_len as u16).to_be_bytes());
                    frame.push(0x01); // MSG_SUBSCRIBE
                    frame.extend_from_slice(&topic_len.to_be_bytes());
                    frame.extend_from_slice(topic_bytes);
                    writer.write_all(&frame).await.unwrap();

                    // Subscribe BEFORE the barriers so we're ready when producers start.
                    b.wait().await;
                    sb.wait().await;

                    // Read loop: [2 bytes total_len][1 byte type][payload]
                    let mut len_buf = [0u8; 2];
                    while received_by_this_consumer < expected_for_this_consumer {
                        match tokio::time::timeout(Duration::from_secs(5), reader.read_exact(&mut len_buf)).await {
                            Ok(Ok(2)) => {
                                let total_len = u16::from_be_bytes(len_buf) as usize;
                                let mut msg_buf = vec![0u8; total_len];
                                match tokio::time::timeout(Duration::from_secs(5), reader.read_exact(&mut msg_buf)).await {
                                    Ok(Ok(_)) => {
                                        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
                                        let msg_type = msg_buf[0];
                                        if msg_type == 0x03 { // MSG_SUBSCRIBE_DATA
                                            let payload_data = &msg_buf[1..];
                                            if payload_data.len() < 4 {
                                                continue;
                                            }
                                            // Format: topic_len(2) + topic + payload_len(2) + payload_data
                                            let t_len = u16::from_be_bytes([payload_data[0], payload_data[1]]) as usize;
                                            if payload_data.len() < 2 + t_len + 2 {
                                                continue;
                                            }
                                            let received_topic = String::from_utf8_lossy(&payload_data[2..2 + t_len]);
                                            if received_topic != topic_name {
                                                eprintln!("CRITICAL ERROR: Received message for topic '{}' on consumer subscribed to '{}'", received_topic, topic_name);
                                                std::process::exit(1);
                                            }
                                            let p_offset = 2 + t_len + 2;
                                            let payload_bytes = &payload_data[p_offset..];
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
                                    }
                                    _ => break,
                                }
                            }
                            _ => break,
                        }
                    }
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
        let url = args.url.clone();
        let total_topics = args.topics;
        let topics_per_prod = args.topics_per_producer;

        handles.push(tokio::spawn(async move {
            let mut local_pub_ack = Histogram::<u64>::new_with_bounds(1, 300_000_000_000, 3).unwrap();

            // Each producer has a set of topics it cycles through
            let my_topics: Vec<String> = (0..topics_per_prod)
                .map(|i| {
                    let idx = (prod_id + i) % total_topics;
                    format!("benchmark_topic_{}", idx)
                })
                .collect();

            match target {
                Target::Tcp => {
                    let stream = tokio::net::TcpStream::connect(url.replace("tcp://", ""))
                        .await
                        .unwrap();
                    let (mut reader, mut writer) = tokio::io::split(stream);

                    b.wait().await;
                    sb.wait().await;

                    let publish_start = Instant::now();

                    for seq in 0..msg_count {
                        let topic_name = &my_topics[seq % my_topics.len()];
                        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
                        let msg = format!("{}:{}:{}:{}:{}", prod_id, seq, now, topic_name, payload);

                        // Binary format: [2 bytes total_len][1 byte type][2 bytes topic_len][topic][payload]
                        let topic_bytes = topic_name.as_bytes();
                        let topic_len = topic_bytes.len() as u16;
                        let payload_bytes = msg.as_bytes();
                        let total_len = 1 + 2 + topic_bytes.len() + payload_bytes.len();
                        let mut frame = Vec::with_capacity(2 + total_len);
                        frame.extend_from_slice(&(total_len as u16).to_be_bytes());
                        frame.push(0x02); // MSG_PUBLISH
                        frame.extend_from_slice(&topic_len.to_be_bytes());
                        frame.extend_from_slice(topic_bytes);
                        frame.extend_from_slice(payload_bytes);

                        if writer.write_all(&frame).await.is_err() {
                            break;
                        }
                    }

                    // Drain all server ACKs
                    let mut len_buf = [0u8; 2];
                    loop {
                        match tokio::time::timeout(Duration::from_secs(5), reader.read_exact(&mut len_buf)).await {
                            Ok(Ok(2)) => {
                                let total_len = u16::from_be_bytes(len_buf) as usize;
                                let mut buf = vec![0u8; total_len];
                                match tokio::time::timeout(Duration::from_secs(5), reader.read_exact(&mut buf)).await {
                                    Ok(Ok(_)) => continue,
                                    _ => break,
                                }
                            }
                            _ => break,
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

    // Spawn a monitor task to print throughput
    let total_received_monitor = Arc::clone(&total_received);
    let monitor_handle = tokio::spawn(async move {
        let mut last_count = 0;
        let start_time = Instant::now();
        while start_time.elapsed() < Duration::from_secs(60) {
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

    // Wait for all producers and consumers to finish
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
