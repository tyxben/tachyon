//! Recovery manager: restores state from snapshots + WAL replay.

use std::collections::HashMap;
use std::path::PathBuf;

use tachyon_core::{EngineEvent, Order, Symbol, SymbolConfig};

use crate::snapshot::{self, SnapshotReader};
use crate::wal::{self, WalReader};

/// Error type for recovery operations.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("WAL error: {0}")]
    Wal(#[from] wal::WalError),
    #[error("snapshot error: {0}")]
    Snapshot(#[from] snapshot::SnapshotError),
    #[error("deserialization error: {0}")]
    Deserialize(String),
}

impl From<bincode::Error> for RecoveryError {
    fn from(e: bincode::Error) -> Self {
        RecoveryError::Deserialize(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, RecoveryError>;

/// Recovered state: symbol configs, orders, and replay metadata.
pub struct RecoveryState {
    /// Per-symbol state: (config, orders).
    pub snapshots: HashMap<Symbol, (SymbolConfig, Vec<Order>)>,
    /// Sequence number of the last successfully recovered entry.
    pub last_sequence: u64,
    /// Number of WAL events replayed after the snapshot.
    pub events_replayed: u64,
}

/// Orchestrates recovery from snapshot + WAL replay.
pub struct RecoveryManager {
    snapshot_dir: PathBuf,
    wal_dir: PathBuf,
}

impl RecoveryManager {
    pub fn new(snapshot_dir: impl Into<PathBuf>, wal_dir: impl Into<PathBuf>) -> Self {
        Self {
            snapshot_dir: snapshot_dir.into(),
            wal_dir: wal_dir.into(),
        }
    }

    /// Recover state from the latest snapshot and subsequent WAL entries.
    pub fn recover(&self) -> Result<RecoveryState> {
        let mut state = RecoveryState {
            snapshots: HashMap::new(),
            last_sequence: 0,
            events_replayed: 0,
        };

        // Step 1: Find and load the latest valid snapshot
        if let Some(snap_path) = snapshot::find_latest_snapshot(&self.snapshot_dir)? {
            match SnapshotReader::read(&snap_path) {
                Ok(snap) => {
                    tracing::info!(
                        sequence = snap.sequence,
                        symbols = snap.symbols.len(),
                        "loaded snapshot"
                    );
                    state.last_sequence = snap.sequence;
                    for sym_snap in snap.symbols {
                        state
                            .snapshots
                            .insert(sym_snap.symbol, (sym_snap.config, sym_snap.orders));
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, path = %snap_path.display(), "failed to load snapshot, starting from empty state");
                }
            }
        } else {
            tracing::info!("no snapshot found, recovering from WAL only");
        }

        // Step 2: Replay WAL entries after last_sequence
        let wal_files = wal::list_wal_files(&self.wal_dir)?;
        let mut prev_seq = state.last_sequence;

        for wal_path in &wal_files {
            let reader = WalReader::open(wal_path)?;
            for entry_result in reader {
                let entry = match entry_result {
                    Ok(e) => e,
                    Err(wal::WalError::CrcMismatch { expected, actual }) => {
                        tracing::warn!(
                            expected = format!("{expected:#010x}"),
                            actual = format!("{actual:#010x}"),
                            path = %wal_path.display(),
                            "CRC mismatch in WAL, stopping replay at last valid entry"
                        );
                        return Ok(state);
                    }
                    Err(wal::WalError::UnexpectedEof) => {
                        tracing::warn!(
                            path = %wal_path.display(),
                            "unexpected EOF in WAL, stopping replay at last valid entry"
                        );
                        return Ok(state);
                    }
                    Err(e) => return Err(e.into()),
                };

                // Skip entries at or before the snapshot sequence
                if entry.sequence <= state.last_sequence && state.last_sequence > 0 {
                    continue;
                }

                // Check sequence continuity
                if prev_seq > 0 && entry.sequence != prev_seq + 1 {
                    tracing::warn!(
                        expected = prev_seq + 1,
                        got = entry.sequence,
                        "sequence gap detected in WAL"
                    );
                }

                // Deserialize to verify the event is valid
                let _event: EngineEvent = bincode::deserialize(&entry.data)?;

                prev_seq = entry.sequence;
                state.last_sequence = entry.sequence;
                state.events_replayed += 1;
            }
        }

        tracing::info!(
            last_sequence = state.last_sequence,
            events_replayed = state.events_replayed,
            symbols = state.snapshots.len(),
            "recovery complete"
        );

        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{Snapshot, SnapshotWriter, SymbolSnapshot};
    use crate::wal::WalWriter;
    use crate::FsyncStrategy;
    use tachyon_core::*;

    fn make_event(order_id: u64, timestamp: u64) -> EngineEvent {
        EngineEvent::OrderAccepted {
            order_id: OrderId::new(order_id),
            symbol: Symbol::new(0),
            side: Side::Buy,
            price: Price::new(50000),
            qty: Quantity::new(100),
            timestamp,
        }
    }

    fn make_snapshot_data(seq: u64) -> Snapshot {
        Snapshot {
            sequence: seq,
            timestamp: seq * 1_000_000,
            symbols: vec![SymbolSnapshot {
                symbol: Symbol::new(0),
                config: SymbolConfig {
                    symbol: Symbol::new(0),
                    tick_size: Price::new(1),
                    lot_size: Quantity::new(1),
                    price_scale: 2,
                    qty_scale: 8,
                    max_price: Price::new(1_000_000),
                    min_price: Price::new(1),
                    max_order_qty: Quantity::new(1_000_000),
                    min_order_qty: Quantity::new(1),
                },
                orders: vec![Order {
                    id: OrderId::new(1),
                    symbol: Symbol::new(0),
                    side: Side::Buy,
                    price: Price::new(50000),
                    quantity: Quantity::new(100),
                    remaining_qty: Quantity::new(100),
                    order_type: OrderType::Limit,
                    time_in_force: TimeInForce::GTC,
                    timestamp: 1000,
                    account_id: 42,
                    prev: NO_LINK,
                    next: NO_LINK,
                }],
            }],
        }
    }

    #[test]
    fn test_recovery_snapshot_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let snap_dir = dir.path().join("snapshots");
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).expect("create wal dir");

        SnapshotWriter::write(&snap_dir, &make_snapshot_data(100)).expect("write snapshot");

        let mgr = RecoveryManager::new(&snap_dir, &wal_dir);
        let state = mgr.recover().expect("recover");

        assert_eq!(state.last_sequence, 100);
        assert_eq!(state.events_replayed, 0);
        assert!(state.snapshots.contains_key(&Symbol::new(0)));
        let (config, orders) = &state.snapshots[&Symbol::new(0)];
        assert_eq!(config.price_scale, 2);
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].id, OrderId::new(1));
    }

    #[test]
    fn test_recovery_wal_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let snap_dir = dir.path().join("snapshots");
        let wal_dir = dir.path().join("wal");

        {
            let mut writer =
                WalWriter::new(&wal_dir, 1024 * 1024, FsyncStrategy::Sync).expect("writer");
            for i in 1..=50 {
                writer.append(i, &make_event(i, i * 1000)).expect("append");
            }
            writer.flush().expect("flush");
        }

        let mgr = RecoveryManager::new(&snap_dir, &wal_dir);
        let state = mgr.recover().expect("recover");

        assert_eq!(state.last_sequence, 50);
        assert_eq!(state.events_replayed, 50);
        assert!(state.snapshots.is_empty());
    }

    #[test]
    fn test_recovery_snapshot_plus_wal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let snap_dir = dir.path().join("snapshots");
        let wal_dir = dir.path().join("wal");

        // Snapshot at sequence 100
        SnapshotWriter::write(&snap_dir, &make_snapshot_data(100)).expect("write snapshot");

        // WAL from 1..200 (entries 1-100 should be skipped, 101-200 replayed)
        {
            let mut writer =
                WalWriter::new(&wal_dir, 1024 * 1024, FsyncStrategy::Sync).expect("writer");
            for i in 1..=200 {
                writer.append(i, &make_event(i, i * 1000)).expect("append");
            }
            writer.flush().expect("flush");
        }

        let mgr = RecoveryManager::new(&snap_dir, &wal_dir);
        let state = mgr.recover().expect("recover");

        assert_eq!(state.last_sequence, 200);
        assert_eq!(state.events_replayed, 100);
        assert!(state.snapshots.contains_key(&Symbol::new(0)));
    }

    #[test]
    fn test_recovery_corrupted_wal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let snap_dir = dir.path().join("snapshots");
        let wal_dir = dir.path().join("wal");

        // Write 10 valid entries
        {
            let mut writer =
                WalWriter::new(&wal_dir, 1024 * 1024, FsyncStrategy::Sync).expect("writer");
            for i in 1..=10 {
                writer.append(i, &make_event(i, i * 1000)).expect("append");
            }
            writer.flush().expect("flush");
        }

        // Append garbage to simulate a partial/corrupt write
        let files = wal::list_wal_files(&wal_dir).expect("list");
        let wal_path = &files[0];
        let mut data = std::fs::read(wal_path).expect("read");
        // Append a valid-looking length but garbage content
        data.extend_from_slice(&100u32.to_le_bytes()); // length = 100
        data.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // garbage
        std::fs::write(wal_path, &data).expect("write corrupted");

        let mgr = RecoveryManager::new(&snap_dir, &wal_dir);
        let state = mgr.recover().expect("recover");

        // Should recover the 10 valid entries and stop
        assert_eq!(state.last_sequence, 10);
        assert_eq!(state.events_replayed, 10);
    }

    #[test]
    fn test_recovery_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let snap_dir = dir.path().join("snapshots");
        let wal_dir = dir.path().join("wal");

        let mgr = RecoveryManager::new(&snap_dir, &wal_dir);
        let state = mgr.recover().expect("recover");

        assert_eq!(state.last_sequence, 0);
        assert_eq!(state.events_replayed, 0);
        assert!(state.snapshots.is_empty());
    }
}
