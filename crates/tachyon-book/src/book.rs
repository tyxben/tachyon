use std::collections::BTreeMap;

use hashbrown::HashMap;
use slab::Slab;
use tachyon_core::{
    EngineError, Order, OrderId, Price, Quantity, Side, Symbol, SymbolConfig, NO_LINK,
};

use crate::level::PriceLevel;

/// A snapshot of one side of the depth: list of (price, total_quantity) pairs.
pub type DepthSide = Vec<(Price, Quantity)>;

/// The core order book for a single trading symbol.
///
/// Uses `BTreeMap` for sorted price levels, `Slab` for O(1) order/level allocation,
/// and `hashbrown::HashMap` for O(1) order lookup by ID.
///
pub struct OrderBook {
    symbol: Symbol,
    config: SymbolConfig,
    /// Buy side price levels: Price -> levels slab key. Traversed in descending order for best bid.
    bids: BTreeMap<Price, usize>,
    /// Sell side price levels: Price -> levels slab key. Traversed in ascending order for best ask.
    asks: BTreeMap<Price, usize>,
    /// All orders stored in a slab for O(1) access by index.
    orders: Slab<Order>,
    /// All price levels stored in a slab.
    levels: Slab<PriceLevel>,
    /// OrderId -> orders slab key for O(1) cancel/lookup.
    order_map: HashMap<OrderId, usize>,
    /// Cached best bid price.
    best_bid: Option<Price>,
    /// Cached best ask price.
    best_ask: Option<Price>,
    /// Local operation counter.
    sequence: u64,
}

impl OrderBook {
    /// Creates a new empty order book for the given symbol and configuration.
    pub fn new(symbol: Symbol, config: SymbolConfig) -> Self {
        OrderBook {
            symbol,
            config,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            orders: Slab::with_capacity(1024),
            levels: Slab::with_capacity(256),
            order_map: HashMap::with_capacity(1024),
            best_bid: None,
            best_ask: None,
            sequence: 0,
        }
    }

    /// Returns the symbol this order book is for.
    #[inline]
    pub fn symbol(&self) -> Symbol {
        self.symbol
    }

    /// Returns the symbol configuration.
    #[inline]
    pub fn config(&self) -> &SymbolConfig {
        &self.config
    }

    /// Returns the current sequence number.
    #[inline]
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    // -----------------------------------------------------------------------
    // Validation helpers
    // -----------------------------------------------------------------------

