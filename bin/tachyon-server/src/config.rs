use std::collections::HashMap;

use serde::Deserialize;
use tachyon_core::{Price, Quantity, Symbol, SymbolConfig};
use tachyon_persist::{FsyncStrategy, PersistConfig};

#[derive(Deserialize, Debug, Clone)]
pub struct ServerConfig {
    pub server: ServerSection,
    #[allow(dead_code)]
    pub engine: EngineSection,
    pub symbols: HashMap<String, SymbolSection>,
    #[serde(default)]
    pub logging: LoggingSection,
    pub persistence: Option<PersistenceSection>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct PersistenceSection {
    pub wal_dir: String,
    pub snapshot_dir: String,
    #[serde(default = "default_fsync")]
    pub fsync_strategy: String,
    #[serde(default = "default_batch_size")]
    pub fsync_batch_size: usize,
    #[serde(default = "default_max_wal_size")]
    pub max_wal_size_mb: u64,
    #[serde(default = "default_snap_events")]
    pub snapshot_interval_events: u64,
    #[serde(default = "default_snap_secs")]
    pub snapshot_interval_secs: u64,
}

fn default_fsync() -> String {
    "batched".to_string()
}

fn default_batch_size() -> usize {
    1000
}

fn default_max_wal_size() -> u64 {
    256
}

fn default_snap_events() -> u64 {
    100_000
}

fn default_snap_secs() -> u64 {
    300
}

/// Parse a fsync strategy string into a `FsyncStrategy` enum.
pub fn parse_fsync_strategy(s: &str, batch_size: usize) -> Result<FsyncStrategy, String> {
    match s {
        "sync" => Ok(FsyncStrategy::Sync),
        "batched" => Ok(FsyncStrategy::Batched { count: batch_size }),
        "async" => Ok(FsyncStrategy::Async),
        other => Err(format!(
            "unknown fsync_strategy: '{}', expected 'sync', 'batched', or 'async'",
            other
        )),
    }
}

impl PersistenceSection {
    /// Convert to a `PersistConfig` used by the persistence layer.
    pub fn to_persist_config(&self) -> Result<PersistConfig, String> {
        let fsync_strategy = parse_fsync_strategy(&self.fsync_strategy, self.fsync_batch_size)?;
        Ok(PersistConfig {
            wal_dir: self.wal_dir.clone().into(),
            snapshot_dir: self.snapshot_dir.clone().into(),
            fsync_strategy,
            max_wal_size: self.max_wal_size_mb * 1024 * 1024,
            snapshot_interval_events: self.snapshot_interval_events,
            snapshot_interval_secs: self.snapshot_interval_secs,
        })
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct ServerSection {
    #[serde(default = "default_bind")]
    pub bind_address: String,
    #[serde(default = "default_ws_port")]
    pub websocket_port: u16,
    #[serde(default = "default_rest_port")]
    pub rest_port: u16,
    #[serde(default = "default_tcp_port")]
    pub tcp_port: u16,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug, Clone)]
pub struct EngineSection {
    #[serde(default)]
    pub sequencer_core: Option<usize>,
    #[serde(default)]
    pub matching_cores: Vec<usize>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct SymbolSection {
    pub tick_size: String,
    pub lot_size: String,
    pub price_scale: u8,
    pub qty_scale: u8,
    pub max_price: String,
    pub min_price: String,
    pub max_order_qty: String,
    #[serde(default = "default_min_qty")]
    pub min_order_qty: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct LoggingSection {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default = "default_log_format")]
    pub format: String,
}

impl Default for LoggingSection {
    fn default() -> Self {
        LoggingSection {
            level: default_log_level(),
            format: default_log_format(),
        }
    }
}

fn default_bind() -> String {
    "0.0.0.0".to_string()
}

fn default_ws_port() -> u16 {
    8080
}

fn default_rest_port() -> u16 {
    8081
}

fn default_tcp_port() -> u16 {
    8082
}

fn default_min_qty() -> String {
    "0.001".to_string()
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_log_format() -> String {
    "json".to_string()
}

/// Load server configuration from a TOML file at the given path.
pub fn load_config(path: &str) -> Result<ServerConfig, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let config: ServerConfig = toml::from_str(&content)?;
    Ok(config)
}

/// Parse a decimal string (e.g., "0.01") into a scaled integer value.
fn parse_scaled_value(s: &str, scale: u8) -> Result<u64, Box<dyn std::error::Error>> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty value".into());
    }
    let parts: Vec<&str> = s.split('.').collect();
    match parts.len() {
        1 => {
            let whole: u64 = parts[0].parse()?;
            let factor = 10u64.checked_pow(scale as u32).ok_or("scale overflow")?;
            whole
                .checked_mul(factor)
                .ok_or_else(|| "value overflow".into())
        }
        2 => {
            let whole: u64 = parts[0].parse()?;
            let frac_str = parts[1];
            let frac_len = frac_str.len();
            if frac_len > scale as usize {
                return Err(
                    format!("too many decimal places: {} (max {})", frac_len, scale).into(),
                );
            }
            let frac: u64 = if frac_str.is_empty() {
                0
            } else {
                frac_str.parse()?
            };
            let factor = 10u64.checked_pow(scale as u32).ok_or("scale overflow")?;
            let frac_factor = 10u64
                .checked_pow((scale as usize - frac_len) as u32)
                .ok_or("frac scale overflow")?;
            let whole_scaled = whole.checked_mul(factor).ok_or("value overflow")?;
            let frac_scaled = frac.checked_mul(frac_factor).ok_or("frac overflow")?;
            whole_scaled
                .checked_add(frac_scaled)
                .ok_or_else(|| "addition overflow".into())
        }
        _ => Err("invalid decimal format".into()),
    }
}

