/// Lock-free SPSC (Single-Producer Single-Consumer) Ring Buffer
///
/// Memory layout:
/// [head: AtomicU64] [padding: 56 bytes] [tail: AtomicU64] [padding: 56 bytes] [data: capacity slots]
///
/// This ensures head and tail are on separate cache lines (64 bytes each),
/// preventing false sharing between producer and consumer threads.
///
/// Memory Ordering:
/// - Producer: writes data → Release store to tail
/// - Consumer: Acquire load of tail → reads data → Release store to head
/// - Producer: Acquire load of head to check available space

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use parking_lot::{Condvar, Mutex};

/// Cache line size for alignment (typical x86/ARM value)
const CACHE_LINE_SIZE: usize = 64;

pub struct SpscRingBuffer {
    /// Consumer reads from here — on its own cache line
    head: AtomicU64,
    _pad1: [u8; CACHE_LINE_SIZE - 8],

    /// Producer writes to here — on its own cache line
    tail: AtomicU64,
    _pad2: [u8; CACHE_LINE_SIZE - 8],

    /// Pre-allocated buffer slots — interior mutability via UnsafeCell
    buffer: Box<[UnsafeCell<Arc<Vec<u8>>>]>,

    /// Condvar for producer blocking when ring is full
    not_full: (Mutex<()>, Condvar),

    capacity: u64,
}

unsafe impl Send for SpscRingBuffer {}
unsafe impl Sync for SpscRingBuffer {}

impl SpscRingBuffer {
    /// Create a new ring buffer with the given capacity.
    /// Capacity must be a power of 2 for efficient modulo arithmetic.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity.is_power_of_two(), "capacity must be power of 2");
        assert!(capacity > 0, "capacity must be > 0");

        let buffer: Box<[UnsafeCell<Arc<Vec<u8>>>]> = (0..capacity)
            .map(|_| UnsafeCell::new(Arc::new(Vec::new())))
            .collect();

        Self {
            head: AtomicU64::new(0),
            _pad1: [0; CACHE_LINE_SIZE - 8],
            tail: AtomicU64::new(0),
            _pad2: [0; CACHE_LINE_SIZE - 8],
            buffer,
            not_full: (Mutex::new(()), Condvar::new()),
            capacity: capacity as u64,
        }
    }

    /// Push a message, blocking until space is available.
    /// This is the correct backpressure mechanism — the producer parks
    /// on a condvar and is woken by the consumer when space is freed.
    #[inline]
    pub fn push(&self, message: Arc<Vec<u8>>) {
        loop {
            let tail = self.tail.load(Ordering::Relaxed);
            let head = self.head.load(Ordering::Acquire);

            if tail.wrapping_sub(head) < self.buffer.len() as u64 {
                // Space available — write and publish
                let mask = (self.buffer.len() - 1) as u64;
                unsafe {
                    *self.buffer[(tail & mask) as usize].get() = message;
                }
                self.tail.store(tail.wrapping_add(1), Ordering::Release);
                return;
            }

            // Ring full — block on condvar
            let mut guard = self.not_full.0.lock();
            // Re-check after acquiring lock
            let tail2 = self.tail.load(Ordering::Relaxed);
            let head2 = self.head.load(Ordering::Acquire);
            if tail2.wrapping_sub(head2) < self.buffer.len() as u64 {
                // Space became available while we were waiting
                drop(guard);
                continue;
            }
            // Wait for consumer to drain
            self.not_full.1.wait(&mut guard);
        }
    }

    /// Wake up any producer blocked on push().
    /// Call this after try_pop() to signal that space is available.
    #[inline]
    pub fn notify_not_full(&self) {
        self.not_full.1.notify_all();
    }

    /// Try to push a message into the ring buffer.
    /// Returns `Ok(())` on success, `Err(message)` if the buffer is full.
    #[inline]
    pub fn try_push(&self, message: Arc<Vec<u8>>) -> Result<(), Arc<Vec<u8>>> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);

        if tail.wrapping_sub(head) >= self.buffer.len() as u64 {
            return Err(message);
        }

        let mask = (self.buffer.len() - 1) as u64;
        unsafe {
            *self.buffer[(tail & mask) as usize].get() = message;
        }

        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// Try to pop a message from the ring buffer.
    /// Returns `Some(message)` on success, `None` if the buffer is empty.
    #[inline]
    pub fn try_pop(&self) -> Option<Arc<Vec<u8>>> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);

        if head >= tail {
            return None;
        }

        let mask = (self.buffer.len() - 1) as u64;
        let message = unsafe {
            std::mem::replace(
                &mut *self.buffer[(head & mask) as usize].get(),
                Arc::new(Vec::new()),
            )
        };

        self.head.store(head.wrapping_add(1), Ordering::Release);
        // Wake up any producers blocked on push()
        self.not_full.1.notify_all();
        Some(message)
    }

    /// Returns the number of items currently in the buffer (approximate).
    #[inline]
    pub fn len(&self) -> u64 {
        let tail = self.tail.load(Ordering::Acquire);
        let head = self.head.load(Ordering::Acquire);
        tail.wrapping_sub(head)
    }

    /// Returns true if the buffer is empty (approximate).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the capacity of the buffer.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.buffer.len()
    }
}

impl Drop for SpscRingBuffer {
    fn drop(&mut self) {
        // Clear any remaining messages in the buffer
        for slot in self.buffer.iter_mut() {
            *slot.get_mut() = Arc::new(Vec::new());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_basic_push_pop() {
        let buf = SpscRingBuffer::new(8);
        let msg = Arc::new(vec![1, 2, 3]);

        assert!(buf.try_push(msg.clone()).is_ok());
        assert_eq!(buf.len(), 1);

        let popped = buf.try_pop().unwrap();
        assert_eq!(*popped, vec![1, 2, 3]);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_full_buffer() {
        let buf = SpscRingBuffer::new(4);

        // Fill the buffer
        for i in 0..4 {
            assert!(buf.try_push(Arc::new(vec![i])).is_ok());
        }

        // Should be full
        assert!(buf.try_push(Arc::new(vec![99])).is_err());
    }

    #[test]
    fn test_concurrent_sp() {
        let buf = Arc::new(SpscRingBuffer::new(65536));
        let buf_prod = Arc::clone(&buf);
        let buf_cons = Arc::clone(&buf);

        let num_items = 1_000_000;

        let producer = thread::spawn(move || {
            for i in 0..num_items {
                loop {
                    let msg = Arc::new(vec![i as u8]);
                    if buf_prod.try_push(msg).is_ok() {
                        break;
                    }
                    // Buffer full — spin briefly
                    std::hint::spin_loop();
                }
            }
        });

        let consumer = thread::spawn(move || {
            let mut count = 0;
            while count < num_items {
                if let Some(_msg) = buf_cons.try_pop() {
                    count += 1;
                } else {
                    // Buffer empty — spin briefly
                    std::hint::spin_loop();
                }
            }
            count
        });

        producer.join().unwrap();
        let received = consumer.join().unwrap();
        assert_eq!(received, num_items);
    }

    #[test]
    fn test_wrap_around() {
        let buf = SpscRingBuffer::new(4);

        // Push 4, pop 4, push 4 again — tests wrap-around
        for i in 0..4 {
            buf.try_push(Arc::new(vec![i])).unwrap();
        }
        for _ in 0..4 {
            buf.try_pop().unwrap();
        }
        for i in 4..8 {
            buf.try_push(Arc::new(vec![i])).unwrap();
        }
        for i in 4..8 {
            let msg = buf.try_pop().unwrap();
            assert_eq!(*msg, vec![i]);
        }
    }
}