    fn validate_order(&self, order: &Order) -> Result<(), EngineError> {
        let price = order.price;
        let qty = order.remaining_qty;

        // Price must be aligned to tick_size
        if !price.is_aligned(self.config.tick_size) {
            return Err(EngineError::InvalidPrice(price));
        }

        // Price within bounds
        if price < self.config.min_price || price > self.config.max_price {
            return Err(EngineError::PriceOutOfRange);
        }

        // Quantity must be aligned to lot_size
        if self.config.lot_size.raw() != 0 && qty.raw() % self.config.lot_size.raw() != 0 {
            return Err(EngineError::InvalidQuantity(qty));
        }

        // Quantity within bounds
        if qty < self.config.min_order_qty || qty > self.config.max_order_qty {
            return Err(EngineError::InvalidQuantity(qty));
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Side helpers
    // -----------------------------------------------------------------------

    #[inline]
    fn side_map(&self, side: Side) -> &BTreeMap<Price, usize> {
        match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        }
    }

    #[inline]
    fn side_map_mut(&mut self, side: Side) -> &mut BTreeMap<Price, usize> {
        match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        }
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// Adds an order to the book.
    ///
    /// Validates price alignment, quantity alignment, and bounds before insertion.
    /// The order is appended to the tail of the corresponding price level's linked list (FIFO).
    #[inline]
    pub fn add_order(&mut self, mut order: Order) -> Result<(), EngineError> {
        self.validate_order(&order)?;

        // Duplicate order check
        if self.order_map.contains_key(&order.id) {
            return Err(EngineError::DuplicateOrderId(order.id));
        }

        // Reset linked list pointers
        order.prev = NO_LINK;
        order.next = NO_LINK;

        let side = order.side;
        let price = order.price;
        let qty = order.remaining_qty;

        // Insert order into slab
        let order_key = self.orders.insert(order);

        // Register in order_map
        self.order_map.insert(self.orders[order_key].id, order_key);

        // Find or create the price level
        let side_map = self.side_map_mut(side);
        let level_key = if let Some(&lk) = side_map.get(&price) {
            lk
        } else {
            // Create new price level
            let new_level = PriceLevel::new(price);
            let lk = self.levels.insert(new_level);
            self.side_map_mut(side).insert(price, lk);
            lk
        };

        // Append order to the price level's linked list tail
        let level = &mut self.levels[level_key];
        if level.is_empty() {
            // First order at this level
            level.head_idx = order_key as u32;
            level.tail_idx = order_key as u32;
        } else {
            // Append to tail
            let old_tail = level.tail_idx;
            self.orders[old_tail as usize].next = order_key as u32;
            self.orders[order_key].prev = old_tail;
            self.levels[level_key].tail_idx = order_key as u32;
        }

        // Update level aggregates
        self.levels[level_key].add_order_qty(qty);

        // Update BBO cache
        match side {
            Side::Buy => match self.best_bid {
                None => self.best_bid = Some(price),
                Some(current) if price > current => self.best_bid = Some(price),
                _ => {}
            },
            Side::Sell => match self.best_ask {
                None => self.best_ask = Some(price),
                Some(current) if price < current => self.best_ask = Some(price),
                _ => {}
            },
        }

        self.sequence = self.sequence.wrapping_add(1);
        Ok(())
    }

    /// Cancels an order by its ID.
    ///
    /// Removes the order from its price level's linked list, updates aggregates,
    /// and removes empty price levels. Returns the removed order.
    pub fn cancel_order(&mut self, order_id: OrderId) -> Result<Order, EngineError> {
        // Look up order
        let &order_key = self
            .order_map
            .get(&order_id)
            .ok_or(EngineError::OrderNotFound(order_id))?;

        let order = &self.orders[order_key];
        let side = order.side;
        let price = order.price;
        let remaining = order.remaining_qty;
        let prev = order.prev;
        let next = order.next;

        // Find the level — if missing, the book is internally inconsistent.
        // Return OrderNotFound rather than panicking in production.
        let &level_key = self
            .side_map(side)
            .get(&price)
            .ok_or(EngineError::OrderNotFound(order_id))?;

        // Unlink from doubly-linked list
        if prev != NO_LINK {
            self.orders[prev as usize].next = next;
        }
        if next != NO_LINK {
            self.orders[next as usize].prev = prev;
        }

        // Update level head/tail
        let level = &mut self.levels[level_key];
        if level.head_idx == order_key as u32 {
            level.head_idx = next;
        }
        if level.tail_idx == order_key as u32 {
            level.tail_idx = prev;
        }

        // Update level aggregates
        level.sub_order_qty(remaining);

        // If level is now empty, remove it
        let level_empty = self.levels[level_key].is_empty();
        if level_empty {
            self.levels.remove(level_key);
            self.side_map_mut(side).remove(&price);

            // Update BBO if we removed the best price
            match side {
                Side::Buy => {
                    if self.best_bid == Some(price) {
                        self.best_bid = self.bids.keys().next_back().copied();
                    }
                }
                Side::Sell => {
                    if self.best_ask == Some(price) {
                        self.best_ask = self.asks.keys().next().copied();
                    }
                }
            }
        }

        // Remove from order_map and orders slab
        self.order_map.remove(&order_id);
        let removed = self.orders.remove(order_key);

        self.sequence = self.sequence.wrapping_add(1);
        Ok(removed)
    }

    /// Returns the best bid price and its price level, or `None` if no bids exist.
    #[inline]
    pub fn best_bid(&self) -> Option<(Price, &PriceLevel)> {
        let price = (self.best_bid)?;
        let &level_key = self.bids.get(&price)?;
        Some((price, &self.levels[level_key]))
    }

    /// Returns the best ask price and its price level, or `None` if no asks exist.
    #[inline]
    pub fn best_ask(&self) -> Option<(Price, &PriceLevel)> {
        let price = (self.best_ask)?;
        let &level_key = self.asks.get(&price)?;
        Some((price, &self.levels[level_key]))
    }

    /// Returns up to `n` levels of depth for bids (descending price) and asks (ascending price).
    pub fn get_depth(&self, n: usize) -> (DepthSide, DepthSide) {
        let bids_depth: Vec<(Price, Quantity)> = self
            .bids
            .iter()
            .rev()
            .take(n)
            .map(|(&price, &level_key)| (price, self.levels[level_key].total_quantity))
            .collect();

        let asks_depth: Vec<(Price, Quantity)> = self
            .asks
            .iter()
            .take(n)
            .map(|(&price, &level_key)| (price, self.levels[level_key].total_quantity))
            .collect();

        (bids_depth, asks_depth)
    }

    /// Looks up an order by ID.
    #[inline]
    pub fn get_order(&self, order_id: OrderId) -> Option<&Order> {
        let &key = self.order_map.get(&order_id)?;
        Some(&self.orders[key])
    }

    /// Modifies an order by cancelling and re-inserting with new price/quantity.
    ///
    /// The order loses its time priority (goes to the tail of the new price level).
    pub fn modify_order(
        &mut self,
        order_id: OrderId,
        new_price: Price,
        new_qty: Quantity,
    ) -> Result<(), EngineError> {
        let old_order = self.cancel_order(order_id)?;
        let new_order = Order {
            id: old_order.id,
            symbol: old_order.symbol,
            side: old_order.side,
            price: new_price,
            quantity: new_qty,
            remaining_qty: new_qty,
            order_type: old_order.order_type,
            time_in_force: old_order.time_in_force,
            timestamp: old_order.timestamp,
            account_id: old_order.account_id,
            prev: NO_LINK,
            next: NO_LINK,
        };
        self.add_order(new_order)
    }

    /// Returns the total number of orders in the book.
    #[inline]
    pub fn order_count(&self) -> usize {
        self.order_map.len()
    }

    /// Returns the total number of price levels in the book (bids + asks).
    #[inline]
    pub fn level_count(&self) -> usize {
        self.bids.len() + self.asks.len()
    }

    /// Returns the spread (best_ask - best_bid), or `None` if either side is empty.
    #[inline]
    pub fn spread(&self) -> Option<Price> {
        match (self.best_ask, self.best_bid) {
            (Some(ask), Some(bid)) => ask.checked_sub(bid),
            _ => None,
        }
    }

    /// Removes and returns the head (oldest) order from the given price level.
    ///
    /// This is used by the matching engine during order execution.
    /// Returns `None` if the level is empty.
    pub fn pop_front_order(&mut self, level_key: usize) -> Option<Order> {
        let level = &self.levels[level_key];
        if level.is_empty() {
            return None;
        }

        let head_key = level.head_idx as usize;
        let order = &self.orders[head_key];
        let next = order.next;
        let remaining = order.remaining_qty;
        let order_id = order.id;
        let side = order.side;
        let price = order.price;

        // Update level linked list
        let level = &mut self.levels[level_key];
        level.head_idx = next;
        if next == NO_LINK {
            level.tail_idx = NO_LINK;
        } else {
            self.orders[next as usize].prev = NO_LINK;
        }

        // Update level aggregates
        self.levels[level_key].sub_order_qty(remaining);

        // Remove from order_map and slab
        self.order_map.remove(&order_id);
        let removed = self.orders.remove(head_key);

        // If level is now empty, remove it
        if self.levels[level_key].is_empty() {
            self.levels.remove(level_key);
            self.side_map_mut(side).remove(&price);

            // Update BBO
            match side {
                Side::Buy => {
                    if self.best_bid == Some(price) {
                        self.best_bid = self.bids.keys().next_back().copied();
                    }
                }
                Side::Sell => {
                    if self.best_ask == Some(price) {
                        self.best_ask = self.asks.keys().next().copied();
                    }
                }
            }
        }

        self.sequence = self.sequence.wrapping_add(1);
        Some(removed)
    }

    /// Returns a reference to the levels slab (used by matching engine).
    #[inline]
    pub fn levels(&self) -> &Slab<PriceLevel> {
        &self.levels
    }

    /// Returns the bid price-level map (used by matching engine).
    #[inline]
    pub fn bids(&self) -> &BTreeMap<Price, usize> {
        &self.bids
    }

    /// Returns the ask price-level map (used by matching engine).
    #[inline]
    pub fn asks(&self) -> &BTreeMap<Price, usize> {
        &self.asks
    }

    /// Provides access to orders slab (used by matching engine).
    #[inline]
    pub fn orders(&self) -> &Slab<Order> {
        &self.orders
    }

    /// Provides mutable access to orders slab (used by matching engine).
    #[inline]
    pub fn orders_mut(&mut self) -> &mut Slab<Order> {
        &mut self.orders
    }

    /// Provides mutable access to the levels slab (used by matching engine).
    #[inline]
    pub fn levels_mut(&mut self) -> &mut Slab<PriceLevel> {
        &mut self.levels
    }

    /// Returns the cached best bid price, or `None` if no bids exist.
    #[inline]
    pub fn best_bid_price(&self) -> Option<Price> {
        self.best_bid
    }

    /// Returns the cached best ask price, or `None` if no asks exist.
    #[inline]
    pub fn best_ask_price(&self) -> Option<Price> {
        self.best_ask
    }

    /// Returns a reference to the order_map.
    #[inline]
    pub fn order_map(&self) -> &HashMap<OrderId, usize> {
        &self.order_map
    }

    /// Removes an order from its price level's linked list and slab.
    ///
    /// This is a low-level operation used by the matching engine when a resting
    /// order is fully filled. It handles unlinking, level aggregate updates,
    /// empty level cleanup, and BBO cache updates.
    #[inline]
    pub fn remove_order_by_key(&mut self, order_key: usize) {
        let order = &self.orders[order_key];
        let side = order.side;
        let price = order.price;
        let remaining = order.remaining_qty;
        let prev = order.prev;
        let next = order.next;
        let order_id = order.id;

        // Find the level key
        let &level_key = match side {
            Side::Buy => self.bids.get(&price),
            Side::Sell => self.asks.get(&price),
        }
        .unwrap_or_else(|| {
            debug_assert!(false, "level must exist for order at price {:?}", price);
            // Defensive: clean up and return early handled below
            &0 // unreachable in correct state, but avoids panic in release
        });

        // Unlink from doubly-linked list
        if prev != NO_LINK {
            self.orders[prev as usize].next = next;
        }
        if next != NO_LINK {
            self.orders[next as usize].prev = prev;
        }

        // Update level head/tail
        let level = &mut self.levels[level_key];
        if level.head_idx == order_key as u32 {
            level.head_idx = next;
        }
        if level.tail_idx == order_key as u32 {
            level.tail_idx = prev;
        }

        // Update level aggregates
        level.sub_order_qty(remaining);

        // If level is now empty, remove it
        if self.levels[level_key].is_empty() {
            self.levels.remove(level_key);
            match side {
                Side::Buy => {
                    self.bids.remove(&price);
                    if self.best_bid == Some(price) {
                        self.best_bid = self.bids.keys().next_back().copied();
                    }
                }
                Side::Sell => {
                    self.asks.remove(&price);
                    if self.best_ask == Some(price) {
                        self.best_ask = self.asks.keys().next().copied();
                    }
                }
            }
        }

        // Remove from order_map and orders slab
        self.order_map.remove(&order_id);
        self.orders.remove(order_key);
    }

    /// Updates the BBO cache by reading from the BTreeMaps.
    #[inline]
    pub fn refresh_bbo(&mut self) {
        self.best_bid = self.bids.keys().next_back().copied();
        self.best_ask = self.asks.keys().next().copied();
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tachyon_core::{OrderType, TimeInForce};

    fn default_config() -> SymbolConfig {
        SymbolConfig {
            symbol: Symbol::new(0),
            tick_size: Price::new(1),
            lot_size: Quantity::new(1),
            price_scale: 2,
            qty_scale: 8,
            max_price: Price::new(1_000_000),
            min_price: Price::new(1),
            max_order_qty: Quantity::new(1_000_000),
            min_order_qty: Quantity::new(1),
        }
    }

    fn make_order(id: u64, side: Side, price: i64, qty: u64) -> Order {
        Order {
            id: OrderId::new(id),
            symbol: Symbol::new(0),
            side,
            price: Price::new(price),
            quantity: Quantity::new(qty),
            remaining_qty: Quantity::new(qty),
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            timestamp: id, // use id as timestamp for simplicity
            account_id: 1,
            prev: NO_LINK,
            next: NO_LINK,
        }
    }

    fn new_book() -> OrderBook {
        OrderBook::new(Symbol::new(0), default_config())
    }

    /// Verifies internal consistency of the order book.
    fn assert_book_consistent(book: &OrderBook) {
        // 1. BBO matches actual BTreeMap state
        let actual_best_bid = book.bids.keys().next_back().copied();
        assert_eq!(
            book.best_bid, actual_best_bid,
            "best_bid cache mismatch: cached={:?}, actual={:?}",
            book.best_bid, actual_best_bid
        );

        let actual_best_ask = book.asks.keys().next().copied();
        assert_eq!(
            book.best_ask, actual_best_ask,
            "best_ask cache mismatch: cached={:?}, actual={:?}",
            book.best_ask, actual_best_ask
        );

        // 2. order_map size matches orders slab size
        assert_eq!(
            book.order_map.len(),
            book.orders.len(),
            "order_map size ({}) != orders slab size ({})",
            book.order_map.len(),
            book.orders.len()
        );

        // 3. Check each side's levels
        for (side, tree) in [(Side::Buy, &book.bids), (Side::Sell, &book.asks)] {
            for (&price, &level_key) in tree {
                let level = &book.levels[level_key];
                assert_eq!(
                    level.price, price,
                    "Level at price {:?} has mismatched level.price {:?}",
                    price, level.price
                );

                // Walk the linked list and verify order_count and total_quantity
                let mut count = 0u32;
                let mut total_qty = Quantity::ZERO;
                let mut current = level.head_idx;
                let mut prev_idx = NO_LINK;

                while current != NO_LINK {
                    let order = &book.orders[current as usize];
                    assert_eq!(
                        order.side, side,
                        "Order {:?} on {:?} side but has side {:?}",
                        order.id, side, order.side
                    );
                    assert_eq!(
                        order.price, price,
                        "Order {:?} at level price {:?} but has price {:?}",
                        order.id, price, order.price
                    );
                    assert_eq!(
                        order.prev, prev_idx,
                        "Order {:?} prev mismatch: expected {:?}, got {:?}",
                        order.id, prev_idx, order.prev
                    );

                    total_qty = total_qty
                        .checked_add(order.remaining_qty)
                        .expect("total_qty overflow in consistency check");
                    count += 1;
                    prev_idx = current;
                    current = order.next;
                }

                assert_eq!(
                    level.order_count, count,
                    "Level at price {:?}: order_count={} but walked {}",
                    price, level.order_count, count
                );
                assert_eq!(
                    level.total_quantity, total_qty,
                    "Level at price {:?}: total_quantity={:?} but summed {:?}",
                    price, level.total_quantity, total_qty
                );

                // Verify tail
                if count > 0 {
                    assert_eq!(
                        level.tail_idx, prev_idx,
                        "Level at price {:?}: tail_idx={} but last walked={}",
                        price, level.tail_idx, prev_idx
                    );
                }
            }
        }

        // 4. All orders in order_map point to valid slab entries
        for (&oid, &key) in &book.order_map {
            assert_eq!(
                book.orders[key].id, oid,
                "order_map[{:?}] -> key {} but order has id {:?}",
                oid, key, book.orders[key].id
            );
        }
    }

    // -----------------------------------------------------------------------
    // Empty book tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_empty_book() {
        let book = new_book();
        assert!(book.best_bid().is_none());
        assert!(book.best_ask().is_none());
        assert_eq!(book.order_count(), 0);
        assert_eq!(book.level_count(), 0);
        assert!(book.spread().is_none());

        let (bids, asks) = book.get_depth(10);
        assert!(bids.is_empty());
        assert!(asks.is_empty());

        assert_book_consistent(&book);
    }

    // -----------------------------------------------------------------------
    // Single order tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_single_bid_becomes_bbo() {
        let mut book = new_book();
        book.add_order(make_order(1, Side::Buy, 100, 10))
            .expect("add should succeed");

        let (price, level) = book.best_bid().expect("should have best bid");
        assert_eq!(price, Price::new(100));
        assert_eq!(level.total_quantity, Quantity::new(10));
        assert_eq!(level.order_count, 1);

        assert!(book.best_ask().is_none());
        assert_eq!(book.order_count(), 1);
        assert_eq!(book.level_count(), 1);
        assert_book_consistent(&book);
    }

