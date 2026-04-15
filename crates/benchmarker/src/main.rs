mod protocol;
mod publisher;
mod subscriber;
mod orchestrator;

use clap::Parser;
use std::net::SocketAddr;

#[derive(Parser, Debug)]
#[command(name = "hugimq-benchmarker")]
#[command(about = "Benchmark HugiMQ UDP pub/sub server")]
struct Cli {
    /// HugiMQ server address
    #[arg(short, long, default_value = "127.0.0.1:6380")]
    server: String,

    /// Number of publishers
    #[arg(short, long, default_value = "10")]
    publishers: u32,

    /// Number of subscribers
    #[arg(long, default_value = "10")]
    subscribers: u32,

    /// Number of topics
    #[arg(short, long, default_value = "10")]
    topics: u32,

    /// Messages per topic per publisher
    #[arg(short = 'm', long, default_value = "100")]
    messages: u64,

    /// Run multiple throughput levels sequentially
    #[arg(long)]
    sweep: bool,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let server_addr: SocketAddr = cli.server.parse().expect("Invalid server address");

    if cli.sweep {
        // Run escalating throughput levels: 100, 1000, 10000, 50000, 100000
        let levels = [100u64, 1_000, 10_000, 50_000, 100_000];
        for &level in &levels {
            println!("\n");
            println!("sweep level: {} messages per topic", level);

            let config = orchestrator::BenchmarkConfig {
                num_publishers: cli.publishers,
                num_subscribers: cli.subscribers,
                num_topics: cli.topics,
                messages_per_topic: level,
                server_addr,
            };

            let result = orchestrator::run_benchmark(config).await;
            orchestrator::print_report(&result);

            // Cool-down between runs
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    } else {
        let config = orchestrator::BenchmarkConfig {
            num_publishers: cli.publishers,
            num_subscribers: cli.subscribers,
            num_topics: cli.topics,
            messages_per_topic: cli.messages,
            server_addr,
        };

        let result = orchestrator::run_benchmark(config).await;
        orchestrator::print_report(&result);
    }
}
