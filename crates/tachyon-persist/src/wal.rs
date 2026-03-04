//! Write-Ahead Log (WAL) writer and reader.
//!
//! Binary entry format on disk:
//! ```text
//! [length: u32 LE][sequence: u64 LE][payload: N bytes][crc32: u32 LE]
//! ```
//! - `length` = 8 (sequence) + payload_len + 4 (crc) = payload_len + 12
//! - CRC32 covers the sequence bytes + payload bytes (not the length field).

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::FsyncStrategy;
use tachyon_core::EngineEvent;

/// A single WAL entry read back from disk.
#[derive(Debug)]
pub struct WalEntry {
    pub sequence: u64,
    /// Serialized `EngineEvent` bytes (bincode).
    pub data: Vec<u8>,
}

/// Appends WAL entries to the current active WAL file with configurable fsync.
pub struct WalWriter {
    dir: PathBuf,
    current_file: BufWriter<File>,
    current_size: u64,
    max_size: u64,
    first_seq: u64,
    last_seq: u64,
    fsync_strategy: FsyncStrategy,
    writes_since_sync: usize,
}

/// Error type for WAL operations.
#[derive(Debug, thiserror::Error)]
pub enum WalError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("serialization error: {0}")]
    Serialize(String),
    #[error("CRC mismatch: expected {expected:#010x}, got {actual:#010x}")]
    CrcMismatch { expected: u32, actual: u32 },
    #[error("unexpected EOF while reading WAL entry")]
    UnexpectedEof,
}

impl From<bincode::Error> for WalError {
    fn from(e: bincode::Error) -> Self {
        WalError::Serialize(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, WalError>;

impl WalWriter {
    /// Create a new WAL writer. Creates the directory if it does not exist.
    /// Opens a new active WAL file named `wal_active.log`.
    pub fn new(
        dir: impl Into<PathBuf>,
        max_size: u64,
        fsync_strategy: FsyncStrategy,
    ) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;

        let active_path = dir.join("wal_active.log");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&active_path)?;
        let current_size = file.metadata()?.len();

        Ok(Self {
            dir,
            current_file: BufWriter::new(file),
            current_size,
            max_size,
            first_seq: 0,
            last_seq: 0,
            fsync_strategy,
            writes_since_sync: 0,
        })
    }

    /// Append an engine event to the WAL.
    pub fn append(&mut self, sequence: u64, event: &EngineEvent) -> Result<()> {
        let payload = bincode::serialize(event)?;

        // length = 8 (sequence) + payload_len + 4 (crc)
        let length: u32 = (8 + payload.len() + 4) as u32;

        // CRC covers sequence + payload
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&sequence.to_le_bytes());
        hasher.update(&payload);
        let crc = hasher.finalize();

        // Write: [length: u32][sequence: u64][payload][crc: u32]
        self.current_file.write_all(&length.to_le_bytes())?;
        self.current_file.write_all(&sequence.to_le_bytes())?;
        self.current_file.write_all(&payload)?;
        self.current_file.write_all(&crc.to_le_bytes())?;

        let entry_size = 4 + length as u64;
        self.current_size += entry_size;

        if self.first_seq == 0 {
            self.first_seq = sequence;
        }
        self.last_seq = sequence;

        // fsync strategy
        self.writes_since_sync += 1;
        match &self.fsync_strategy {
            FsyncStrategy::Sync => {
                self.current_file.flush()?;
                self.current_file.get_ref().sync_data()?;
                self.writes_since_sync = 0;
            }
            FsyncStrategy::Batched { count } => {
                if self.writes_since_sync >= *count {
                    self.current_file.flush()?;
                    self.current_file.get_ref().sync_data()?;
                    self.writes_since_sync = 0;
                }
            }
            FsyncStrategy::Async => {}
        }

        // Rotation check
        if self.current_size >= self.max_size {
            self.rotate()?;
        }

        Ok(())
    }

    /// Flush all buffered data and fsync.
    pub fn flush(&mut self) -> Result<()> {
        self.current_file.flush()?;
        self.current_file.get_ref().sync_data()?;
        self.writes_since_sync = 0;
        Ok(())
    }