    #[test]
    fn test_single_ask_becomes_bbo() {
        let mut book = new_book();
        book.add_order(make_order(1, Side::Sell, 200, 10))
            .expect("add should succeed");

        let (price, level) = book.best_ask().expect("should have best ask");
        assert_eq!(price, Price::new(200));
        assert_eq!(level.total_quantity, Quantity::new(10));

        assert!(book.best_bid().is_none());
        assert_book_consistent(&book);
    }

    #[test]
    fn test_cancel_only_order_restores_none() {
        let mut book = new_book();
        book.add_order(make_order(1, Side::Buy, 100, 10))
            .expect("add");

        let removed = book.cancel_order(OrderId::new(1)).expect("cancel");
        assert_eq!(removed.id, OrderId::new(1));

        assert!(book.best_bid().is_none());
        assert_eq!(book.order_count(), 0);
        assert_eq!(book.level_count(), 0);
        assert_book_consistent(&book);
    }

    // -----------------------------------------------------------------------
    // Same-price multiple orders (FIFO)
    // -----------------------------------------------------------------------

    #[test]
    fn test_same_price_fifo_order() {
        let mut book = new_book();
        book.add_order(make_order(1, Side::Buy, 100, 10))
            .expect("add 1");
        book.add_order(make_order(2, Side::Buy, 100, 20))
            .expect("add 2");
        book.add_order(make_order(3, Side::Buy, 100, 30))
            .expect("add 3");

        let (_, level) = book.best_bid().expect("has bbo");
        assert_eq!(level.order_count, 3);
        assert_eq!(level.total_quantity, Quantity::new(60));
        assert_book_consistent(&book);

        // Head should be order 1 (first in)
        let head_key = level.head_idx as usize;
        assert_eq!(book.orders[head_key].id, OrderId::new(1));
    }

