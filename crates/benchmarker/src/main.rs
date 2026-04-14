//! HugiMQ Benchmarker — Aeron UDP Multiplexed benchmark
//!
//! Architecture:
//!   - Single Media Driver for benchmarker
//!   - All producers use Port 6379, Stream 100+topic_idx
//!   - All consumers use Port 6380, Stream 200+topic_idx
//!   - Payload: "{topic_name}:{prod_id}:{seq}:{timestamp}:{body}"

use rusteron_client::*;
use rusteron_client::bindings::*;
use rusteron_media_driver::{AeronDriverContext, AeronDriver};
use clap::Parser;
use hdrhistogram::Histogram;
use std::ffi::CString;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use std::thread;
use std::sync::Barrier;

// ─── Configuration ───
const INGEST_CHANNEL: &str = "aeron:udp?endpoint=127.0.0.1:6379";
const DELIVERY_CHANNEL_BASE: &str = "aeron:udp?control=127.0.0.1:6380|control-mode=dynamic";
const AERON_TIMEOUT: Duration = Duration::from_secs(10);
const FRAGMENT_LIMIT: usize = 1024;

// ─── CLI Args ───
#[derive(clap::Parser, Debug)]
#[command(author, version, about = "HugiMQ Aeron UDP Multiplexed benchmarker")]
struct Args {
    #[arg(short, long, default_value_t = 20)]
    connections: usize,

    #[arg(short, long, default_value_t = 100000)]
    messages_per_conn: usize,

    #[arg(short, long, default_value_t = 128)]
    payload_size: usize,

    #[arg(short, long, default_value = "127.0.0.1")]
    server_ip: String,
}

struct BenchFragmentHandler {
    expected_topic: String,
    e2e_hist: Arc<parking_lot::Mutex<Histogram<u64>>>,
    received_count: Arc<AtomicU64>,
}

