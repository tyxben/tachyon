//! Snapshot persistence: full state serialization for fast recovery.
//!
//! Snapshots capture the complete order book state at a given sequence number.
//! File format: `[bincode payload][crc32: u32 LE]`
//! Files are written atomically via temp-file + rename.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tachyon_core::{Order, Symbol, SymbolConfig};

/// Error type for snapshot operations.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serialize(String),
    #[error("CRC mismatch: expected {expected:#010x}, got {actual:#010x}")]
    CrcMismatch { expected: u32, actual: u32 },
    #[error("snapshot file too small")]
    TooSmall,
}

impl From<bincode::Error> for SnapshotError {
    fn from(e: bincode::Error) -> Self {
        SnapshotError::Serialize(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, SnapshotError>;

/// Full state snapshot at a particular WAL sequence number.
#[derive(Serialize, Deserialize, Debug)]
pub struct Snapshot {
    pub sequence: u64,
    pub timestamp: u64,
    pub symbols: Vec<SymbolSnapshot>,
}

/// Per-symbol state within a snapshot.
#[derive(Serialize, Deserialize, Debug)]
pub struct SymbolSnapshot {
    pub symbol: Symbol,
    pub config: SymbolConfig,
    pub orders: Vec<Order>,
    /// The next order ID the engine should assign (preserves ID uniqueness across restarts).
    #[serde(default)]
    pub next_order_id: u64,
    /// The engine sequence counter at snapshot time.
    #[serde(default)]
    pub sequence: u64,
    /// The matcher's trade ID counter at snapshot time.
    #[serde(default)]
    pub trade_id_counter: u64,
}

/// Writes snapshots to disk with CRC integrity and atomic rename.
pub struct SnapshotWriter;

impl SnapshotWriter {
    /// Write a snapshot to the given directory. Returns the final file path.
    ///
    /// The write is atomic: data is written to a temp file first, then renamed
    /// to `snapshot_{sequence}.bin`.
    pub fn write(dir: impl AsRef<Path>, snapshot: &Snapshot) -> Result<PathBuf> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir)?;

        let payload = bincode::serialize(snapshot)?;
        let crc = crc32fast::hash(&payload);

        let temp_path = dir.join(format!(".snapshot_{}.tmp", snapshot.sequence));
        let final_path = dir.join(format!("snapshot_{}.bin", snapshot.sequence));

        {
            let mut file = File::create(&temp_path)?;
            file.write_all(&payload)?;
            file.write_all(&crc.to_le_bytes())?;
            file.sync_all()?;
        }

        fs::rename(&temp_path, &final_path)?;

        tracing::info!(
            path = %final_path.display(),
            sequence = snapshot.sequence,
            "snapshot written"
        );

        Ok(final_path)
    }
}

/// Reads and verifies snapshots from disk.
pub struct SnapshotReader;

impl SnapshotReader {
    /// Read and verify a snapshot from the given path.
    pub fn read(path: impl AsRef<Path>) -> Result<Snapshot> {
        let data = fs::read(path.as_ref())?;

        if data.len() < 4 {
            return Err(SnapshotError::TooSmall);
        }

        let crc_offset = data.len() - 4;
        let payload = &data[..crc_offset];
        let stored_crc = u32::from_le_bytes(
            data[crc_offset..]
                .try_into()
                .map_err(|_| SnapshotError::TooSmall)?,
        );

        let computed_crc = crc32fast::hash(payload);
        if stored_crc != computed_crc {
            return Err(SnapshotError::CrcMismatch {
                expected: stored_crc,
                actual: computed_crc,
            });
        }

        let snapshot: Snapshot = bincode::deserialize(payload)?;
        Ok(snapshot)
    }
}

/// Find the latest snapshot file (highest sequence number) in a directory.
pub fn find_latest_snapshot(dir: impl AsRef<Path>) -> Result<Option<PathBuf>> {
    let dir = dir.as_ref();
    if !dir.exists() {
        return Ok(None);
    }

    let mut best: Option<(u64, PathBuf)> = None;

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with("snapshot_") && name.ends_with(".bin") {
                let seq_str = &name[9..name.len() - 4]; // strip "snapshot_" and ".bin"
                if let Ok(seq) = seq_str.parse::<u64>() {
                    match &best {
                        Some((current_best, _)) if seq <= *current_best => {}
                        _ => {
                            best = Some((seq, path));
                        }
                    }
                }
            }
        }
    }

    Ok(best.map(|(_, p)| p))
}