    #[test]
    fn test_cancel_middle_order_keeps_list_intact() {
        let mut book = new_book();
        book.add_order(make_order(1, Side::Buy, 100, 10))
            .expect("add 1");
        book.add_order(make_order(2, Side::Buy, 100, 20))
            .expect("add 2");
        book.add_order(make_order(3, Side::Buy, 100, 30))
            .expect("add 3");

        // Cancel middle order
        book.cancel_order(OrderId::new(2)).expect("cancel 2");

        let (_, level) = book.best_bid().expect("has bbo");
        assert_eq!(level.order_count, 2);
        assert_eq!(level.total_quantity, Quantity::new(40));
        assert_book_consistent(&book);

        // Verify linked list: order 1 -> order 3
        let head_key = level.head_idx as usize;
        let order1 = &book.orders[head_key];
        assert_eq!(order1.id, OrderId::new(1));
        assert_ne!(order1.next, NO_LINK);
        let order3 = &book.orders[order1.next as usize];
        assert_eq!(order3.id, OrderId::new(3));
        assert_eq!(order3.next, NO_LINK);
    }

    #[test]
    fn test_cancel_head_order() {
        let mut book = new_book();
        book.add_order(make_order(1, Side::Buy, 100, 10))
            .expect("add 1");
        book.add_order(make_order(2, Side::Buy, 100, 20))
            .expect("add 2");

        book.cancel_order(OrderId::new(1)).expect("cancel 1");

        let (_, level) = book.best_bid().expect("has bbo");
        assert_eq!(level.order_count, 1);
        let head_key = level.head_idx as usize;
        assert_eq!(book.orders[head_key].id, OrderId::new(2));
        assert_book_consistent(&book);
    }

