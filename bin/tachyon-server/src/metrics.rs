//! Lock-free metrics for the Tachyon matching engine server.
//!
//! Uses `AtomicU64` counters and gauges to avoid pulling in the full prometheus crate
//! on the hot path. A simple histogram with pre-defined buckets captures latency
//! distributions. The [`Metrics::encode_prometheus`] method outputs Prometheus text
//! exposition format, and [`MetricsSnapshot`] provides a JSON-serializable view.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Instant;

use serde::Serialize;
use tachyon_gateway::MetricsProvider;

// ---------------------------------------------------------------------------
// Histogram
// ---------------------------------------------------------------------------

/// Pre-defined histogram bucket upper bounds in nanoseconds.
const HISTOGRAM_BUCKETS: &[u64] = &[
    100,       // 100 ns
    500,       // 500 ns
    1_000,     // 1 us
    5_000,     // 5 us
    10_000,    // 10 us
    50_000,    // 50 us
    100_000,   // 100 us
    500_000,   // 500 us
    1_000_000, // 1 ms
];

/// A simple HDR-style histogram backed by atomic counters for each bucket
/// plus a `+Inf` overflow bucket.
pub struct AtomicHistogram {
    /// One counter per bucket (index matches [`HISTOGRAM_BUCKETS`]).
    buckets: Vec<AtomicU64>,
    /// Overflow bucket (`+Inf`).
    inf: AtomicU64,
    /// Running sum of all observed values.
    sum: AtomicU64,
    /// Total number of observations.
    count: AtomicU64,
}

