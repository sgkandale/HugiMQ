//! HugiMQ Benchmarker — Aeron P2P MDC Rendezvous
use rusteron_client::*;
use rusteron_client::bindings::*;
use rusteron_media_driver::{AeronDriverContext, AeronDriver};
use rusteron_media_driver::bindings::aeron_threading_mode_enum;
use clap::Parser;
use hdrhistogram::Histogram;
use std::ffi::CString;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use std::thread;
use std::sync::Barrier;
use std::net::UdpSocket;

const AERON_TIMEOUT: Duration = Duration::from_secs(10);
const FRAGMENT_LIMIT: usize = 1024;
const STREAM_ID_START: i32 = 200;

#[derive(clap::Parser, Debug)]
struct Args {
    #[arg(short, long, default_value = "both")] mode: String, // producer, consumer, both
    #[arg(short, long, default_value_t = 20)] connections: usize,
    #[arg(short, long, default_value_t = 100000)] messages_per_conn: usize,
    #[arg(short, long, default_value_t = 128)] payload_size: usize,
    #[arg(short, long, default_value = "127.0.0.1")] server_ip: String,
    #[arg(short, long, default_value = "127.0.0.1")] my_ip: String,
}

struct BenchFragmentHandler {
    expected_topic: Vec<u8>,
    e2e_hist: Arc<parking_lot::Mutex<Histogram<u64>>>,
    received_count: Arc<AtomicU64>,
}

