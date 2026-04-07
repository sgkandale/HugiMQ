use clap::{Parser, ValueEnum};
use redis::AsyncCommands;
use std::sync::Arc;
use tokio::sync::{Barrier, Mutex};
use tokio::time::{Duration, Instant};
use futures::{StreamExt, TryStreamExt};
use std::sync::atomic::{AtomicU64, Ordering};
use hdrhistogram::Histogram;
use std::time::SystemTime;

pub mod hugimq {
    tonic::include_proto!("hugimq");
}

use hugimq::hugi_mq_service_client::HugiMqServiceClient;
use hugimq::{PublishRequest, SubscribeRequest};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "High-performance pub/sub benchmarker",
    after_help = "EXAMPLES:\n  Run HugiMQ benchmark:\n    cargo run --release -p benchmarker -- hugimq --connections 20 --messages-per-conn 50000\n\n  Run Redis benchmark:\n    cargo run --release -p benchmarker -- redis --connections 20 --messages-per-conn 50000\n\n  Test with 5KB payloads:\n    cargo run --release -p benchmarker -- hugimq --payload-size 5120"
)]
struct Args {
    #[arg(help = "The system to benchmark (redis or hugimq)")]
    target: Target,

    #[arg(short, long, default_value_t = 10)]
    connections: usize,

    #[arg(short, long, default_value_t = 100000)]
    messages_per_conn: usize,

    #[arg(short, long, default_value_t = 128)]
    payload_size: usize,

    #[arg(long, default_value = "redis://127.0.0.1/")]
    redis_url: String,

    #[arg(long, default_value = "http://127.0.0.1:6379")]
    hugimq_url: String,

    #[arg(long, default_value_t = 1)]
    topics: usize,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Debug)]
#[value(rename_all = "lowercase")]
enum Target {
    Redis,
    Hugimq,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    if args.connections < 2 {
        eprintln!("Error: Connections must be at least 2 (1 producer, 1 consumer)");
        std::process::exit(1);
    }

    println!("Target: {:?} | Connections: {} | Topics: {} | Msg/Conn: {} | Payload: {} bytes",
             args.target, args.connections, args.topics, args.messages_per_conn, args.payload_size);

    let num_producers = args.connections / 2;
    let num_consumers = args.connections - num_producers;

    let total_expected_messages = num_producers * args.messages_per_conn;

    // Calculate total expected deliveries based on how topics are distributed.
    // Each producer publishes to ONE topic. Each consumer subscribes to ONE topic.
    // A message is delivered to all consumers on that topic.
    let mut total_expected_deliveries = 0u64;
    for p_id in 0..num_producers {
        let topic_idx = p_id % args.topics;
        let consumers_on_topic = (0..num_consumers).filter(|&c_id| c_id % args.topics == topic_idx).count();
        total_expected_deliveries += (args.messages_per_conn * consumers_on_topic) as u64;
    }

    let total_received = Arc::new(AtomicU64::new(0));

    let barrier = Arc::new(Barrier::new(args.connections));
    let start_barrier = Arc::new(Barrier::new(args.connections + 1));

    let pub_ack_hist = Arc::new(Mutex::new(Histogram::<u64>::new_with_bounds(1, 10_000_000_000, 3).unwrap()));
    let e2e_hist = Arc::new(Mutex::new(Histogram::<u64>::new_with_bounds(1, 10_000_000_000, 3).unwrap()));

    let mut handles = Vec::new();
    let payload = Arc::new("a".repeat(args.payload_size));

    println!("Spawning {} consumers and {} producers...", num_consumers, num_producers);

