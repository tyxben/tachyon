//! Prometheus metrics for the Tachyon matching engine server.

use prometheus::{
    Encoder, Histogram, HistogramOpts, IntCounter, IntGaugeVec, Opts, Registry, TextEncoder,
};

/// Central metrics registry for the Tachyon server.
#[allow(dead_code)]
pub struct Metrics {
    pub registry: Registry,
    pub orders_total: IntCounter,
    pub trades_total: IntCounter,
    pub match_latency_ns: Histogram,
    pub active_orders: IntGaugeVec,
    pub book_depth: IntGaugeVec,
}

impl Metrics {
    /// Create and register all metrics with a new registry.
    pub fn new() -> Self {
        let registry = Registry::new();

        let orders_total = IntCounter::with_opts(Opts::new(
            "tachyon_orders_total",
            "Total number of orders processed",
        ))
        .expect("metric creation should not fail");

        let trades_total = IntCounter::with_opts(Opts::new(
            "tachyon_trades_total",
            "Total number of trades executed",
        ))
        .expect("metric creation should not fail");

        let match_latency_ns = Histogram::with_opts(
            HistogramOpts::new(
                "tachyon_match_latency_ns",
                "Order matching latency in nanoseconds",
            )
            .buckets(vec![
                100.0, 500.0, 1000.0, 5000.0, 10000.0, 50000.0, 100000.0,
            ]),
        )
        .expect("metric creation should not fail");

        let active_orders = IntGaugeVec::new(
            Opts::new(
                "tachyon_active_orders",
                "Number of active orders per symbol",
            ),
            &["symbol"],
        )
        .expect("metric creation should not fail");

        let book_depth = IntGaugeVec::new(
            Opts::new(
                "tachyon_book_depth",
                "Order book depth (price levels) per symbol and side",
            ),
            &["symbol", "side"],
        )
        .expect("metric creation should not fail");

        registry
            .register(Box::new(orders_total.clone()))
            .expect("metric registration should not fail");
        registry
            .register(Box::new(trades_total.clone()))
            .expect("metric registration should not fail");
        registry
            .register(Box::new(match_latency_ns.clone()))
            .expect("metric registration should not fail");
        registry
            .register(Box::new(active_orders.clone()))
            .expect("metric registration should not fail");
        registry
            .register(Box::new(book_depth.clone()))
            .expect("metric registration should not fail");

        Self {
            registry,
            orders_total,
            trades_total,
            match_latency_ns,
            active_orders,
            book_depth,
        }
    }

    /// Encode all metrics in Prometheus text exposition format.
    pub fn encode(&self) -> String {
        let encoder = TextEncoder::new();
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        encoder
            .encode(&metric_families, &mut buffer)
            .expect("encoding should not fail");
        String::from_utf8(buffer).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_encode_returns_valid_prometheus_format() {
        let metrics = Metrics::new();

        // Record some values
        metrics.orders_total.inc_by(42);
        metrics.trades_total.inc_by(10);
        metrics.match_latency_ns.observe(500.0);
        metrics.match_latency_ns.observe(1500.0);
        metrics
            .active_orders
            .with_label_values(&["BTCUSDT"])
            .set(100);
        metrics
            .book_depth
            .with_label_values(&["BTCUSDT", "buy"])
            .set(50);
        metrics
            .book_depth
            .with_label_values(&["BTCUSDT", "sell"])
            .set(45);

        let output = metrics.encode();

        // Verify prometheus text format contains expected metrics
        assert!(
            output.contains("tachyon_orders_total 42"),
            "should contain orders_total: {}",
            output
        );
        assert!(
            output.contains("tachyon_trades_total 10"),
            "should contain trades_total"
        );
        assert!(
            output.contains("tachyon_match_latency_ns_bucket"),
            "should contain histogram buckets"
        );
        assert!(
            output.contains("tachyon_match_latency_ns_count 2"),
            "should contain histogram count"
        );
        assert!(
            output.contains("tachyon_active_orders"),
            "should contain active_orders"
        );
        assert!(
            output.contains("tachyon_book_depth"),
            "should contain book_depth"
        );
        assert!(output.contains("BTCUSDT"), "should contain symbol labels");

        // Verify it contains HELP and TYPE lines (standard prometheus format)
        assert!(
            output.contains("# HELP tachyon_orders_total"),
            "should contain HELP line"
        );
        assert!(
            output.contains("# TYPE tachyon_orders_total counter"),
            "should contain TYPE line"
        );
    }

    #[test]
    fn test_metrics_encode_empty() {
        let metrics = Metrics::new();
        let output = metrics.encode();

        // Even with no observations, the metrics should still be present
        assert!(output.contains("tachyon_orders_total 0"));
        assert!(output.contains("tachyon_trades_total 0"));
    }
}
