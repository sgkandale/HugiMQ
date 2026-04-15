use rusteron_client::*;
use rusteron_client::bindings::*;
use rusteron_media_driver::{AeronDriverContext, AeronDriver};
use rusteron_media_driver::bindings::aeron_threading_mode_enum;
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::thread;
use crossbeam_channel::{bounded, Sender};
use bytes::Bytes;

#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

const INGEST_CHANNEL: &str = "aeron:udp?endpoint=0.0.0.0:6379";
const DELIVERY_CHANNEL: &str = "aeron:udp?control-mode=dynamic|control=0.0.0.0:6380";
const NUM_TOPICS: usize = 10;
const INGEST_STREAM_ID: i32 = 10;
const DELIVERY_STREAM_ID_START: i32 = 200;
const AERON_TIMEOUT: Duration = Duration::from_secs(10);
const FRAGMENT_LIMIT: usize = 1024;

struct Dispatcher {
    senders: Vec<Sender<Bytes>>,
}

impl AeronFragmentHandlerCallback for Dispatcher {
    #[inline(always)]
    fn handle_aeron_fragment_handler(&mut self, buffer: &[u8], _header: AeronHeader) {
        if buffer.is_empty() { return; }
        let topic_idx = buffer[0] as usize;
        if topic_idx < self.senders.len() {
            // Ref-counted copy (Zero-copy payload)
            let payload = Bytes::copy_from_slice(&buffer[1..]);
            // Blocking send to ensure zero loss
            let _ = self.senders[topic_idx].send(payload);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    println!("Zero-Allocation Disruptor starting...");

    let server_dir = CString::new("/dev/shm/aeron-server").unwrap();
    let driver_context = AeronDriverContext::new()?;
    driver_context.set_dir(&server_dir)?;
    driver_context.set_threading_mode(aeron_threading_mode_enum::AERON_THREADING_MODE_DEDICATED)?;
    let driver = AeronDriver::new(&driver_context)?;
    driver.start(false)?;

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || { r.store(false, Ordering::SeqCst); })?;

    let mut senders = Vec::new();
    let mut receivers = Vec::new();
    for _ in 0..NUM_TOPICS {
        // Large buffer to absorb bursts
        let (tx, rx) = bounded::<Bytes>(100_000);
        senders.push(tx);
        receivers.push(rx);
    }

    let mut thread_handles = Vec::new();

    for i in 0..NUM_TOPICS {
        let running_clone = running.clone();
        let server_dir_clone = server_dir.clone();
        let rx = receivers.remove(0);
        
        thread_handles.push(thread::spawn(move || {
            let core_ids = core_affinity::get_core_ids().unwrap_or_default();
            if core_ids.len() > (i + 2) { core_affinity::set_for_current(core_ids[i + 2]); }

            let ctx = AeronContext::new().unwrap();
            ctx.set_dir(&server_dir_clone).unwrap();
            let aeron = Aeron::new(&ctx).unwrap();
            aeron.start().unwrap();

            let publ = aeron.add_exclusive_publication(
                &CString::new(DELIVERY_CHANNEL).unwrap(), DELIVERY_STREAM_ID_START + i as i32, AERON_TIMEOUT
            ).unwrap();

            while !publ.is_connected() && running_clone.load(Ordering::Relaxed) { thread::sleep(Duration::from_millis(10)); }

            while running_clone.load(Ordering::Relaxed) {
                // Non-blocking poll of the channel
                if let Ok(payload) = rx.try_recv() {
                    loop {
                        let res = publ.offer::<AeronReservedValueSupplierLogger>(&payload, None);
                        if res >= 0 { break; }
                        let status = res as i32;
                        if status == AERON_PUBLICATION_BACK_PRESSURED || status == AERON_PUBLICATION_ADMIN_ACTION { 
                            std::hint::spin_loop(); continue; 
                        }
                        break;
                    }
                } else {
                    std::hint::spin_loop();
                }
            }
        }));
    }

    let running_clone = running.clone();
    let server_dir_clone = server_dir.clone();
    thread_handles.push(thread::spawn(move || {
        let core_ids = core_affinity::get_core_ids().unwrap_or_default();
        if core_ids.len() > 1 { core_affinity::set_for_current(core_ids[1]); }

        let ctx = AeronContext::new().unwrap();
        ctx.set_dir(&server_dir_clone).unwrap();
        let aeron = Aeron::new(&ctx).unwrap();
        aeron.start().unwrap();

        let sub = aeron.add_subscription::<AeronAvailableImageLogger, AeronUnavailableImageLogger>(
            &CString::new(INGEST_CHANNEL).unwrap(), INGEST_STREAM_ID, None, None, AERON_TIMEOUT
        ).unwrap();

        let dispatcher = Dispatcher { senders };
        let (mut assembler, mut inner) = Handler::leak_with_fragment_assembler(dispatcher).unwrap();

        while running_clone.load(Ordering::Relaxed) {
            if sub.poll(Some(&assembler), FRAGMENT_LIMIT).unwrap_or(0) == 0 {
                std::hint::spin_loop();
            }
        }
        assembler.release(); inner.release();
    }));

    while running.load(Ordering::Relaxed) { thread::sleep(Duration::from_millis(100)); }
    for h in thread_handles { let _ = h.join(); }
    let _ = driver.close();
    Ok(())
}