impl SymbolSection {
    /// Convert this SymbolSection into a tachyon_core::SymbolConfig.
    pub fn to_symbol_config(
        &self,
        symbol: Symbol,
    ) -> Result<SymbolConfig, Box<dyn std::error::Error>> {
        let tick_size_raw = parse_scaled_value(&self.tick_size, self.price_scale)?;
        let lot_size_raw = parse_scaled_value(&self.lot_size, self.qty_scale)?;
        let max_price_raw = parse_scaled_value(&self.max_price, self.price_scale)?;
        let min_price_raw = parse_scaled_value(&self.min_price, self.price_scale)?;
        let max_order_qty_raw = parse_scaled_value(&self.max_order_qty, self.qty_scale)?;
        let min_order_qty_raw = parse_scaled_value(&self.min_order_qty, self.qty_scale)?;

        Ok(SymbolConfig {
            symbol,
            tick_size: Price::new(tick_size_raw as i64),
            lot_size: Quantity::new(lot_size_raw),
            price_scale: self.price_scale,
            qty_scale: self.qty_scale,
            max_price: Price::new(max_price_raw as i64),
            min_price: Price::new(min_price_raw as i64),
            max_order_qty: Quantity::new(max_order_qty_raw),
            min_order_qty: Quantity::new(min_order_qty_raw),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_TOML: &str = r#"
[server]
bind_address = "127.0.0.1"
websocket_port = 9090
rest_port = 9091

[engine]
sequencer_core = 0
matching_cores = [1, 2]

[logging]
level = "debug"
format = "pretty"

[symbols.BTCUSDT]
tick_size = "0.01"
lot_size = "0.001"
price_scale = 2
qty_scale = 3
max_price = "1000000.00"
min_price = "0.01"
max_order_qty = "1000.000"
min_order_qty = "0.001"

[symbols.ETHUSDT]
tick_size = "0.01"
lot_size = "0.01"
price_scale = 2
qty_scale = 2
max_price = "100000.00"
min_price = "0.01"
max_order_qty = "10000.00"
min_order_qty = "0.01"
"#;

    #[test]
    fn test_parse_valid_config() {
        let config: ServerConfig = toml::from_str(VALID_TOML).expect("parse config");
        assert_eq!(config.server.bind_address, "127.0.0.1");
        assert_eq!(config.server.websocket_port, 9090);
        assert_eq!(config.server.rest_port, 9091);
        assert_eq!(config.engine.sequencer_core, Some(0));
        assert_eq!(config.engine.matching_cores, vec![1, 2]);
        assert_eq!(config.logging.level, "debug");
        assert_eq!(config.logging.format, "pretty");
        assert_eq!(config.symbols.len(), 2);
        assert!(config.symbols.contains_key("BTCUSDT"));
        assert!(config.symbols.contains_key("ETHUSDT"));
    }

    #[test]
    fn test_config_defaults() {
        let toml_str = r#"
[server]

[engine]

[symbols.BTCUSDT]
tick_size = "0.01"
lot_size = "0.01"
price_scale = 2
qty_scale = 2
max_price = "100000.00"
min_price = "0.01"
max_order_qty = "1000.00"
"#;
        let config: ServerConfig = toml::from_str(toml_str).expect("parse config");
        assert_eq!(config.server.bind_address, "0.0.0.0");
        assert_eq!(config.server.websocket_port, 8080);
        assert_eq!(config.server.rest_port, 8081);
        assert_eq!(config.engine.sequencer_core, None);
        assert!(config.engine.matching_cores.is_empty());
        assert_eq!(config.logging.level, "info");
        assert_eq!(config.logging.format, "json");
        assert_eq!(config.symbols["BTCUSDT"].min_order_qty, "0.001");
    }

    #[test]
    fn test_config_missing_required_field() {
        let toml_str = r#"
[server]

[engine]

[symbols.BTCUSDT]
tick_size = "0.01"
price_scale = 2
qty_scale = 2
max_price = "100000.00"
min_price = "0.01"
max_order_qty = "1000.00"
"#;
        // lot_size is missing
        let result: Result<ServerConfig, _> = toml::from_str(toml_str);
        assert!(result.is_err());
    }

    #[test]
    fn test_symbol_section_to_symbol_config() {
        let section = SymbolSection {
            tick_size: "0.01".to_string(),
            lot_size: "0.001".to_string(),
            price_scale: 2,
            qty_scale: 3,
            max_price: "1000000.00".to_string(),
            min_price: "0.01".to_string(),
            max_order_qty: "1000.000".to_string(),
            min_order_qty: "0.001".to_string(),
        };

        let symbol = Symbol::new(0);
        let config = section.to_symbol_config(symbol).expect("convert");
        assert_eq!(config.symbol, symbol);
        assert_eq!(config.tick_size, Price::new(1));
        assert_eq!(config.lot_size, Quantity::new(1));
        assert_eq!(config.price_scale, 2);
        assert_eq!(config.qty_scale, 3);
        assert_eq!(config.max_price, Price::new(100_000_000));
        assert_eq!(config.min_price, Price::new(1));
        assert_eq!(config.max_order_qty, Quantity::new(1_000_000));
        assert_eq!(config.min_order_qty, Quantity::new(1));
    }

    #[test]
    fn test_parse_scaled_value_integer() {
        assert_eq!(parse_scaled_value("100", 2).unwrap(), 10_000);
        assert_eq!(parse_scaled_value("0", 2).unwrap(), 0);
    }

    #[test]
    fn test_parse_scaled_value_decimal() {
        assert_eq!(parse_scaled_value("50001.50", 2).unwrap(), 5_000_150);
        assert_eq!(parse_scaled_value("1.5", 2).unwrap(), 150);
        assert_eq!(parse_scaled_value("0.01", 2).unwrap(), 1);
        assert_eq!(parse_scaled_value("0.001", 3).unwrap(), 1);
    }

    #[test]
    fn test_parse_scaled_value_too_many_decimals() {
        assert!(parse_scaled_value("1.234", 2).is_err());
    }

    #[test]
    fn test_parse_scaled_value_empty() {
        assert!(parse_scaled_value("", 2).is_err());
    }

    #[test]
    fn test_parse_scaled_value_invalid() {
        assert!(parse_scaled_value("abc", 2).is_err());
    }

    #[test]
    fn test_parse_scaled_value_negative() {
        assert!(parse_scaled_value("-1.00", 2).is_err());
        assert!(parse_scaled_value("-100", 2).is_err());
    }

    #[test]
    fn test_parse_scaled_value_overflow() {
        assert!(parse_scaled_value("18446744073709551616", 0).is_err());
    }

    #[test]
    fn test_parse_scaled_value_whitespace() {
        assert_eq!(parse_scaled_value("  100  ", 2).unwrap(), 10_000);
    }

    #[test]
    fn test_load_config_file_not_found() {
        let result = load_config("/nonexistent/path/config.toml");
        assert!(result.is_err());
    }

    #[test]
    fn test_config_with_persistence_section() {
        let toml_str = r#"
[server]
bind_address = "127.0.0.1"
rest_port = 9091
websocket_port = 9090

[engine]

[logging]
level = "info"
format = "json"

[symbols.BTCUSDT]
tick_size = "0.01"
lot_size = "0.001"
price_scale = 2
qty_scale = 3
max_price = "1000000.00"
min_price = "0.01"
max_order_qty = "1000.000"
min_order_qty = "0.001"

[persistence]
wal_dir = "/tmp/tachyon/wal"
snapshot_dir = "/tmp/tachyon/snapshots"
fsync_strategy = "batched"
fsync_batch_size = 500
max_wal_size_mb = 128
snapshot_interval_events = 50000
snapshot_interval_secs = 600
"#;
        let config: ServerConfig = toml::from_str(toml_str).expect("parse config");
        let persist = config
            .persistence
            .expect("persistence section should exist");
        assert_eq!(persist.wal_dir, "/tmp/tachyon/wal");
        assert_eq!(persist.snapshot_dir, "/tmp/tachyon/snapshots");
        assert_eq!(persist.fsync_strategy, "batched");
        assert_eq!(persist.fsync_batch_size, 500);
        assert_eq!(persist.max_wal_size_mb, 128);
        assert_eq!(persist.snapshot_interval_events, 50000);
        assert_eq!(persist.snapshot_interval_secs, 600);

        // Test conversion to PersistConfig
        let persist_config = persist.to_persist_config().expect("convert");
        assert_eq!(persist_config.max_wal_size, 128 * 1024 * 1024);
        assert_eq!(persist_config.snapshot_interval_events, 50000);
        assert!(matches!(
            persist_config.fsync_strategy,
            tachyon_persist::FsyncStrategy::Batched { count: 500 }
        ));
    }

    #[test]
    fn test_config_without_persistence_section() {
        let toml_str = r#"
[server]

[engine]

[symbols.BTCUSDT]
tick_size = "0.01"
lot_size = "0.01"
price_scale = 2
qty_scale = 2
max_price = "100000.00"
min_price = "0.01"
max_order_qty = "1000.00"
"#;
        let config: ServerConfig = toml::from_str(toml_str).expect("parse config");
        assert!(config.persistence.is_none());
    }

    #[test]
    fn test_persistence_section_defaults() {
        let toml_str = r#"
[server]

[engine]

[symbols.BTCUSDT]
tick_size = "0.01"
lot_size = "0.01"
price_scale = 2
qty_scale = 2
max_price = "100000.00"
min_price = "0.01"
max_order_qty = "1000.00"

[persistence]
wal_dir = "/tmp/wal"
snapshot_dir = "/tmp/snap"
"#;
        let config: ServerConfig = toml::from_str(toml_str).expect("parse config");
        let persist = config.persistence.expect("should have persistence");
        assert_eq!(persist.fsync_strategy, "batched");
        assert_eq!(persist.fsync_batch_size, 1000);
        assert_eq!(persist.max_wal_size_mb, 256);
        assert_eq!(persist.snapshot_interval_events, 100_000);
        assert_eq!(persist.snapshot_interval_secs, 300);
    }

    #[test]
    fn test_fsync_strategy_parsing() {
        use super::parse_fsync_strategy;
        use tachyon_persist::FsyncStrategy;

        let sync = parse_fsync_strategy("sync", 0).expect("sync");
        assert!(matches!(sync, FsyncStrategy::Sync));

        let batched = parse_fsync_strategy("batched", 100).expect("batched");
        assert!(matches!(batched, FsyncStrategy::Batched { count: 100 }));

        let async_strat = parse_fsync_strategy("async", 0).expect("async");
        assert!(matches!(async_strat, FsyncStrategy::Async));

        let unknown = parse_fsync_strategy("unknown", 0);
        assert!(unknown.is_err());
    }
}
