use slab::Slab;
use tachyon_core::Order;

/// Thin wrapper around `slab::Slab<Order>` providing a pre-allocated object pool.
///
/// Orders are stored in a contiguous `Vec` internally, with O(1) insert and O(1) remove.
pub struct OrderPool {
    orders: Slab<Order>,
}

impl OrderPool {
    /// Creates an order pool with pre-allocated capacity.
    #[inline]
    pub fn with_capacity(cap: usize) -> Self {
        OrderPool {
            orders: Slab::with_capacity(cap),
        }
    }

    /// Inserts an order and returns its slab key.
    #[inline]
    pub fn insert(&mut self, order: Order) -> usize {
        self.orders.insert(order)
    }

    /// Removes and returns the order at the given key.
    ///
    /// # Panics
    /// Panics if the key is invalid.
    #[inline]
    pub fn remove(&mut self, key: usize) -> Order {
        self.orders.remove(key)
    }

    /// Returns a reference to the order at the given key.
    ///
    /// # Panics
    /// Panics if the key is invalid.
    #[inline]
    pub fn get(&self, key: usize) -> &Order {
        &self.orders[key]
    }

    /// Returns a mutable reference to the order at the given key.
    ///
    /// # Panics
    /// Panics if the key is invalid.
    #[inline]
    pub fn get_mut(&mut self, key: usize) -> &mut Order {
        &mut self.orders[key]
    }

    /// Returns the number of orders currently stored.
    #[inline]
    pub fn len(&self) -> usize {
        self.orders.len()
    }

    /// Returns `true` if no orders are stored.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tachyon_core::*;

    fn make_order(id: u64, price: i64, qty: u64) -> Order {
        Order {
            id: OrderId::new(id),
            symbol: Symbol::new(0),
            side: Side::Buy,
            price: Price::new(price),
            quantity: Quantity::new(qty),
            remaining_qty: Quantity::new(qty),
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            timestamp: 0,
            account_id: 0,
            prev: NO_LINK,
            next: NO_LINK,
        }
    }

    #[test]
    fn test_pool_insert_and_get() {
        let mut pool = OrderPool::with_capacity(16);
        let key = pool.insert(make_order(1, 100, 10));
        assert_eq!(pool.get(key).id, OrderId::new(1));
        assert_eq!(pool.len(), 1);
        assert!(!pool.is_empty());
    }

    #[test]
    fn test_pool_remove() {
        let mut pool = OrderPool::with_capacity(16);
        let key = pool.insert(make_order(1, 100, 10));
        let order = pool.remove(key);
        assert_eq!(order.id, OrderId::new(1));
        assert!(pool.is_empty());
    }

    #[test]
    fn test_pool_get_mut() {
        let mut pool = OrderPool::with_capacity(16);
        let key = pool.insert(make_order(1, 100, 10));
        pool.get_mut(key).remaining_qty = Quantity::new(5);
        assert_eq!(pool.get(key).remaining_qty, Quantity::new(5));
    }

    #[test]
    fn test_pool_reuse_keys() {
        let mut pool = OrderPool::with_capacity(16);
        let k1 = pool.insert(make_order(1, 100, 10));
        pool.remove(k1);
        let k2 = pool.insert(make_order(2, 200, 20));
        // Slab reuses the freed slot
        assert_eq!(k1, k2);
        assert_eq!(pool.get(k2).id, OrderId::new(2));
    }
}
