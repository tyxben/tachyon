use smallvec::SmallVec;
use tachyon_book::OrderBook;
use tachyon_core::*;

use crate::stp::{check_self_trade, StpAction, StpMode};

/// Pure matching logic. No I/O, no mutable state beyond what is passed in.
pub struct Matcher {
    pub stp_mode: StpMode,
    trade_id_counter: u64,
}

impl Matcher {
    /// Creates a new Matcher with the given STP mode.
    #[inline]
    pub fn new(stp_mode: StpMode) -> Self {
        Matcher {
            stp_mode,
            trade_id_counter: 0,
        }
    }

    #[inline(always)]
    fn next_trade_id(&mut self) -> u64 {
        self.trade_id_counter += 1;
        self.trade_id_counter
    }

    /// Returns the current trade ID counter value.
    #[inline]
    pub fn trade_id_counter(&self) -> u64 {
        self.trade_id_counter
    }

    /// Sets the trade ID counter (used during recovery).
    #[inline]
    pub fn set_trade_id_counter(&mut self, val: u64) {
        self.trade_id_counter = val;
    }

    /// Main entry point: match an incoming order against the book.
    ///
    /// Dispatches based on order_type + time_in_force, emits events into a SmallVec.
    #[inline]
    pub fn match_order(
        &mut self,
        book: &mut OrderBook,
        mut order: Order,
    ) -> SmallVec<[EngineEvent; 8]> {
        let mut events = SmallVec::new();

        match order.order_type {
            OrderType::Market => {
                // Market orders: accept then match aggressively with no price limit
                events.push(EngineEvent::OrderAccepted {
                    order_id: order.id,
                    symbol: order.symbol,
                    side: order.side,
                    price: order.price,
                    qty: order.quantity,
                    timestamp: order.timestamp,
                });

                self.match_aggressive(book, &mut order, &mut events);

                // Market order remainder is always cancelled (no book placement)
                if !order.remaining_qty.is_zero() {
                    events.push(EngineEvent::OrderCancelled {
                        order_id: order.id,
                        remaining_qty: order.remaining_qty,
                        timestamp: order.timestamp,
                    });
                }
            }
            OrderType::Limit => match order.time_in_force {
                TimeInForce::GTC | TimeInForce::GTD(_) => {
                    events.push(EngineEvent::OrderAccepted {
                        order_id: order.id,
                        symbol: order.symbol,
                        side: order.side,
                        price: order.price,
                        qty: order.quantity,
                        timestamp: order.timestamp,
                    });

                    self.match_aggressive(book, &mut order, &mut events);

                    // Add remainder to book
                    if !order.remaining_qty.is_zero() {
                        let rem_id = order.id;
                        let rem_qty = order.remaining_qty;
                        let rem_ts = order.timestamp;
                        match book.add_order(order) {
                            Ok(_) => {} // order resting on book
                            Err(_) => {
                                // Book couldn't accept the order, cancel the remainder
                                events.push(EngineEvent::OrderCancelled {
                                    order_id: rem_id,
                                    remaining_qty: rem_qty,
                                    timestamp: rem_ts,
                                });
                            }
                        }
                    }
                }
                TimeInForce::IOC => {
                    events.push(EngineEvent::OrderAccepted {
                        order_id: order.id,
                        symbol: order.symbol,
                        side: order.side,
                        price: order.price,
                        qty: order.quantity,
                        timestamp: order.timestamp,
                    });

                    self.match_aggressive(book, &mut order, &mut events);

                    // Cancel any remainder
                    if !order.remaining_qty.is_zero() {
                        events.push(EngineEvent::OrderCancelled {
                            order_id: order.id,
                            remaining_qty: order.remaining_qty,
                            timestamp: order.timestamp,
                        });
                    }
                }
                TimeInForce::FOK => {
                    if !self.check_fok_liquidity(book, &order) {
                        events.push(EngineEvent::OrderRejected {
                            order_id: order.id,
                            reason: RejectReason::InsufficientLiquidity,
                            timestamp: order.timestamp,
                        });
                        return events;
                    }
                    events.push(EngineEvent::OrderAccepted {
                        order_id: order.id,
                        symbol: order.symbol,
                        side: order.side,
                        price: order.price,
                        qty: order.quantity,
                        timestamp: order.timestamp,
                    });

                    self.match_aggressive(book, &mut order, &mut events);
                }
                TimeInForce::PostOnly => {
                    if self.would_cross(book, &order) {
                        events.push(EngineEvent::OrderRejected {
                            order_id: order.id,
                            reason: RejectReason::PostOnlyWouldTake,
                            timestamp: order.timestamp,
                        });
                        return events;
                    }
                    events.push(EngineEvent::OrderAccepted {
                        order_id: order.id,
                        symbol: order.symbol,
                        side: order.side,
                        price: order.price,
                        qty: order.quantity,
                        timestamp: order.timestamp,
                    });

                    let rem_id = order.id;
                    let rem_qty = order.remaining_qty;
                    let rem_ts = order.timestamp;
                    match book.add_order(order) {
                        Ok(_) => {} // order resting on book
                        Err(_) => {
                            // Book couldn't accept the order, cancel it
                            events.push(EngineEvent::OrderCancelled {
                                order_id: rem_id,
                                remaining_qty: rem_qty,
                                timestamp: rem_ts,
                            });
                        }
                    }
                }
            },
        }

        events
    }

