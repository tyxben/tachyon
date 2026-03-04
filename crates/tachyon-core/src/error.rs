use crate::{OrderId, Price, Quantity, Symbol};

/// Errors returned by the matching engine.
#[derive(thiserror::Error, Debug)]
pub enum EngineError {
    #[error("order not found: {0}")]
    OrderNotFound(OrderId),

    #[error("symbol not found: {0:?}")]
    SymbolNotFound(Symbol),

    #[error("invalid price: {0:?}")]
    InvalidPrice(Price),

    #[error("invalid quantity: {0:?}")]
    InvalidQuantity(Quantity),

    #[error("order book full")]
    BookFull,

    #[error("price out of range")]
    PriceOutOfRange,

    #[error("rate limit exceeded")]
    RateLimitExceeded,

    #[error("self trade prevented")]
    SelfTradePrevented,

    #[error("duplicate order id: {0}")]
    DuplicateOrderId(OrderId),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_engine_error_display() {
        let err = EngineError::OrderNotFound(OrderId::new(42));
        assert_eq!(format!("{err}"), "order not found: 42");

        let err = EngineError::BookFull;
        assert_eq!(format!("{err}"), "order book full");

        let err = EngineError::PriceOutOfRange;
        assert_eq!(format!("{err}"), "price out of range");

        let err = EngineError::RateLimitExceeded;
        assert_eq!(format!("{err}"), "rate limit exceeded");

        let err = EngineError::SelfTradePrevented;
        assert_eq!(format!("{err}"), "self trade prevented");
    }

    #[test]
    fn test_engine_error_is_std_error() {
        let err: Box<dyn std::error::Error> = Box::new(EngineError::OrderNotFound(OrderId::new(1)));
        let _ = format!("{err}");
    }

    #[test]
    fn test_engine_error_debug() {
        let err = EngineError::InvalidPrice(Price::new(999));
        let debug = format!("{err:?}");
        assert!(debug.contains("InvalidPrice"));
    }
}
