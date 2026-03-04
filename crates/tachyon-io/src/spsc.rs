//! Wait-free Single-Producer Single-Consumer (SPSC) ring buffer queue.
//!
//! This is the core communication primitive for all inter-thread messaging
//! in Tachyon. Designed for zero-allocation, cache-friendly operation on
//! the hot path.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};

use arrayvec::ArrayVec;

/// Cache-line-padded wrapper to prevent false sharing.
///
/// Alignment matches the target architecture's cache line size:
/// - x86/x86_64: 64 bytes
/// - aarch64 (Apple Silicon M1-M4, AWS Graviton): 128 bytes
/// - other architectures: 128 bytes (conservative fallback)
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[repr(align(64))]
struct CachePadded<T> {
    value: T,
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
#[repr(align(128))]
struct CachePadded<T> {
    value: T,
}

impl<T> CachePadded<T> {
    fn new(value: T) -> Self {
        Self { value }
    }
}

/// Wait-free SPSC ring buffer queue.
///
/// - Enqueue: wait-free, single atomic store (Release)
/// - Dequeue: wait-free, single atomic load (Acquire)
/// - Zero allocation after construction
/// - Cache-line padded head/tail to prevent false sharing
pub struct SpscQueue<T> {
    /// Pre-allocated slot array.
    buffer: Box<[UnsafeCell<MaybeUninit<T>>]>,
    /// Capacity (must be a power of 2).
    capacity: usize,
    /// Bitmask for fast modulo: capacity - 1.
    mask: usize,
    /// Consumer read position (only consumer writes, producer reads).
    head: CachePadded<AtomicUsize>,
    /// Producer write position (only producer writes, consumer reads).
    tail: CachePadded<AtomicUsize>,
}

// SAFETY: SpscQueue can be sent across threads when T: Send.
// The SPSC contract ensures exactly one producer and one consumer thread,
// so there is no data race on the buffer slots.
unsafe impl<T: Send> Send for SpscQueue<T> {}

// SAFETY: SpscQueue can be shared across threads (via &SpscQueue) when T: Send.
// The producer calls try_push (writes tail, reads head) and the consumer calls
// try_pop (writes head, reads tail). Each slot is accessed by at most one thread
// at a time due to the head/tail protocol.
unsafe impl<T: Send> Sync for SpscQueue<T> {}

impl<T> SpscQueue<T> {
    /// Creates a new SPSC queue with the given capacity.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is not a power of two or is zero.
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity.is_power_of_two() && capacity > 0,
            "SpscQueue capacity must be a non-zero power of two, got {capacity}"
        );

        let buffer: Vec<UnsafeCell<MaybeUninit<T>>> = (0..capacity)
            .map(|_| UnsafeCell::new(MaybeUninit::uninit()))
            .collect();

        Self {
            buffer: buffer.into_boxed_slice(),
            capacity,
            mask: capacity - 1,
            head: CachePadded::new(AtomicUsize::new(0)),
            tail: CachePadded::new(AtomicUsize::new(0)),
        }
    }

    /// Producer: attempt to enqueue a value.
    ///
    /// Returns `Err(value)` if the queue is full.
    /// Must only be called from the single producer thread.
    #[inline]
    pub fn try_push(&self, value: T) -> Result<(), T> {
        let tail = self.tail.value.load(Ordering::Relaxed);
        let head = self.head.value.load(Ordering::Acquire);

        if tail - head >= self.capacity {
            return Err(value);
        }

        let slot = tail & self.mask;
        // SAFETY: Single-producer guarantee means no other thread is writing to this slot.
        // The slot is not readable by the consumer until we advance tail below.
        unsafe {
            std::ptr::write((*self.buffer[slot].get()).as_mut_ptr(), value);
        }

        // Release: ensures the write to the slot is visible before the consumer sees the new tail.
        self.tail.value.store(tail + 1, Ordering::Release);
        Ok(())
    }

    /// Consumer: attempt to dequeue a value.
    ///
    /// Returns `None` if the queue is empty.
    /// Must only be called from the single consumer thread.
    #[inline]
    pub fn try_pop(&self) -> Option<T> {
        let head = self.head.value.load(Ordering::Relaxed);
        let tail = self.tail.value.load(Ordering::Acquire);

        if head >= tail {
            return None;
        }

        let slot = head & self.mask;
        // SAFETY: Single-consumer guarantee means no other thread is reading from this slot.
        // The producer will not overwrite this slot until we advance head below.
        let value = unsafe { std::ptr::read((*self.buffer[slot].get()).as_ptr()) };

        // Release: notifies the producer that this slot has been consumed and can be reused.
        self.head.value.store(head + 1, Ordering::Release);
        Some(value)
    }

    /// Consumer: batch dequeue into an `ArrayVec` (LMAX catch-up pattern).
    ///
    /// Reads up to `N` available items in one batch, performing only a single
    /// atomic store to advance head. Returns the number of items drained.
    /// Must only be called from the single consumer thread.
    #[inline]
    pub fn drain_batch<const N: usize>(&self, out: &mut ArrayVec<T, N>) -> usize {
        let head = self.head.value.load(Ordering::Relaxed);
        let tail = self.tail.value.load(Ordering::Acquire);

        let available = tail - head;
        let remaining_capacity = N - out.len();
        let count = available.min(remaining_capacity);
        if count == 0 {
            return 0;
        }

        for i in 0..count {
            let slot = (head + i) & self.mask;
            // SAFETY: Single-consumer guarantee. Each slot in [head, head+count) has been
            // written by the producer (tail > head+i) and will not be overwritten until
            // we advance head. push_unchecked is safe because count <= remaining_capacity.
            unsafe {
                out.push_unchecked(std::ptr::read((*self.buffer[slot].get()).as_ptr()));
            }
        }

        // Single Release store to advance head by the full batch.
        self.head.value.store(head + count, Ordering::Release);
        count
    }

    /// Returns the approximate number of items in the queue.
    ///
    /// This is inherently racy in a concurrent context — the value may be
    /// stale by the time it is used.
    #[inline]
    pub fn len(&self) -> usize {
        let tail = self.tail.value.load(Ordering::Relaxed);
        let head = self.head.value.load(Ordering::Relaxed);
        tail.wrapping_sub(head)
    }

    /// Returns the total capacity of the queue.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns `true` if the queue appears empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns `true` if the queue appears full.
    #[inline]
    pub fn is_full(&self) -> bool {
        self.len() >= self.capacity
    }
}

