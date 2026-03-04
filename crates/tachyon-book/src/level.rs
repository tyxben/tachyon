use tachyon_core::{Price, Quantity};

/// A price level in the order book.
///
/// Contains metadata about all orders at a given price, plus head/tail indices
/// into the intrusive doubly-linked list stored in the orders slab.
#[derive(Clone, Debug)]
pub struct PriceLevel {
    pub price: Price,
    pub total_quantity: Quantity,
    pub order_count: u32,
    /// Slab index of the first (oldest) order at this price level.
    pub head_idx: u32,
    /// Slab index of the last (newest) order at this price level.
    pub tail_idx: u32,
}

impl PriceLevel {
    /// Creates a new empty price level at the given price.
    #[inline]
    pub fn new(price: Price) -> Self {
        PriceLevel {
            price,
            total_quantity: Quantity::ZERO,
            order_count: 0,
            head_idx: tachyon_core::NO_LINK,
            tail_idx: tachyon_core::NO_LINK,
        }
    }

    /// Returns `true` if this price level has no orders.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.order_count == 0
    }

    /// Adds the given quantity to the level's total.
    #[inline]
    pub fn add_order_qty(&mut self, qty: Quantity) {
        // Use checked_add in case of overflow; saturate to avoid panic.
        self.total_quantity = self
            .total_quantity
            .checked_add(qty)
            .unwrap_or(Quantity::new(u64::MAX));
        self.order_count = self.order_count.saturating_add(1);
    }

    /// Subtracts the given quantity from the level's total.
    #[inline]
    pub fn sub_order_qty(&mut self, qty: Quantity) {
        self.total_quantity = self.total_quantity.saturating_sub(qty);
        self.order_count = self.order_count.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_level_is_empty() {
        let level = PriceLevel::new(Price::new(100));
        assert!(level.is_empty());
        assert_eq!(level.total_quantity, Quantity::ZERO);
        assert_eq!(level.order_count, 0);
        assert_eq!(level.head_idx, tachyon_core::NO_LINK);
        assert_eq!(level.tail_idx, tachyon_core::NO_LINK);
    }

    #[test]
    fn test_add_order_qty() {
        let mut level = PriceLevel::new(Price::new(100));
        level.add_order_qty(Quantity::new(50));
        assert_eq!(level.total_quantity, Quantity::new(50));
        assert_eq!(level.order_count, 1);
        assert!(!level.is_empty());

        level.add_order_qty(Quantity::new(30));
        assert_eq!(level.total_quantity, Quantity::new(80));
        assert_eq!(level.order_count, 2);
    }

    #[test]
    fn test_sub_order_qty() {
        let mut level = PriceLevel::new(Price::new(100));
        level.add_order_qty(Quantity::new(50));
        level.add_order_qty(Quantity::new(30));

        level.sub_order_qty(Quantity::new(50));
        assert_eq!(level.total_quantity, Quantity::new(30));
        assert_eq!(level.order_count, 1);
    }

    #[test]
    fn test_sub_order_qty_saturates() {
        let mut level = PriceLevel::new(Price::new(100));
        level.sub_order_qty(Quantity::new(100));
        assert_eq!(level.total_quantity, Quantity::ZERO);
        assert_eq!(level.order_count, 0);
    }
}
