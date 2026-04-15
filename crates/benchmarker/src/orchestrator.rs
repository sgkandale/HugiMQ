/// Benchmark orchestrator.
///
/// Spawns publishers and subscribers, waits for completion, merges histograms,
/// and prints a summary report.

use std::sync::Arc;
use std::net::SocketAddr;
use std::time::Instant;

use hdrhistogram::Histogram;
use tokio::sync::Mutex;

use crate::publisher::{PublisherConfig, PublisherResult, run_publisher};
use crate::subscriber::{SubscriberConfig, SubscriberResult, run_subscriber};

pub struct BenchmarkConfig {
    pub num_publishers: u32,
    pub num_subscribers: u32,
    pub num_topics: u32,
    pub messages_per_topic: u64,
    pub server_addr: SocketAddr,
}

pub struct BenchmarkResult {
    pub publisher_results: Vec<PublisherResult>,
    pub subscriber_results: Vec<SubscriberResult>,
    pub merged_histogram: Histogram<u64>,
    pub total_messages_sent: u64,
    pub total_messages_received: u64,
    pub total_messages_lost: u64,
    pub total_nacks_sent: u64,
    pub total_retransmissions: u64,
    pub total_elapsed: std::time::Duration,
}

pub async fn run_benchmark(config: BenchmarkConfig) -> BenchmarkResult {
    println!("=== HugiMQ Benchmark ===");
    println!(
        "Publishers: {}, Subscribers: {}, Topics: {}, Messages/topic: {}",
        config.num_publishers, config.num_subscribers, config.num_topics, config.messages_per_topic
    );
    println!("Server: {}", config.server_addr);
    println!();

    let done_flag = Arc::new(Mutex::new(false));

    // Build publisher configs - each publisher handles 1 topic (round-robin)
    let mut publisher_configs = Vec::new();
    for pid in 0..config.num_publishers {
        let topic_id = pid % config.num_topics;
        publisher_configs.push(PublisherConfig {
            publisher_id: pid,
            topics: vec![topic_id],
            server_addr: config.server_addr,
            messages_per_topic: config.messages_per_topic,
        });
    }

    // Build subscriber configs - each subscriber subscribes to matching topics
    let mut subscriber_configs = Vec::new();
    let mut next_port = 6000u16;
    for sid in 0..config.num_subscribers {
        // Each subscriber subscribes to the same topic as its matching publisher
        let topic_id = sid % config.num_topics;
        subscriber_configs.push(SubscriberConfig {
            subscriber_id: sid,
            topics: vec![topic_id],
            server_addr: config.server_addr,
            listen_port: next_port,
            expected_messages_per_topic: config.messages_per_topic,
            done_flag: done_flag.clone(),
        });
        next_port += 1;
    }

    // Spawn subscribers
    let mut subscriber_handles = Vec::new();
    for sconfig in subscriber_configs {
        let handle = tokio::spawn(async move {
            run_subscriber(sconfig).await
        });
        subscriber_handles.push(handle);
    }

    // Give subscribers time to subscribe
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let bench_start = Instant::now();

    // Spawn publishers
    let mut publisher_handles = Vec::new();
    for pconfig in publisher_configs {
        let handle = tokio::spawn(async move {
            run_publisher(pconfig).await
        });
        publisher_handles.push(handle);
    }

    // Wait for all publishers to finish
    let mut publisher_results = Vec::new();
    for handle in publisher_handles {
        match handle.await {
            Ok(result) => publisher_results.push(result),
            Err(e) => eprintln!("Publisher task panicked: {:?}", e),
        }
    }

    println!("\nAll publishers finished.");

    // Signal subscribers that publishing is done
    *done_flag.lock().await = true;

    // Wait for all subscribers to finish
    let mut subscriber_results = Vec::new();
    for handle in subscriber_handles {
        match handle.await {
            Ok(result) => subscriber_results.push(result),
            Err(e) => eprintln!("Subscriber task panicked: {:?}", e),
        }
    }

    let total_elapsed = bench_start.elapsed();

    // Merge histograms
    let mut merged_histogram = Histogram::<u64>::new_with_bounds(1, 60_000_000_000, 3)
        .expect("Failed to create merged histogram");

    for result in &subscriber_results {
        let _ = merged_histogram.add(&result.latency_histogram);
    }

    let total_sent: u64 = publisher_results.iter().map(|r| r.total_sent).sum();
    let total_received: u64 = subscriber_results.iter().map(|r| r.total_received).sum();
    let total_lost: u64 = subscriber_results.iter().map(|r| r.messages_lost).sum();
    let total_nacks: u64 = subscriber_results.iter().map(|r| r.total_nacks_sent).sum();
    let total_retrans: u64 = subscriber_results.iter().map(|r| r.total_retransmissions).sum();

    BenchmarkResult {
        publisher_results,
        subscriber_results,
        merged_histogram,
        total_messages_sent: total_sent,
        total_messages_received: total_received,
        total_messages_lost: total_lost,
        total_nacks_sent: total_nacks,
        total_retransmissions: total_retrans,
        total_elapsed,
    }
}

