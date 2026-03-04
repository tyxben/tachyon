use smallvec::SmallVec;
use tachyon_book::OrderBook;
use tachyon_core::*;

use crate::command::Command;
use crate::matcher::Matcher;
use crate::risk::{RiskConfig, RiskManager};
use crate::stp::StpMode;

/// The top-level engine for a single symbol.
///
/// Combines the order book, matcher, and risk manager into a cohesive unit.
pub struct SymbolEngine {
    symbol: Symbol,
    book: OrderBook,
    matcher: Matcher,
    risk: RiskManager,
    sequence: u64,
    next_order_id: u64,
}

impl SymbolEngine {
    /// Creates a new SymbolEngine.
    pub fn new(
        symbol: Symbol,
        config: SymbolConfig,
        stp_mode: StpMode,
        risk_config: RiskConfig,
    ) -> Self {
        // Encode symbol id in the upper bits so order IDs are globally unique across symbols.
        let id_base = (symbol.raw() as u64) << 40;
        SymbolEngine {
            symbol,
            book: OrderBook::new(symbol, config),
            matcher: Matcher::new(stp_mode),
            risk: RiskManager::new(risk_config),
            sequence: 0,
            next_order_id: id_base + 1,
        }
    }

    /// Processes a single command and returns the resulting events.
    pub fn process_command(
        &mut self,
        cmd: Command,
        account_id: u64,
        timestamp: u64,
    ) -> SmallVec<[EngineEvent; 8]> {
        self.sequence += 1;

        match cmd {
            Command::PlaceOrder(mut order) => {
                // Assign a unique order ID if the caller didn't provide one.
                if order.id.raw() == 0 {
                    order.id = OrderId::new(self.next_order_id);
                    self.next_order_id += 1;
                }
                order.account_id = account_id;
                order.timestamp = timestamp;
                order.symbol = self.symbol;

                // Risk check
                if let Err(err) = self.risk.check(&order, &self.book) {
                    let reason = match err {
                        EngineError::PriceOutOfRange => RejectReason::PriceOutOfRange,
                        EngineError::InvalidQuantity(_) => RejectReason::InvalidQuantity,
                        EngineError::InvalidPrice(_) => RejectReason::InvalidPrice,
                        _ => RejectReason::PriceOutOfRange,
                    };
                    let mut events = SmallVec::new();
                    events.push(EngineEvent::OrderRejected {
                        order_id: order.id,
                        reason,
                        timestamp,
                    });
                    return events;
                }

                self.matcher.match_order(&mut self.book, order)
            }
            Command::CancelOrder(order_id) => {
                let mut events = SmallVec::new();
                match self.book.cancel_order(order_id) {
                    Ok(order) => {
                        events.push(EngineEvent::OrderCancelled {
                            order_id,
                            remaining_qty: order.remaining_qty,
                            timestamp,
                        });
                    }
                    Err(_) => {
                        events.push(EngineEvent::OrderRejected {
                            order_id,
                            reason: RejectReason::InvalidPrice, // order not found
                            timestamp,
                        });
                    }
                }
                events
            }
            Command::ModifyOrder {
                order_id,
                new_price,
                new_qty,
            } => {
                let mut events = SmallVec::new();

                // Look up the existing order first
                let existing = self.book.get_order(order_id).cloned();
                match existing {
                    Some(old) => {
                        let price = new_price.unwrap_or(old.price);
                        let qty = new_qty.unwrap_or(old.quantity);

                        match self.book.modify_order(order_id, price, qty) {
                            Ok(()) => {
                                // Cancel old + accept new
                                events.push(EngineEvent::OrderCancelled {
                                    order_id,
                                    remaining_qty: old.remaining_qty,
                                    timestamp,
                                });
                                events.push(EngineEvent::OrderAccepted {
                                    order_id,
                                    symbol: self.symbol,
                                    side: old.side,
                                    price,
                                    qty,
                                    timestamp,
                                });
                            }
                            Err(_) => {
                                events.push(EngineEvent::OrderRejected {
                                    order_id,
                                    reason: RejectReason::InvalidPrice,
                                    timestamp,
                                });
                            }
                        }
                    }
                    None => {
                        events.push(EngineEvent::OrderRejected {
                            order_id,
                            reason: RejectReason::InvalidPrice,
                            timestamp,
                        });
                    }
                }
                events
            }
        }
    }

    /// Returns a read-only reference to the order book.
    #[inline]
    pub fn book(&self) -> &OrderBook {
        &self.book
    }

    /// Returns the current engine sequence number.
    #[inline]
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
}
