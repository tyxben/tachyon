//! Recovery manager: restores state from snapshots + WAL replay.
//!
//! v2 recovery: replays `WalCommand` entries through the engine for deterministic replay.
//! v1 legacy recovery: reads `EngineEvent` entries for backward compatibility.

use std::collections::HashMap;
use std::path::PathBuf;

use tachyon_core::{Order, Symbol, SymbolConfig};

use crate::snapshot::{self, SnapshotReader};
use crate::wal::{self, WalCommand, WalEntry, WalReader};

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

/// Per-symbol engine state recovered from a snapshot.
pub struct RecoveredSymbolState {
    pub config: SymbolConfig,
    pub orders: Vec<Order>,
    pub next_order_id: u64,
    pub sequence: u64,
    pub trade_id_counter: u64,
}

/// Recovered state: symbol configs, orders, and replay metadata.
pub struct RecoveryState {
    /// Per-symbol state recovered from snapshot.
    pub snapshots: HashMap<Symbol, RecoveredSymbolState>,
    /// Sequence number of the last successfully recovered entry.
    pub last_sequence: u64,
    /// Number of WAL entries replayed after the snapshot.
    pub events_replayed: u64,
    /// v2 WAL commands that occurred after the snapshot, for deterministic replay.
    /// The caller should replay these through `SymbolEngine::process_command`.
    pub wal_commands: Vec<WalCommand>,
    /// v1 legacy WAL events (raw deserialized) for backward compatibility.
    /// Only populated when reading v1 WAL files. The caller must apply these
    /// through the old event-based recovery path.
    pub legacy_wal_events: Vec<(u64, tachyon_core::EngineEvent)>,
}

/// Manages crash recovery by loading snapshots and replaying WAL entries.
pub struct RecoveryManager {
    snapshot_dir: PathBuf,
    wal_dir: PathBuf,
}

impl RecoveryManager {
    /// Creates a new RecoveryManager.
    pub fn new(snapshot_dir: impl Into<PathBuf>, wal_dir: impl Into<PathBuf>) -> Self {
        Self {
            snapshot_dir: snapshot_dir.into(),
            wal_dir: wal_dir.into(),
        }
    }

    /// Performs crash recovery:
    /// 1. Load the latest snapshot (if any).
    /// 2. Read all WAL entries after the snapshot sequence.
    /// 3. Return the combined state for the caller to replay.
    pub fn recover(&self) -> Result<RecoveryState> {
        let mut state = RecoveryState {
            snapshots: HashMap::new(),
            last_sequence: 0,
            events_replayed: 0,
            wal_commands: Vec::new(),
            legacy_wal_events: Vec::new(),
        };

        // Step 1: Load latest snapshot
        let snapshot_seq =
            if let Some(snap_path) = snapshot::find_latest_snapshot(&self.snapshot_dir)? {
                let snap = SnapshotReader::read(&snap_path)?;
                tracing::info!(
                    sequence = snap.sequence,
                    symbols = snap.symbols.len(),
                    path = %snap_path.display(),
                    "Loaded snapshot"
                );

                state.last_sequence = snap.sequence;

                for sym_snap in snap.symbols {
                    state.snapshots.insert(
                        sym_snap.symbol,
                        RecoveredSymbolState {
                            config: sym_snap.config,
                            orders: sym_snap.orders,
                            next_order_id: sym_snap.next_order_id,
                            sequence: sym_snap.sequence,
                            trade_id_counter: sym_snap.trade_id_counter,
                        },
                    );
                }

                snap.sequence
            } else {
                0
            };

        // Step 2: Read WAL entries after the snapshot
        let wal_files = wal::list_wal_files(&self.wal_dir)?;
        if wal_files.is_empty() {
            return Ok(state);
        }

        for wal_path in &wal_files {
            let reader = WalReader::open(wal_path)?;
            for entry_result in reader {
                let entry = entry_result?;
                let seq = entry.sequence();

                // Skip entries already covered by the snapshot
                if seq <= snapshot_seq {
                    continue;
                }

                match entry {
                    WalEntry::Command(wal_cmd) => {
                        state.wal_commands.push(wal_cmd);
                    }
                    WalEntry::LegacyEvent { sequence, data } => {
                        let event: tachyon_core::EngineEvent = bincode::deserialize(&data)?;
                        state.legacy_wal_events.push((sequence, event));
                    }
                }

                state.events_replayed += 1;
                if seq > state.last_sequence {
                    state.last_sequence = seq;
                }
            }
        }

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
    use tachyon_engine::Command;

    fn make_test_config() -> SymbolConfig {
        SymbolConfig {
            symbol: Symbol::new(0),
            tick_size: Price::new(1),
            lot_size: Quantity::new(1),
            price_scale: 2,
            qty_scale: 8,
            max_price: Price::new(1_000_000),
            min_price: Price::new(1),
            max_order_qty: Quantity::new(1_000_000),
            min_order_qty: Quantity::new(1),
        }
    }

    fn make_test_order(id: u64, side: Side, price: i64, qty: u64) -> Order {
        Order {
            id: OrderId::new(id),
            symbol: Symbol::new(0),
            side,
            price: Price::new(price),
            quantity: Quantity::new(qty),
            remaining_qty: Quantity::new(qty),
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            timestamp: id * 1000,
            account_id: 1,
            prev: NO_LINK,
            next: NO_LINK,
        }
    }

    fn make_wal_cmd(seq: u64, order_id: u64, side: Side, price: i64, qty: u64) -> WalCommand {
        WalCommand {
            sequence: seq,
            symbol_id: 0,
            command: Command::PlaceOrder(make_test_order(order_id, side, price, qty)),
            account_id: 1,
            timestamp: seq * 1000,
        }
    }

    #[test]
    fn test_recovery_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let snap_dir = dir.path().join("snap");
        let wal_dir = dir.path().join("wal");

        let mgr = RecoveryManager::new(&snap_dir, &wal_dir);
        let state = mgr.recover().expect("recover");

        assert_eq!(state.last_sequence, 0);
        assert!(state.snapshots.is_empty());
        assert!(state.wal_commands.is_empty());
        assert!(state.legacy_wal_events.is_empty());
    }