pub fn print_report(result: &BenchmarkResult) {
    println!("\n");
    println!("============================================================");
    println!("                    HUGIMQ BENCHMARK REPORT                  ");
    println!("============================================================");
    println!();

    // Publisher summary
    println!("--- Publishers ---");
    let total_sent = result.publisher_results.iter().map(|r| r.total_sent).sum::<u64>();
    let total_pub_throughput: u64 = result.publisher_results.iter().map(|r| r.throughput).sum();
    println!(
        "  Total messages sent:     {}",
        total_sent
    );
    println!(
        "  Aggregate throughput:  {} msg/s",
        total_pub_throughput
    );
    println!();

    // Subscriber summary
    println!("--- Subscribers ---");
    let total_received = result.subscriber_results.iter().map(|r| r.total_received).sum::<u64>();
    let total_lost = result.subscriber_results.iter().map(|r| r.messages_lost).sum::<u64>();
    let total_nacks = result.subscriber_results.iter().map(|r| r.total_nacks_sent).sum::<u64>();
    let total_retrans = result.subscriber_results.iter().map(|r| r.total_retransmissions).sum::<u64>();

    println!(
        "  Total messages received: {}",
        total_received
    );
    println!(
        "  Total messages lost:      {} {}",
        total_lost,
        if total_lost == 0 { "✓ ZERO LOSS" } else { "✗ LOSS DETECTED" }
    );
    println!(
        "  NACKs sent:              {}",
        total_nacks
    );
    println!(
        "  Retransmissions:         {}",
        total_retrans
    );
    println!();

    // Latency stats
    println!("--- Latency (one-way, nanoseconds) ---");
    let h = &result.merged_histogram;
    if h.len() > 0 {
        println!("  P50:   {:>12} ns  ({:.3} ms)", h.value_at_quantile(0.5), h.value_at_quantile(0.5) as f64 / 1_000_000.0);
        println!("  P90:   {:>12} ns  ({:.3} ms)", h.value_at_quantile(0.9), h.value_at_quantile(0.9) as f64 / 1_000_000.0);
        println!("  P99:   {:>12} ns  ({:.3} ms)", h.value_at_quantile(0.99), h.value_at_quantile(0.99) as f64 / 1_000_000.0);
        println!("  P999:  {:>12} ns  ({:.3} ms)", h.value_at_quantile(0.999), h.value_at_quantile(0.999) as f64 / 1_000_000.0);
        println!("  Max:   {:>12} ns  ({:.3} ms)", h.max(), h.max() as f64 / 1_000_000.0);
        println!("  Mean:  {:>12} ns  ({:.3} ms)", h.mean() as u64, h.mean() / 1_000_000.0);
    } else {
        println!("  No latency data recorded.");
    }
    println!();

    // Overall
    println!("--- Overall ---");
    println!(
        "  Benchmark duration:    {:.3} s",
        result.total_elapsed.as_secs_f64()
    );
    let effective_throughput = (total_received as f64 / result.total_elapsed.as_secs_f64()) as u64;
    println!(
        "  Effective throughput:  {} msg/s per subscriber",
        effective_throughput
    );
    println!("============================================================");
}
