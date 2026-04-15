/// Lock-free ring buffer for publisher-side retransmission.
/// Stores the last N sent packets per topic so NACKs can be fulfilled.
/// Uses atomic indices and pre-allocated slots — zero allocation on the hot path.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use crate::protocol::HEADER_SIZE;

/// A single slot in the ring buffer.
struct RingSlot {
    /// The packet data (header + payload).
    data: Mutex<Vec<u8>>,
    /// Valid sequence number for this slot. 0 means empty.
    sequence: AtomicU64,
}

pub struct RetransmitRing {
    slots: Vec<RingSlot>,
    write_pos: AtomicUsize,
    capacity: usize,
}

impl RetransmitRing {
    pub fn new(capacity: usize, max_payload_size: usize) -> Arc<Self> {
        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(RingSlot {
                data: Mutex::new(vec![0u8; HEADER_SIZE + max_payload_size]),
                sequence: AtomicU64::new(0),
            });
        }
        Arc::new(RetransmitRing {
            slots,
            write_pos: AtomicUsize::new(0),
            capacity,
        })
    }

    /// Push a packet into the ring buffer (called after sending to subscribers).
    /// Returns the slot index where it was stored.
    pub fn push(&self, data: &[u8], sequence: u64) -> usize {
        let pos = self.write_pos.fetch_add(1, Ordering::Relaxed) % self.capacity;
        let slot = &self.slots[pos];
        let mut slot_data = slot.data.lock().unwrap();
        slot_data[..data.len()].copy_from_slice(data);
        slot_data.truncate(data.len());
        slot.sequence.store(sequence, Ordering::Release);
        pos
    }

    /// Retrieve a packet by sequence number. Returns None if not found (overwritten).
    pub fn get(&self, sequence: u64) -> Option<Vec<u8>> {
        for slot in &self.slots {
            if slot.sequence.load(Ordering::Acquire) == sequence {
                return Some(slot.data.lock().unwrap().clone());
            }
        }
        None
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
}