    /// Core price-time priority matching loop.
    ///
    /// Matches the incoming order against the opposing side of the book,
    /// emitting Trade and BookUpdate events.
    #[inline]
    fn match_aggressive(
        &mut self,
        book: &mut OrderBook,
        order: &mut Order,
        events: &mut SmallVec<[EngineEvent; 8]>,
    ) {
        let is_market = order.order_type == OrderType::Market;

        while !order.remaining_qty.is_zero() {
            // Get best opposing price
            let best_price = match order.side {
                Side::Buy => book.best_ask_price(),
                Side::Sell => book.best_bid_price(),
            };

            let best_price = match best_price {
                Some(p) => p,
                None => break, // No opposing liquidity
            };

            // Price check for limit orders
            if !is_market {
                match order.side {
                    Side::Buy => {
                        if best_price > order.price {
                            break;
                        }
                    }
                    Side::Sell => {
                        if best_price < order.price {
                            break;
                        }
                    }
                }
            }

            // Get the level key for the best opposing price
            let level_key = match order.side {
                Side::Buy => match book.asks().get(&best_price) {
                    Some(&lk) => lk,
                    None => break,
                },
                Side::Sell => match book.bids().get(&best_price) {
                    Some(&lk) => lk,
                    None => break,
                },
            };

            // Match orders at this price level
            self.match_at_level(book, order, level_key, best_price, events);

            if order.remaining_qty.is_zero() {
                break;
            }

            // Check if the level is now empty — if so, it was already cleaned up
            // by remove_order_by_key or pop_front_order. Just continue to next level.
        }
    }

