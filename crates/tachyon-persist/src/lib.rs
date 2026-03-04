//! tachyon-persist: Write-ahead log, snapshot persistence, and trade history.
//!
//! Provides durable storage through WAL (write-ahead log),
//! periodic snapshots for crash recovery, and append-only
//! trade storage for historical queries.

pub mod recovery;
pub mod snapshot;
pub mod trade_store;
pub mod wal;

pub use recovery::{RecoveredSymbolState, RecoveryManager, RecoveryState};
pub use snapshot::{Snapshot, SnapshotReader, SnapshotWriter, SymbolSnapshot};
pub use trade_store::{Kline, TradeReader, TradeRecord, TradeStore, TradeWriter};
pub use wal::{WalCommand, WalEntry, WalReader, WalWriter};

use std::path::PathBuf;

/// Configuration for the persistence layer.
#[derive(Clone, Debug)]
pub struct PersistConfig {
    pub wal_dir: PathBuf,
    pub snapshot_dir: PathBuf,
    pub fsync_strategy: FsyncStrategy,
    pub max_wal_size: u64,
    pub snapshot_interval_events: u64,
    pub snapshot_interval_secs: u64,
}

/// Controls when fsync is called after WAL writes.
#[derive(Clone, Debug)]
pub enum FsyncStrategy {
    /// fsync after every write -- strongest durability, highest latency.
    Sync,
    /// fsync after every `count` writes.
    Batched { count: usize },
    /// Never explicitly fsync -- relies on OS page cache flush.
    Async,
}
