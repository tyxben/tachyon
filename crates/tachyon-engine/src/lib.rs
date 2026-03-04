//! tachyon-engine: Matching engine and sequencer.
//!
//! Contains the core matching logic, order sequencing, and the main
//! engine event loop that drives the Tachyon matching engine.

pub mod command;
pub mod engine;
pub mod matcher;
pub mod risk;
pub mod stp;

pub use command::{Command, InboundCommand, SequencedCommand};
pub use engine::{EngineSnapshot, SymbolEngine};
pub use matcher::Matcher;
pub use risk::{RiskConfig, RiskManager};
pub use stp::{StpAction, StpMode};

#[cfg(test)]
mod tests {
    use super::*;
    use smallvec::SmallVec;
    use tachyon_book::OrderBook;
    use tachyon_core::*;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

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

    fn new_book() -> OrderBook {
        OrderBook::new(Symbol::new(0), default_config())
    }

    fn new_matcher() -> Matcher {
        Matcher::new(StpMode::None)
    }

    fn make_limit_order(id: u64, side: Side, price: i64, qty: u64, tif: TimeInForce) -> Order {
        Order {
            id: OrderId::new(id),
            symbol: Symbol::new(0),
            side,
            price: Price::new(price),
            quantity: Quantity::new(qty),
            remaining_qty: Quantity::new(qty),
            order_type: OrderType::Limit,
            time_in_force: tif,
            timestamp: id,
            account_id: 1,
            prev: NO_LINK,
            next: NO_LINK,
        }
    }

    fn make_market_order(id: u64, side: Side, qty: u64) -> Order {
        let price = match side {
            Side::Buy => Price::new(i64::MAX),
            Side::Sell => Price::new(1), // minimum valid price
        };
        Order {
            id: OrderId::new(id),
            symbol: Symbol::new(0),
            side,
            price,
            quantity: Quantity::new(qty),
            remaining_qty: Quantity::new(qty),
            order_type: OrderType::Market,
            time_in_force: TimeInForce::GTC,
            timestamp: id,
            account_id: 1,
            prev: NO_LINK,
            next: NO_LINK,
        }
    }

    fn make_limit_order_with_account(
        id: u64,
        side: Side,
        price: i64,
        qty: u64,
        account_id: u64,
    ) -> Order {
        Order {
            id: OrderId::new(id),
            symbol: Symbol::new(0),
            side,
            price: Price::new(price),
            quantity: Quantity::new(qty),
            remaining_qty: Quantity::new(qty),
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            timestamp: id,
            account_id,
            prev: NO_LINK,
            next: NO_LINK,
        }
    }