impl AtomicHistogram {
    /// Creates a new histogram with the standard Tachyon latency buckets.
    pub fn new() -> Self {
        let buckets = (0..HISTOGRAM_BUCKETS.len())
            .map(|_| AtomicU64::new(0))
            .collect();
        Self {
            buckets,
            inf: AtomicU64::new(0),
            sum: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    /// Record an observation (value in nanoseconds).
    pub fn observe(&self, value: u64) {
        self.sum.fetch_add(value, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        for (i, &upper) in HISTOGRAM_BUCKETS.iter().enumerate() {
            if value <= upper {
                self.buckets[i].fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        self.inf.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot for rendering.
    fn snapshot(&self) -> HistogramSnapshot {
        let bucket_counts: Vec<u64> = self
            .buckets
            .iter()
            .map(|b| b.load(Ordering::Relaxed))
            .collect();
        HistogramSnapshot {
            bucket_bounds: HISTOGRAM_BUCKETS.to_vec(),
            bucket_counts,
            inf: self.inf.load(Ordering::Relaxed),
            sum: self.sum.load(Ordering::Relaxed),
            count: self.count.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HistogramSnapshot {
    pub bucket_bounds: Vec<u64>,
    pub bucket_counts: Vec<u64>,
    pub inf: u64,
    pub sum: u64,
    pub count: u64,
}

impl HistogramSnapshot {
    /// Encode as Prometheus text format lines (cumulative buckets).
    fn encode_prometheus(&self, name: &str, help: &str, buf: &mut String) {
        buf.push_str(&format!("# HELP {} {}\n", name, help));
        buf.push_str(&format!("# TYPE {} histogram\n", name));
        let mut cumulative: u64 = 0;
        for (i, &bound) in self.bucket_bounds.iter().enumerate() {
            cumulative += self.bucket_counts[i];
            buf.push_str(&format!(
                "{}_bucket{{le=\"{}\"}} {}\n",
                name, bound, cumulative
            ));
        }
        cumulative += self.inf;
        buf.push_str(&format!("{}_bucket{{le=\"+Inf\"}} {}\n", name, cumulative));
        buf.push_str(&format!("{}_sum {}\n", name, self.sum));
        buf.push_str(&format!("{}_count {}\n", name, self.count));
    }
}

// ---------------------------------------------------------------------------
// Labeled Counter helpers
// ---------------------------------------------------------------------------

/// A counter that tracks values per (side, type, symbol) label triple for orders.
pub struct OrderCounter {
    buy_limit: AtomicU64,
    buy_market: AtomicU64,
    sell_limit: AtomicU64,
    sell_market: AtomicU64,
}

impl OrderCounter {
    pub fn new() -> Self {
        Self {
            buy_limit: AtomicU64::new(0),
            buy_market: AtomicU64::new(0),
            sell_limit: AtomicU64::new(0),
            sell_market: AtomicU64::new(0),
        }
    }

    /// Increment the appropriate bucket.
    pub fn inc(&self, side: &str, order_type: &str) {
        match (side, order_type) {
            ("buy", "limit") => self.buy_limit.fetch_add(1, Ordering::Relaxed),
            ("buy", "market") => self.buy_market.fetch_add(1, Ordering::Relaxed),
            ("sell", "limit") => self.sell_limit.fetch_add(1, Ordering::Relaxed),
            ("sell", "market") => self.sell_market.fetch_add(1, Ordering::Relaxed),
            _ => 0,
        };
    }

    /// Total across all labels.
    #[cfg(test)]
    pub fn total(&self) -> u64 {
        self.buy_limit.load(Ordering::Relaxed)
            + self.buy_market.load(Ordering::Relaxed)
            + self.sell_limit.load(Ordering::Relaxed)
            + self.sell_market.load(Ordering::Relaxed)
    }

    fn snapshot(&self) -> OrderCounterSnapshot {
        OrderCounterSnapshot {
            buy_limit: self.buy_limit.load(Ordering::Relaxed),
            buy_market: self.buy_market.load(Ordering::Relaxed),
            sell_limit: self.sell_limit.load(Ordering::Relaxed),
            sell_market: self.sell_market.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OrderCounterSnapshot {
    pub buy_limit: u64,
    pub buy_market: u64,
    pub sell_limit: u64,
    pub sell_market: u64,
}

/// Counter that tracks rejections by reason.
pub struct RejectCounter {
    pub insufficient_liquidity: AtomicU64,
    pub price_out_of_range: AtomicU64,
    pub invalid_quantity: AtomicU64,
    pub invalid_price: AtomicU64,
    pub book_full: AtomicU64,
    pub rate_limit: AtomicU64,
    pub self_trade: AtomicU64,
    pub post_only: AtomicU64,
    pub duplicate_order: AtomicU64,
    pub other: AtomicU64,
}

impl RejectCounter {
    pub fn new() -> Self {
        Self {
            insufficient_liquidity: AtomicU64::new(0),
            price_out_of_range: AtomicU64::new(0),
            invalid_quantity: AtomicU64::new(0),
            invalid_price: AtomicU64::new(0),
            book_full: AtomicU64::new(0),
            rate_limit: AtomicU64::new(0),
            self_trade: AtomicU64::new(0),
            post_only: AtomicU64::new(0),
            duplicate_order: AtomicU64::new(0),
            other: AtomicU64::new(0),
        }
    }

    /// Increment the counter for the given reason string.
    pub fn inc(&self, reason: &str) {
        match reason {
            "InsufficientLiquidity" => {
                self.insufficient_liquidity.fetch_add(1, Ordering::Relaxed);
            }
            "PriceOutOfRange" => {
                self.price_out_of_range.fetch_add(1, Ordering::Relaxed);
            }
            "InvalidQuantity" => {
                self.invalid_quantity.fetch_add(1, Ordering::Relaxed);
            }
            "InvalidPrice" => {
                self.invalid_price.fetch_add(1, Ordering::Relaxed);
            }
            "BookFull" => {
                self.book_full.fetch_add(1, Ordering::Relaxed);
            }
            "RateLimitExceeded" => {
                self.rate_limit.fetch_add(1, Ordering::Relaxed);
            }
            "SelfTradePrevented" => {
                self.self_trade.fetch_add(1, Ordering::Relaxed);
            }
            "PostOnlyWouldTake" => {
                self.post_only.fetch_add(1, Ordering::Relaxed);
            }
            "DuplicateOrderId" => {
                self.duplicate_order.fetch_add(1, Ordering::Relaxed);
            }
            _ => {
                self.other.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Total across all reasons.
    #[cfg(test)]
    pub fn total(&self) -> u64 {
        self.insufficient_liquidity.load(Ordering::Relaxed)
            + self.price_out_of_range.load(Ordering::Relaxed)
            + self.invalid_quantity.load(Ordering::Relaxed)
            + self.invalid_price.load(Ordering::Relaxed)
            + self.book_full.load(Ordering::Relaxed)
            + self.rate_limit.load(Ordering::Relaxed)
            + self.self_trade.load(Ordering::Relaxed)
            + self.post_only.load(Ordering::Relaxed)
            + self.duplicate_order.load(Ordering::Relaxed)
            + self.other.load(Ordering::Relaxed)
    }

    fn snapshot(&self) -> RejectCounterSnapshot {
        RejectCounterSnapshot {
            insufficient_liquidity: self.insufficient_liquidity.load(Ordering::Relaxed),
            price_out_of_range: self.price_out_of_range.load(Ordering::Relaxed),
            invalid_quantity: self.invalid_quantity.load(Ordering::Relaxed),
            invalid_price: self.invalid_price.load(Ordering::Relaxed),
            book_full: self.book_full.load(Ordering::Relaxed),
            rate_limit: self.rate_limit.load(Ordering::Relaxed),
            self_trade: self.self_trade.load(Ordering::Relaxed),
            post_only: self.post_only.load(Ordering::Relaxed),
            duplicate_order: self.duplicate_order.load(Ordering::Relaxed),
            other: self.other.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RejectCounterSnapshot {
    pub insufficient_liquidity: u64,
    pub price_out_of_range: u64,
    pub invalid_quantity: u64,
    pub invalid_price: u64,
    pub book_full: u64,
    pub rate_limit: u64,
    pub self_trade: u64,
    pub post_only: u64,
    pub duplicate_order: u64,
    pub other: u64,
}

// ---------------------------------------------------------------------------
// Central Metrics struct
// ---------------------------------------------------------------------------

/// Central metrics registry for the Tachyon server.
///
/// All fields use lock-free atomics so they can be updated from engine threads
/// without contention.
pub struct Metrics {
    /// Server start time for uptime calculation.
    start_time: Instant,

    // -- Counters --
    /// Orders processed, labeled by (side, type).
    pub orders_total: OrderCounter,
    /// Trades executed (total).
    pub trades_total: AtomicU64,
    /// Cancellations.
    pub cancels_total: AtomicU64,
    /// Rejections labeled by reason.
    pub rejects_total: RejectCounter,
    /// WebSocket connections ever opened.
    pub ws_connections_total: AtomicU64,
    /// TCP connections ever opened.
    pub tcp_connections_total: AtomicU64,
    /// WS messages pushed to clients.
    pub ws_messages_sent_total: AtomicU64,

    // -- Gauges --
    /// Number of bid price levels (set per symbol, we aggregate).
    pub book_depth_bids: AtomicI64,
    /// Number of ask price levels.
    pub book_depth_asks: AtomicI64,
    /// Total resting orders.
    pub book_orders_count: AtomicI64,
    /// Active WebSocket connections right now.
    pub ws_connections_active: AtomicI64,
    /// Active TCP connections right now.
    pub tcp_connections_active: AtomicI64,

    // -- Histograms --
    /// End-to-end order processing latency in nanoseconds (bridge send to response).
    pub order_latency_ns: AtomicHistogram,
    /// Pure matching engine latency per command in nanoseconds.
    pub match_latency_ns: AtomicHistogram,
}

impl Metrics {
    /// Create a fresh metrics instance.
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            orders_total: OrderCounter::new(),
            trades_total: AtomicU64::new(0),
            cancels_total: AtomicU64::new(0),
            rejects_total: RejectCounter::new(),
            ws_connections_total: AtomicU64::new(0),
            tcp_connections_total: AtomicU64::new(0),
            ws_messages_sent_total: AtomicU64::new(0),
            book_depth_bids: AtomicI64::new(0),
            book_depth_asks: AtomicI64::new(0),
            book_orders_count: AtomicI64::new(0),
            ws_connections_active: AtomicI64::new(0),
            tcp_connections_active: AtomicI64::new(0),
            order_latency_ns: AtomicHistogram::new(),
            match_latency_ns: AtomicHistogram::new(),
        }
    }

    /// Current uptime in seconds.
    pub fn uptime_secs(&self) -> u64 {
        self.start_time.elapsed().as_secs()
    }

    /// Build a JSON-serializable snapshot of all metrics.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            uptime_secs: self.uptime_secs(),
            orders_total: self.orders_total.snapshot(),
            trades_total: self.trades_total.load(Ordering::Relaxed),
            cancels_total: self.cancels_total.load(Ordering::Relaxed),
            rejects_total: self.rejects_total.snapshot(),
            ws_connections_total: self.ws_connections_total.load(Ordering::Relaxed),
            tcp_connections_total: self.tcp_connections_total.load(Ordering::Relaxed),
            ws_messages_sent_total: self.ws_messages_sent_total.load(Ordering::Relaxed),
            book_depth_bids: self.book_depth_bids.load(Ordering::Relaxed),
            book_depth_asks: self.book_depth_asks.load(Ordering::Relaxed),
            book_orders_count: self.book_orders_count.load(Ordering::Relaxed),
            ws_connections_active: self.ws_connections_active.load(Ordering::Relaxed),
            tcp_connections_active: self.tcp_connections_active.load(Ordering::Relaxed),
            order_latency_ns: self.order_latency_ns.snapshot(),
            match_latency_ns: self.match_latency_ns.snapshot(),
        }
    }

    /// Encode all metrics in Prometheus text exposition format.
    pub fn encode_prometheus(&self) -> String {
        let mut buf = String::with_capacity(4096);

        // -- Counters --

        // tachyon_orders_total (labeled)
        let oc = self.orders_total.snapshot();
        buf.push_str("# HELP tachyon_orders_total Total number of orders processed\n");
        buf.push_str("# TYPE tachyon_orders_total counter\n");
        buf.push_str(&format!(
            "tachyon_orders_total{{side=\"buy\",type=\"limit\"}} {}\n",
            oc.buy_limit
        ));
        buf.push_str(&format!(
            "tachyon_orders_total{{side=\"buy\",type=\"market\"}} {}\n",
            oc.buy_market
        ));
        buf.push_str(&format!(
            "tachyon_orders_total{{side=\"sell\",type=\"limit\"}} {}\n",
            oc.sell_limit
        ));
        buf.push_str(&format!(
            "tachyon_orders_total{{side=\"sell\",type=\"market\"}} {}\n",
            oc.sell_market
        ));

        // tachyon_trades_total
        let trades = self.trades_total.load(Ordering::Relaxed);
        buf.push_str("# HELP tachyon_trades_total Total number of trades executed\n");
        buf.push_str("# TYPE tachyon_trades_total counter\n");
        buf.push_str(&format!("tachyon_trades_total {}\n", trades));

        // tachyon_cancels_total
        let cancels = self.cancels_total.load(Ordering::Relaxed);
        buf.push_str("# HELP tachyon_cancels_total Total number of order cancellations\n");
        buf.push_str("# TYPE tachyon_cancels_total counter\n");
        buf.push_str(&format!("tachyon_cancels_total {}\n", cancels));

        // tachyon_rejects_total (labeled by reason)
        let rc = self.rejects_total.snapshot();
        buf.push_str("# HELP tachyon_rejects_total Total number of order rejections\n");
        buf.push_str("# TYPE tachyon_rejects_total counter\n");
        buf.push_str(&format!(
            "tachyon_rejects_total{{reason=\"InsufficientLiquidity\"}} {}\n",
            rc.insufficient_liquidity
        ));
        buf.push_str(&format!(
            "tachyon_rejects_total{{reason=\"PriceOutOfRange\"}} {}\n",
            rc.price_out_of_range
        ));
        buf.push_str(&format!(
            "tachyon_rejects_total{{reason=\"InvalidQuantity\"}} {}\n",
            rc.invalid_quantity
        ));
        buf.push_str(&format!(
            "tachyon_rejects_total{{reason=\"InvalidPrice\"}} {}\n",
            rc.invalid_price
        ));
        buf.push_str(&format!(
            "tachyon_rejects_total{{reason=\"BookFull\"}} {}\n",
            rc.book_full
        ));
        buf.push_str(&format!(
            "tachyon_rejects_total{{reason=\"RateLimitExceeded\"}} {}\n",
            rc.rate_limit
        ));
        buf.push_str(&format!(
            "tachyon_rejects_total{{reason=\"SelfTradePrevented\"}} {}\n",
            rc.self_trade
        ));
        buf.push_str(&format!(
            "tachyon_rejects_total{{reason=\"PostOnlyWouldTake\"}} {}\n",
            rc.post_only
        ));
        buf.push_str(&format!(
            "tachyon_rejects_total{{reason=\"DuplicateOrderId\"}} {}\n",
            rc.duplicate_order
        ));

        // tachyon_ws_connections_total
        buf.push_str("# HELP tachyon_ws_connections_total Total WebSocket connections opened\n");
        buf.push_str("# TYPE tachyon_ws_connections_total counter\n");
        buf.push_str(&format!(
            "tachyon_ws_connections_total {}\n",
            self.ws_connections_total.load(Ordering::Relaxed)
        ));

        // tachyon_tcp_connections_total
        buf.push_str("# HELP tachyon_tcp_connections_total Total TCP connections opened\n");
        buf.push_str("# TYPE tachyon_tcp_connections_total counter\n");
        buf.push_str(&format!(
            "tachyon_tcp_connections_total {}\n",
            self.tcp_connections_total.load(Ordering::Relaxed)
        ));

        // tachyon_ws_messages_sent_total
        buf.push_str(
            "# HELP tachyon_ws_messages_sent_total Total WebSocket messages pushed to clients\n",
        );
        buf.push_str("# TYPE tachyon_ws_messages_sent_total counter\n");
        buf.push_str(&format!(
            "tachyon_ws_messages_sent_total {}\n",
            self.ws_messages_sent_total.load(Ordering::Relaxed)
        ));

        // -- Gauges --

        // tachyon_book_depth_levels
        buf.push_str("# HELP tachyon_book_depth_levels Number of price levels per side\n");
        buf.push_str("# TYPE tachyon_book_depth_levels gauge\n");
        buf.push_str(&format!(
            "tachyon_book_depth_levels{{side=\"bid\"}} {}\n",
            self.book_depth_bids.load(Ordering::Relaxed)
        ));
        buf.push_str(&format!(
            "tachyon_book_depth_levels{{side=\"ask\"}} {}\n",
            self.book_depth_asks.load(Ordering::Relaxed)
        ));

        // tachyon_book_orders_count
        buf.push_str("# HELP tachyon_book_orders_count Total resting orders in the book\n");
        buf.push_str("# TYPE tachyon_book_orders_count gauge\n");
        buf.push_str(&format!(
            "tachyon_book_orders_count {}\n",
            self.book_orders_count.load(Ordering::Relaxed)
        ));

        // tachyon_ws_connections_active
        buf.push_str("# HELP tachyon_ws_connections_active Current active WebSocket connections\n");
        buf.push_str("# TYPE tachyon_ws_connections_active gauge\n");
        buf.push_str(&format!(
            "tachyon_ws_connections_active {}\n",
            self.ws_connections_active.load(Ordering::Relaxed)
        ));

        // tachyon_tcp_connections_active
        buf.push_str("# HELP tachyon_tcp_connections_active Current active TCP connections\n");
        buf.push_str("# TYPE tachyon_tcp_connections_active gauge\n");
        buf.push_str(&format!(
            "tachyon_tcp_connections_active {}\n",
            self.tcp_connections_active.load(Ordering::Relaxed)
        ));

        // tachyon_uptime_seconds
        buf.push_str("# HELP tachyon_uptime_seconds Server uptime in seconds\n");
        buf.push_str("# TYPE tachyon_uptime_seconds gauge\n");
        buf.push_str(&format!("tachyon_uptime_seconds {}\n", self.uptime_secs()));

        // -- Histograms --
        self.order_latency_ns.snapshot().encode_prometheus(
            "tachyon_order_latency_ns",
            "End-to-end order processing latency in nanoseconds",
            &mut buf,
        );
        self.match_latency_ns.snapshot().encode_prometheus(
            "tachyon_match_latency_ns",
            "Pure matching engine latency per command in nanoseconds",
            &mut buf,
        );

        buf
    }
}

// ---------------------------------------------------------------------------
// MetricsSnapshot (JSON)
// ---------------------------------------------------------------------------

/// JSON-serializable snapshot of all metrics.
#[derive(Debug, Clone, Serialize)]
pub struct MetricsSnapshot {
    pub uptime_secs: u64,
    pub orders_total: OrderCounterSnapshot,
    pub trades_total: u64,
    pub cancels_total: u64,
    pub rejects_total: RejectCounterSnapshot,
    pub ws_connections_total: u64,
    pub tcp_connections_total: u64,
    pub ws_messages_sent_total: u64,
    pub book_depth_bids: i64,
    pub book_depth_asks: i64,
    pub book_orders_count: i64,
    pub ws_connections_active: i64,
    pub tcp_connections_active: i64,
    pub order_latency_ns: HistogramSnapshot,
    pub match_latency_ns: HistogramSnapshot,
}

// ---------------------------------------------------------------------------
// MetricsProvider trait implementation
// ---------------------------------------------------------------------------

impl MetricsProvider for Metrics {
    fn encode_prometheus(&self) -> String {
        self.encode_prometheus()
    }

    fn encode_json(&self) -> String {
        let snap = self.snapshot();
        serde_json::to_string(&snap).unwrap_or_else(|_| "{}".to_string())
    }

    fn uptime_secs(&self) -> u64 {
        self.uptime_secs()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_histogram_observe_and_snapshot() {
        let h = AtomicHistogram::new();
        h.observe(50); // bucket 100
        h.observe(200); // bucket 500
        h.observe(1_500); // bucket 5000
        h.observe(999_999); // bucket 1_000_000
        h.observe(2_000_000); // +Inf

        let snap = h.snapshot();
        assert_eq!(snap.count, 5);
        assert_eq!(snap.sum, 50 + 200 + 1_500 + 999_999 + 2_000_000);

        // Bucket 100ns should have 1
        assert_eq!(snap.bucket_counts[0], 1);
        // Bucket 500ns should have 1
        assert_eq!(snap.bucket_counts[1], 1);
        // Bucket 1000ns should have 0
        assert_eq!(snap.bucket_counts[2], 0);
        // Bucket 5000ns should have 1
        assert_eq!(snap.bucket_counts[3], 1);
        // Bucket 1_000_000ns should have 1
        assert_eq!(snap.bucket_counts[8], 1);
        // Inf should have 1
        assert_eq!(snap.inf, 1);
    }

    #[test]
    fn test_histogram_boundary_values() {
        let h = AtomicHistogram::new();
        // Exactly on boundary should go into that bucket
        h.observe(100);
        h.observe(500);
        h.observe(1_000);

        let snap = h.snapshot();
        assert_eq!(snap.bucket_counts[0], 1); // <= 100
        assert_eq!(snap.bucket_counts[1], 1); // <= 500
        assert_eq!(snap.bucket_counts[2], 1); // <= 1000
        assert_eq!(snap.count, 3);
    }

    #[test]
    fn test_histogram_zero_value() {
        let h = AtomicHistogram::new();
        h.observe(0);
        let snap = h.snapshot();
        assert_eq!(snap.bucket_counts[0], 1); // 0 <= 100
        assert_eq!(snap.count, 1);
        assert_eq!(snap.sum, 0);
    }

    #[test]
    fn test_order_counter_inc_and_total() {
        let c = OrderCounter::new();
        c.inc("buy", "limit");
        c.inc("buy", "limit");
        c.inc("sell", "market");
        c.inc("buy", "market");

        assert_eq!(c.total(), 4);
        let snap = c.snapshot();
        assert_eq!(snap.buy_limit, 2);
        assert_eq!(snap.buy_market, 1);
        assert_eq!(snap.sell_limit, 0);
        assert_eq!(snap.sell_market, 1);
    }

    #[test]
    fn test_reject_counter_inc_and_total() {
        let r = RejectCounter::new();
        r.inc("InsufficientLiquidity");
        r.inc("InsufficientLiquidity");
        r.inc("PostOnlyWouldTake");
        r.inc("SomethingUnknown");

        assert_eq!(r.total(), 4);
        let snap = r.snapshot();
        assert_eq!(snap.insufficient_liquidity, 2);
        assert_eq!(snap.post_only, 1);
        assert_eq!(snap.other, 1);
    }

    #[test]
    fn test_metrics_new() {
        let m = Metrics::new();
        assert_eq!(m.trades_total.load(Ordering::Relaxed), 0);
        assert_eq!(m.cancels_total.load(Ordering::Relaxed), 0);
        assert_eq!(m.orders_total.total(), 0);
        assert_eq!(m.rejects_total.total(), 0);
    }

    #[test]
    fn test_metrics_counter_increments() {
        let m = Metrics::new();

        m.orders_total.inc("buy", "limit");
        m.orders_total.inc("sell", "market");
        m.trades_total.fetch_add(5, Ordering::Relaxed);
        m.cancels_total.fetch_add(2, Ordering::Relaxed);
        m.rejects_total.inc("InvalidPrice");
        m.ws_connections_total.fetch_add(3, Ordering::Relaxed);
        m.tcp_connections_total.fetch_add(1, Ordering::Relaxed);
        m.ws_messages_sent_total.fetch_add(100, Ordering::Relaxed);

        assert_eq!(m.orders_total.total(), 2);
        assert_eq!(m.trades_total.load(Ordering::Relaxed), 5);
        assert_eq!(m.cancels_total.load(Ordering::Relaxed), 2);
        assert_eq!(m.rejects_total.total(), 1);
        assert_eq!(m.ws_connections_total.load(Ordering::Relaxed), 3);
        assert_eq!(m.tcp_connections_total.load(Ordering::Relaxed), 1);
        assert_eq!(m.ws_messages_sent_total.load(Ordering::Relaxed), 100);
    }

    #[test]
    fn test_metrics_encode_prometheus_format() {
        let m = Metrics::new();
        m.orders_total.inc("buy", "limit");
        m.orders_total.inc("buy", "limit");
        m.trades_total.fetch_add(10, Ordering::Relaxed);
        m.cancels_total.fetch_add(3, Ordering::Relaxed);
        m.rejects_total.inc("PostOnlyWouldTake");
        m.match_latency_ns.observe(500);
        m.match_latency_ns.observe(1_500);

        let output = m.encode_prometheus();

        // Counters
        assert!(
            output.contains("# HELP tachyon_orders_total"),
            "should contain HELP line for orders"
        );
        assert!(
            output.contains("# TYPE tachyon_orders_total counter"),
            "should contain TYPE line for orders"
        );
        assert!(
            output.contains("tachyon_orders_total{side=\"buy\",type=\"limit\"} 2"),
            "should contain labeled order count"
        );
        assert!(
            output.contains("tachyon_trades_total 10"),
            "should contain trades_total"
        );
        assert!(
            output.contains("tachyon_cancels_total 3"),
            "should contain cancels_total"
        );
        assert!(
            output.contains("tachyon_rejects_total{reason=\"PostOnlyWouldTake\"} 1"),
            "should contain labeled reject"
        );

        // Gauges
        assert!(
            output.contains("# TYPE tachyon_uptime_seconds gauge"),
            "should contain uptime gauge"
        );
        assert!(
            output.contains("tachyon_uptime_seconds"),
            "should contain uptime value"
        );
        assert!(
            output.contains("tachyon_book_depth_levels{side=\"bid\"}"),
            "should contain book depth bid"
        );
        assert!(
            output.contains("tachyon_book_depth_levels{side=\"ask\"}"),
            "should contain book depth ask"
        );
        assert!(
            output.contains("tachyon_ws_connections_active"),
            "should contain ws active gauge"
        );
        assert!(
            output.contains("tachyon_tcp_connections_active"),
            "should contain tcp active gauge"
        );

        // Histograms
        assert!(
            output.contains("tachyon_match_latency_ns_bucket"),
            "should contain histogram buckets"
        );
        assert!(
            output.contains("tachyon_match_latency_ns_count 2"),
            "should contain histogram count"
        );
        assert!(
            output.contains("tachyon_match_latency_ns_sum 2000"),
            "should contain histogram sum"
        );
        assert!(
            output.contains("tachyon_order_latency_ns_bucket"),
            "should contain order latency histogram"
        );
    }

    #[test]
    fn test_metrics_encode_empty() {
        let m = Metrics::new();
        let output = m.encode_prometheus();

        // Even with no observations, metric lines should be present
        assert!(output.contains("tachyon_orders_total{side=\"buy\",type=\"limit\"} 0"));
        assert!(output.contains("tachyon_trades_total 0"));
        assert!(output.contains("tachyon_cancels_total 0"));
        assert!(output.contains("tachyon_match_latency_ns_count 0"));
    }

    #[test]
    fn test_prometheus_histogram_cumulative_buckets() {
        let h = AtomicHistogram::new();
        h.observe(50); // bucket 100
        h.observe(300); // bucket 500
        h.observe(800); // bucket 1000

        let snap = h.snapshot();
        let mut buf = String::new();
        snap.encode_prometheus("test_hist", "a test", &mut buf);

        // Cumulative: bucket 100 = 1, bucket 500 = 2, bucket 1000 = 3
        assert!(buf.contains("test_hist_bucket{le=\"100\"} 1"));
        assert!(buf.contains("test_hist_bucket{le=\"500\"} 2"));
        assert!(buf.contains("test_hist_bucket{le=\"1000\"} 3"));
        assert!(buf.contains("test_hist_bucket{le=\"+Inf\"} 3"));
        assert!(buf.contains("test_hist_count 3"));
        assert!(buf.contains(&format!("test_hist_sum {}", 50 + 300 + 800)));
    }

    #[test]
    fn test_metrics_snapshot_json_roundtrip() {
        let m = Metrics::new();
        m.orders_total.inc("buy", "limit");
        m.trades_total.fetch_add(1, Ordering::Relaxed);
        m.cancels_total.fetch_add(1, Ordering::Relaxed);
        m.rejects_total.inc("InvalidPrice");
        m.match_latency_ns.observe(42);

        let snap = m.snapshot();
        let json = serde_json::to_string(&snap).expect("serialize snapshot");

        assert!(json.contains("\"trades_total\":1"));
        assert!(json.contains("\"cancels_total\":1"));
        assert!(json.contains("\"buy_limit\":1"));

        // Verify it can be deserialized back (at least to Value)
        let val: serde_json::Value = serde_json::from_str(&json).expect("parse json");
        assert_eq!(val["trades_total"], 1);
        assert_eq!(val["cancels_total"], 1);
    }

    #[test]
    fn test_metrics_uptime() {
        let m = Metrics::new();
        // Uptime should be 0 or very small
        assert!(m.uptime_secs() < 2);
    }

    #[test]
    fn test_metrics_gauges() {
        let m = Metrics::new();
        m.book_depth_bids.store(15, Ordering::Relaxed);
        m.book_depth_asks.store(12, Ordering::Relaxed);
        m.book_orders_count.store(100, Ordering::Relaxed);
        m.ws_connections_active.store(5, Ordering::Relaxed);
        m.tcp_connections_active.store(3, Ordering::Relaxed);

        let snap = m.snapshot();
        assert_eq!(snap.book_depth_bids, 15);
        assert_eq!(snap.book_depth_asks, 12);
        assert_eq!(snap.book_orders_count, 100);
        assert_eq!(snap.ws_connections_active, 5);
        assert_eq!(snap.tcp_connections_active, 3);

        let output = m.encode_prometheus();
        assert!(output.contains("tachyon_book_depth_levels{side=\"bid\"} 15"));
        assert!(output.contains("tachyon_book_depth_levels{side=\"ask\"} 12"));
        assert!(output.contains("tachyon_book_orders_count 100"));
        assert!(output.contains("tachyon_ws_connections_active 5"));
        assert!(output.contains("tachyon_tcp_connections_active 3"));
    }

    #[test]
    fn test_reject_counter_all_reasons() {
        let r = RejectCounter::new();
        r.inc("InsufficientLiquidity");
        r.inc("PriceOutOfRange");
        r.inc("InvalidQuantity");
        r.inc("InvalidPrice");
        r.inc("BookFull");
        r.inc("RateLimitExceeded");
        r.inc("SelfTradePrevented");
        r.inc("PostOnlyWouldTake");
        r.inc("DuplicateOrderId");

        assert_eq!(r.total(), 9);
        let snap = r.snapshot();
        assert_eq!(snap.insufficient_liquidity, 1);
        assert_eq!(snap.price_out_of_range, 1);
        assert_eq!(snap.invalid_quantity, 1);
        assert_eq!(snap.invalid_price, 1);
        assert_eq!(snap.book_full, 1);
        assert_eq!(snap.rate_limit, 1);
        assert_eq!(snap.self_trade, 1);
        assert_eq!(snap.post_only, 1);
        assert_eq!(snap.duplicate_order, 1);
    }
}
