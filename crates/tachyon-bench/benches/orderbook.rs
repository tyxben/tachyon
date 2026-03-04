//! Order book micro-benchmarks.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use tachyon_core::*;

fn make_config() -> SymbolConfig {
    SymbolConfig {
        symbol: Symbol::new(0),
        tick_size: Price::new(1),
        lot_size: Quantity::new(1),
        price_scale: 2,
        qty_scale: 8,
        max_price: Price::new(i64::MAX / 2),
        min_price: Price::new(1),
        max_order_qty: Quantity::new(u64::MAX / 2),
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
        timestamp: 0,
        account_id: 1,
        prev: NO_LINK,
        next: NO_LINK,
    }
}

fn bench_add_order(c: &mut Criterion) {
    let mut group = c.benchmark_group("orderbook/add_order");

    for &depth in &[10, 100, 1000, 10_000] {
        group.bench_with_input(BenchmarkId::new("depth", depth), &depth, |b, &depth| {
            b.iter_custom(|iters| {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let mut book = tachyon_book::OrderBook::new(Symbol::new(0), make_config());

                    // Pre-fill with `depth` levels
                    for i in 0..depth {
                        let _ =
                            book.add_order(make_order(i as u64, Side::Buy, 50000 - i as i64, 100));
                    }

                    // Benchmark adding one more order
                    let order = make_order(depth as u64 + 1, Side::Buy, 50001, 100);
                    let start = std::time::Instant::now();
                    let _ = black_box(book.add_order(order));
                    total += start.elapsed();
                }
                total
            });
        });
    }

    group.finish();
}

fn bench_cancel_order(c: &mut Criterion) {
    let mut group = c.benchmark_group("orderbook/cancel_order");

    for &depth in &[10, 100, 1000, 10_000] {
        group.bench_with_input(BenchmarkId::new("depth", depth), &depth, |b, &depth| {
            b.iter_custom(|iters| {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let mut book = tachyon_book::OrderBook::new(Symbol::new(0), make_config());

                    for i in 0..depth {
                        let _ =
                            book.add_order(make_order(i as u64, Side::Buy, 50000 - i as i64, 100));
                    }

                    // Cancel the middle order
                    let target = OrderId::new((depth / 2) as u64);
                    let start = std::time::Instant::now();
                    let _ = black_box(book.cancel_order(target));
                    total += start.elapsed();
                }
                total
            });
        });
    }

    group.finish();
}

fn bench_get_depth(c: &mut Criterion) {
    let mut group = c.benchmark_group("orderbook/get_depth");

    // Build a book with 1000 bid + 1000 ask levels
    let mut book = tachyon_book::OrderBook::new(Symbol::new(0), make_config());
    for i in 0..1000u64 {
        let _ = book.add_order(make_order(i, Side::Buy, 50000 - i as i64, 100));
        let _ = book.add_order(make_order(1000 + i, Side::Sell, 50001 + i as i64, 100));
    }

    for &n in &[5, 20, 50] {
        group.bench_with_input(BenchmarkId::new("levels", n), &n, |b, &n| {
            b.iter(|| black_box(book.get_depth(n)));
        });
    }

    group.finish();
}

fn bench_best_bid_ask(c: &mut Criterion) {
    let mut book = tachyon_book::OrderBook::new(Symbol::new(0), make_config());
    for i in 0..1000u64 {
        let _ = book.add_order(make_order(i, Side::Buy, 50000 - i as i64, 100));
        let _ = book.add_order(make_order(1000 + i, Side::Sell, 50001 + i as i64, 100));
    }

    c.bench_function("orderbook/best_bid_price", |b| {
        b.iter(|| black_box(book.best_bid_price()));
    });

    c.bench_function("orderbook/best_ask_price", |b| {
        b.iter(|| black_box(book.best_ask_price()));
    });
}

criterion_group!(
    benches,
    bench_add_order,
    bench_cancel_order,
    bench_get_depth,
    bench_best_bid_ask,
);
criterion_main!(benches);