    /// Match orders within a single price level.
    #[inline]
    fn match_at_level(
        &mut self,
        book: &mut OrderBook,
        order: &mut Order,
        level_key: usize,
        match_price: Price,
        events: &mut SmallVec<[EngineEvent; 8]>,
    ) {
        loop {
            if order.remaining_qty.is_zero() {
                break;
            }

            // Get the head order of this level
            let head_idx = {
                // Check if level still exists in the slab
                if !book.levels().contains(level_key) {
                    break;
                }
                let level = &book.levels()[level_key];
                if level.is_empty() {
                    break;
                }
                level.head_idx
            };

            if head_idx == NO_LINK {
                break;
            }

            let resting_key = head_idx as usize;
            let resting_account = book.orders()[resting_key].account_id;
            let resting_id = book.orders()[resting_key].id;
            let resting_remaining = book.orders()[resting_key].remaining_qty;
            let resting_side = book.orders()[resting_key].side;

            // STP check
            let stp_action = check_self_trade(order.account_id, resting_account, self.stp_mode);
            match stp_action {
                StpAction::Proceed => { /* continue to matching */ }
                StpAction::CancelIncoming => {
                    // Cancel the incoming order
                    events.push(EngineEvent::OrderCancelled {
                        order_id: order.id,
                        remaining_qty: order.remaining_qty,
                        timestamp: order.timestamp,
                    });
                    order.remaining_qty = Quantity::ZERO;
                    return;
                }
                StpAction::CancelResting => {
                    // Cancel the resting order and continue
                    events.push(EngineEvent::OrderCancelled {
                        order_id: resting_id,
                        remaining_qty: resting_remaining,
                        timestamp: order.timestamp,
                    });
                    book.remove_order_by_key(resting_key);

                    // Emit BookUpdate for the affected price level
                    let new_total = if book.levels().contains(level_key) {
                        book.levels()[level_key].total_quantity
                    } else {
                        Quantity::ZERO
                    };
                    events.push(EngineEvent::BookUpdate {
                        symbol: order.symbol,
                        side: resting_side,
                        price: match_price,
                        new_total_qty: new_total,
                        timestamp: order.timestamp,
                    });
                    continue;
                }
                StpAction::CancelBoth => {
                    // Cancel both
                    events.push(EngineEvent::OrderCancelled {
                        order_id: resting_id,
                        remaining_qty: resting_remaining,
                        timestamp: order.timestamp,
                    });
                    book.remove_order_by_key(resting_key);

                    let new_total = if book.levels().contains(level_key) {
                        book.levels()[level_key].total_quantity
                    } else {
                        Quantity::ZERO
                    };
                    events.push(EngineEvent::BookUpdate {
                        symbol: order.symbol,
                        side: resting_side,
                        price: match_price,
                        new_total_qty: new_total,
                        timestamp: order.timestamp,
                    });

                    events.push(EngineEvent::OrderCancelled {
                        order_id: order.id,
                        remaining_qty: order.remaining_qty,
                        timestamp: order.timestamp,
                    });
                    order.remaining_qty = Quantity::ZERO;
                    return;
                }
            }

            // Calculate fill quantity
            let fill_qty = order.remaining_qty.min(resting_remaining);

            // Update incoming order
            order.remaining_qty = order.remaining_qty.saturating_sub(fill_qty);

            // Emit Trade event (trade price is the resting/maker price)
            let trade_id = self.next_trade_id();
            events.push(EngineEvent::Trade {
                trade: Trade {
                    trade_id,
                    symbol: order.symbol,
                    price: match_price,
                    quantity: fill_qty,
                    maker_order_id: resting_id,
                    taker_order_id: order.id,
                    maker_side: resting_side,
                    timestamp: order.timestamp,
                },
            });

            let new_resting_remaining = resting_remaining.saturating_sub(fill_qty);

            if new_resting_remaining.is_zero() {
                // Resting order fully filled — remove it
                book.remove_order_by_key(resting_key);
            } else {
                // Resting order partially filled — update remaining qty and level total
                book.orders_mut()[resting_key].remaining_qty = new_resting_remaining;
                if book.levels().contains(level_key) {
                    book.levels_mut()[level_key].total_quantity = book.levels()[level_key]
                        .total_quantity
                        .saturating_sub(fill_qty);
                }
            }

            // Emit BookUpdate for the affected price level
            let new_total = if book.levels().contains(level_key) {
                book.levels()[level_key].total_quantity
            } else {
                Quantity::ZERO
            };
            events.push(EngineEvent::BookUpdate {
                symbol: order.symbol,
                side: resting_side,
                price: match_price,
                new_total_qty: new_total,
                timestamp: order.timestamp,
            });
        }
    }

    /// Check if a PostOnly order would cross the spread.
    #[inline]
    fn would_cross(&self, book: &OrderBook, order: &Order) -> bool {
        match order.side {
            Side::Buy => {
                if let Some(best_ask) = book.best_ask_price() {
                    order.price >= best_ask
                } else {
                    false
                }
            }
            Side::Sell => {
                if let Some(best_bid) = book.best_bid_price() {
                    order.price <= best_bid
                } else {
                    false
                }
            }
        }
    }

    /// Check if there is sufficient liquidity for a FOK order.
    ///
    /// Uses two separate branches for buy/sell because BTreeMap iterators
    /// (Iter vs Rev<Iter>) are different types.
    #[inline]
    fn check_fok_liquidity(&self, book: &OrderBook, order: &Order) -> bool {
        let mut remaining = order.remaining_qty;

        match order.side {
            Side::Buy => {
                // Buy eats asks, ascending order (lowest ask first)
                for (&price, &level_key) in book.asks().iter() {
                    if price > order.price {
                        break; // Can't cross this price
                    }
                    let level = &book.levels()[level_key];
                    if level.total_quantity >= remaining {
                        return true;
                    }
                    remaining = remaining.saturating_sub(level.total_quantity);
                }
            }
            Side::Sell => {
                // Sell eats bids, descending order (highest bid first)
                for (&price, &level_key) in book.bids().iter().rev() {
                    if price < order.price {
                        break; // Can't cross this price
                    }
                    let level = &book.levels()[level_key];
                    if level.total_quantity >= remaining {
                        return true;
                    }
                    remaining = remaining.saturating_sub(level.total_quantity);
                }
            }
        }

        false
    }
}
