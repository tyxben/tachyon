//! tachyon-io: Lock-free I/O primitives for the Tachyon matching engine.
//!
//! Provides SPSC and MPSC queues designed for ultra-low-latency
//! inter-thread communication on the hot path.

pub mod mpsc;
pub mod spsc;

pub use mpsc::MpscQueue;
pub use spsc::SpscQueue;
