//! HugiMQ Benchmarker — Step 4: True Zero-Allocation Producer
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

const AERON_TIMEOUT: Duration = Duration::from_secs(10);
const FRAGMENT_LIMIT: usize = 1024;
const INGEST_STREAM_ID: i32 = 10;

#[derive(clap::Parser, Debug)]
struct Args {
    #[arg(short, long, default_value_t = 20)] connections: usize,
    #[arg(short, long, default_value_t = 100000)] messages_per_conn: usize,
    #[arg(short, long, default_value_t = 128)] payload_size: usize,
    #[arg(short, long, default_value = "127.0.0.1")] server_ip: String,
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

// Fast integer to ASCII helper
fn write_int(buf: &mut [u8], mut val: u128) -> usize {
    if val == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut i = 0;
    while val > 0 {
        buf[i] = (val % 10) as u8 + b'0';
        val /= 10;
        i += 1;
    }
    buf[..i].reverse();
    i
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let num_producers = args.connections / 2;
    let num_consumers = args.connections - num_producers;
    let total_expected = (num_producers as u64) * (args.messages_per_conn as u64);

    let driver_context = AeronDriverContext::new()?;
    let bench_dir = CString::new("/dev/shm/aeron-benchmarker").unwrap();
    driver_context.set_dir(&bench_dir)?;
    driver_context.set_threading_mode(aeron_threading_mode_enum::AERON_THREADING_MODE_SHARED)?;
    let driver = AeronDriver::new(&driver_context)?;
    driver.start(false)?;

    let total_received = Arc::new(AtomicU64::new(0));
    let e2e_hist = Arc::new(parking_lot::Mutex::new(Histogram::<u64>::new_with_bounds(1, 300_000_000_000, 3).unwrap()));
    let barrier = Arc::new(Barrier::new(args.connections));
    let start_barrier = Arc::new(Barrier::new(args.connections + 1));
    let mut handles = Vec::new();

    for i in 0..num_consumers {
        let total_received = Arc::clone(&total_received);
        let e2e_hist = Arc::clone(&e2e_hist);
        let b = Arc::clone(&barrier);
        let sb = Arc::clone(&start_barrier);
        let server_ip = args.server_ip.clone();
        let topic_idx = i % 10;
        let bench_dir_clone = bench_dir.clone();
        let expected_topic = format!("topic_{}", topic_idx).into_bytes();

        handles.push(thread::spawn(move || {
            let core_ids = core_affinity::get_core_ids().unwrap_or_default();
            if core_ids.len() > (i + 11) { core_affinity::set_for_current(core_ids[i + 11]); }
            let ctx = AeronContext::new().unwrap();
            ctx.set_dir(&bench_dir_clone).unwrap();
            let aeron = Aeron::new(&ctx).unwrap();
            aeron.start().unwrap();
            let sub = aeron.add_subscription::<AeronAvailableImageLogger, AeronUnavailableImageLogger>(
                &CString::new(format!("aeron:udp?control={}:6380|control-mode=dynamic", server_ip)).unwrap(), 200 + topic_idx as i32, None, None, AERON_TIMEOUT
            ).unwrap();
            while sub.image_count().unwrap_or(0) == 0 { thread::sleep(Duration::from_millis(10)); }
            b.wait(); sb.wait();
            let handler = BenchFragmentHandler { expected_topic, e2e_hist, received_count: total_received };
            let (mut assembler, mut inner) = Handler::leak_with_fragment_assembler(handler).unwrap();
            let expected = (args.messages_per_conn * num_producers / num_consumers) as u64;
            let mut my_received = 0;
            while my_received < expected {
                if let Ok(f) = sub.poll(Some(&assembler), FRAGMENT_LIMIT) { my_received += f as u64; }
                else { std::hint::spin_loop(); }
            }
            assembler.release(); inner.release();
        }));
    }

    for i in 0..num_producers {
        let b = Arc::clone(&barrier);
        let sb = Arc::clone(&start_barrier);
        let server_ip = args.server_ip.clone();
        let topic_idx = i % 10;
        let msg_count = args.messages_per_conn;
        let bench_dir_clone = bench_dir.clone();
        let payload_size = args.payload_size;

        handles.push(thread::spawn(move || {
            let core_ids = core_affinity::get_core_ids().unwrap_or_default();
            if core_ids.len() > (i + 21) { core_affinity::set_for_current(core_ids[i + 21]); }
            let ctx = AeronContext::new().unwrap();
            ctx.set_dir(&bench_dir_clone).unwrap();
            let aeron = Aeron::new(&ctx).unwrap();
            aeron.start().unwrap();
            let publ = aeron.add_exclusive_publication(
                &CString::new(format!("aeron:udp?endpoint={}:6379", server_ip)).unwrap(), INGEST_STREAM_ID, AERON_TIMEOUT
            ).unwrap();
            while !publ.is_connected() { thread::sleep(Duration::from_millis(10)); }
            b.wait(); sb.wait();
            
            let topic_name = format!("topic_{}", topic_idx);
            let body = "a".repeat(payload_size);
            
            // Pre-allocate buffer for the frame
            let mut frame = vec![0u8; 1024];
            
            for seq in 0..msg_count {
                let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
                
                // Manual Zero-Allocation Frame Building:
                // [1B Topic Index] + "topic_N:prod_id:seq:timestamp:body"
                frame[0] = topic_idx as u8;
                let mut pos = 1;
                
                let topic_bytes = topic_name.as_bytes();
                frame[pos..pos+topic_bytes.len()].copy_from_slice(topic_bytes);
                pos += topic_bytes.len();
                frame[pos] = b':'; pos += 1;
                
                pos += write_int(&mut frame[pos..], i as u128);
                frame[pos] = b':'; pos += 1;
                
                pos += write_int(&mut frame[pos..], seq as u128);
                frame[pos] = b':'; pos += 1;
                
                pos += write_int(&mut frame[pos..], now);
                frame[pos] = b':'; pos += 1;
                
                let body_bytes = body.as_bytes();
                frame[pos..pos+body_bytes.len()].copy_from_slice(body_bytes);
                pos += body_bytes.len();

                let current_frame = &frame[..pos];

                loop {
                    let result = publ.offer::<AeronReservedValueSupplierLogger>(current_frame, None);
                    if result >= 0 { break; }
                    if result == AERON_PUBLICATION_BACK_PRESSURED as i64 || result == AERON_PUBLICATION_ADMIN_ACTION as i64 { 
                        std::hint::spin_loop(); continue; 
                    }
                    break;
                }
            }
            thread::sleep(Duration::from_millis(500));
        }));
    }

    thread::sleep(Duration::from_secs(1));
    start_barrier.wait();
    let global_start = Instant::now();
    let total_received_monitor = Arc::clone(&total_received);
    let monitor = thread::spawn(move || {
        let mut last = 0;
        while total_received_monitor.load(Ordering::Relaxed) < total_expected {
            thread::sleep(Duration::from_secs(1));
            let curr = total_received_monitor.load(Ordering::Relaxed);
            println!("Throughput: {} msg/s | Total: {}/{}", curr - last, curr, total_expected);
            last = curr;
            if global_start.elapsed().as_secs() > 60 { break; }
        }
    });
    for h in handles { let _ = h.join(); }
    let _ = monitor.join();
    let duration = global_start.elapsed();
    let total = total_received.load(Ordering::Relaxed);
    println!("\nFinal Results:\nThroughput: {:.0} msg/s\nReceived: {}/{}", total as f64 / duration.as_secs_f64(), total, total_expected);
    let h = e2e_hist.lock();
    if h.len() > 0 { println!("P50: {} ns\nP99: {} ns", h.value_at_quantile(0.5), h.value_at_quantile(0.99)); }
    let _ = driver.close();
    Ok(())
}