    for consumer_id in 0..num_consumers {
        let total_received = Arc::clone(&total_received);
        let b = Arc::clone(&barrier);
        let sb = Arc::clone(&start_barrier);
        let target = args.target;
        let e2e_hist = Arc::clone(&e2e_hist);
        let redis_url = args.redis_url.clone();
        let hugimq_url = args.hugimq_url.clone();
        let topic_idx = consumer_id % args.topics;
        let topic_name = if args.topics > 1 {
            format!("benchmark_topic_{}", topic_idx)
        } else {
            "benchmark_topic".to_string()
        };

        // Each consumer expects messages from all producers on the SAME topic
        let producers_on_topic = (0..num_producers).filter(|&p_id| p_id % args.topics == topic_idx).count();
        let expected_for_this_consumer = (args.messages_per_conn * producers_on_topic) as u64;

        handles.push(tokio::spawn(async move {
            let mut received_by_this_consumer = 0u64;
            let mut local_e2e = Histogram::<u64>::new_with_bounds(1, 10_000_000_000, 3).unwrap();
            match target {
                Target::Redis => {
                    let client = redis::Client::open(redis_url.as_str()).unwrap();
                    let con = client.get_async_connection().await.unwrap();
                    let mut pubsub = con.into_pubsub();
                    pubsub.subscribe(&topic_name).await.unwrap();

                    b.wait().await;
                    sb.wait().await;

                    let mut stream = pubsub.on_message();
                    while received_by_this_consumer < expected_for_this_consumer {
                        match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
                            Ok(Some(msg)) => {
                                let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
                                let payload_bytes: Vec<u8> = msg.get_payload().unwrap();
                                let payload_str = String::from_utf8_lossy(&payload_bytes);

                                let parts: Vec<&str> = payload_str.splitn(4, ':').collect();
                                if parts.len() >= 3 {
                                    if let Ok(sent_at) = parts[2].parse::<u128>() {
                                        let latency = (now.saturating_sub(sent_at)) as u64;
                                        let _ = local_e2e.record(latency);
                                    }
                                }

                                total_received.fetch_add(1, Ordering::Relaxed);
                                received_by_this_consumer += 1;
                            }
                            Ok(None) => break,
                            Err(_) => {
                                eprintln!("Consumer {} timed out waiting for message (received {}/{})", consumer_id, received_by_this_consumer, expected_for_this_consumer);
                                break;
                            }
                        }
                    }
                }
                Target::Hugimq => {
                    let mut client = HugiMqServiceClient::connect(hugimq_url.to_string()).await.unwrap();
                    b.wait().await;
                    sb.wait().await;

                    let response = client.subscribe(SubscribeRequest { topic: topic_name.clone() }).await.unwrap();
                    let mut stream = response.into_inner();

                    while received_by_this_consumer < expected_for_this_consumer {
                        match tokio::time::timeout(Duration::from_secs(5), stream.try_next()).await {
                            Ok(Ok(Some(msg))) => {
                                let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
                                let data = String::from_utf8_lossy(&msg.payload);

                                let parts: Vec<&str> = data.splitn(5, ':').collect();
                                if parts.len() >= 4 {
                                    let received_topic = parts[3];
                                    if received_topic != topic_name {
                                        eprintln!("CRITICAL ERROR: Received message for topic '{}' on consumer subscribed to '{}'", received_topic, topic_name);
                                        std::process::exit(1);
                                    }

                                    if let Ok(sent_at) = parts[2].parse::<u128>() {
                                        let latency = (now.saturating_sub(sent_at)) as u64;
                                        let _ = local_e2e.record(latency);
                                    }
                                }

                                total_received.fetch_add(1, Ordering::Relaxed);
                                received_by_this_consumer += 1;
                            }
                            Ok(Ok(None)) => break,
                            Ok(Err(e)) => {
                                eprintln!("Consumer {} stream error: {:?}", consumer_id, e);
                                break;
                            }
                            Err(_) => {
                                eprintln!("Consumer {} timed out waiting for message (received {}/{})", consumer_id, received_by_this_consumer, expected_for_this_consumer);
                                break;
                            }
                        }
                    }
                }
            }
            let mut global_e2e = e2e_hist.lock().await;
            global_e2e.add(local_e2e).unwrap();
        }));
    }

    for prod_id in 0..num_producers {
        let b = Arc::clone(&barrier);
        let sb = Arc::clone(&start_barrier);
        let target = args.target;
        let msg_count = args.messages_per_conn;
        let payload = Arc::clone(&payload);
        let pub_ack_hist = Arc::clone(&pub_ack_hist);
        let redis_url = args.redis_url.clone();
        let hugimq_url = args.hugimq_url.clone();
        let topic_name = if args.topics > 1 {
            format!("benchmark_topic_{}", prod_id % args.topics)
        } else {
            "benchmark_topic".to_string()
        };

        handles.push(tokio::spawn(async move {
            let mut local_pub_ack = Histogram::<u64>::new_with_bounds(1, 10_000_000_000, 3).unwrap();
            match target {
                Target::Redis => {
                    let client = redis::Client::open(redis_url.as_str()).unwrap();
                    let mut con = client.get_async_connection().await.unwrap();

                    b.wait().await;
                    sb.wait().await;

                    for seq in 0..msg_count {
                        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
                        let msg = format!("{}:{}:{}:{}:{}", prod_id, seq, now, topic_name, payload);

                        let ack_start = Instant::now();
                        let _: () = con.publish(&topic_name, msg).await.unwrap();
                        let ack_latency = ack_start.elapsed().as_nanos() as u64;
                        let _ = local_pub_ack.record(ack_latency);
                    }
                }
                Target::Hugimq => {
                    let mut client = HugiMqServiceClient::connect(hugimq_url.to_string()).await.unwrap();
                    b.wait().await;
                    sb.wait().await;

                    for seq in 0..msg_count {
                        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
                        let msg = format!("{}:{}:{}:{}:{}", prod_id, seq, now, topic_name, payload);

                        let ack_start = Instant::now();
                        let resp = client.publish(PublishRequest {
                            topic: topic_name.clone(),
                            payload: msg.into_bytes().into(),
                        }).await;

                        if let Ok(resp) = resp {
                            if resp.get_ref().ok {
                                let ack_latency = ack_start.elapsed().as_nanos() as u64;
                                let _ = local_pub_ack.record(ack_latency);
                            }
                        }
                    }
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
