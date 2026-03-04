//! tachyon-persist: Write-ahead log and snapshot persistence.
//!
//! Provides durable storage through WAL (write-ahead log) and
//! periodic snapshots for crash recovery.

pub mod recovery;
pub mod snapshot;
pub mod wal;

pub use recovery::{RecoveryManager, RecoveryState};
pub use snapshot::{Snapshot, SnapshotReader, SnapshotWriter, SymbolSnapshot};
pub use wal::{WalEntry, WalReader, WalWriter};

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
