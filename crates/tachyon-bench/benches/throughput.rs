//! End-to-end throughput benchmark: measures sustained ops/sec through the engine.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use rand::prelude::*;
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

/// Realistic workload: 60% new limit orders, 30% cancels, 10% market orders.
/// Orders cluster around a mid price to produce realistic matching.
fn bench_mixed_workload(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput/mixed");

    for &batch_size in &[1000, 10_000, 100_000] {
        group.throughput(Throughput::Elements(batch_size));
        group.bench_function(format!("ops_{batch_size}"), |b| {
            b.iter_custom(|iters| {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let mut engine = SymbolEngine::new(
                        Symbol::new(0),
                        make_config(),
                        StpMode::None,
                        make_risk(),
                    );
                    let mut rng = StdRng::seed_from_u64(42);
                    let mut next_id: u64 = 1;
                    let mut active_orders: Vec<u64> = Vec::with_capacity(batch_size as usize);
                    let mid_price: i64 = 5_000_000; // 50000.00 scaled

                    let start = std::time::Instant::now();
                    for _ in 0..batch_size {
                        let roll: f64 = rng.gen();
                        if roll < 0.6 {
                            // 60%: place limit order
                            let side = if rng.gen_bool(0.5) {
                                Side::Buy
                            } else {
                                Side::Sell
                            };
                            let offset = rng.gen_range(-500i64..500);
                            let price = mid_price + offset;
                            let qty = rng.gen_range(1u64..1000);
                            let order = Order {
                                id: OrderId::new(next_id),
                                symbol: Symbol::new(0),
                                side,
                                price: Price::new(price),
                                quantity: Quantity::new(qty),
                                remaining_qty: Quantity::new(qty),
                                order_type: OrderType::Limit,
                                time_in_force: TimeInForce::GTC,
                                timestamp: 0,
                                account_id: rng.gen_range(1..100),
                                prev: NO_LINK,
                                next: NO_LINK,
                            };
                            let events = engine.process_command(
                                Command::PlaceOrder(order),
                                rng.gen_range(1..100),
                                0,
                            );
                            // Track if order was placed on book (accepted and not fully filled)
                            let mut accepted = false;
                            let mut fully_consumed = false;
                            for e in &events {
                                match e {
                                    EngineEvent::OrderAccepted { .. } => accepted = true,
                                    EngineEvent::OrderCancelled { order_id, .. }
                                        if order_id.raw() == next_id =>
                                    {
                                        fully_consumed = true;
                                    }
                                    _ => {}
                                }
                            }
                            if accepted && !fully_consumed {
                                active_orders.push(next_id);
                            }
                            black_box(events);
                            next_id += 1;
                        } else if roll < 0.9 && !active_orders.is_empty() {
                            // 30%: cancel random active order
                            let idx = rng.gen_range(0..active_orders.len());
                            let oid = active_orders.swap_remove(idx);
                            let events = engine.process_command(
                                Command::CancelOrder(OrderId::new(oid)),
                                1,
                                0,
                            );
                            black_box(events);
                        } else {
                            // 10%: market order
                            let side = if rng.gen_bool(0.5) {
                                Side::Buy
                            } else {
                                Side::Sell
                            };
                            let qty = rng.gen_range(1u64..500);
                            let order = Order {
                                id: OrderId::new(next_id),
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
                                account_id: rng.gen_range(1..100),
                                prev: NO_LINK,
                                next: NO_LINK,
                            };
                            let events = engine.process_command(
                                Command::PlaceOrder(order),
                                rng.gen_range(1..100),
                                0,
                            );
                            black_box(events);
                            next_id += 1;
                        }
                    }
                    total += start.elapsed();
                }
                total
            });
        });
    }

    group.finish();
}

/// Pure insertion throughput: measure how fast we can insert non-crossing limit orders.
fn bench_insert_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput/insert_only");
    let batch: u64 = 100_000;
    group.throughput(Throughput::Elements(batch));

    group.bench_function(format!("ops_{batch}"), |b| {
        b.iter_custom(|iters| {
            let mut total = std::time::Duration::ZERO;
            for _ in 0..iters {
                let mut engine = SymbolEngine::new(
                    Symbol::new(0),
                    make_config(),
                    StpMode::None,
                    make_risk(),
                );

                let start = std::time::Instant::now();
                for i in 0..batch {
                    let side = if i % 2 == 0 { Side::Buy } else { Side::Sell };
                    let price = if side == Side::Buy {
                        5_000_000 - (i as i64 / 2) - 1
                    } else {
                        5_000_001 + (i as i64 / 2)
                    };
                    let order = Order {
                        id: OrderId::new(i + 1),
                        symbol: Symbol::new(0),
                        side,
                        price: Price::new(price),
                        quantity: Quantity::new(100),
                        remaining_qty: Quantity::new(100),
                        order_type: OrderType::Limit,
                        time_in_force: TimeInForce::GTC,
                        timestamp: 0,
                        account_id: 1,
                        prev: NO_LINK,
                        next: NO_LINK,
                    };
                    let events =
                        engine.process_command(Command::PlaceOrder(order), 1, 0);
                    black_box(events);
                }
                total += start.elapsed();
            }
            total
        });
    });

    group.finish();
}

/// Cancel-heavy workload: insert then cancel all orders.
fn bench_insert_cancel(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput/insert_cancel");
    let batch: u64 = 50_000;
    group.throughput(Throughput::Elements(batch * 2)); // insert + cancel = 2 ops each

    group.bench_function(format!("ops_{}", batch * 2), |b| {
        b.iter_custom(|iters| {
            let mut total = std::time::Duration::ZERO;
            for _ in 0..iters {
                let mut engine = SymbolEngine::new(
                    Symbol::new(0),
                    make_config(),
                    StpMode::None,
                    make_risk(),
                );

                let start = std::time::Instant::now();
                // Insert
                for i in 0..batch {
                    let order = Order {
                        id: OrderId::new(i + 1),
                        symbol: Symbol::new(0),
                        side: Side::Buy,
                        price: Price::new(5_000_000 - i as i64),
                        quantity: Quantity::new(100),
                        remaining_qty: Quantity::new(100),
                        order_type: OrderType::Limit,
                        time_in_force: TimeInForce::GTC,
                        timestamp: 0,
                        account_id: 1,
                        prev: NO_LINK,
                        next: NO_LINK,
                    };
                    let events =
                        engine.process_command(Command::PlaceOrder(order), 1, 0);
                    black_box(events);
                }
                // Cancel all
                for i in 0..batch {
                    let events = engine.process_command(
                        Command::CancelOrder(OrderId::new(i + 1)),
                        1,
                        0,
                    );
                    black_box(events);
                }
                total += start.elapsed();
            }
            total
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_mixed_workload,
    bench_insert_only,
    bench_insert_cancel,
);
criterion_main!(benches);
