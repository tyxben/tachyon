use crate::{Price, Quantity, Symbol};

/// Per-symbol configuration defining price/quantity constraints and scaling.
///
/// Different trading pairs have different precisions and limits. For example,
/// BTC/USDT may have `tick_size = 0.01` while a small-cap token might use `0.000001`.
/// These parameters are set when the symbol is created and should not change
/// while trading is active.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SymbolConfig {
    /// The symbol this configuration applies to.
    pub symbol: Symbol,
    /// Minimum price increment (as a scaled integer).
    pub tick_size: Price,
    /// Minimum quantity increment (as a scaled integer).
    pub lot_size: Quantity,
    /// Number of decimal places for prices (e.g., 8 means scale factor = 10^8).
    pub price_scale: u8,
    /// Number of decimal places for quantities.
    pub qty_scale: u8,
    /// Maximum allowed price.
    pub max_price: Price,
    /// Minimum allowed price.
    pub min_price: Price,
    /// Maximum quantity per order.
    pub max_order_qty: Quantity,
    /// Minimum quantity per order.
    pub min_order_qty: Quantity,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symbol_config_creation() {
        let config = SymbolConfig {
            symbol: Symbol::new(0),
            tick_size: Price::new(1),
            lot_size: Quantity::new(1),
            price_scale: 2,
            qty_scale: 8,
            max_price: Price::new(1_000_000_00),
            min_price: Price::new(1),
            max_order_qty: Quantity::new(1_000_000_00000000),
            min_order_qty: Quantity::new(1),
        };
        assert_eq!(config.symbol, Symbol::new(0));
        assert_eq!(config.price_scale, 2);
        assert_eq!(config.qty_scale, 8);
    }

    #[test]
    fn test_symbol_config_clone() {
        let config = SymbolConfig {
            symbol: Symbol::new(1),
            tick_size: Price::new(100),
            lot_size: Quantity::new(100),
            price_scale: 8,
            qty_scale: 8,
            max_price: Price::new(i64::MAX),
            min_price: Price::new(0),
            max_order_qty: Quantity::new(u64::MAX),
            min_order_qty: Quantity::new(1),
        };
        let cloned = config.clone();
        assert_eq!(cloned.symbol, config.symbol);
        assert_eq!(cloned.tick_size, config.tick_size);
        assert_eq!(cloned.price_scale, config.price_scale);
    }
}
