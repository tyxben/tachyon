use tachyon_book::OrderBook;
use tachyon_core::*;

/// Configuration for the risk manager.
#[derive(Clone, Debug)]
pub struct RiskConfig {
    /// Price band in basis points (e.g., 500 = 5%).
    pub price_band_pct: u64,
    /// Maximum allowed order quantity.
    pub max_order_qty: Quantity,
}

/// Pre-trade risk manager that runs inline before matching.
pub struct RiskManager {
    config: RiskConfig,
}

impl RiskManager {
    /// Creates a new RiskManager with the given configuration.
    pub fn new(config: RiskConfig) -> Self {
        RiskManager { config }
    }

    /// Runs all pre-trade risk checks on the order.
    ///
    /// Returns `Ok(())` if the order passes all checks, or an appropriate `EngineError`.
    pub fn check(&self, order: &Order, book: &OrderBook) -> Result<(), EngineError> {
        self.check_max_qty(order)?;
        self.check_price_band(order, book)?;
        Ok(())
    }

    /// Rejects orders whose quantity exceeds the configured maximum.
    fn check_max_qty(&self, order: &Order) -> Result<(), EngineError> {
        if order.quantity > self.config.max_order_qty {
            return Err(EngineError::InvalidQuantity(order.quantity));
        }
        Ok(())
    }

    /// Rejects limit orders whose price deviates too far from the current mid price.
    ///
    /// If BBO (best bid and offer) exists, computes the mid price and checks whether
    /// the order price is within `price_band_pct` basis points.
    /// Market orders skip this check.
    fn check_price_band(&self, order: &Order, book: &OrderBook) -> Result<(), EngineError> {
        // Skip for market orders
        if order.order_type == OrderType::Market {
            return Ok(());
        }

        // Skip if no band configured
        if self.config.price_band_pct == 0 {
            return Ok(());
        }

        let best_bid = book.best_bid_price();
        let best_ask = book.best_ask_price();

        // Only check if both sides have quotes
        let (bid, ask) = match (best_bid, best_ask) {
            (Some(b), Some(a)) => (b, a),
            _ => return Ok(()),
        };

        // Mid price = (bid + ask) / 2
        let mid = match bid.checked_add(ask) {
            Some(sum) => Price::new(sum.raw() / 2),
            None => return Ok(()),
        };

        if mid.is_zero() {
            return Ok(());
        }

        // deviation_bps = |order_price - mid| * 10000 / |mid|
        let diff = (order.price.raw() - mid.raw()).unsigned_abs();
        let deviation_bps = (diff as u128 * 10_000) / mid.raw().unsigned_abs() as u128;

        if deviation_bps > self.config.price_band_pct as u128 {
            return Err(EngineError::PriceOutOfRange);
        }

        Ok(())
    }
}