    /// Rotate the current WAL file: rename active to `wal_{first}_{last}.log`, open new active.
    fn rotate(&mut self) -> Result<()> {
        self.current_file.flush()?;
        self.current_file.get_ref().sync_data()?;

        let rotated_name = format!("wal_{}_{}.log", self.first_seq, self.last_seq);
        let active_path = self.dir.join("wal_active.log");
        let rotated_path = self.dir.join(&rotated_name);

        // We need to drop the old file handle before renaming on some platforms.
        // Re-open after rename.
        drop(std::mem::replace(
            &mut self.current_file,
            BufWriter::new(File::open("/dev/null").map_err(WalError::Io)?),
        ));

        fs::rename(&active_path, &rotated_path)?;

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&active_path)?;
        self.current_file = BufWriter::new(file);
        self.current_size = 0;
        self.first_seq = 0;
        self.last_seq = 0;
        self.writes_since_sync = 0;

        tracing::info!(path = %rotated_path.display(), "WAL file rotated");
        Ok(())
    }
}

/// Reads WAL entries from a single WAL file.
pub struct WalReader {
    reader: BufReader<File>,
}

impl WalReader {
    /// Open a WAL file for reading.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = File::open(path.as_ref())?;
        Ok(Self {
            reader: BufReader::new(file),
        })
    }

    /// Read the next entry. Returns `None` on clean EOF.
    pub fn next_entry(&mut self) -> Result<Option<WalEntry>> {
        // Read length (u32 LE)
        let mut len_buf = [0u8; 4];
        match self.reader.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(WalError::Io(e)),
        }
        let length = u32::from_le_bytes(len_buf) as usize;

        // Read the rest: `length` bytes = sequence(8) + payload(N) + crc(4)
        if length < 12 {
            return Err(WalError::UnexpectedEof);
        }
        let mut data = vec![0u8; length];
        match self.reader.read_exact(&mut data) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(WalError::UnexpectedEof);
            }
            Err(e) => return Err(WalError::Io(e)),
        }

        // Parse: first 8 bytes = sequence, last 4 = crc, middle = payload
        let seq_bytes: [u8; 8] = data[..8].try_into().map_err(|_| WalError::UnexpectedEof)?;
        let sequence = u64::from_le_bytes(seq_bytes);

        let crc_offset = length - 4;
        let crc_bytes: [u8; 4] = data[crc_offset..]
            .try_into()
            .map_err(|_| WalError::UnexpectedEof)?;
        let stored_crc = u32::from_le_bytes(crc_bytes);

        let payload = &data[8..crc_offset];

        // Verify CRC: covers sequence + payload
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&data[..8]); // sequence bytes
        hasher.update(payload);
        let computed_crc = hasher.finalize();

        if stored_crc != computed_crc {
            return Err(WalError::CrcMismatch {
                expected: stored_crc,
                actual: computed_crc,
            });
        }

        Ok(Some(WalEntry {
            sequence,
            data: payload.to_vec(),
        }))
    }
}

