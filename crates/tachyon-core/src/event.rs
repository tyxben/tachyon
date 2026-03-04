use crate::{OrderId, Price, Quantity, Side, Symbol, Trade};

/// Reason an order was rejected by the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RejectReason {
    InsufficientLiquidity,
    PriceOutOfRange,
    InvalidQuantity,
    InvalidPrice,
    BookFull,
    RateLimitExceeded,
    SelfTradePrevented,
    PostOnlyWouldTake,
    DuplicateOrderId,
}

/// Events emitted by the matching engine.
///
/// All state changes are expressed as events, forming the basis of event sourcing.
/// This enables deterministic replay, crash recovery, and auditing.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum EngineEvent {
    OrderAccepted {
        order_id: OrderId,
        symbol: Symbol,
        side: Side,
        price: Price,
        qty: Quantity,
        timestamp: u64,
    },
    OrderRejected {
        order_id: OrderId,
        reason: RejectReason,
        timestamp: u64,
    },
    OrderCancelled {
        order_id: OrderId,
        remaining_qty: Quantity,
        timestamp: u64,
    },
    OrderExpired {
        order_id: OrderId,
        timestamp: u64,
    },
    Trade {
        trade: Trade,
    },
    BookUpdate {
        symbol: Symbol,
        side: Side,
        price: Price,
        new_total_qty: Quantity,
        timestamp: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reject_reason_variants() {
        let reasons = [
            RejectReason::InsufficientLiquidity,
            RejectReason::PriceOutOfRange,
            RejectReason::InvalidQuantity,
            RejectReason::InvalidPrice,
            RejectReason::BookFull,
            RejectReason::RateLimitExceeded,
            RejectReason::SelfTradePrevented,
            RejectReason::PostOnlyWouldTake,
            RejectReason::DuplicateOrderId,
        ];
        // Each variant should be distinct
        for (i, a) in reasons.iter().enumerate() {
            for (j, b) in reasons.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b);
                } else {
                    assert_ne!(a, b);
                }
            }
        }
    }

    #[test]
    fn test_engine_event_order_accepted() {
        let event = EngineEvent::OrderAccepted {
            order_id: OrderId::new(1),
            symbol: Symbol::new(0),
            side: Side::Buy,
            price: Price::new(100),
            qty: Quantity::new(10),
            timestamp: 1000,
        };
        // Verify it can be cloned and debug-printed
        let _cloned = event.clone();
        let _debug = format!("{:?}", event);
    }

    #[test]
    fn test_engine_event_trade() {
        let trade = Trade {
            trade_id: 1,
            symbol: Symbol::new(0),
            price: Price::new(50000),
            quantity: Quantity::new(100),
            maker_order_id: OrderId::new(1),
            taker_order_id: OrderId::new(2),
            maker_side: Side::Sell,
            timestamp: 1000,
        };
        let event = EngineEvent::Trade { trade };
        let _debug = format!("{:?}", event);
    }

    #[test]
    fn test_engine_event_book_update() {
        let event = EngineEvent::BookUpdate {
            symbol: Symbol::new(0),
            side: Side::Buy,
            price: Price::new(50000),
            new_total_qty: Quantity::new(500),
            timestamp: 2000,
        };
        let _debug = format!("{:?}", event);
    }
}
