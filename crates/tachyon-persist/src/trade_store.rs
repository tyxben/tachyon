//! Persistent trade storage for historical queries.
//!
//! Stores trades in append-only binary files with fixed-size 64-byte records.
//! Records are naturally time-ordered (engine processes sequentially), enabling
//! binary search for time-range queries without a separate index.
//!
//! File layout: `{dir}/trades_{symbol_id}.dat`
//! Each file is a flat sequence of `RECORD_SIZE`-byte records.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Size of a single trade record in bytes.
pub const RECORD_SIZE: usize = 56;

/// A single trade record stored on disk.
///
/// Fixed 56-byte layout for O(1) random access. Fields ordered by alignment
/// to avoid internal padding.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TradeRecord {
    /// Nanoseconds since UNIX epoch.
    pub timestamp: u64,
    /// Unique trade ID.
    pub trade_id: u64,
    /// Scaled price (same as engine internal representation).
    pub price: i64,
    /// Scaled quantity.
    pub quantity: u64,
    /// Maker order ID.
    pub maker_order_id: u64,
    /// Taker order ID.
    pub taker_order_id: u64,
    /// Symbol ID.
    pub symbol_id: u32,
    /// 0 = Buy, 1 = Sell (maker side).
    pub maker_side: u8,
    /// Reserved padding.
    pub _pad: [u8; 3],
}

// Compile-time assertion: TradeRecord must be exactly RECORD_SIZE bytes.
const _: () = assert!(std::mem::size_of::<TradeRecord>() == RECORD_SIZE);

impl TradeRecord {
    /// Serialize to bytes (little-endian).
    pub fn to_bytes(&self) -> [u8; RECORD_SIZE] {
        let mut buf = [0u8; RECORD_SIZE];
        buf[0..8].copy_from_slice(&self.timestamp.to_le_bytes());
        buf[8..16].copy_from_slice(&self.trade_id.to_le_bytes());
        buf[16..24].copy_from_slice(&self.price.to_le_bytes());
        buf[24..32].copy_from_slice(&self.quantity.to_le_bytes());
        buf[32..40].copy_from_slice(&self.maker_order_id.to_le_bytes());
        buf[40..48].copy_from_slice(&self.taker_order_id.to_le_bytes());
        buf[48..52].copy_from_slice(&self.symbol_id.to_le_bytes());
        buf[52] = self.maker_side;
        // 53..56 = padding (zeros)
        buf
    }

    /// Deserialize from bytes (little-endian).
    pub fn from_bytes(buf: &[u8; RECORD_SIZE]) -> Self {
        TradeRecord {
            timestamp: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            trade_id: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            price: i64::from_le_bytes(buf[16..24].try_into().unwrap()),
            quantity: u64::from_le_bytes(buf[24..32].try_into().unwrap()),
            maker_order_id: u64::from_le_bytes(buf[32..40].try_into().unwrap()),
            taker_order_id: u64::from_le_bytes(buf[40..48].try_into().unwrap()),
            symbol_id: u32::from_le_bytes(buf[48..52].try_into().unwrap()),
            maker_side: buf[52],
            _pad: [0; 3],
        }
    }
}

/// Append-only trade writer for a single symbol.
pub struct TradeWriter {
    file: File,
    record_count: u64,
}

impl TradeWriter {
    /// Open or create a trade file for the given symbol.
    pub fn open(dir: &Path, symbol_id: u32) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let path = trade_file_path(dir, symbol_id);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        let file_len = file.metadata()?.len();
        let record_count = file_len / RECORD_SIZE as u64;
        Ok(TradeWriter { file, record_count })
    }

    /// Append a trade record.
    #[inline]
    pub fn append(&mut self, record: &TradeRecord) -> io::Result<()> {
        let bytes = record.to_bytes();
        self.file.write_all(&bytes)?;
        self.record_count += 1;
        Ok(())
    }

    /// Flush buffered writes to OS.
    pub fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }

    /// Returns the number of records written.
    pub fn record_count(&self) -> u64 {
        self.record_count
    }
}

/// Read-only trade reader for historical queries.
pub struct TradeReader {
    path: PathBuf,
    record_count: u64,
}

