use rusteron_client::*;
use rusteron_client::bindings::*;
use rusteron_media_driver::{AeronDriverContext, AeronDriver};
use rusteron_media_driver::bindings::aeron_threading_mode_enum;
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::thread;
use tracing::{info, warn, error};

#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

// ─── Configuration ───
const INGEST_CHANNEL: &str = "aeron:udp?endpoint=0.0.0.0:6379";
const DELIVERY_CHANNEL: &str = "aeron:udp?control-mode=dynamic|control=0.0.0.0:6380";
const NUM_TOPICS: i32 = 10;
const INGEST_STREAM_ID_START: i32 = 100;
const DELIVERY_STREAM_ID_START: i32 = 200;
const AERON_TIMEOUT: Duration = Duration::from_secs(10);
const FRAGMENT_LIMIT: usize = 1024;

struct RelayHandler {
    publication: AeronExclusivePublication,
}

impl AeronFragmentHandlerCallback for RelayHandler {
    fn handle_aeron_fragment_handler(&mut self, buffer: &[u8], _header: AeronHeader) {
        loop {
            let result = self.publication.offer::<AeronReservedValueSupplierLogger>(buffer, None);
            if result >= 0 {
                break;
            }
            let status = result as i32;
            if status == AERON_PUBLICATION_BACK_PRESSURED 
                || status == AERON_PUBLICATION_NOT_CONNECTED 
                || status == AERON_PUBLICATION_ADMIN_ACTION 
            {
                std::hint::spin_loop();
                continue;
            }
            // If closed or other fatal error, we have to break to avoid infinite loop
            break;
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    info!("Starting HugiMQ Aeron UDP Server...");

    let server_dir_path = "/dev/shm/aeron-server";
    let server_dir = CString::new(server_dir_path).unwrap();

    // 1. Start a single Media Driver for the server
    let driver_context = AeronDriverContext::new()?;
    driver_context.set_dir(&server_dir)?;
    // Use DEDICATED threading mode for max performance
    driver_context.set_threading_mode(aeron_threading_mode_enum::AERON_THREADING_MODE_DEDICATED)?;
    // Large term buffers to reduce context switching during burst traffic
    std::env::set_var("AERON_TERM_BUFFER_LENGTH", "67108864");  // 64MB
    std::env::set_var("AERON_IPC_TERM_BUFFER_LENGTH", "67108864");  // 64MB
    // Increase socket buffer sizes for UDP throughput
    std::env::set_var("AERON_SOCKET_SO_RCVBUF", "67108864");  // 64MB
    std::env::set_var("AERON_SOCKET_SO_SNDBUF", "67108864");  // 64MB
    
    let driver = AeronDriver::new(&driver_context)?;
    driver.start(false)?;
    info!("Aeron Media Driver (Dedicated) started at {}", server_dir_path);

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })?;

    let mut thread_handles = Vec::new();

    for i in 0..NUM_TOPICS {
        let running_clone = running.clone();
        let server_dir_clone = server_dir.clone();
        
        let handle = thread::spawn(move || {
            let ingest_stream_id = INGEST_STREAM_ID_START + i;
            let delivery_stream_id = DELIVERY_STREAM_ID_START + i;

            // Pin to core (Start from 1, core 0 reserved for driver threads if possible)
            let core_ids = core_affinity::get_core_ids().unwrap_or_default();
            if core_ids.len() > (i as usize + 1) {
                core_affinity::set_for_current(core_ids[i as usize + 1]);
            }

            // Each thread gets its own Aeron client instance connecting to the shared driver
            let aeron_context = AeronContext::new().expect("Failed to create AeronContext");
            aeron_context.set_dir(&server_dir_clone).expect("Failed to set dir");
            let aeron = Aeron::new(&aeron_context).expect("Failed to create Aeron");
            aeron.start().expect("Failed to start Aeron");

            let ingest_channel = CString::new(INGEST_CHANNEL).unwrap();
            let delivery_channel = CString::new(DELIVERY_CHANNEL).unwrap();

            let subscription = aeron.add_subscription::<AeronAvailableImageLogger, AeronUnavailableImageLogger>(
                &ingest_channel,
                ingest_stream_id,
                None,
                None,
                AERON_TIMEOUT,
            ).expect("Failed to add subscription");

            let publication = aeron.add_exclusive_publication(
                &delivery_channel,
                delivery_stream_id,
                AERON_TIMEOUT,
            ).expect("Failed to add publication");

            // Wait for at least one subscriber on the delivery path
            while !publication.is_connected() && running_clone.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(10));
            }

            let handler = RelayHandler { publication: publication.clone() };
            let (mut assembler_handler, mut inner) = Handler::leak_with_fragment_assembler(handler).expect("Failed to leak handler");

            info!("Topic {} relay active: Ingest Stream {} -> Delivery Stream {}", i, ingest_stream_id, delivery_stream_id);

            let mut idle_counter = 0;
            while running_clone.load(Ordering::Relaxed) {
                match subscription.poll(Some(&assembler_handler), FRAGMENT_LIMIT) {
                    Ok(fragments) => {
                        if fragments > 0 {
                            idle_counter = 0;
                        } else {
                            idle_counter += 1;
                            if idle_counter < 1000 {
                                std::hint::spin_loop();
                            } else if idle_counter < 10000 {
                                thread::yield_now();
                            } else {
                                thread::sleep(Duration::from_micros(100));
                                idle_counter = 0;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }

            assembler_handler.release();
            inner.release();
        });
        
        thread_handles.push(handle);
    }

    while running.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(100));
    }

    for h in thread_handles {
        let _ = h.join();
    }

    info!("Server shutting down...");
    let _ = driver.close();

    Ok(())
}