    #[test]
    fn test_cancel_tail_order() {
        let mut book = new_book();
        book.add_order(make_order(1, Side::Buy, 100, 10))
            .expect("add 1");
        book.add_order(make_order(2, Side::Buy, 100, 20))
            .expect("add 2");

        book.cancel_order(OrderId::new(2)).expect("cancel 2");

        let (_, level) = book.best_bid().expect("has bbo");
        assert_eq!(level.order_count, 1);
        let tail_key = level.tail_idx as usize;
        assert_eq!(book.orders[tail_key].id, OrderId::new(1));
        assert_book_consistent(&book);
    }

    // -----------------------------------------------------------------------
    // Cancel nonexistent order
    // -----------------------------------------------------------------------

    #[test]
    fn test_cancel_nonexistent_returns_error() {
        let mut book = new_book();
        let result = book.cancel_order(OrderId::new(999));
        assert!(result.is_err());
        match result {
            Err(EngineError::OrderNotFound(id)) => assert_eq!(id, OrderId::new(999)),
            other => panic!("expected OrderNotFound, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Cancel last order at price (level removed, BBO updates)
    // -----------------------------------------------------------------------

    #[test]
    fn test_cancel_last_order_removes_level_and_updates_bbo() {
        let mut book = new_book();
        book.add_order(make_order(1, Side::Buy, 100, 10))
            .expect("add 1");
        book.add_order(make_order(2, Side::Buy, 90, 20))
            .expect("add 2");

        // Best bid is 100
        assert_eq!(book.best_bid().expect("bbo").0, Price::new(100));

        // Cancel the order at 100
        book.cancel_order(OrderId::new(1)).expect("cancel 1");

        // Best bid should now be 90
        assert_eq!(book.best_bid().expect("bbo").0, Price::new(90));
        assert_eq!(book.level_count(), 1);
        assert_book_consistent(&book);
    }

    #[test]
    fn test_cancel_last_ask_updates_bbo() {
        let mut book = new_book();
        book.add_order(make_order(1, Side::Sell, 100, 10))
            .expect("add 1");
        book.add_order(make_order(2, Side::Sell, 110, 20))
            .expect("add 2");

        assert_eq!(book.best_ask().expect("bbo").0, Price::new(100));

        book.cancel_order(OrderId::new(1)).expect("cancel 1");

        assert_eq!(book.best_ask().expect("bbo").0, Price::new(110));
        assert_book_consistent(&book);
    }

    // -----------------------------------------------------------------------
    // BBO update validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_bbo_updates_on_add() {
        let mut book = new_book();

        // Add bid at 100
        book.add_order(make_order(1, Side::Buy, 100, 10))
            .expect("add");
        assert_eq!(book.best_bid().expect("bbo").0, Price::new(100));

        // Add bid at 110 -> new best
        book.add_order(make_order(2, Side::Buy, 110, 10))
            .expect("add");
        assert_eq!(book.best_bid().expect("bbo").0, Price::new(110));

        // Add bid at 105 -> no change
        book.add_order(make_order(3, Side::Buy, 105, 10))
            .expect("add");
        assert_eq!(book.best_bid().expect("bbo").0, Price::new(110));

        // Add ask at 200
        book.add_order(make_order(4, Side::Sell, 200, 10))
            .expect("add");
        assert_eq!(book.best_ask().expect("bbo").0, Price::new(200));

        // Add ask at 190 -> new best
        book.add_order(make_order(5, Side::Sell, 190, 10))
            .expect("add");
        assert_eq!(book.best_ask().expect("bbo").0, Price::new(190));

        assert_book_consistent(&book);
    }

    // -----------------------------------------------------------------------
    // Large scale test
    // -----------------------------------------------------------------------

    #[test]
    fn test_large_scale_insert_and_cancel() {
        let mut book = new_book();
        let count = 10_000u64;

        // Insert 10,000 orders across many price levels
        for i in 1..=count {
            let price = (i % 500) + 1; // prices from 1 to 500
            let side = if i % 2 == 0 { Side::Buy } else { Side::Sell };
            book.add_order(make_order(i, side, price as i64, 10))
                .expect("add");
        }
        assert_eq!(book.order_count(), count as usize);
        assert_book_consistent(&book);

        // Cancel every other order
        for i in (1..=count).step_by(2) {
            book.cancel_order(OrderId::new(i)).expect("cancel");
        }
        assert_eq!(book.order_count(), (count / 2) as usize);
        assert_book_consistent(&book);

        // Cancel remaining
        for i in (2..=count).step_by(2) {
            book.cancel_order(OrderId::new(i)).expect("cancel");
        }
        assert_eq!(book.order_count(), 0);
        assert!(book.best_bid().is_none());
        assert!(book.best_ask().is_none());
        assert_book_consistent(&book);
    }

    // -----------------------------------------------------------------------
    // Price boundary tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_price_boundary_values() {
        // Use a config that allows extreme prices
        let config = SymbolConfig {
            symbol: Symbol::new(0),
            tick_size: Price::new(1),
            lot_size: Quantity::new(1),
            price_scale: 0,
            qty_scale: 0,
            max_price: Price::new(i64::MAX),
            min_price: Price::new(1),
            max_order_qty: Quantity::new(u64::MAX),
            min_order_qty: Quantity::new(1),
        };
        let mut book = OrderBook::new(Symbol::new(0), config);

        // Large price should not panic
        book.add_order(make_order(1, Side::Buy, i64::MAX, 1))
            .expect("add extreme price");
        assert_eq!(book.best_bid().expect("bbo").0, Price::new(i64::MAX));

        book.cancel_order(OrderId::new(1)).expect("cancel");
        assert!(book.best_bid().is_none());
        assert_book_consistent(&book);
    }

    #[test]
    fn test_price_boundary_min_price() {
        let config = SymbolConfig {
            symbol: Symbol::new(0),
            tick_size: Price::new(1),
            lot_size: Quantity::new(1),
            price_scale: 0,
            qty_scale: 0,
            max_price: Price::new(i64::MAX),
            min_price: Price::new(1),
            max_order_qty: Quantity::new(u64::MAX),
            min_order_qty: Quantity::new(1),
        };
        let mut book = OrderBook::new(Symbol::new(0), config);

        book.add_order(make_order(1, Side::Sell, 1, 1))
            .expect("add min price");
        assert_eq!(book.best_ask().expect("bbo").0, Price::new(1));
        assert_book_consistent(&book);
    }

    // -----------------------------------------------------------------------
    // Modify order
    // -----------------------------------------------------------------------

    #[test]
    fn test_modify_order_loses_time_priority() {
        let mut book = new_book();
        book.add_order(make_order(1, Side::Buy, 100, 10))
            .expect("add 1");
        book.add_order(make_order(2, Side::Buy, 100, 20))
            .expect("add 2");

        // Modify order 1: same price, new quantity -> should go to tail
        book.modify_order(OrderId::new(1), Price::new(100), Quantity::new(15))
            .expect("modify");

        let (_, level) = book.best_bid().expect("bbo");
        // Head should now be order 2 (previously second)
        let head_key = level.head_idx as usize;
        assert_eq!(book.orders[head_key].id, OrderId::new(2));

        // Tail should be order 1 (re-inserted)
        let tail_key = level.tail_idx as usize;
        assert_eq!(book.orders[tail_key].id, OrderId::new(1));
        assert_eq!(book.orders[tail_key].remaining_qty, Quantity::new(15));

        assert_book_consistent(&book);
    }

    #[test]
    fn test_modify_order_changes_price() {
        let mut book = new_book();
        book.add_order(make_order(1, Side::Buy, 100, 10))
            .expect("add 1");
        book.add_order(make_order(2, Side::Buy, 100, 20))
            .expect("add 2");

        // Modify order 1 to a new price
        book.modify_order(OrderId::new(1), Price::new(110), Quantity::new(10))
            .expect("modify");

        // BBO should now be 110
        assert_eq!(book.best_bid().expect("bbo").0, Price::new(110));
        // Two levels: 110 and 100
        assert_eq!(book.level_count(), 2);
        assert_book_consistent(&book);
    }

    #[test]
    fn test_modify_nonexistent_order() {
        let mut book = new_book();
        let result = book.modify_order(OrderId::new(999), Price::new(100), Quantity::new(10));
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // get_depth
    // -----------------------------------------------------------------------

    #[test]
    fn test_get_depth_ordering() {
        let mut book = new_book();
        // Add bids at various prices
        book.add_order(make_order(1, Side::Buy, 100, 10))
            .expect("add");
        book.add_order(make_order(2, Side::Buy, 90, 20))
            .expect("add");
        book.add_order(make_order(3, Side::Buy, 110, 30))
            .expect("add");

        // Add asks at various prices
        book.add_order(make_order(4, Side::Sell, 200, 10))
            .expect("add");
        book.add_order(make_order(5, Side::Sell, 210, 20))
            .expect("add");
        book.add_order(make_order(6, Side::Sell, 190, 30))
            .expect("add");

        let (bids, asks) = book.get_depth(10);

        // Bids should be descending: 110, 100, 90
        assert_eq!(bids.len(), 3);
        assert_eq!(bids[0], (Price::new(110), Quantity::new(30)));
        assert_eq!(bids[1], (Price::new(100), Quantity::new(10)));
        assert_eq!(bids[2], (Price::new(90), Quantity::new(20)));

        // Asks should be ascending: 190, 200, 210
        assert_eq!(asks.len(), 3);
        assert_eq!(asks[0], (Price::new(190), Quantity::new(30)));
        assert_eq!(asks[1], (Price::new(200), Quantity::new(10)));
        assert_eq!(asks[2], (Price::new(210), Quantity::new(20)));

        assert_book_consistent(&book);
    }

    #[test]
    fn test_get_depth_limited() {
        let mut book = new_book();
        for i in 1..=10 {
            book.add_order(make_order(i, Side::Buy, i as i64 * 10, 10))
                .expect("add");
        }

        let (bids, _) = book.get_depth(3);
        assert_eq!(bids.len(), 3);
        // Should be the top 3 prices: 100, 90, 80
        assert_eq!(bids[0].0, Price::new(100));
        assert_eq!(bids[1].0, Price::new(90));
        assert_eq!(bids[2].0, Price::new(80));
    }

    // -----------------------------------------------------------------------
    // spread
    // -----------------------------------------------------------------------

    #[test]
    fn test_spread() {
        let mut book = new_book();
        book.add_order(make_order(1, Side::Buy, 100, 10))
            .expect("add");
        book.add_order(make_order(2, Side::Sell, 110, 10))
            .expect("add");

        let spread = book.spread().expect("should have spread");
        assert_eq!(spread, Price::new(10));
    }

    #[test]
    fn test_spread_one_side_empty() {
        let mut book = new_book();
        book.add_order(make_order(1, Side::Buy, 100, 10))
            .expect("add");
        assert!(book.spread().is_none());
    }

    // -----------------------------------------------------------------------
    // get_order
    // -----------------------------------------------------------------------

    #[test]
    fn test_get_order() {
        let mut book = new_book();
        book.add_order(make_order(1, Side::Buy, 100, 10))
            .expect("add");

        let order = book.get_order(OrderId::new(1)).expect("should find");
        assert_eq!(order.id, OrderId::new(1));
        assert_eq!(order.price, Price::new(100));

        assert!(book.get_order(OrderId::new(999)).is_none());
    }

    // -----------------------------------------------------------------------
    // pop_front_order
    // -----------------------------------------------------------------------

    #[test]
    fn test_pop_front_order() {
        let mut book = new_book();
        book.add_order(make_order(1, Side::Buy, 100, 10))
            .expect("add 1");
        book.add_order(make_order(2, Side::Buy, 100, 20))
            .expect("add 2");
        book.add_order(make_order(3, Side::Buy, 100, 30))
            .expect("add 3");

        let level_key = *book.bids().get(&Price::new(100)).expect("level exists");

        // Pop head (order 1)
        let popped = book.pop_front_order(level_key).expect("should pop");
        assert_eq!(popped.id, OrderId::new(1));
        assert_eq!(book.order_count(), 2);

        // Pop next head (order 2)
        let popped = book.pop_front_order(level_key).expect("should pop");
        assert_eq!(popped.id, OrderId::new(2));
        assert_eq!(book.order_count(), 1);

        // Pop last (order 3) - this removes the level
        let popped = book.pop_front_order(level_key).expect("should pop");
        assert_eq!(popped.id, OrderId::new(3));
        assert_eq!(book.order_count(), 0);
        assert!(book.best_bid().is_none());
        assert_eq!(book.level_count(), 0);
    }

    #[test]
    fn test_pop_front_updates_level_correctly() {
        let mut book = new_book();
        book.add_order(make_order(1, Side::Sell, 200, 10))
            .expect("add 1");
        book.add_order(make_order(2, Side::Sell, 200, 20))
            .expect("add 2");

        let level_key = *book.asks().get(&Price::new(200)).expect("level exists");

        let popped = book.pop_front_order(level_key).expect("pop");
        assert_eq!(popped.id, OrderId::new(1));

        // Remaining order should have prev = NO_LINK
        let remaining_key = book.levels()[level_key].head_idx as usize;
        assert_eq!(book.orders()[remaining_key].prev, NO_LINK);
        assert_book_consistent(&book);
    }

    // -----------------------------------------------------------------------
    // Validation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_invalid_price_alignment() {
        let config = SymbolConfig {
            symbol: Symbol::new(0),
            tick_size: Price::new(5),
            lot_size: Quantity::new(1),
            price_scale: 2,
            qty_scale: 8,
            max_price: Price::new(1_000_000),
            min_price: Price::new(1),
            max_order_qty: Quantity::new(1_000_000),
            min_order_qty: Quantity::new(1),
        };
        let mut book = OrderBook::new(Symbol::new(0), config);

        // Price 7 is not aligned to tick_size 5
        let result = book.add_order(make_order(1, Side::Buy, 7, 10));
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_quantity_alignment() {
        let config = SymbolConfig {
            symbol: Symbol::new(0),
            tick_size: Price::new(1),
            lot_size: Quantity::new(10),
            price_scale: 2,
            qty_scale: 8,
            max_price: Price::new(1_000_000),
            min_price: Price::new(1),
            max_order_qty: Quantity::new(1_000_000),
            min_order_qty: Quantity::new(10),
        };
        let mut book = OrderBook::new(Symbol::new(0), config);

        // Qty 7 is not aligned to lot_size 10
        let result = book.add_order(make_order(1, Side::Buy, 100, 7));
        assert!(result.is_err());
    }

    #[test]
    fn test_price_out_of_range() {
        let mut book = new_book();
        // Price 0 is below min_price (1)
        let result = book.add_order(make_order(1, Side::Buy, 0, 10));
        assert!(result.is_err());

        // Price above max
        let result = book.add_order(make_order(2, Side::Buy, 2_000_000, 10));
        assert!(result.is_err());
    }

    #[test]
    fn test_quantity_out_of_range() {
        let mut book = new_book();
        // Qty 0 is below min_order_qty (1)
        let mut order = make_order(1, Side::Buy, 100, 0);
        order.remaining_qty = Quantity::new(0);
        let result = book.add_order(order);
        assert!(result.is_err());

        // Qty above max
        let mut order = make_order(2, Side::Buy, 100, 2_000_000);
        order.remaining_qty = Quantity::new(2_000_000);
        let result = book.add_order(order);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Sequence counter
    // -----------------------------------------------------------------------

    #[test]
    fn test_sequence_increments() {
        let mut book = new_book();
        assert_eq!(book.sequence(), 0);

        book.add_order(make_order(1, Side::Buy, 100, 10))
            .expect("add");
        assert_eq!(book.sequence(), 1);

        book.cancel_order(OrderId::new(1)).expect("cancel");
        assert_eq!(book.sequence(), 2);
    }

    // -----------------------------------------------------------------------
    // Multiple price levels BBO
    // -----------------------------------------------------------------------

    #[test]
    fn test_multiple_bid_levels_bbo() {
        let mut book = new_book();
        book.add_order(make_order(1, Side::Buy, 100, 10))
            .expect("add");
        book.add_order(make_order(2, Side::Buy, 110, 10))
            .expect("add");
        book.add_order(make_order(3, Side::Buy, 90, 10))
            .expect("add");

        assert_eq!(book.best_bid().expect("bbo").0, Price::new(110));

        // Cancel best bid
        book.cancel_order(OrderId::new(2)).expect("cancel");
        assert_eq!(book.best_bid().expect("bbo").0, Price::new(100));

        // Cancel new best bid
        book.cancel_order(OrderId::new(1)).expect("cancel");
        assert_eq!(book.best_bid().expect("bbo").0, Price::new(90));

        // Cancel last
        book.cancel_order(OrderId::new(3)).expect("cancel");
        assert!(book.best_bid().is_none());
        assert_book_consistent(&book);
    }

    #[test]
    fn test_multiple_ask_levels_bbo() {
        let mut book = new_book();
        book.add_order(make_order(1, Side::Sell, 200, 10))
            .expect("add");
        book.add_order(make_order(2, Side::Sell, 190, 10))
            .expect("add");
        book.add_order(make_order(3, Side::Sell, 210, 10))
            .expect("add");

        assert_eq!(book.best_ask().expect("bbo").0, Price::new(190));

        // Cancel best ask
        book.cancel_order(OrderId::new(2)).expect("cancel");
        assert_eq!(book.best_ask().expect("bbo").0, Price::new(200));
        assert_book_consistent(&book);
    }

    // -----------------------------------------------------------------------
    // Both sides with spread
    // -----------------------------------------------------------------------

    #[test]
    fn test_full_book_with_spread() {
        let mut book = new_book();

        // Add bids
        book.add_order(make_order(1, Side::Buy, 100, 10))
            .expect("add");
        book.add_order(make_order(2, Side::Buy, 99, 20))
            .expect("add");
        book.add_order(make_order(3, Side::Buy, 98, 30))
            .expect("add");

        // Add asks
        book.add_order(make_order(4, Side::Sell, 101, 10))
            .expect("add");
        book.add_order(make_order(5, Side::Sell, 102, 20))
            .expect("add");
        book.add_order(make_order(6, Side::Sell, 103, 30))
            .expect("add");

        assert_eq!(book.best_bid().expect("bbo").0, Price::new(100));
        assert_eq!(book.best_ask().expect("bbo").0, Price::new(101));
        assert_eq!(book.spread(), Some(Price::new(1)));
        assert_eq!(book.order_count(), 6);
        assert_eq!(book.level_count(), 6);

        assert_book_consistent(&book);
    }
}