impl AeronFragmentHandlerCallback for BenchFragmentHandler {
    fn handle_aeron_fragment_handler(&mut self, buffer: &[u8], _header: AeronHeader) {
        let payload_str = String::from_utf8_lossy(buffer);
        let parts: Vec<&str> = payload_str.splitn(5, ':').collect();
        
        if parts.len() < 4 { return; }

        let received_topic = parts[0];
        if received_topic != self.expected_topic {
            eprintln!("TOPIC MISMATCH! Expected {}, received {}", self.expected_topic, received_topic);
            return;
        }

        if let Ok(sent_at) = parts[3].parse::<u128>() {
            let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
            let latency = now.saturating_sub(sent_at) as u64;
            let mut hist = self.e2e_hist.lock();
            let _ = hist.record(latency);
        }
        
        self.received_count.fetch_add(1, Ordering::Relaxed);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let num_producers = args.connections / 2;
    let num_consumers = args.connections - num_producers;
    let total_expected = (num_producers as u64) * (args.messages_per_conn as u64);

    println!("Target: Aeron UDP Multiplexed | Server: {} | Connections: {} | Total: {}", args.server_ip, args.connections, total_expected);

    println!("Starting local Aeron media driver...");
    let driver_context = AeronDriverContext::new()?;
    let bench_dir_path = "/dev/shm/aeron-benchmarker";
    let bench_dir = CString::new(bench_dir_path).unwrap();
    driver_context.set_dir(&bench_dir)?;
    let driver = AeronDriver::new(&driver_context)?;
    driver.start(false)?;

    let total_received = Arc::new(AtomicU64::new(0));
    let e2e_hist = Arc::new(parking_lot::Mutex::new(Histogram::<u64>::new_with_bounds(1, 300_000_000_000, 3).unwrap()));
    let barrier = Arc::new(Barrier::new(args.connections));
    let start_barrier = Arc::new(Barrier::new(args.connections + 1));
    let payload_body = "a".repeat(args.payload_size);
    let mut handles = Vec::new();

    // ─── Consumers ───
    for consumer_id in 0..num_consumers {
        let total_received = Arc::clone(&total_received);
        let e2e_hist = Arc::clone(&e2e_hist);
        let b = Arc::clone(&barrier);
        let sb = Arc::clone(&start_barrier);
        let server_ip = args.server_ip.clone();
        let topic_idx = (consumer_id % 10) as i32;
        let stream_id = 200 + topic_idx;
        let expected = (args.messages_per_conn * num_producers / num_consumers) as u64;
        let bench_dir_clone = bench_dir.clone();
        let expected_topic = format!("topic_{}", topic_idx);

        let handle = thread::spawn(move || {
            let core_ids = core_affinity::get_core_ids().unwrap_or_default();
            if core_ids.len() > (consumer_id + 11) { core_affinity::set_for_current(core_ids[consumer_id + 11]); }

            let aeron_context = AeronContext::new().unwrap();
            aeron_context.set_dir(&bench_dir_clone).unwrap();
            let aeron = Aeron::new(&aeron_context).unwrap();
            aeron.start().unwrap();

            let channel_str = format!("aeron:udp?control={}:6380|control-mode=dynamic", server_ip);
            let channel = CString::new(channel_str).unwrap();
            let subscription = aeron.add_subscription::<AeronAvailableImageLogger, AeronUnavailableImageLogger>(&channel, stream_id, None, None, AERON_TIMEOUT).unwrap();

            while subscription.image_count().unwrap_or(0) == 0 { thread::sleep(Duration::from_millis(10)); }
            b.wait();
            sb.wait();

            let handler = BenchFragmentHandler { 
                expected_topic,
                e2e_hist, 
                received_count: total_received 
            };
            let (mut assembler_handler, mut inner) = Handler::leak_with_fragment_assembler(handler).unwrap();

            let mut my_received = 0;
            let mut idle_counter = 0;
            while my_received < expected {
                let fragments = subscription.poll(Some(&assembler_handler), FRAGMENT_LIMIT).unwrap_or(0);
                if fragments > 0 {
                    my_received += fragments as u64;
                    idle_counter = 0;
                } else {
                    idle_counter += 1;
                    if idle_counter < 1000 { std::hint::spin_loop(); }
                    else if idle_counter < 10000 { thread::yield_now(); }
                    else { thread::sleep(Duration::from_micros(100)); }
                }
            }
            assembler_handler.release();
            inner.release();
        });
        handles.push(handle);
    }

    // ─── Producers ───
    for prod_id in 0..num_producers {
        let b = Arc::clone(&barrier);
        let sb = Arc::clone(&start_barrier);
        let server_ip = args.server_ip.clone();
        let topic_idx = (prod_id % 10) as i32;
        let topic_name = format!("topic_{}", topic_idx);
        let stream_id = 100 + topic_idx;
        let msg_count = args.messages_per_conn;
        let local_payload = payload_body.clone();
        let bench_dir_clone = bench_dir.clone();

        let handle = thread::spawn(move || {
            let core_ids = core_affinity::get_core_ids().unwrap_or_default();
            if core_ids.len() > (prod_id + 21) { core_affinity::set_for_current(core_ids[prod_id + 21]); }

            let aeron_context = AeronContext::new().unwrap();
            aeron_context.set_dir(&bench_dir_clone).unwrap();
            let aeron = Aeron::new(&aeron_context).unwrap();
            aeron.start().unwrap();

            let channel_str = format!("aeron:udp?endpoint={}:6379", server_ip);
            let channel = CString::new(channel_str).unwrap();
            let publication = aeron.add_exclusive_publication(&channel, stream_id, AERON_TIMEOUT).unwrap();

            while !publication.is_connected() { thread::sleep(Duration::from_millis(10)); }
            b.wait();
            sb.wait();

            for seq in 0..msg_count {
                let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
                let data = format!("{}:{}:{}:{}:{}", topic_name, prod_id, seq, now, local_payload);
                let frame = data.as_bytes();

                loop {
                    let result = publication.offer::<AeronReservedValueSupplierLogger>(frame, None);
                    if result >= 0 { break; }
                    let status = result as i32;
                    if status == AERON_PUBLICATION_BACK_PRESSURED || status == AERON_PUBLICATION_ADMIN_ACTION { 
                        std::hint::spin_loop(); 
                        continue; 
                    }
                    std::hint::spin_loop();
                }
            }
            thread::sleep(Duration::from_millis(500));
        });
        handles.push(handle);
    }

    thread::sleep(Duration::from_secs(1));
    start_barrier.wait();
    let global_start = Instant::now();

    let total_received_monitor = Arc::clone(&total_received);
    let monitor_handle = thread::spawn(move || {
        let mut last_count = 0;
        while total_received_monitor.load(Ordering::Relaxed) < total_expected {
            thread::sleep(Duration::from_secs(1));
            let current = total_received_monitor.load(Ordering::Relaxed);
            println!("Throughput: {} msg/s | Total: {}/{}", current - last_count, current, total_expected);
            last_count = current;
            if global_start.elapsed() > Duration::from_secs(120) { break; }
        }
    });

    for h in handles { let _ = h.join(); }
    let _ = monitor_handle.join();

    let duration = global_start.elapsed();
    let total_count = total_received.load(Ordering::Relaxed);
    println!("\nFinal Results:\nThroughput: {:.0} msg/s\nReceived: {}/{}", total_count as f64 / duration.as_secs_f64(), total_count, total_expected);
    let h = e2e_hist.lock();
    if h.len() > 0 { println!("P50: {} ns\nP99: {} ns", h.value_at_quantile(0.5), h.value_at_quantile(0.99)); }

    let _ = driver.close();
    Ok(())
}
