//! Matching engine micro-benchmarks.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use tachyon_core::*;
use tachyon_engine::{Command, RiskConfig, StpMode, SymbolEngine};

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

fn make_risk() -> RiskConfig {
    RiskConfig {
        price_band_pct: 0,
        max_order_qty: Quantity::new(u64::MAX / 2),
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

fn make_market_order(id: u64, side: Side, qty: u64) -> Order {
    Order {
        id: OrderId::new(id),
        symbol: Symbol::new(0),
        side,
        price: match side {
            Side::Buy => Price::new(i64::MAX),
            Side::Sell => Price::new(1),
        },
        quantity: Quantity::new(qty),
        remaining_qty: Quantity::new(qty),
        order_type: OrderType::Market,
        time_in_force: TimeInForce::IOC,
        timestamp: 0,
        account_id: 2,
        prev: NO_LINK,
        next: NO_LINK,
    }
}

/// Benchmark: place a limit order that does NOT cross (pure book insertion).
fn bench_place_no_match(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine/place_no_match");

    for &depth in &[0, 100, 1000, 10_000] {
        group.bench_with_input(BenchmarkId::new("depth", depth), &depth, |b, &depth| {
            b.iter_custom(|iters| {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let mut engine = SymbolEngine::new(
                        Symbol::new(0),
                        make_config(),
                        StpMode::None,
                        make_risk(),
                    );

                    // Pre-fill book
                    for i in 0..depth {
                        let order = make_order(i as u64, Side::Buy, 50000 - i as i64, 100);
                        engine.process_command(Command::PlaceOrder(order), 1, 0);
                    }

                    // Place a non-crossing sell far from best bid
                    let order = make_order(depth as u64 + 1, Side::Sell, 60000, 100);
                    let start = std::time::Instant::now();
                    let _ = black_box(engine.process_command(Command::PlaceOrder(order), 2, 0));
                    total += start.elapsed();
                }
                total
            });
        });
    }

    group.finish();
}

/// Benchmark: limit order that crosses and fills exactly one resting order.
fn bench_single_fill(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine/single_fill");

    for &depth in &[1, 100, 1000] {
        group.bench_with_input(BenchmarkId::new("depth", depth), &depth, |b, &depth| {
            b.iter_custom(|iters| {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let mut engine = SymbolEngine::new(
                        Symbol::new(0),
                        make_config(),
                        StpMode::None,
                        make_risk(),
                    );

                    // Pre-fill asks: best ask at 50001, then 50002, ...
                    for i in 0..depth {
                        let order = make_order(i as u64, Side::Sell, 50001 + i as i64, 100);
                        engine.process_command(Command::PlaceOrder(order), 1, 0);
                    }

                    // Aggressive buy that fills only the best ask
                    let order = make_order(depth as u64 + 1, Side::Buy, 50001, 100);
                    let start = std::time::Instant::now();
                    let _ = black_box(engine.process_command(Command::PlaceOrder(order), 2, 0));
                    total += start.elapsed();
                }
                total
            });
        });
    }

    group.finish();
}

/// Benchmark: market order that sweeps N price levels.
fn bench_sweep_levels(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine/sweep_levels");

    for &levels in &[1, 5, 10, 50] {
        group.bench_with_input(BenchmarkId::new("levels", levels), &levels, |b, &levels| {
            b.iter_custom(|iters| {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let mut engine = SymbolEngine::new(
                        Symbol::new(0),
                        make_config(),
                        StpMode::None,
                        make_risk(),
                    );

                    // Fill `levels` ask price levels, 1 order each
                    for i in 0..levels {
                        let order = make_order(i as u64, Side::Sell, 50001 + i as i64, 100);
                        engine.process_command(Command::PlaceOrder(order), 1, 0);
                    }

                    // Market buy sweeps all levels
                    let total_qty = (levels as u64) * 100;
                    let order = make_market_order(levels as u64 + 1, Side::Buy, total_qty);
                    let start = std::time::Instant::now();
                    let _ = black_box(engine.process_command(Command::PlaceOrder(order), 2, 0));
                    total += start.elapsed();
                }
                total
            });
        });
    }

    group.finish();
}

/// Benchmark: cancel order.
fn bench_cancel(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine/cancel");

    for &depth in &[100, 1000, 10_000] {
        group.bench_with_input(BenchmarkId::new("depth", depth), &depth, |b, &depth| {
            b.iter_custom(|iters| {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let mut engine = SymbolEngine::new(
                        Symbol::new(0),
                        make_config(),
                        StpMode::None,
                        make_risk(),
                    );

                    for i in 0..depth {
                        let order = make_order(i as u64, Side::Buy, 50000 - i as i64, 100);
                        engine.process_command(Command::PlaceOrder(order), 1, 0);
                    }

                    // Cancel the middle order
                    let target = OrderId::new((depth / 2) as u64);
                    let start = std::time::Instant::now();
                    let _ = black_box(engine.process_command(Command::CancelOrder(target), 1, 0));
                    total += start.elapsed();
                }
                total
            });
        });
    }

    group.finish();
}

/// Benchmark: FOK order that checks liquidity then fills.
fn bench_fok(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine/fok");

    // FOK that succeeds (enough liquidity)
    group.bench_function("success", |b| {
        b.iter_custom(|iters| {
            let mut total = std::time::Duration::ZERO;
            for _ in 0..iters {
                let mut engine =
                    SymbolEngine::new(Symbol::new(0), make_config(), StpMode::None, make_risk());

                // 10 ask levels with 100 qty each = 1000 total
                for i in 0..10 {
                    let order = make_order(i, Side::Sell, 50001 + i as i64, 100);
                    engine.process_command(Command::PlaceOrder(order), 1, 0);
                }

                let mut fok = make_order(100, Side::Buy, 50010, 500);
                fok.time_in_force = TimeInForce::FOK;
                let start = std::time::Instant::now();
                let _ = black_box(engine.process_command(Command::PlaceOrder(fok), 2, 0));
                total += start.elapsed();
            }
            total
        });
    });

    // FOK that fails (insufficient liquidity)
    group.bench_function("reject", |b| {
        b.iter_custom(|iters| {
            let mut total = std::time::Duration::ZERO;
            for _ in 0..iters {
                let mut engine =
                    SymbolEngine::new(Symbol::new(0), make_config(), StpMode::None, make_risk());

                for i in 0..10 {
                    let order = make_order(i, Side::Sell, 50001 + i as i64, 100);
                    engine.process_command(Command::PlaceOrder(order), 1, 0);
                }

                let mut fok = make_order(100, Side::Buy, 50010, 5000); // more than available
                fok.time_in_force = TimeInForce::FOK;
                let start = std::time::Instant::now();
                let _ = black_box(engine.process_command(Command::PlaceOrder(fok), 2, 0));
                total += start.elapsed();
            }
            total
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_place_no_match,
    bench_single_fill,
    bench_sweep_levels,
    bench_cancel,
    bench_fok,
);
criterion_main!(benches);
