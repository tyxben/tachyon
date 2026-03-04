//! Lock-free Multi-Producer Single-Consumer (MPSC) queue.
//!
//! Implemented as a collection of independent SPSC queues — one per producer.
//! The consumer round-robins across all SPSC queues. This avoids CAS contention
//! that plagues traditional lock-free MPSC designs.

use arrayvec::ArrayVec;

use crate::spsc::SpscQueue;

/// Lock-free MPSC queue backed by multiple SPSC queues.
///
/// Each producer is assigned a dedicated [`SpscQueue`] via index.
/// The single consumer drains from all queues in round-robin order.
pub struct MpscQueue<T> {
    queues: Vec<SpscQueue<T>>,
    /// Round-robin cursor for `try_pop`. Not atomic because only the
    /// single consumer thread calls `try_pop` / `drain_all`.
    next: std::cell::Cell<usize>,
}

// SAFETY: MpscQueue is Send+Sync when T: Send because:
// - Each inner SpscQueue is Send+Sync for T: Send.
// - The `next` Cell is only accessed by the single consumer thread, matching
//   the MPSC contract (multiple producers, single consumer).
unsafe impl<T: Send> Send for MpscQueue<T> {}
unsafe impl<T: Send> Sync for MpscQueue<T> {}

impl<T> MpscQueue<T> {
    /// Creates a new MPSC queue.
    ///
    /// # Arguments
    ///
    /// * `producer_count` — number of producer slots (one SPSC queue each)
    /// * `capacity_per_producer` — capacity of each SPSC queue (must be power of 2)
    pub fn new(producer_count: usize, capacity_per_producer: usize) -> Self {
        let queues = (0..producer_count)
            .map(|_| SpscQueue::new(capacity_per_producer))
            .collect();
        Self {
            queues,
            next: std::cell::Cell::new(0),
        }
    }

    /// Returns a reference to the SPSC queue for producer `index`.
    ///
    /// Each producer thread should use its own index to avoid contention.
    ///
    /// # Panics
    ///
    /// Panics if `index >= producer_count`.
    #[inline]
    pub fn producer(&self, index: usize) -> &SpscQueue<T> {
        &self.queues[index]
    }

    /// Consumer: attempt to dequeue a single item by round-robin polling.
    ///
    /// Checks each producer's SPSC queue starting from the last cursor
    /// position, wrapping around once. Returns the first available item,
    /// or `None` if all queues are empty.
    ///
    /// Must only be called from the single consumer thread.
    #[inline]
    pub fn try_pop(&self) -> Option<T> {
        let len = self.queues.len();
        let start = self.next.get();
        for i in 0..len {
            let idx = (start + i) % len;
            if let Some(value) = self.queues[idx].try_pop() {
                self.next.set((idx + 1) % len);
                return Some(value);
            }
        }
        None
    }

    /// Consumer: drain items from all producer queues into `out`.
    ///
    /// Iterates over all SPSC queues and batch-drains each one.
    /// Returns the total number of items drained.
    ///
    /// Must only be called from the single consumer thread.
    pub fn drain_all<const N: usize>(&self, out: &mut ArrayVec<T, N>) -> usize {
        let mut total = 0;
        for q in &self.queues {
            if out.len() >= N {
                break;
            }
            total += q.drain_batch(out);
        }
        total
    }

    /// Returns the number of producer slots.
    #[inline]
    pub fn producer_count(&self) -> usize {
        self.queues.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_mpsc_basic() {
        let q = Arc::new(MpscQueue::new(2, 64));
        let count_per_producer: usize = 100;

        // Consumer must run concurrently; producers will block on full queues.
        let q_consumer = Arc::clone(&q);
        let total = count_per_producer * 2;
        let consumer = thread::spawn(move || {
            let mut received = Vec::with_capacity(total);
            while received.len() < total {
                if let Some(v) = q_consumer.try_pop() {
                    received.push(v);
                } else {
                    std::hint::spin_loop();
                }
            }
            received
        });

        let q0 = Arc::clone(&q);
        let q1 = Arc::clone(&q);
        let p0 = thread::spawn(move || {
            for i in 0..count_per_producer {
                while q0.producer(0).try_push(i).is_err() {
                    std::hint::spin_loop();
                }
            }
        });
        let p1 = thread::spawn(move || {
            for i in 0..count_per_producer {
                while q1.producer(1).try_push(count_per_producer + i).is_err() {
                    std::hint::spin_loop();
                }
            }
        });

        p0.join().expect("producer 0 panicked");
        p1.join().expect("producer 1 panicked");
        let received = consumer.join().expect("consumer panicked");

        assert_eq!(received.len(), total);

        // Verify all items from each producer are present.
        let mut from_p0: Vec<_> = received
            .iter()
            .copied()
            .filter(|&v| v < count_per_producer)
            .collect();
        let mut from_p1: Vec<_> = received
            .iter()
            .copied()
            .filter(|&v| v >= count_per_producer)
            .collect();
        from_p0.sort();
        from_p1.sort();
        let expected_p0: Vec<usize> = (0..count_per_producer).collect();
        let expected_p1: Vec<usize> = (count_per_producer..count_per_producer * 2).collect();
        assert_eq!(from_p0, expected_p0);
        assert_eq!(from_p1, expected_p1);
    }

    #[test]
    fn test_mpsc_stress() {
        let producer_count = 4;
        let items_per_producer: usize = 10_000;
        let q = Arc::new(MpscQueue::new(producer_count, 1024));
        let total_expected = producer_count * items_per_producer;

        // Consumer must run concurrently with producers.
        let q_consumer = Arc::clone(&q);
        let consumer = thread::spawn(move || {
            let mut received = Vec::with_capacity(total_expected);
            while received.len() < total_expected {
                if let Some(v) = q_consumer.try_pop() {
                    received.push(v);
                } else {
                    std::hint::spin_loop();
                }
            }
            received
        });

        let producers: Vec<_> = (0..producer_count)
            .map(|idx| {
                let q = Arc::clone(&q);
                thread::spawn(move || {
                    for i in 0..items_per_producer {
                        let value = idx * items_per_producer + i;
                        while q.producer(idx).try_push(value).is_err() {
                            std::hint::spin_loop();
                        }
                    }
                })
            })
            .collect();

        for p in producers {
            p.join().expect("producer panicked");
        }
        let received = consumer.join().expect("consumer panicked");

        assert_eq!(received.len(), total_expected);

        // Verify all values from each producer are present.
        for idx in 0..producer_count {
            let start = idx * items_per_producer;
            let end = start + items_per_producer;
            let mut from_producer: Vec<_> = received
                .iter()
                .copied()
                .filter(|&v| v >= start && v < end)
                .collect();
            from_producer.sort();
            let expected: Vec<usize> = (start..end).collect();
            assert_eq!(from_producer, expected);
        }
    }
}