impl TradeReader {
    /// Open a trade file for reading. Returns None if file doesn't exist.
    pub fn open(dir: &Path, symbol_id: u32) -> Option<Self> {
        let path = trade_file_path(dir, symbol_id);
        let meta = fs::metadata(&path).ok()?;
        let record_count = meta.len() / RECORD_SIZE as u64;
        if record_count == 0 {
            return None;
        }
        Some(TradeReader { path, record_count })
    }

    /// Returns the total number of trade records.
    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    /// Read a single record by index (0-based).
    pub fn read_record(&self, index: u64) -> io::Result<TradeRecord> {
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(index * RECORD_SIZE as u64))?;
        let mut buf = [0u8; RECORD_SIZE];
        file.read_exact(&mut buf)?;
        Ok(TradeRecord::from_bytes(&buf))
    }

    /// Read a range of records [start_index, end_index).
    pub fn read_range(&self, start_index: u64, end_index: u64) -> io::Result<Vec<TradeRecord>> {
        let end = end_index.min(self.record_count);
        if start_index >= end {
            return Ok(Vec::new());
        }
        let count = (end - start_index) as usize;
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(start_index * RECORD_SIZE as u64))?;

        let mut buf = vec![0u8; count * RECORD_SIZE];
        file.read_exact(&mut buf)?;

        let records: Vec<TradeRecord> = buf
            .chunks_exact(RECORD_SIZE)
            .map(|chunk| {
                let arr: &[u8; RECORD_SIZE] = chunk.try_into().unwrap();
                TradeRecord::from_bytes(arr)
            })
            .collect();

        Ok(records)
    }

    /// Binary search for the first record with timestamp >= `target_ts`.
    /// Returns the index, or `record_count` if all records have timestamp < target_ts.
    pub fn lower_bound(&self, target_ts: u64) -> io::Result<u64> {
        if self.record_count == 0 {
            return Ok(0);
        }
        let mut file = File::open(&self.path)?;
        let mut lo: u64 = 0;
        let mut hi: u64 = self.record_count;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            file.seek(SeekFrom::Start(mid * RECORD_SIZE as u64))?;
            let mut ts_buf = [0u8; 8];
            file.read_exact(&mut ts_buf)?;
            let ts = u64::from_le_bytes(ts_buf);
            if ts < target_ts {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }

    /// Binary search for the first record with timestamp > `target_ts`.
    /// Returns the index, or `record_count` if all records have timestamp <= target_ts.
    pub fn upper_bound(&self, target_ts: u64) -> io::Result<u64> {
        if self.record_count == 0 {
            return Ok(0);
        }
        let mut file = File::open(&self.path)?;
        let mut lo: u64 = 0;
        let mut hi: u64 = self.record_count;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            file.seek(SeekFrom::Start(mid * RECORD_SIZE as u64))?;
            let mut ts_buf = [0u8; 8];
            file.read_exact(&mut ts_buf)?;
            let ts = u64::from_le_bytes(ts_buf);
            if ts <= target_ts {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }

    /// Query trades in a time range [start_time, end_time) with limit.
    pub fn query_time_range(
        &self,
        start_time: u64,
        end_time: u64,
        limit: usize,
    ) -> io::Result<Vec<TradeRecord>> {
        let start_idx = self.lower_bound(start_time)?;
        let end_idx = self.upper_bound(end_time.saturating_sub(1))?;

        let actual_end = if limit > 0 {
            end_idx.min(start_idx + limit as u64)
        } else {
            end_idx
        };

        self.read_range(start_idx, actual_end)
    }

    /// Query trades starting from a given trade_id (inclusive), up to `limit` records.
    pub fn query_from_id(&self, from_trade_id: u64, limit: usize) -> io::Result<Vec<TradeRecord>> {
        // Linear scan from the end to find the trade_id position
        // (trade IDs are monotonically increasing, so we can binary search)
        let idx = self.lower_bound_by_trade_id(from_trade_id)?;
        let end = self.record_count.min(idx + limit as u64);
        self.read_range(idx, end)
    }

    /// Binary search for the first record with trade_id >= target.
    fn lower_bound_by_trade_id(&self, target_id: u64) -> io::Result<u64> {
        if self.record_count == 0 {
            return Ok(0);
        }
        let mut file = File::open(&self.path)?;
        let mut lo: u64 = 0;
        let mut hi: u64 = self.record_count;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            file.seek(SeekFrom::Start(mid * RECORD_SIZE as u64 + 8))?; // trade_id at offset 8
            let mut id_buf = [0u8; 8];
            file.read_exact(&mut id_buf)?;
            let id = u64::from_le_bytes(id_buf);
            if id < target_id {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }

    /// Get the most recent `limit` trades (tail of file).
    pub fn query_recent(&self, limit: usize) -> io::Result<Vec<TradeRecord>> {
        let start = self.record_count.saturating_sub(limit as u64);
        self.read_range(start, self.record_count)
    }

    /// Compute K-line (OHLCV candlestick) data for a time range and interval.
    ///
    /// `interval_ns` is the candlestick interval in nanoseconds.
    /// Returns a vec of (open_time, open, high, low, close, volume, trade_count).
    pub fn compute_klines(
        &self,
        start_time: u64,
        end_time: u64,
        interval_ns: u64,
        limit: usize,
    ) -> io::Result<Vec<Kline>> {
        let start_idx = self.lower_bound(start_time)?;
        let end_idx = if end_time == u64::MAX {
            self.record_count
        } else {
            self.upper_bound(end_time.saturating_sub(1))?
        };

        let records = self.read_range(start_idx, end_idx)?;
        let mut klines: Vec<Kline> = Vec::new();

        for record in &records {
            let bucket_start = (record.timestamp / interval_ns) * interval_ns;

            if let Some(last) = klines.last_mut() {
                if last.open_time == bucket_start {
                    // Update existing candle
                    if record.price > last.high {
                        last.high = record.price;
                    }
                    if record.price < last.low {
                        last.low = record.price;
                    }
                    last.close = record.price;
                    last.volume += record.quantity;
                    last.trade_count += 1;
                    continue;
                }
            }

            // New candle
            if klines.len() >= limit && limit > 0 {
                break;
            }

            klines.push(Kline {
                open_time: bucket_start,
                close_time: bucket_start + interval_ns - 1,
                open: record.price,
                high: record.price,
                low: record.price,
                close: record.price,
                volume: record.quantity,
                trade_count: 1,
            });
        }

        Ok(klines)
    }
}

/// OHLCV candlestick data point.
#[derive(Debug, Clone, PartialEq)]
pub struct Kline {
    pub open_time: u64,
    pub close_time: u64,
    pub open: i64,
    pub high: i64,
    pub low: i64,
    pub close: i64,
    pub volume: u64,
    pub trade_count: u64,
}

/// Manages trade storage for multiple symbols.
pub struct TradeStore {
    dir: PathBuf,
}

impl TradeStore {
    /// Create a new trade store backed by the given directory.
    pub fn new(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        Ok(TradeStore {
            dir: dir.to_path_buf(),
        })
    }

    /// Get the storage directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Open a writer for the given symbol.
    pub fn writer(&self, symbol_id: u32) -> io::Result<TradeWriter> {
        TradeWriter::open(&self.dir, symbol_id)
    }

    /// Open a reader for the given symbol. Returns None if no trades exist.
    pub fn reader(&self, symbol_id: u32) -> Option<TradeReader> {
        TradeReader::open(&self.dir, symbol_id)
    }

    /// Get total trade count across all symbols.
    pub fn total_records(&self) -> u64 {
        let mut total = 0;
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("trades_") && n.ends_with(".dat"))
                {
                    if let Ok(meta) = fs::metadata(&path) {
                        total += meta.len() / RECORD_SIZE as u64;
                    }
                }
            }
        }
        total
    }
}