impl Iterator for WalReader {
    type Item = Result<WalEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_entry() {
            Ok(Some(entry)) => Some(Ok(entry)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

/// List all WAL files (both rotated and active) in a directory, sorted by first sequence number.
/// Rotated files are named `wal_{first}_{last}.log`; the active file is `wal_active.log`
/// (sorted last since its first_seq is unknown until read).
pub fn list_wal_files(dir: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
    let dir = dir.as_ref();
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut rotated: Vec<(u64, PathBuf)> = Vec::new();
    let mut active: Option<PathBuf> = None;

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name == "wal_active.log" {
                active = Some(path);
            } else if name.starts_with("wal_") && name.ends_with(".log") {
                // Parse wal_{first}_{last}.log
                let stem = &name[4..name.len() - 4]; // strip "wal_" and ".log"
                if let Some((first_str, _last_str)) = stem.split_once('_') {
                    if let Ok(first) = first_str.parse::<u64>() {
                        rotated.push((first, path));
                    }
                }
            }
        }
    }

    rotated.sort_by_key(|(seq, _)| *seq);
    let mut result: Vec<PathBuf> = rotated.into_iter().map(|(_, p)| p).collect();
    if let Some(a) = active {
        result.push(a);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn test_wal_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wal_dir = dir.path().join("wal");

        {
            let mut writer =
                WalWriter::new(&wal_dir, 1024 * 1024, FsyncStrategy::Sync).expect("writer");
            for i in 1..=100 {
                let event = make_event(i, i * 1000);
                writer.append(i, &event).expect("append");
            }
            writer.flush().expect("flush");
        }

        // Read back
        let files = list_wal_files(&wal_dir).expect("list");
        assert!(!files.is_empty());

        let mut all_entries = Vec::new();
        for file in &files {
            let reader = WalReader::open(file).expect("open");
            for entry in reader {
                all_entries.push(entry.expect("read entry"));
            }
        }

        assert_eq!(all_entries.len(), 100);
        for (i, entry) in all_entries.iter().enumerate() {
            let seq = (i + 1) as u64;
            assert_eq!(entry.sequence, seq);
            let event: EngineEvent = bincode::deserialize(&entry.data).expect("deserialize");
            match event {
                EngineEvent::OrderAccepted {
                    order_id,
                    timestamp,
                    ..
                } => {
                    assert_eq!(order_id.raw(), seq);
                    assert_eq!(timestamp, seq * 1000);
                }
                _ => panic!("unexpected event variant"),
            }
        }
    }

    #[test]
    fn test_wal_crc_validation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wal_dir = dir.path().join("wal");

        {
            let mut writer =
                WalWriter::new(&wal_dir, 1024 * 1024, FsyncStrategy::Sync).expect("writer");
            writer.append(1, &make_event(1, 1000)).expect("append");
            writer.flush().expect("flush");
        }

        // Corrupt a byte in the payload area
        let files = list_wal_files(&wal_dir).expect("list");
        let file_path = &files[0];
        let mut data = fs::read(file_path).expect("read file");
        // Corrupt byte at offset 20 (inside payload)
        if data.len() > 20 {
            data[20] ^= 0xFF;
        }
        fs::write(file_path, &data).expect("write corrupted");

        let mut reader = WalReader::open(file_path).expect("open");
        let result = reader.next_entry();
        assert!(
            matches!(result, Err(WalError::CrcMismatch { .. })),
            "expected CrcMismatch, got {result:?}"
        );
    }

    #[test]
    fn test_wal_rotation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wal_dir = dir.path().join("wal");

        // Use very small max_size to force rotation
        let mut writer = WalWriter::new(&wal_dir, 100, FsyncStrategy::Sync).expect("writer");
        for i in 1..=50 {
            writer.append(i, &make_event(i, i * 1000)).expect("append");
        }
        writer.flush().expect("flush");

        let files = list_wal_files(&wal_dir).expect("list");
        assert!(
            files.len() >= 2,
            "expected at least 2 WAL files after rotation, got {}",
            files.len()
        );

        // Verify all entries readable across all files
        let mut total = 0;
        for file in &files {
            let reader = WalReader::open(file).expect("open");
            for entry in reader {
                entry.expect("read entry");
                total += 1;
            }
        }
        assert_eq!(total, 50);
    }

    #[test]
    fn test_wal_empty_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wal_dir = dir.path().join("wal");
        fs::create_dir_all(&wal_dir).expect("create dir");

        // Create an empty WAL file
        let path = wal_dir.join("wal_active.log");
        File::create(&path).expect("create empty file");

        let mut reader = WalReader::open(&path).expect("open");
        let entry = reader.next_entry().expect("read");
        assert!(entry.is_none());
    }

    #[test]
    fn test_wal_fsync_batched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wal_dir = dir.path().join("wal");

        let mut writer = WalWriter::new(&wal_dir, 1024 * 1024, FsyncStrategy::Batched { count: 5 })
            .expect("writer");
        for i in 1..=12 {
            writer.append(i, &make_event(i, i * 1000)).expect("append");
        }
        writer.flush().expect("flush");

        // Verify all entries are readable
        let files = list_wal_files(&wal_dir).expect("list");
        let mut total = 0;
        for file in &files {
            let reader = WalReader::open(file).expect("open");
            for entry in reader {
                entry.expect("read entry");
                total += 1;
            }
        }
        assert_eq!(total, 12);
    }
}