    #[test]
    fn test_recovery_snapshot_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let snap_dir = dir.path().join("snap");
        let wal_dir = dir.path().join("wal");

        let snapshot = Snapshot {
            sequence: 50,
            timestamp: 50_000_000,
            symbols: vec![SymbolSnapshot {
                symbol: Symbol::new(0),
                config: make_test_config(),
                orders: vec![make_test_order(1, Side::Buy, 100, 10)],
                next_order_id: 2,
                sequence: 50,
                trade_id_counter: 0,
            }],
        };
        SnapshotWriter::write(&snap_dir, &snapshot).expect("write snapshot");

        let mgr = RecoveryManager::new(&snap_dir, &wal_dir);
        let state = mgr.recover().expect("recover");

        assert_eq!(state.last_sequence, 50);
        assert_eq!(state.snapshots.len(), 1);
        let sym_state = state.snapshots.get(&Symbol::new(0)).expect("symbol 0");
        assert_eq!(sym_state.orders.len(), 1);
        assert_eq!(sym_state.next_order_id, 2);
    }

    #[test]
    fn test_recovery_wal_commands_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let snap_dir = dir.path().join("snap");
        let wal_dir = dir.path().join("wal");

        {
            let mut writer =
                WalWriter::new(&wal_dir, 1024 * 1024, FsyncStrategy::Sync).expect("writer");
            for i in 1..=10 {
                let cmd = make_wal_cmd(i, i, Side::Buy, 100 + i as i64, 10);
                writer.append_command(&cmd).expect("append");
            }
            writer.flush().expect("flush");
        }

        let mgr = RecoveryManager::new(&snap_dir, &wal_dir);
        let state = mgr.recover().expect("recover");

        assert_eq!(state.last_sequence, 10);
        assert_eq!(state.events_replayed, 10);
        assert_eq!(state.wal_commands.len(), 10);
        assert!(state.legacy_wal_events.is_empty());
        assert!(state.snapshots.is_empty());
    }

    #[test]
    fn test_recovery_snapshot_plus_wal_commands() {
        let dir = tempfile::tempdir().expect("tempdir");
        let snap_dir = dir.path().join("snap");
        let wal_dir = dir.path().join("wal");

        // Write a snapshot at sequence 5
        let snapshot = Snapshot {
            sequence: 5,
            timestamp: 5_000_000,
            symbols: vec![SymbolSnapshot {
                symbol: Symbol::new(0),
                config: make_test_config(),
                orders: vec![make_test_order(1, Side::Buy, 100, 10)],
                next_order_id: 6,
                sequence: 5,
                trade_id_counter: 0,
            }],
        };
        SnapshotWriter::write(&snap_dir, &snapshot).expect("write snapshot");

        // Write WAL commands from 1..10 (first 5 should be skipped)
        {
            let mut writer =
                WalWriter::new(&wal_dir, 1024 * 1024, FsyncStrategy::Sync).expect("writer");
            for i in 1..=10 {
                let cmd = make_wal_cmd(i, i + 10, Side::Sell, 200, 5);
                writer.append_command(&cmd).expect("append");
            }
            writer.flush().expect("flush");
        }

        let mgr = RecoveryManager::new(&snap_dir, &wal_dir);
        let state = mgr.recover().expect("recover");

        assert_eq!(state.last_sequence, 10);
        // Only entries 6-10 should be in wal_commands (after snapshot at 5)
        assert_eq!(state.wal_commands.len(), 5);
        assert_eq!(state.wal_commands[0].sequence, 6);
        assert_eq!(state.wal_commands[4].sequence, 10);
        // Snapshot data should be present
        assert_eq!(state.snapshots.len(), 1);
    }

    #[test]
    fn test_recovery_v1_legacy_wal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let snap_dir = dir.path().join("snap");
        let wal_dir = dir.path().join("wal");

        // Write v1 legacy events
        {
            let mut writer =
                WalWriter::new(&wal_dir, 1024 * 1024, FsyncStrategy::Sync).expect("writer");
            for i in 1..=5 {
                let event = EngineEvent::OrderAccepted {
                    order_id: OrderId::new(i),
                    symbol: Symbol::new(0),
                    side: Side::Buy,
                    price: Price::new(100),
                    qty: Quantity::new(10),
                    timestamp: i * 1000,
                };
                writer.append_legacy(i, &event).expect("append");
            }
            writer.flush().expect("flush");
        }

        let mgr = RecoveryManager::new(&snap_dir, &wal_dir);
        let state = mgr.recover().expect("recover");

        assert_eq!(state.last_sequence, 5);
        assert_eq!(state.events_replayed, 5);
        assert!(state.wal_commands.is_empty());
        assert_eq!(state.legacy_wal_events.len(), 5);
    }

    #[test]
    fn test_recovery_deterministic_replay() {
        // This test verifies that replaying WAL commands through SymbolEngine
        // produces identical book state.
        use tachyon_engine::{RiskConfig, StpMode, SymbolEngine};

        let config = make_test_config();
        let risk_config = RiskConfig {
            price_band_pct: 0,
            max_order_qty: Quantity::new(1_000_000),
        };

        // Build a sequence of commands
        let commands = vec![
            make_wal_cmd(1, 0, Side::Buy, 100, 50), // id assigned by engine
            make_wal_cmd(2, 0, Side::Buy, 99, 30),  // id assigned by engine
            make_wal_cmd(3, 0, Side::Sell, 101, 40), // id assigned by engine
            make_wal_cmd(4, 0, Side::Sell, 100, 20), // crosses best bid, partial fill
            make_wal_cmd(5, 0, Side::Buy, 101, 10), // crosses best ask
        ];

        // Run 1: process all commands
        let mut engine1 = SymbolEngine::new(
            Symbol::new(0),
            config.clone(),
            StpMode::None,
            risk_config.clone(),
        );
        let mut all_events1 = Vec::new();
        for cmd in &commands {
            let events =
                engine1.process_command(cmd.command.clone(), cmd.account_id, cmd.timestamp);
            for e in &events {
                all_events1.push(format!("{:?}", e));
            }
        }

        // Run 2: replay the same commands on a fresh engine
        let mut engine2 = SymbolEngine::new(Symbol::new(0), config, StpMode::None, risk_config);
        let mut all_events2 = Vec::new();
        for cmd in &commands {
            let events =
                engine2.process_command(cmd.command.clone(), cmd.account_id, cmd.timestamp);
            for e in &events {
                all_events2.push(format!("{:?}", e));
            }
        }

        // Both runs must produce identical events
        assert_eq!(all_events1.len(), all_events2.len());
        for (a, b) in all_events1.iter().zip(all_events2.iter()) {
            assert_eq!(a, b);
        }

        // Both engines must have identical book state
        assert_eq!(engine1.book().order_count(), engine2.book().order_count());
        assert_eq!(engine1.sequence(), engine2.sequence());
        assert_eq!(engine1.next_order_id(), engine2.next_order_id());
        assert_eq!(engine1.trade_id_counter(), engine2.trade_id_counter());

        // Check the actual order book depth
        let (bids1, asks1) = engine1.book().get_depth(100);
        let (bids2, asks2) = engine2.book().get_depth(100);
        assert_eq!(bids1.len(), bids2.len());
        assert_eq!(asks1.len(), asks2.len());
        for ((p1, q1), (p2, q2)) in bids1.iter().zip(bids2.iter()) {
            assert_eq!(p1, p2);
            assert_eq!(q1, q2);
        }
        for ((p1, q1), (p2, q2)) in asks1.iter().zip(asks2.iter()) {
            assert_eq!(p1, p2);
            assert_eq!(q1, q2);
        }
    }

    #[test]
    fn test_recovery_full_cycle_with_wal_replay() {
        // End-to-end test: write commands to WAL, recover, replay, verify state.
        use tachyon_engine::{RiskConfig, StpMode, SymbolEngine};

        let dir = tempfile::tempdir().expect("tempdir");
        let snap_dir = dir.path().join("snap");
        let wal_dir = dir.path().join("wal");

        let config = make_test_config();
        let risk_config = RiskConfig {
            price_band_pct: 0,
            max_order_qty: Quantity::new(1_000_000),
        };

        // Phase 1: Process commands and write to WAL (simulating live operation)
        let commands = vec![
            make_wal_cmd(1, 0, Side::Buy, 100, 50),
            make_wal_cmd(2, 0, Side::Buy, 99, 30),
            make_wal_cmd(3, 0, Side::Sell, 101, 40),
        ];

        let mut original_engine = SymbolEngine::new(
            Symbol::new(0),
            config.clone(),
            StpMode::None,
            risk_config.clone(),
        );

        {
            let mut wal_writer =
                WalWriter::new(&wal_dir, 1024 * 1024, FsyncStrategy::Sync).expect("writer");
            for cmd in &commands {
                // Write-ahead: log command BEFORE processing
                wal_writer.append_command(cmd).expect("append");
                // Then process
                original_engine.process_command(cmd.command.clone(), cmd.account_id, cmd.timestamp);
            }
            wal_writer.flush().expect("flush");
        }

        // Phase 2: Simulate crash recovery
        let mgr = RecoveryManager::new(&snap_dir, &wal_dir);
        let state = mgr.recover().expect("recover");

        assert_eq!(state.wal_commands.len(), 3);

        // Phase 3: Replay on fresh engine
        let mut recovered_engine =
            SymbolEngine::new(Symbol::new(0), config, StpMode::None, risk_config);

        for wal_cmd in &state.wal_commands {
            recovered_engine.process_command(
                wal_cmd.command.clone(),
                wal_cmd.account_id,
                wal_cmd.timestamp,
            );
        }

        // Verify identical state
        assert_eq!(
            original_engine.book().order_count(),
            recovered_engine.book().order_count()
        );
        assert_eq!(original_engine.sequence(), recovered_engine.sequence());
        assert_eq!(
            original_engine.next_order_id(),
            recovered_engine.next_order_id()
        );
        assert_eq!(
            original_engine.trade_id_counter(),
            recovered_engine.trade_id_counter()
        );

        let (orig_bids, orig_asks) = original_engine.book().get_depth(100);
        let (recv_bids, recv_asks) = recovered_engine.book().get_depth(100);
        assert_eq!(orig_bids, recv_bids);
        assert_eq!(orig_asks, recv_asks);
    }

    #[test]
    fn test_recovery_snapshot_plus_wal_replay() {
        // Test: snapshot at seq 3, then WAL commands 4-6, recover and verify.
        use crate::snapshot::{Snapshot, SnapshotWriter, SymbolSnapshot};
        use tachyon_engine::{RiskConfig, StpMode, SymbolEngine};

        let dir = tempfile::tempdir().expect("tempdir");
        let snap_dir = dir.path().join("snap");
        let wal_dir = dir.path().join("wal");

        let config = make_test_config();
        let risk_config = RiskConfig {
            price_band_pct: 0,
            max_order_qty: Quantity::new(1_000_000),
        };

        // Phase 1: Build engine state with 6 commands
        let commands = vec![
            make_wal_cmd(1, 0, Side::Buy, 100, 50),
            make_wal_cmd(2, 0, Side::Buy, 99, 30),
            make_wal_cmd(3, 0, Side::Sell, 101, 40),
            make_wal_cmd(4, 0, Side::Sell, 100, 20),
            make_wal_cmd(5, 0, Side::Buy, 101, 10),
            make_wal_cmd(6, 0, Side::Buy, 98, 25),
        ];

        let mut original_engine = SymbolEngine::new(
            Symbol::new(0),
            config.clone(),
            StpMode::None,
            risk_config.clone(),
        );

        for cmd in &commands {
            original_engine.process_command(cmd.command.clone(), cmd.account_id, cmd.timestamp);
        }

        // Take snapshot at seq 3: replay first 3 commands on a temp engine for snapshot
        let mut snap_engine = SymbolEngine::new(
            Symbol::new(0),
            config.clone(),
            StpMode::None,
            risk_config.clone(),
        );
        for cmd in &commands[..3] {
            snap_engine.process_command(cmd.command.clone(), cmd.account_id, cmd.timestamp);
        }
        let eng_snap = snap_engine.snapshot_full_state();

        let snapshot = Snapshot {
            sequence: 3,
            timestamp: 3000,
            symbols: vec![SymbolSnapshot {
                symbol: eng_snap.symbol,
                config: eng_snap.config,
                orders: eng_snap.orders,
                next_order_id: eng_snap.next_order_id,
                sequence: eng_snap.sequence,
                trade_id_counter: eng_snap.trade_id_counter,
            }],
        };
        SnapshotWriter::write(&snap_dir, &snapshot).expect("write snapshot");

        // Write ALL commands to WAL (1-6), recovery will skip 1-3
        {
            let mut wal_writer =
                WalWriter::new(&wal_dir, 1024 * 1024, FsyncStrategy::Sync).expect("writer");
            for cmd in &commands {
                wal_writer.append_command(cmd).expect("append");
            }
            wal_writer.flush().expect("flush");
        }

        // Phase 2: Recover
        let mgr = RecoveryManager::new(&snap_dir, &wal_dir);
        let state = mgr.recover().expect("recover");

        // Should have snapshot + 3 WAL commands (4, 5, 6)
        assert_eq!(state.snapshots.len(), 1);
        assert_eq!(state.wal_commands.len(), 3);

        // Phase 3: Rebuild engine from snapshot + replay
        let sym_state = state.snapshots.get(&Symbol::new(0)).expect("symbol 0");
        let mut recovered_engine = SymbolEngine::new(
            Symbol::new(0),
            sym_state.config.clone(),
            StpMode::None,
            risk_config,
        );
        // Restore snapshot state
        for order in &sym_state.orders {
            recovered_engine
                .book_mut()
                .restore_order(order.clone())
                .expect("restore order");
        }
        recovered_engine.set_sequence(sym_state.sequence);
        recovered_engine.set_next_order_id(sym_state.next_order_id);
        recovered_engine.set_trade_id_counter(sym_state.trade_id_counter);

        // Replay WAL commands
        for wal_cmd in &state.wal_commands {
            recovered_engine.process_command(
                wal_cmd.command.clone(),
                wal_cmd.account_id,
                wal_cmd.timestamp,
            );
        }

        // Verify identical state
        assert_eq!(
            original_engine.book().order_count(),
            recovered_engine.book().order_count()
        );
        assert_eq!(original_engine.sequence(), recovered_engine.sequence());
        assert_eq!(
            original_engine.next_order_id(),
            recovered_engine.next_order_id()
        );

        let (orig_bids, orig_asks) = original_engine.book().get_depth(100);
        let (recv_bids, recv_asks) = recovered_engine.book().get_depth(100);
        assert_eq!(orig_bids, recv_bids);
        assert_eq!(orig_asks, recv_asks);
    }
}