fn trade_file_path(dir: &Path, symbol_id: u32) -> PathBuf {
    dir.join(format!("trades_{symbol_id}.dat"))
}

/// Parse a K-line interval string (e.g., "1m", "5m", "1h", "1d") into nanoseconds.
pub fn parse_kline_interval(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num_str, unit) = s.split_at(s.len() - 1);
    let num: u64 = num_str.parse().ok()?;
    let ns_per_unit = match unit {
        "s" => 1_000_000_000u64,
        "m" => 60_000_000_000u64,
        "h" => 3_600_000_000_000u64,
        "d" => 86_400_000_000_000u64,
        "w" => 604_800_000_000_000u64,
        _ => return None,
    };
    num.checked_mul(ns_per_unit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn make_record(ts: u64, id: u64, price: i64, qty: u64) -> TradeRecord {
        TradeRecord {
            timestamp: ts,
            trade_id: id,
            symbol_id: 0,
            maker_side: 0,
            _pad: [0; 3],
            price,
            quantity: qty,
            maker_order_id: 100,
            taker_order_id: 200,
        }
    }

    #[test]
    fn test_record_roundtrip() {
        let record = make_record(1_000_000_000, 42, 50000_00, 1_00000000);
        let bytes = record.to_bytes();
        assert_eq!(bytes.len(), RECORD_SIZE);
        let decoded = TradeRecord::from_bytes(&bytes);
        assert_eq!(decoded.timestamp, record.timestamp);
        assert_eq!(decoded.trade_id, record.trade_id);
        assert_eq!(decoded.price, record.price);
        assert_eq!(decoded.quantity, record.quantity);
        assert_eq!(decoded.maker_order_id, record.maker_order_id);
        assert_eq!(decoded.taker_order_id, record.taker_order_id);
        assert_eq!(decoded.maker_side, record.maker_side);
    }

    #[test]
    fn test_record_size() {
        assert_eq!(std::mem::size_of::<TradeRecord>(), RECORD_SIZE);
    }

    #[test]
    fn test_writer_reader_basic() {
        let dir = tempfile::tempdir().unwrap();
        let store = TradeStore::new(dir.path()).unwrap();

        // Write some trades
        {
            let mut writer = store.writer(0).unwrap();
            for i in 0..10 {
                let record = make_record(1000 + i * 100, i + 1, 50000 + i as i64, 100);
                writer.append(&record).unwrap();
            }
            writer.flush().unwrap();
            assert_eq!(writer.record_count(), 10);
        }

        // Read them back
        let reader = store.reader(0).unwrap();
        assert_eq!(reader.record_count(), 10);

        let first = reader.read_record(0).unwrap();
        assert_eq!(first.timestamp, 1000);
        assert_eq!(first.trade_id, 1);

        let last = reader.read_record(9).unwrap();
        assert_eq!(last.timestamp, 1900);
        assert_eq!(last.trade_id, 10);
    }

    #[test]
    fn test_time_range_query() {
        let dir = tempfile::tempdir().unwrap();
        let store = TradeStore::new(dir.path()).unwrap();

        {
            let mut writer = store.writer(0).unwrap();
            // Write 100 trades, 100ns apart
            for i in 0..100u64 {
                let record = make_record(1000 + i * 100, i + 1, 50000, 100);
                writer.append(&record).unwrap();
            }
            writer.flush().unwrap();
        }

        let reader = store.reader(0).unwrap();

        // Query [1500, 2500) should return trades at ts=1500,1600,1700,1800,1900,2000,2100,2200,2300,2400
        let results = reader.query_time_range(1500, 2500, 0).unwrap();
        assert_eq!(results.len(), 10);
        assert_eq!(results[0].timestamp, 1500);
        assert_eq!(results[9].timestamp, 2400);

        // With limit
        let results = reader.query_time_range(1500, 2500, 3).unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_query_from_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = TradeStore::new(dir.path()).unwrap();

        {
            let mut writer = store.writer(0).unwrap();
            for i in 0..50u64 {
                let record = make_record(1000 + i * 10, i * 2 + 1, 50000, 100);
                writer.append(&record).unwrap();
            }
            writer.flush().unwrap();
        }

        let reader = store.reader(0).unwrap();

        // Query from trade_id=21 (which is at index 10), limit 5
        let results = reader.query_from_id(21, 5).unwrap();
        assert_eq!(results.len(), 5);
        assert_eq!(results[0].trade_id, 21);
        assert_eq!(results[4].trade_id, 29);
    }

    #[test]
    fn test_query_recent() {
        let dir = tempfile::tempdir().unwrap();
        let store = TradeStore::new(dir.path()).unwrap();

        {
            let mut writer = store.writer(0).unwrap();
            for i in 0..20u64 {
                let record = make_record(1000 + i * 10, i + 1, 50000, 100);
                writer.append(&record).unwrap();
            }
            writer.flush().unwrap();
        }

        let reader = store.reader(0).unwrap();
        let results = reader.query_recent(5).unwrap();
        assert_eq!(results.len(), 5);
        assert_eq!(results[0].trade_id, 16);
        assert_eq!(results[4].trade_id, 20);
    }

    #[test]
    fn test_klines() {
        let dir = tempfile::tempdir().unwrap();
        let store = TradeStore::new(dir.path()).unwrap();

        // Write trades: 2 candles of 1-second each
        {
            let mut writer = store.writer(0).unwrap();
            // Candle 1: [0ns, 1_000_000_000ns)
            writer
                .append(&make_record(100_000_000, 1, 100, 10))
                .unwrap(); // open
            writer
                .append(&make_record(500_000_000, 2, 120, 20))
                .unwrap(); // high
            writer
                .append(&make_record(800_000_000, 3, 90, 15))
                .unwrap(); // low, close

            // Candle 2: [1_000_000_000ns, 2_000_000_000ns)
            writer
                .append(&make_record(1_100_000_000, 4, 95, 25))
                .unwrap(); // open
            writer
                .append(&make_record(1_500_000_000, 5, 110, 30))
                .unwrap(); // high, close

            writer.flush().unwrap();
        }

        let reader = store.reader(0).unwrap();
        let klines = reader
            .compute_klines(0, 2_000_000_000, 1_000_000_000, 100)
            .unwrap();

        assert_eq!(klines.len(), 2);

        // Candle 1
        assert_eq!(klines[0].open_time, 0);
        assert_eq!(klines[0].open, 100);
        assert_eq!(klines[0].high, 120);
        assert_eq!(klines[0].low, 90);
        assert_eq!(klines[0].close, 90);
        assert_eq!(klines[0].volume, 45); // 10 + 20 + 15
        assert_eq!(klines[0].trade_count, 3);

        // Candle 2
        assert_eq!(klines[1].open_time, 1_000_000_000);
        assert_eq!(klines[1].open, 95);
        assert_eq!(klines[1].high, 110);
        assert_eq!(klines[1].close, 110);
        assert_eq!(klines[1].volume, 55); // 25 + 30
        assert_eq!(klines[1].trade_count, 2);
    }

    #[test]
    fn test_klines_limit() {
        let dir = tempfile::tempdir().unwrap();
        let store = TradeStore::new(dir.path()).unwrap();

        {
            let mut writer = store.writer(0).unwrap();
            for i in 0..100u64 {
                // 1 trade per second
                writer
                    .append(&make_record(i * 1_000_000_000, i + 1, 100, 10))
                    .unwrap();
            }
            writer.flush().unwrap();
        }

        let reader = store.reader(0).unwrap();
        // 1-second candles, limit 10
        let klines = reader
            .compute_klines(0, u64::MAX, 1_000_000_000, 10)
            .unwrap();
        assert_eq!(klines.len(), 10);
    }

    #[test]
    fn test_parse_kline_interval() {
        assert_eq!(parse_kline_interval("1s"), Some(1_000_000_000));
        assert_eq!(parse_kline_interval("1m"), Some(60_000_000_000));
        assert_eq!(parse_kline_interval("5m"), Some(300_000_000_000));
        assert_eq!(parse_kline_interval("15m"), Some(900_000_000_000));
        assert_eq!(parse_kline_interval("1h"), Some(3_600_000_000_000));
        assert_eq!(parse_kline_interval("4h"), Some(14_400_000_000_000));
        assert_eq!(parse_kline_interval("1d"), Some(86_400_000_000_000));
        assert_eq!(parse_kline_interval("1w"), Some(604_800_000_000_000));
        assert_eq!(parse_kline_interval(""), None);
        assert_eq!(parse_kline_interval("1x"), None);
        assert_eq!(parse_kline_interval("abc"), None);
    }

    #[test]
    fn test_multi_symbol_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = TradeStore::new(dir.path()).unwrap();

        // Write to two symbols
        {
            let mut w0 = store.writer(0).unwrap();
            let mut w1 = store.writer(1).unwrap();
            for i in 0..5u64 {
                w0.append(&make_record(1000 + i, i + 1, 50000, 100))
                    .unwrap();
                w1.append(&make_record(2000 + i, i + 100, 3000, 50))
                    .unwrap();
            }
            w0.flush().unwrap();
            w1.flush().unwrap();
        }

        assert_eq!(store.total_records(), 10);

        let r0 = store.reader(0).unwrap();
        assert_eq!(r0.record_count(), 5);
        let r1 = store.reader(1).unwrap();
        assert_eq!(r1.record_count(), 5);

        // No trades for symbol 2
        assert!(store.reader(2).is_none());
    }

    #[test]
    fn test_empty_reader() {
        let dir = tempfile::tempdir().unwrap();
        let store = TradeStore::new(dir.path()).unwrap();
        assert!(store.reader(0).is_none());
    }

    #[test]
    fn test_writer_append_persists() {
        let dir = tempfile::tempdir().unwrap();
        let store = TradeStore::new(dir.path()).unwrap();

        // Write 5 records, close, reopen and write 5 more
        {
            let mut writer = store.writer(0).unwrap();
            for i in 0..5u64 {
                writer.append(&make_record(i * 100, i + 1, 100, 10)).unwrap();
            }
            writer.flush().unwrap();
        }
        {
            let mut writer = store.writer(0).unwrap();
            assert_eq!(writer.record_count(), 5); // picks up existing count
            for i in 5..10u64 {
                writer.append(&make_record(i * 100, i + 1, 100, 10)).unwrap();
            }
            writer.flush().unwrap();
            assert_eq!(writer.record_count(), 10);
        }

        let reader = store.reader(0).unwrap();
        assert_eq!(reader.record_count(), 10);
        let all = reader.read_range(0, 10).unwrap();
        assert_eq!(all.len(), 10);
        assert_eq!(all[0].trade_id, 1);
        assert_eq!(all[9].trade_id, 10);
    }

    #[test]
    fn test_lower_bound_edge_cases() {
        let dir = tempfile::tempdir().unwrap();
        let store = TradeStore::new(dir.path()).unwrap();

        {
            let mut writer = store.writer(0).unwrap();
            writer.append(&make_record(100, 1, 50, 10)).unwrap();
            writer.append(&make_record(200, 2, 50, 10)).unwrap();
            writer.append(&make_record(300, 3, 50, 10)).unwrap();
            writer.flush().unwrap();
        }

        let reader = store.reader(0).unwrap();

        // Before all records
        assert_eq!(reader.lower_bound(0).unwrap(), 0);
        assert_eq!(reader.lower_bound(50).unwrap(), 0);

        // Exact match
        assert_eq!(reader.lower_bound(100).unwrap(), 0);
        assert_eq!(reader.lower_bound(200).unwrap(), 1);
        assert_eq!(reader.lower_bound(300).unwrap(), 2);

        // Between records
        assert_eq!(reader.lower_bound(150).unwrap(), 1);
        assert_eq!(reader.lower_bound(250).unwrap(), 2);

        // After all records
        assert_eq!(reader.lower_bound(400).unwrap(), 3);
    }

    #[test]
    fn test_query_time_range_empty_result() {
        let dir = tempfile::tempdir().unwrap();
        let store = TradeStore::new(dir.path()).unwrap();

        {
            let mut writer = store.writer(0).unwrap();
            writer.append(&make_record(1000, 1, 50, 10)).unwrap();
            writer.append(&make_record(2000, 2, 50, 10)).unwrap();
            writer.flush().unwrap();
        }

        let reader = store.reader(0).unwrap();

        // Range entirely before data
        let r = reader.query_time_range(0, 500, 0).unwrap();
        assert!(r.is_empty());

        // Range entirely after data
        let r = reader.query_time_range(3000, 4000, 0).unwrap();
        assert!(r.is_empty());
    }
}