/// Clean up old snapshots, keeping only the `keep` most recent.
pub fn cleanup_old_snapshots(dir: impl AsRef<Path>, keep: usize) -> Result<()> {
    let dir = dir.as_ref();
    if !dir.exists() {
        return Ok(());
    }

    let mut snapshots: Vec<(u64, PathBuf)> = Vec::new();

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with("snapshot_") && name.ends_with(".bin") {
                let seq_str = &name[9..name.len() - 4];
                if let Ok(seq) = seq_str.parse::<u64>() {
                    snapshots.push((seq, path));
                }
            }
        }
    }

    if snapshots.len() <= keep {
        return Ok(());
    }

    snapshots.sort_by_key(|(seq, _)| *seq);
    let to_remove = snapshots.len() - keep;
    for (_, path) in snapshots.into_iter().take(to_remove) {
        fs::remove_file(&path)?;
        tracing::info!(path = %path.display(), "old snapshot removed");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tachyon_core::*;

    fn make_snapshot(seq: u64) -> Snapshot {
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
                orders: vec![
                    Order {
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
                    },
                    Order {
                        id: OrderId::new(2),
                        symbol: Symbol::new(0),
                        side: Side::Sell,
                        price: Price::new(51000),
                        quantity: Quantity::new(200),
                        remaining_qty: Quantity::new(150),
                        order_type: OrderType::Limit,
                        time_in_force: TimeInForce::GTC,
                        timestamp: 2000,
                        account_id: 43,
                        prev: NO_LINK,
                        next: NO_LINK,
                    },
                ],
                next_order_id: 3,
                sequence: seq,
                trade_id_counter: 0,
            }],
        }
    }

    #[test]
    fn test_snapshot_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let snap = make_snapshot(100);

        let path = SnapshotWriter::write(dir.path(), &snap).expect("write");
        let loaded = SnapshotReader::read(&path).expect("read");

        assert_eq!(loaded.sequence, 100);
        assert_eq!(loaded.timestamp, 100_000_000);
        assert_eq!(loaded.symbols.len(), 1);
        assert_eq!(loaded.symbols[0].orders.len(), 2);
        assert_eq!(loaded.symbols[0].orders[0].id, OrderId::new(1));
        assert_eq!(loaded.symbols[0].orders[1].id, OrderId::new(2));
        assert_eq!(
            loaded.symbols[0].orders[1].remaining_qty,
            Quantity::new(150)
        );
    }

    #[test]
    fn test_snapshot_crc_validation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let snap = make_snapshot(100);

        let path = SnapshotWriter::write(dir.path(), &snap).expect("write");

        // Corrupt a byte
        let mut data = fs::read(&path).expect("read");
        data[10] ^= 0xFF;
        fs::write(&path, &data).expect("write corrupted");

        let result = SnapshotReader::read(&path);
        assert!(
            matches!(result, Err(SnapshotError::CrcMismatch { .. })),
            "expected CrcMismatch, got {result:?}"
        );
    }

    #[test]
    fn test_snapshot_atomic_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let snap = make_snapshot(100);

        let path = SnapshotWriter::write(dir.path(), &snap).expect("write");

        // Temp file should not exist
        let temp_path = dir.path().join(".snapshot_100.tmp");
        assert!(
            !temp_path.exists(),
            "temp file should not remain after write"
        );

        // Final file should exist
        assert!(path.exists());
    }

    #[test]
    fn test_find_latest_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");

        // Write multiple snapshots
        SnapshotWriter::write(dir.path(), &make_snapshot(50)).expect("write 50");
        SnapshotWriter::write(dir.path(), &make_snapshot(100)).expect("write 100");
        SnapshotWriter::write(dir.path(), &make_snapshot(75)).expect("write 75");

        let latest = find_latest_snapshot(dir.path())
            .expect("find")
            .expect("should find");
        assert!(
            latest.file_name().and_then(|n| n.to_str()) == Some("snapshot_100.bin"),
            "expected snapshot_100.bin, got {:?}",
            latest.file_name()
        );
    }

    #[test]
    fn test_find_latest_snapshot_empty_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = find_latest_snapshot(dir.path()).expect("find");
        assert!(result.is_none());
    }

    #[test]
    fn test_find_latest_snapshot_nonexistent_dir() {
        let result = find_latest_snapshot("/tmp/nonexistent_tachyon_dir_12345").expect("find");
        assert!(result.is_none());
    }

    #[test]
    fn test_cleanup_old_snapshots() {
        let dir = tempfile::tempdir().expect("tempdir");

        for seq in [10, 20, 30, 40, 50] {
            SnapshotWriter::write(dir.path(), &make_snapshot(seq)).expect("write");
        }

        cleanup_old_snapshots(dir.path(), 2).expect("cleanup");

        // Only the 2 most recent should remain (40, 50)
        let mut remaining: Vec<String> = fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(|e| {
                e.ok()
                    .and_then(|e| e.file_name().into_string().ok())
                    .filter(|n| n.starts_with("snapshot_"))
            })
            .collect();
        remaining.sort();
        assert_eq!(remaining, vec!["snapshot_40.bin", "snapshot_50.bin"]);
    }
}