    /// Count trades in an event list.
    fn count_trades(events: &SmallVec<[EngineEvent; 8]>) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, EngineEvent::Trade { .. }))
            .count()
    }

    /// Get the total traded quantity from events.
    fn total_traded_qty(events: &SmallVec<[EngineEvent; 8]>) -> Quantity {
        let mut total = Quantity::ZERO;
        for e in events {
            if let EngineEvent::Trade { trade } = e {
                total = total.checked_add(trade.quantity).unwrap_or(total);
            }
        }
        total
    }

    fn has_accepted(events: &SmallVec<[EngineEvent; 8]>) -> bool {
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::OrderAccepted { .. }))
    }

    fn has_rejected(events: &SmallVec<[EngineEvent; 8]>) -> bool {
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::OrderRejected { .. }))
    }

    fn has_cancelled(events: &SmallVec<[EngineEvent; 8]>) -> bool {
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::OrderCancelled { .. }))
    }

    fn get_reject_reason(events: &SmallVec<[EngineEvent; 8]>) -> Option<RejectReason> {
        for e in events {
            if let EngineEvent::OrderRejected { reason, .. } = e {
                return Some(*reason);
            }
        }
        None
    }

    // -----------------------------------------------------------------------
    // 1. Buy limit crosses best ask -> trade at ask price
    // -----------------------------------------------------------------------
    #[test]
    fn test_limit_buy_crosses_best_ask() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        // Place a sell order at 100
        let sell = make_limit_order(1, Side::Sell, 100, 10, TimeInForce::GTC);
        book.add_order(sell).unwrap();

        // Place a buy order at 100 (crosses the ask)
        let buy = make_limit_order(2, Side::Buy, 100, 10, TimeInForce::GTC);
        let events = matcher.match_order(&mut book, buy);

        assert!(has_accepted(&events));
        assert_eq!(count_trades(&events), 1);
        if let EngineEvent::Trade { trade } = &events[1] {
            assert_eq!(trade.price, Price::new(100)); // trades at resting price
            assert_eq!(trade.quantity, Quantity::new(10));
            assert_eq!(trade.maker_order_id, OrderId::new(1));
            assert_eq!(trade.taker_order_id, OrderId::new(2));
            assert_eq!(trade.maker_side, Side::Sell);
        } else {
            panic!("Expected Trade event");
        }
        assert_eq!(book.order_count(), 0);
    }

    // -----------------------------------------------------------------------
    // 2. Sell limit crosses best bid -> trade at bid price
    // -----------------------------------------------------------------------
    #[test]
    fn test_limit_sell_crosses_best_bid() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        // Place a buy order at 100
        let buy = make_limit_order(1, Side::Buy, 100, 10, TimeInForce::GTC);
        book.add_order(buy).unwrap();

        // Place a sell at 100
        let sell = make_limit_order(2, Side::Sell, 100, 10, TimeInForce::GTC);
        let events = matcher.match_order(&mut book, sell);

        assert!(has_accepted(&events));
        assert_eq!(count_trades(&events), 1);
        if let EngineEvent::Trade { trade } = &events[1] {
            assert_eq!(trade.price, Price::new(100));
            assert_eq!(trade.quantity, Quantity::new(10));
            assert_eq!(trade.maker_side, Side::Buy);
        } else {
            panic!("Expected Trade event");
        }
        assert_eq!(book.order_count(), 0);
    }

    // -----------------------------------------------------------------------
    // 3. Partial fill: incoming qty < resting qty
    // -----------------------------------------------------------------------
    #[test]
    fn test_partial_fill_incoming_less_than_resting() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        // Resting sell: 100 qty at price 100
        let sell = make_limit_order(1, Side::Sell, 100, 100, TimeInForce::GTC);
        book.add_order(sell).unwrap();

        // Incoming buy: 30 qty at price 100
        let buy = make_limit_order(2, Side::Buy, 100, 30, TimeInForce::GTC);
        let events = matcher.match_order(&mut book, buy);

        assert_eq!(count_trades(&events), 1);
        assert_eq!(total_traded_qty(&events), Quantity::new(30));

        // Resting order should have 70 remaining
        let resting = book.get_order(OrderId::new(1)).unwrap();
        assert_eq!(resting.remaining_qty, Quantity::new(70));
        assert_eq!(book.order_count(), 1);
    }

    // -----------------------------------------------------------------------
    // 4. Full fill of resting, incoming remainder goes to book (GTC)
    // -----------------------------------------------------------------------
    #[test]
    fn test_full_fill_resting_remainder_to_book() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        // Resting sell: 30 qty at price 100
        let sell = make_limit_order(1, Side::Sell, 100, 30, TimeInForce::GTC);
        book.add_order(sell).unwrap();

        // Incoming buy: 50 qty at price 100
        let buy = make_limit_order(2, Side::Buy, 100, 50, TimeInForce::GTC);
        let events = matcher.match_order(&mut book, buy);

        assert_eq!(count_trades(&events), 1);
        assert_eq!(total_traded_qty(&events), Quantity::new(30));

        // Sell order fully filled (removed)
        assert!(book.get_order(OrderId::new(1)).is_none());

        // Buy order remainder (20) should be on the book
        let remaining_buy = book.get_order(OrderId::new(2)).unwrap();
        assert_eq!(remaining_buy.remaining_qty, Quantity::new(20));
    }

    // -----------------------------------------------------------------------
    // 5. No cross: limit price doesn't reach opposing side -> goes to book
    // -----------------------------------------------------------------------
    #[test]
    fn test_no_cross_goes_to_book() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        // Resting sell at 110
        let sell = make_limit_order(1, Side::Sell, 110, 10, TimeInForce::GTC);
        book.add_order(sell).unwrap();

        // Buy at 100 — does not cross
        let buy = make_limit_order(2, Side::Buy, 100, 10, TimeInForce::GTC);
        let events = matcher.match_order(&mut book, buy);

        assert!(has_accepted(&events));
        assert_eq!(count_trades(&events), 0);
        assert_eq!(book.order_count(), 2);
    }

    // -----------------------------------------------------------------------
    // 6. Multiple fills: incoming sweeps 3+ price levels
    // -----------------------------------------------------------------------
    #[test]
    fn test_multiple_fills_sweep_levels() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        // Place 3 sell orders at different prices
        book.add_order(make_limit_order(1, Side::Sell, 100, 10, TimeInForce::GTC))
            .unwrap();
        book.add_order(make_limit_order(2, Side::Sell, 101, 10, TimeInForce::GTC))
            .unwrap();
        book.add_order(make_limit_order(3, Side::Sell, 102, 10, TimeInForce::GTC))
            .unwrap();

        // Aggressive buy sweeps all 3 levels
        let buy = make_limit_order(4, Side::Buy, 105, 30, TimeInForce::GTC);
        let events = matcher.match_order(&mut book, buy);

        assert_eq!(count_trades(&events), 3);
        assert_eq!(total_traded_qty(&events), Quantity::new(30));
        assert_eq!(book.order_count(), 0);
    }

    // -----------------------------------------------------------------------
    // 7. Price-time priority: two orders at same price, earlier one fills first
    // -----------------------------------------------------------------------
    #[test]
    fn test_price_time_priority() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        // Two sells at same price, order 1 first (earlier timestamp)
        book.add_order(make_limit_order(1, Side::Sell, 100, 10, TimeInForce::GTC))
            .unwrap();
        book.add_order(make_limit_order(2, Side::Sell, 100, 10, TimeInForce::GTC))
            .unwrap();

        // Buy only 10 — should match order 1 (earlier)
        let buy = make_limit_order(3, Side::Buy, 100, 10, TimeInForce::GTC);
        let events = matcher.match_order(&mut book, buy);

        assert_eq!(count_trades(&events), 1);
        if let EngineEvent::Trade { trade } = &events[1] {
            assert_eq!(trade.maker_order_id, OrderId::new(1));
        } else {
            panic!("Expected Trade event");
        }

        // Order 2 should still be on the book
        assert!(book.get_order(OrderId::new(2)).is_some());
        assert!(book.get_order(OrderId::new(1)).is_none());
    }

    // -----------------------------------------------------------------------
    // 8. Market buy sweeps all asks
    // -----------------------------------------------------------------------
    #[test]
    fn test_market_buy_sweeps_asks() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        book.add_order(make_limit_order(1, Side::Sell, 100, 10, TimeInForce::GTC))
            .unwrap();
        book.add_order(make_limit_order(2, Side::Sell, 105, 10, TimeInForce::GTC))
            .unwrap();

        let buy = make_market_order(3, Side::Buy, 20);
        let events = matcher.match_order(&mut book, buy);

        assert!(has_accepted(&events));
        assert_eq!(count_trades(&events), 2);
        assert_eq!(total_traded_qty(&events), Quantity::new(20));
        assert_eq!(book.order_count(), 0);
    }

    // -----------------------------------------------------------------------
    // 9. Market sell sweeps all bids
    // -----------------------------------------------------------------------
    #[test]
    fn test_market_sell_sweeps_bids() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        book.add_order(make_limit_order(1, Side::Buy, 100, 10, TimeInForce::GTC))
            .unwrap();
        book.add_order(make_limit_order(2, Side::Buy, 95, 10, TimeInForce::GTC))
            .unwrap();

        let sell = make_market_order(3, Side::Sell, 20);
        let events = matcher.match_order(&mut book, sell);

        assert!(has_accepted(&events));
        assert_eq!(count_trades(&events), 2);
        assert_eq!(total_traded_qty(&events), Quantity::new(20));
        assert_eq!(book.order_count(), 0);
    }

    // -----------------------------------------------------------------------
    // 10. Market order on empty book -> accepted then cancelled (no liquidity)
    // -----------------------------------------------------------------------
    #[test]
    fn test_market_order_empty_book() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        let buy = make_market_order(1, Side::Buy, 10);
        let events = matcher.match_order(&mut book, buy);

        assert!(has_accepted(&events));
        assert_eq!(count_trades(&events), 0);
        assert!(has_cancelled(&events));
    }

    // -----------------------------------------------------------------------
    // 11. IOC partial fill -> remainder cancelled
    // -----------------------------------------------------------------------
    #[test]
    fn test_ioc_partial_fill() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        book.add_order(make_limit_order(1, Side::Sell, 100, 5, TimeInForce::GTC))
            .unwrap();

        let buy = make_limit_order(2, Side::Buy, 100, 10, TimeInForce::IOC);
        let events = matcher.match_order(&mut book, buy);

        assert!(has_accepted(&events));
        assert_eq!(count_trades(&events), 1);
        assert_eq!(total_traded_qty(&events), Quantity::new(5));
        assert!(has_cancelled(&events));

        // The IOC order should not be on the book
        assert!(book.get_order(OrderId::new(2)).is_none());
    }

    // -----------------------------------------------------------------------
    // 12. IOC full fill -> no cancel event
    // -----------------------------------------------------------------------
    #[test]
    fn test_ioc_full_fill() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        book.add_order(make_limit_order(1, Side::Sell, 100, 10, TimeInForce::GTC))
            .unwrap();

        let buy = make_limit_order(2, Side::Buy, 100, 10, TimeInForce::IOC);
        let events = matcher.match_order(&mut book, buy);

        assert!(has_accepted(&events));
        assert_eq!(count_trades(&events), 1);
        assert!(!has_cancelled(&events));
    }

    // -----------------------------------------------------------------------
    // 13. IOC no fill -> entire order cancelled
    // -----------------------------------------------------------------------
    #[test]
    fn test_ioc_no_fill() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        // No opposing liquidity at crossable price
        book.add_order(make_limit_order(1, Side::Sell, 110, 10, TimeInForce::GTC))
            .unwrap();

        let buy = make_limit_order(2, Side::Buy, 100, 10, TimeInForce::IOC);
        let events = matcher.match_order(&mut book, buy);

        assert!(has_accepted(&events));
        assert_eq!(count_trades(&events), 0);
        assert!(has_cancelled(&events));
    }

    // -----------------------------------------------------------------------
    // 14. FOK sufficient liquidity -> full fill
    // -----------------------------------------------------------------------
    #[test]
    fn test_fok_sufficient_liquidity() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        book.add_order(make_limit_order(1, Side::Sell, 100, 10, TimeInForce::GTC))
            .unwrap();
        book.add_order(make_limit_order(2, Side::Sell, 101, 10, TimeInForce::GTC))
            .unwrap();

        let buy = make_limit_order(3, Side::Buy, 101, 15, TimeInForce::FOK);
        let events = matcher.match_order(&mut book, buy);

        assert!(has_accepted(&events));
        assert_eq!(count_trades(&events), 2);
        assert_eq!(total_traded_qty(&events), Quantity::new(15));
        assert!(!has_rejected(&events));
    }

    // -----------------------------------------------------------------------
    // 15. FOK insufficient liquidity -> rejected, book unchanged
    // -----------------------------------------------------------------------
    #[test]
    fn test_fok_insufficient_liquidity() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        book.add_order(make_limit_order(1, Side::Sell, 100, 5, TimeInForce::GTC))
            .unwrap();

        let buy = make_limit_order(2, Side::Buy, 100, 10, TimeInForce::FOK);
        let events = matcher.match_order(&mut book, buy);

        assert!(has_rejected(&events));
        assert_eq!(
            get_reject_reason(&events),
            Some(RejectReason::InsufficientLiquidity)
        );
        assert_eq!(count_trades(&events), 0);
        // Book should be unchanged
        assert_eq!(book.order_count(), 1);
        assert!(book.get_order(OrderId::new(1)).is_some());
    }

    // -----------------------------------------------------------------------
    // 16. FOK exact liquidity -> full fill
    // -----------------------------------------------------------------------
    #[test]
    fn test_fok_exact_liquidity() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        book.add_order(make_limit_order(1, Side::Sell, 100, 10, TimeInForce::GTC))
            .unwrap();

        let buy = make_limit_order(2, Side::Buy, 100, 10, TimeInForce::FOK);
        let events = matcher.match_order(&mut book, buy);

        assert!(has_accepted(&events));
        assert_eq!(count_trades(&events), 1);
        assert_eq!(total_traded_qty(&events), Quantity::new(10));
        assert_eq!(book.order_count(), 0);
    }

    // -----------------------------------------------------------------------
    // 17. PostOnly would cross -> rejected
    // -----------------------------------------------------------------------
    #[test]
    fn test_post_only_would_cross() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        book.add_order(make_limit_order(1, Side::Sell, 100, 10, TimeInForce::GTC))
            .unwrap();

        let buy = make_limit_order(2, Side::Buy, 100, 10, TimeInForce::PostOnly);
        let events = matcher.match_order(&mut book, buy);

        assert!(has_rejected(&events));
        assert_eq!(
            get_reject_reason(&events),
            Some(RejectReason::PostOnlyWouldTake)
        );
        assert_eq!(count_trades(&events), 0);
        // Book unchanged
        assert_eq!(book.order_count(), 1);
    }

    // -----------------------------------------------------------------------
    // 18. PostOnly doesn't cross -> added to book
    // -----------------------------------------------------------------------
    #[test]
    fn test_post_only_doesnt_cross() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        book.add_order(make_limit_order(1, Side::Sell, 110, 10, TimeInForce::GTC))
            .unwrap();

        let buy = make_limit_order(2, Side::Buy, 100, 10, TimeInForce::PostOnly);
        let events = matcher.match_order(&mut book, buy);

        assert!(has_accepted(&events));
        assert!(!has_rejected(&events));
        assert_eq!(count_trades(&events), 0);
        assert_eq!(book.order_count(), 2);
        assert!(book.get_order(OrderId::new(2)).is_some());
    }

    // -----------------------------------------------------------------------
    // 19. STP CancelNewest: same account -> incoming cancelled
    // -----------------------------------------------------------------------
    #[test]
    fn test_stp_cancel_newest() {
        let mut book = new_book();
        let mut matcher = Matcher::new(StpMode::CancelNewest);

        // Resting sell from account 1
        let sell = make_limit_order_with_account(1, Side::Sell, 100, 10, 1);
        book.add_order(sell).unwrap();

        // Incoming buy from same account 1
        let buy = make_limit_order_with_account(2, Side::Buy, 100, 10, 1);
        let events = matcher.match_order(&mut book, buy);

        assert!(has_accepted(&events));
        assert!(has_cancelled(&events));
        assert_eq!(count_trades(&events), 0);
        // Resting order should still be on the book
        assert!(book.get_order(OrderId::new(1)).is_some());
        assert!(book.get_order(OrderId::new(2)).is_none());
    }

    // -----------------------------------------------------------------------
    // 20. STP CancelOldest: same account -> resting cancelled
    // -----------------------------------------------------------------------
    #[test]
    fn test_stp_cancel_oldest() {
        let mut book = new_book();
        let mut matcher = Matcher::new(StpMode::CancelOldest);

        // Resting sell from account 1
        let sell = make_limit_order_with_account(1, Side::Sell, 100, 10, 1);
        book.add_order(sell).unwrap();

        // Incoming buy from same account 1
        let buy = make_limit_order_with_account(2, Side::Buy, 100, 10, 1);
        let events = matcher.match_order(&mut book, buy);

        assert!(has_accepted(&events));
        // Resting order should be cancelled
        assert!(book.get_order(OrderId::new(1)).is_none());
        assert_eq!(count_trades(&events), 0);
    }

    // -----------------------------------------------------------------------
    // 21. Different account_id -> normal match
    // -----------------------------------------------------------------------
    #[test]
    fn test_stp_different_accounts_normal_match() {
        let mut book = new_book();
        let mut matcher = Matcher::new(StpMode::CancelNewest);

        // Resting sell from account 1
        let sell = make_limit_order_with_account(1, Side::Sell, 100, 10, 1);
        book.add_order(sell).unwrap();

        // Incoming buy from account 2
        let buy = make_limit_order_with_account(2, Side::Buy, 100, 10, 2);
        let events = matcher.match_order(&mut book, buy);

        assert!(has_accepted(&events));
        assert_eq!(count_trades(&events), 1);
        assert_eq!(book.order_count(), 0);
    }

    // -----------------------------------------------------------------------
    // 22. Risk: price band violation -> rejected
    // -----------------------------------------------------------------------
    #[test]
    fn test_risk_price_band_violation() {
        let config = default_config();
        let risk_config = RiskConfig {
            price_band_pct: 500, // 5%
            max_order_qty: Quantity::new(1_000_000),
        };
        let mut engine = SymbolEngine::new(Symbol::new(0), config, StpMode::None, risk_config);

        // Place orders to establish BBO: bid=100, ask=102
        engine.process_command(
            Command::PlaceOrder(make_limit_order(1, Side::Buy, 100, 10, TimeInForce::GTC)),
            1,
            1,
        );
        engine.process_command(
            Command::PlaceOrder(make_limit_order(2, Side::Sell, 102, 10, TimeInForce::GTC)),
            1,
            2,
        );

        // Mid price = 101. 5% band = [95.95, 106.05].
        // Place a buy at 200 — way outside the band
        let events = engine.process_command(
            Command::PlaceOrder(make_limit_order(3, Side::Buy, 200, 10, TimeInForce::GTC)),
            1,
            3,
        );

        assert!(has_rejected(&events));
        assert_eq!(
            get_reject_reason(&events),
            Some(RejectReason::PriceOutOfRange)
        );
    }

    // -----------------------------------------------------------------------
    // 23. Risk: max qty violation -> rejected
    // -----------------------------------------------------------------------
    #[test]
    fn test_risk_max_qty_violation() {
        let config = default_config();
        let risk_config = RiskConfig {
            price_band_pct: 0, // disabled
            max_order_qty: Quantity::new(100),
        };
        let mut engine = SymbolEngine::new(Symbol::new(0), config, StpMode::None, risk_config);

        let events = engine.process_command(
            Command::PlaceOrder(make_limit_order(1, Side::Buy, 100, 200, TimeInForce::GTC)),
            1,
            1,
        );

        assert!(has_rejected(&events));
        assert_eq!(
            get_reject_reason(&events),
            Some(RejectReason::InvalidQuantity)
        );
    }

    // -----------------------------------------------------------------------
    // 24. Risk: normal order passes checks
    // -----------------------------------------------------------------------
    #[test]
    fn test_risk_normal_order_passes() {
        let config = default_config();
        let risk_config = RiskConfig {
            price_band_pct: 500,
            max_order_qty: Quantity::new(1_000_000),
        };
        let mut engine = SymbolEngine::new(Symbol::new(0), config, StpMode::None, risk_config);

        let events = engine.process_command(
            Command::PlaceOrder(make_limit_order(1, Side::Buy, 100, 10, TimeInForce::GTC)),
            1,
            1,
        );

        assert!(has_accepted(&events));
        assert!(!has_rejected(&events));
    }

    // -----------------------------------------------------------------------
    // 25. Integration: build up book, then aggressive order sweeps it
    // -----------------------------------------------------------------------
    #[test]
    fn test_integration_build_and_sweep() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        // Build up ask side with 5 levels
        for i in 1..=5 {
            let sell = make_limit_order(i, Side::Sell, 100 + i as i64, 10, TimeInForce::GTC);
            book.add_order(sell).unwrap();
        }

        assert_eq!(book.order_count(), 5);

        // Aggressive buy sweeps all 5 levels
        let buy = make_limit_order(10, Side::Buy, 110, 50, TimeInForce::GTC);
        let events = matcher.match_order(&mut book, buy);

        assert!(has_accepted(&events));
        assert_eq!(count_trades(&events), 5);
        assert_eq!(total_traded_qty(&events), Quantity::new(50));
        assert_eq!(book.order_count(), 0);
    }

    // -----------------------------------------------------------------------
    // 26. Determinism: same sequence of commands -> same events
    // -----------------------------------------------------------------------
    #[test]
    fn test_determinism() {
        fn run_scenario() -> Vec<String> {
            let mut book = new_book();
            let mut matcher = Matcher::new(StpMode::None);

            let mut all_events = Vec::new();

            // Place some orders
            let orders = vec![
                make_limit_order(1, Side::Sell, 100, 10, TimeInForce::GTC),
                make_limit_order(2, Side::Sell, 101, 20, TimeInForce::GTC),
                make_limit_order(3, Side::Buy, 99, 15, TimeInForce::GTC),
                make_limit_order(4, Side::Buy, 101, 25, TimeInForce::GTC), // crosses
            ];

            for order in orders {
                let events = matcher.match_order(&mut book, order);
                for e in &events {
                    all_events.push(format!("{:?}", e));
                }
            }

            all_events
        }

        let run1 = run_scenario();
        let run2 = run_scenario();

        assert_eq!(run1.len(), run2.len());
        for (a, b) in run1.iter().zip(run2.iter()) {
            assert_eq!(a, b);
        }
    }

    // -----------------------------------------------------------------------
    // Additional edge case tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_limit_buy_better_price_trades_at_resting() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        // Sell at 100
        book.add_order(make_limit_order(1, Side::Sell, 100, 10, TimeInForce::GTC))
            .unwrap();

        // Buy at 110 — should trade at 100 (resting price)
        let buy = make_limit_order(2, Side::Buy, 110, 10, TimeInForce::GTC);
        let events = matcher.match_order(&mut book, buy);

        if let EngineEvent::Trade { trade } = &events[1] {
            assert_eq!(trade.price, Price::new(100)); // maker price
        } else {
            panic!("Expected Trade event");
        }
    }

    #[test]
    fn test_fok_sell() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        book.add_order(make_limit_order(1, Side::Buy, 100, 5, TimeInForce::GTC))
            .unwrap();
        book.add_order(make_limit_order(2, Side::Buy, 99, 5, TimeInForce::GTC))
            .unwrap();

        // FOK sell for 10 at price 99 — should succeed
        let sell = make_limit_order(3, Side::Sell, 99, 10, TimeInForce::FOK);
        let events = matcher.match_order(&mut book, sell);

        assert!(has_accepted(&events));
        assert_eq!(count_trades(&events), 2);
        assert_eq!(total_traded_qty(&events), Quantity::new(10));
    }

    #[test]
    fn test_post_only_sell_below_bid_rejected() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        book.add_order(make_limit_order(1, Side::Buy, 100, 10, TimeInForce::GTC))
            .unwrap();

        // PostOnly sell at 100 — would cross
        let sell = make_limit_order(2, Side::Sell, 100, 10, TimeInForce::PostOnly);
        let events = matcher.match_order(&mut book, sell);

        assert!(has_rejected(&events));
        assert_eq!(
            get_reject_reason(&events),
            Some(RejectReason::PostOnlyWouldTake)
        );
    }

    #[test]
    fn test_engine_cancel_order() {
        let config = default_config();
        let risk_config = RiskConfig {
            price_band_pct: 0,
            max_order_qty: Quantity::new(1_000_000),
        };
        let mut engine = SymbolEngine::new(Symbol::new(0), config, StpMode::None, risk_config);

        engine.process_command(
            Command::PlaceOrder(make_limit_order(1, Side::Buy, 100, 10, TimeInForce::GTC)),
            1,
            1,
        );

        let events = engine.process_command(Command::CancelOrder(OrderId::new(1)), 1, 2);

        assert!(has_cancelled(&events));
        assert_eq!(engine.book().order_count(), 0);
    }

    #[test]
    fn test_engine_modify_order() {
        let config = default_config();
        let risk_config = RiskConfig {
            price_band_pct: 0,
            max_order_qty: Quantity::new(1_000_000),
        };
        let mut engine = SymbolEngine::new(Symbol::new(0), config, StpMode::None, risk_config);

        engine.process_command(
            Command::PlaceOrder(make_limit_order(1, Side::Buy, 100, 10, TimeInForce::GTC)),
            1,
            1,
        );

        let events = engine.process_command(
            Command::ModifyOrder {
                order_id: OrderId::new(1),
                new_price: Some(Price::new(105)),
                new_qty: Some(Quantity::new(20)),
            },
            1,
            2,
        );

        // Should have cancel + accept
        assert!(has_cancelled(&events));
        assert!(has_accepted(&events));

        let order = engine.book().get_order(OrderId::new(1)).unwrap();
        assert_eq!(order.price, Price::new(105));
        assert_eq!(order.quantity, Quantity::new(20));
    }

    #[test]
    fn test_market_order_partial_fill_remainder_cancelled() {
        let mut book = new_book();
        let mut matcher = new_matcher();

        // Only 5 units available
        book.add_order(make_limit_order(1, Side::Sell, 100, 5, TimeInForce::GTC))
            .unwrap();

        // Market buy for 10
        let buy = make_market_order(2, Side::Buy, 10);
        let events = matcher.match_order(&mut book, buy);

        assert!(has_accepted(&events));
        assert_eq!(count_trades(&events), 1);
        assert_eq!(total_traded_qty(&events), Quantity::new(5));
        assert!(has_cancelled(&events)); // remainder cancelled
    }
}
