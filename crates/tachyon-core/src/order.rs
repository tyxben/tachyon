use std::fmt;

use crate::{Price, Quantity};

/// Globally unique order identifier, assigned by the Sequencer.
///
/// Monotonically increasing. A 64-bit space supports ~1M orders/second for ~584,000 years.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub struct OrderId(pub u64);

impl OrderId {
    /// Creates a new OrderId.
    #[inline]
    pub const fn new(id: u64) -> Self {
        OrderId(id)
    }

    /// Returns the raw inner value.
    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Trading pair identifier. Uses `u32` internally for minimal memory and fast comparison.
///
/// The mapping between human-readable names (e.g. "BTC/USDT") and `Symbol(u32)` values
/// is maintained externally in a `SymbolRegistry`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub struct Symbol(pub u32);

impl Symbol {
    /// Creates a new Symbol.
    #[inline]
    pub const fn new(id: u32) -> Self {
        Symbol(id)
    }

    /// Returns the raw inner value.
    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Order side: buy or sell.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    /// Returns the opposite side.
    #[inline]
    pub const fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

/// Order type: limit or market.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum OrderType {
    Limit,
    Market,
}

/// Time-in-force policy governing how long an order stays active.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum TimeInForce {
    /// Good-Til-Cancel: remains on the book until explicitly cancelled.
    GTC,
    /// Immediate-or-Cancel: fills as much as possible immediately, cancels remainder.
    IOC,
    /// Fill-or-Kill: must be filled entirely or rejected entirely.
    FOK,
    /// Post-Only: rejected if it would match immediately (maker only).
    PostOnly,
    /// Good-Til-Date: expires at the given nanosecond timestamp.
    GTD(u64),
}

/// Sentinel value indicating no linked-list link (prev/next).
pub const NO_LINK: u32 = u32::MAX;

/// An order stored in the order book.
///
/// The `prev` and `next` fields form an intrusive doubly-linked list within a price level,
/// using slab indices (`u32`). `u32::MAX` (`NO_LINK`) is the sentinel for "no link".
///
/// Field order is optimized to minimize padding with `#[repr(C)]`:
/// 8-byte aligned fields first, then 4-byte, then 1-byte.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[repr(C)]
pub struct Order {
    // 8-byte aligned fields (48 bytes)
    pub id: OrderId,
    pub price: Price,
    pub quantity: Quantity,
    pub remaining_qty: Quantity,
    pub timestamp: u64,
    pub account_id: u64,
    // 16-byte enum (tag + u64 payload)
    pub time_in_force: TimeInForce,
    // 4-byte aligned fields (12 bytes)
    pub symbol: Symbol,
    /// Previous order in the same price level (slab index), `NO_LINK` if none.
    pub prev: u32,
    /// Next order in the same price level (slab index), `NO_LINK` if none.
    pub next: u32,
    // 1-byte fields (2 bytes + 2 bytes padding = 4 bytes)
    pub side: Side,
    pub order_type: OrderType,
}

/// A completed trade between a maker and a taker order.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Trade {
    pub trade_id: u64,
    pub symbol: Symbol,
    pub price: Price,
    pub quantity: Quantity,
    pub maker_order_id: OrderId,
    pub taker_order_id: OrderId,
    pub maker_side: Side,
    pub timestamp: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_order_id_display() {
        assert_eq!(format!("{}", OrderId::new(42)), "42");
    }

    #[test]
    fn test_order_id_equality() {
        assert_eq!(OrderId::new(1), OrderId::new(1));
        assert_ne!(OrderId::new(1), OrderId::new(2));
    }

    #[test]
    fn test_symbol_display() {
        assert_eq!(format!("{}", Symbol::new(7)), "7");
    }

    #[test]
    fn test_symbol_equality() {
        assert_eq!(Symbol::new(0), Symbol::new(0));
        assert_ne!(Symbol::new(0), Symbol::new(1));
    }

    #[test]
    fn test_side_opposite() {
        assert_eq!(Side::Buy.opposite(), Side::Sell);
        assert_eq!(Side::Sell.opposite(), Side::Buy);
        // Double opposite is identity
        assert_eq!(Side::Buy.opposite().opposite(), Side::Buy);
        assert_eq!(Side::Sell.opposite().opposite(), Side::Sell);
    }

    #[test]
    fn test_order_type_equality() {
        assert_eq!(OrderType::Limit, OrderType::Limit);
        assert_eq!(OrderType::Market, OrderType::Market);
        assert_ne!(OrderType::Limit, OrderType::Market);
    }

    #[test]
    fn test_time_in_force_equality() {
        assert_eq!(TimeInForce::GTC, TimeInForce::GTC);
        assert_eq!(TimeInForce::IOC, TimeInForce::IOC);
        assert_eq!(TimeInForce::FOK, TimeInForce::FOK);
        assert_eq!(TimeInForce::PostOnly, TimeInForce::PostOnly);
        assert_eq!(TimeInForce::GTD(1000), TimeInForce::GTD(1000));
        assert_ne!(TimeInForce::GTD(1000), TimeInForce::GTD(2000));
        assert_ne!(TimeInForce::GTC, TimeInForce::IOC);
    }

    #[test]
    fn test_no_link_sentinel() {
        assert_eq!(NO_LINK, u32::MAX);
    }

    #[test]
    fn test_order_sentinel_values() {
        let order = Order {
            id: OrderId::new(1),
            symbol: Symbol::new(0),
            side: Side::Buy,
            price: Price::new(100),
            quantity: Quantity::new(10),
            remaining_qty: Quantity::new(10),
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            timestamp: 0,
            account_id: 0,
            prev: NO_LINK,
            next: NO_LINK,
        };
        assert_eq!(order.prev, u32::MAX);
        assert_eq!(order.next, u32::MAX);
    }

    #[test]
    fn test_order_remaining_qty_invariant() {
        let order = Order {
            id: OrderId::new(1),
            symbol: Symbol::new(0),
            side: Side::Buy,
            price: Price::new(100),
            quantity: Quantity::new(100),
            remaining_qty: Quantity::new(50),
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            timestamp: 0,
            account_id: 0,
            prev: NO_LINK,
            next: NO_LINK,
        };
        assert!(order.remaining_qty <= order.quantity);
    }

    #[test]
    fn test_trade_creation() {
        let trade = Trade {
            trade_id: 1,
            symbol: Symbol::new(0),
            price: Price::new(50000),
            quantity: Quantity::new(100),
            maker_order_id: OrderId::new(1),
            taker_order_id: OrderId::new(2),
            maker_side: Side::Buy,
            timestamp: 1000,
        };
        assert_eq!(trade.trade_id, 1);
        assert_eq!(trade.maker_side, Side::Buy);
    }
}
