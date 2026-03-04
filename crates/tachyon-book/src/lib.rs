//! tachyon-book: Order book data structures and operations.
//!
//! Provides the high-performance order book implementation using slab allocation,
//! hash maps for O(1) order lookup, and sorted price levels.

pub mod book;
pub mod level;
pub mod pool;

pub use book::OrderBook;
pub use level::PriceLevel;
pub use pool::OrderPool;