impl AeronFragmentHandlerCallback for BenchFragmentHandler {
    #[inline(always)]
    fn handle_aeron_fragment_handler(&mut self, buffer: &[u8], _header: AeronHeader) {
        let mut parts = buffer.splitn(5, |&b| b == b':');
        if let Some(topic) = parts.next() {
            if topic == self.expected_topic {
                let _prod = parts.next();
                let _seq = parts.next();
                if let Some(ts_bytes) = parts.next() {
                    let mut ts: u128 = 0;
                    for &b in ts_bytes { if b >= b'0' && b <= b'9' { ts = ts * 10 + (b - b'0') as u128; } }
                    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
                    let _ = self.e2e_hist.lock().record(now.saturating_sub(ts) as u64);
                }
                self.received_count.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let num_producers = if args.mode == "consumer" { 0 } else { args.connections / 2 };
    let num_consumers = if args.mode == "producer" { 0 } else { args.connections - (args.connections / 2) };
    
    println!("Mode: {} | Server: {} | Connections: {}/{}", args.mode, args.server_ip, num_producers, num_consumers);

    let driver_context = AeronDriverContext::new()?;
    let bench_dir = CString::new(format!("/dev/shm/aeron-bench-{}", args.mode)).unwrap();
    driver_context.set_dir(&bench_dir)?;
    driver_context.set_threading_mode(aeron_threading_mode_enum::AERON_THREADING_MODE_SHARED)?;
    let driver = AeronDriver::new(&driver_context)?;
    driver.start(false)?;

    let total_received = Arc::new(AtomicU64::new(0));
    let e2e_hist = Arc::new(parking_lot::Mutex::new(Histogram::<u64>::new_with_bounds(1, 300_000_000_000, 3).unwrap()));
    
    // Adjust barriers based on mode
    let participant_count = if args.mode == "both" { args.connections } else if args.mode == "producer" { num_producers } else { num_consumers };
    let barrier = Arc::new(Barrier::new(participant_count));
    let start_barrier = Arc::new(Barrier::new(participant_count + 1));
    let mut handles = Vec::new();

    // ─── Consumers ───
    for i in 0..num_consumers {
        let total_received = Arc::clone(&total_received);
        let e2e_hist = Arc::clone(&e2e_hist);
        let b = Arc::clone(&barrier);
        let sb = Arc::clone(&start_barrier);
        let server_ip = args.server_ip.clone();
        let topic_idx = i % 10;
        let bench_dir_clone = bench_dir.clone();
        let expected_topic = format!("topic_{}", topic_idx).into_bytes();
        let msg_count = args.messages_per_conn;
        let num_p = args.connections / 2;
        let num_c = args.connections - num_p;
        let expected = (msg_count * num_p / num_c) as u64;

        handles.push(thread::spawn(move || {
            let ctx = AeronContext::new().unwrap();
            ctx.set_dir(&bench_dir_clone).unwrap();
            let aeron = Aeron::new(&ctx).unwrap();
            aeron.start().unwrap();

            // 1. Discover Producer URI from Directory Server
            let discovery_socket = UdpSocket::bind("0.0.0.0:0").unwrap();
            discovery_socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let sub_req = format!("SUB topic_{}", topic_idx);
            
            let mut producer_uri = String::new();
            while producer_uri.is_empty() {
                discovery_socket.send_to(sub_req.as_bytes(), format!("{}:6379", server_ip)).unwrap();
                let mut buf = [0u8; 1024];
                if let Ok((amt, _)) = discovery_socket.recv_from(&mut buf) {
                    let resp = String::from_utf8_lossy(&buf[..amt]);
                    if resp != "NOT_FOUND" {
                        producer_uri = resp.to_string();
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(500));
            }

            // 2. Subscribe Directly to Producer
            let channel_str = format!("aeron:udp?endpoint=0.0.0.0:0|control={}", producer_uri);
            let sub = aeron.add_subscription::<AeronAvailableImageLogger, AeronUnavailableImageLogger>(
                &CString::new(channel_str).unwrap(), STREAM_ID_START + topic_idx as i32, None, None, AERON_TIMEOUT
            ).unwrap();

            while sub.image_count().unwrap_or(0) == 0 { thread::sleep(Duration::from_millis(10)); }
            b.wait(); sb.wait();

            let handler = BenchFragmentHandler { expected_topic, e2e_hist, received_count: total_received };
            let (mut assembler, mut inner) = Handler::leak_with_fragment_assembler(handler).unwrap();
            let mut my_received = 0;
            while my_received < expected {
                if let Ok(f) = sub.poll(Some(&assembler), FRAGMENT_LIMIT) {
                    if f > 0 { my_received += f as u64; }
                    else { std::hint::spin_loop(); }
                } else { break; }
            }
            assembler.release(); inner.release();
        }));
    }

    // ─── Producers ───
    for i in 0..num_producers {
        let b = Arc::clone(&barrier);
        let sb = Arc::clone(&start_barrier);
        let server_ip = args.server_ip.clone();
        let my_ip = args.my_ip.clone();
        let topic_idx = i % 10;
        let msg_count = args.messages_per_conn;
        let bench_dir_clone = bench_dir.clone();
        let payload_size = args.payload_size;

        handles.push(thread::spawn(move || {
            let ctx = AeronContext::new().unwrap();
            ctx.set_dir(&bench_dir_clone).unwrap();
            let aeron = Aeron::new(&ctx).unwrap();
            aeron.start().unwrap();

            // 1. Bind MDC Control Port
            let control_port = 7000 + topic_idx;
            let control_uri = format!("{}:{}", my_ip, control_port);
            let channel_str = format!("aeron:udp?control-mode=dynamic|control={}", control_uri);
            
            let publ = aeron.add_exclusive_publication(
                &CString::new(channel_str).unwrap(), STREAM_ID_START + topic_idx as i32, AERON_TIMEOUT
            ).unwrap();

            // 2. Register with Directory Server
            let discovery_socket = UdpSocket::bind("0.0.0.0:0").unwrap();
            let pub_msg = format!("PUB topic_{} {}", topic_idx, control_uri);
            discovery_socket.send_to(pub_msg.as_bytes(), format!("{}:6379", server_ip)).unwrap();

            while !publ.is_connected() { thread::sleep(Duration::from_millis(10)); }
            b.wait(); sb.wait();

            let topic_name = format!("topic_{}", topic_idx);
            let body = "a".repeat(payload_size);
            for seq in 0..msg_count {
                let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
                let data = format!("{}:{}:{}:{}:{}", topic_name, i, seq, now, body);
                let bytes = data.as_bytes();
                loop {
                    let result = publ.offer::<AeronReservedValueSupplierLogger>(bytes, None);
                    if result >= 0 { break; }
                    let status = result as i32;
                    if status == AERON_PUBLICATION_BACK_PRESSURED || status == AERON_PUBLICATION_ADMIN_ACTION { 
                        std::hint::spin_loop(); continue; 
                    }
                    std::hint::spin_loop();
                }
            }
            thread::sleep(Duration::from_millis(1000));
        }));
    }

    if participant_count > 0 {
        thread::sleep(Duration::from_secs(2));
        start_barrier.wait();
        let global_start = Instant::now();
        let total_expected_local = (num_producers as u64) * (args.messages_per_conn as u64);
        
        if args.mode != "producer" {
            let total_received_monitor = Arc::clone(&total_received);
            let monitor = thread::spawn(move || {
                let mut last = 0;
                while total_received_monitor.load(Ordering::Relaxed) < total_expected_local || total_expected_local == 0 {
                    thread::sleep(Duration::from_secs(1));
                    let curr = total_received_monitor.load(Ordering::Relaxed);
                    println!("Throughput: {} msg/s | Total: {}/{}", curr - last, curr, total_expected_local);
                    last = curr;
                    if global_start.elapsed().as_secs() > 120 { break; }
                }
            });
            let _ = monitor.join();
        } else {
            // Producer only mode: just wait for threads
            for h in handles { let _ = h.join(); }
        }

        let duration = global_start.elapsed();
        let total = total_received.load(Ordering::Relaxed);
        if total > 0 {
            println!("\nFinal Results:\nThroughput: {:.0} msg/s\nReceived: {}/{}", total as f64 / duration.as_secs_f64(), total, total_expected_local);
            let h = e2e_hist.lock();
            if h.len() > 0 { println!("P50: {} ns\nP99: {} ns", h.value_at_quantile(0.5), h.value_at_quantile(0.99)); }
        }
    }

    let _ = driver.close();
    Ok(())
}
