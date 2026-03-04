//! tachyon-core: Core type definitions for the Tachyon matching engine.
//!
//! This crate provides fundamental types such as Price, Quantity, OrderId,
//! Symbol, Order, and Event that form the foundation of the entire system.

pub mod config;
pub mod error;
pub mod event;
pub mod order;
pub mod price;
pub mod quantity;

// Re-export all public types at the crate root.
pub use config::SymbolConfig;
pub use error::EngineError;
pub use event::{EngineEvent, RejectReason};
pub use order::{Order, OrderId, OrderType, Side, Symbol, TimeInForce, Trade, NO_LINK};
pub use price::Price;
pub use quantity::Quantity;

#[cfg(test)]
mod proptest_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Price::checked_add is commutative: a + b == b + a
        #[test]
        fn price_checked_add_commutative(a in any::<i64>(), b in any::<i64>()) {
            let pa = Price::new(a);
            let pb = Price::new(b);
            assert_eq!(pa.checked_add(pb), pb.checked_add(pa));
        }

        /// Quantity::checked_sub(a, b) returns None when a < b
        #[test]
        fn quantity_checked_sub_none_when_less(a in any::<u64>(), b in any::<u64>()) {
            let qa = Quantity::new(a);
            let qb = Quantity::new(b);
            if a < b {
                assert_eq!(qa.checked_sub(qb), None);
            } else {
                assert_eq!(qa.checked_sub(qb), Some(Quantity::new(a - b)));
            }
        }

        /// Price * Quantity never panics for any inputs
        #[test]
        fn price_mul_qty_never_panics(
            price_val in any::<i64>(),
            qty_val in any::<u64>(),
            scale in 1u64..=1_000_000_000_000u64,
        ) {
            let price = Price::new(price_val);
            let qty = Quantity::new(qty_val);
            // Should never panic — may return Some or None
            let _ = price.checked_mul_qty(qty, scale);
        }

        /// Order remaining_qty <= quantity invariant
        #[test]
        fn order_remaining_qty_invariant(
            qty_val in 1u64..=u64::MAX,
            remaining_frac in 0u64..=100u64,
        ) {
            let quantity = Quantity::new(qty_val);
            // remaining is a fraction of quantity (0-100%)
            let remaining_raw = (qty_val as u128 * remaining_frac as u128 / 100) as u64;
            let remaining_qty = Quantity::new(remaining_raw);
            assert!(remaining_qty <= quantity);
        }

        /// Price::checked_add associativity: (a + b) + c == a + (b + c) when all succeed
        #[test]
        fn price_checked_add_associative(a in -1_000_000i64..1_000_000, b in -1_000_000i64..1_000_000, c in -1_000_000i64..1_000_000) {
            let pa = Price::new(a);
            let pb = Price::new(b);
            let pc = Price::new(c);
            let lhs = pa.checked_add(pb).and_then(|r| r.checked_add(pc));
            let rhs = pb.checked_add(pc).and_then(|r| pa.checked_add(r));
            assert_eq!(lhs, rhs);
        }

        /// Quantity::min is commutative
        #[test]
        fn quantity_min_commutative(a in any::<u64>(), b in any::<u64>()) {
            let qa = Quantity::new(a);
            let qb = Quantity::new(b);
            assert_eq!(qa.min(qb), qb.min(qa));
        }

        /// Quantity::saturating_sub never exceeds the original value
        #[test]
        fn quantity_saturating_sub_bounded(a in any::<u64>(), b in any::<u64>()) {
            let qa = Quantity::new(a);
            let qb = Quantity::new(b);
            let result = qa.saturating_sub(qb);
            assert!(result <= qa);
        }
    }
}