impl<T> Drop for SpscQueue<T> {
    fn drop(&mut self) {
        // Drop any remaining items in the queue.
        while self.try_pop().is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_basic_push_pop() {
        let q = SpscQueue::new(4);
        assert!(q.try_push(42).is_ok());
        assert_eq!(q.try_pop(), Some(42));
    }

    #[test]
    fn test_empty_pop() {
        let q = SpscQueue::<u32>::new(4);
        assert_eq!(q.try_pop(), None);
    }

    #[test]
    fn test_full_push() {
        let q = SpscQueue::new(4);
        for i in 0..4 {
            assert!(q.try_push(i).is_ok());
        }
        assert_eq!(q.try_push(99), Err(99));
    }

    #[test]
    fn test_fifo_order() {
        let q = SpscQueue::new(8);
        for i in 0..8 {
            assert!(q.try_push(i).is_ok());
        }
        for i in 0..8 {
            assert_eq!(q.try_pop(), Some(i));
        }
    }

    #[test]
    fn test_wraparound() {
        let q = SpscQueue::new(4);
        // Fill and drain multiple times to force wraparound.
        for round in 0..10 {
            for i in 0..4 {
                assert!(q.try_push(round * 4 + i).is_ok());
            }
            for i in 0..4 {
                assert_eq!(q.try_pop(), Some(round * 4 + i));
            }
        }
    }

    #[test]
    #[should_panic(expected = "power of two")]
    fn test_capacity_power_of_two() {
        let _ = SpscQueue::<u32>::new(3);
    }

    #[test]
    fn test_drain_batch() {
        let q = SpscQueue::new(16);
        for i in 0..10 {
            assert!(q.try_push(i).is_ok());
        }
        let mut out = ArrayVec::<u32, 16>::new();
        let count = q.drain_batch(&mut out);
        assert_eq!(count, 10);
        assert_eq!(out.as_slice(), &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn test_drain_batch_partial() {
        let q = SpscQueue::new(16);
        for i in 0..10 {
            assert!(q.try_push(i).is_ok());
        }
        let mut out = ArrayVec::<u32, 4>::new();
        let count = q.drain_batch(&mut out);
        assert_eq!(count, 4);
        assert_eq!(out.as_slice(), &[0, 1, 2, 3]);
        // Remaining items are still in the queue.
        assert_eq!(q.len(), 6);
    }

    #[test]
    fn test_cross_thread() {
        let q = Arc::new(SpscQueue::new(1024));
        let q_producer = Arc::clone(&q);
        let q_consumer = Arc::clone(&q);
        let count = 100_000;

        let producer = thread::spawn(move || {
            for i in 0..count {
                while q_producer.try_push(i).is_err() {
                    std::hint::spin_loop();
                }
            }
        });

        let consumer = thread::spawn(move || {
            let mut received = Vec::with_capacity(count);
            while received.len() < count {
                if let Some(v) = q_consumer.try_pop() {
                    received.push(v);
                } else {
                    std::hint::spin_loop();
                }
            }
            received
        });

        producer.join().expect("producer panicked");
        let received = consumer.join().expect("consumer panicked");
        let expected: Vec<usize> = (0..count).collect();
        assert_eq!(received, expected);
    }

    #[test]
    fn test_cross_thread_stress() {
        let q = Arc::new(SpscQueue::new(4096));
        let q_producer = Arc::clone(&q);
        let q_consumer = Arc::clone(&q);
        let count: usize = 1_000_000;

        let producer = thread::spawn(move || {
            for i in 0..count {
                while q_producer.try_push(i).is_err() {
                    std::hint::spin_loop();
                }
            }
        });

        let consumer = thread::spawn(move || {
            let mut total = 0usize;
            while total < count {
                if q_consumer.try_pop().is_some() {
                    total += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
            total
        });

        producer.join().expect("producer panicked");
        let total = consumer.join().expect("consumer panicked");
        assert_eq!(total, count);
    }

    #[test]
    fn test_len_and_capacity() {
        let q = SpscQueue::new(8);
        assert_eq!(q.capacity(), 8);
        assert_eq!(q.len(), 0);
        assert!(q.is_empty());
        assert!(!q.is_full());

        for i in 0..8 {
            assert!(q.try_push(i).is_ok());
        }
        assert_eq!(q.len(), 8);
        assert!(!q.is_empty());
        assert!(q.is_full());

        assert!(q.try_pop().is_some());
        assert_eq!(q.len(), 7);
        assert!(!q.is_empty());
        assert!(!q.is_full());
    }
}
