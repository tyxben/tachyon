//! Tachyon matching engine server entry point.

mod config;
mod metrics;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use arrayvec::ArrayVec;
use mimalloc::MiMalloc;
use tachyon_core::*;
use tachyon_engine::{RiskConfig, StpMode, SymbolEngine};
use tachyon_gateway::bridge::{EngineBridge, EngineCommand};
use tachyon_gateway::rest::{rest_router, AppState};
use tachyon_gateway::types::{
    OrderBookLevel, OrderBookResponse, TickerResponse, TradeResponse, WsServerMessage,
};
use tachyon_gateway::ws::{ws_handler, WsState};
use tachyon_io::SpscQueue;
use tokio::sync::broadcast;
use tracing_subscriber::prelude::*;

use crate::config::{load_config, LoggingSection};
use crate::metrics::Metrics;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn init_tracing(logging: &LoggingSection) {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&logging.level));

    match logging.format.as_str() {
        "json" => {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(tracing_subscriber::fmt::layer().json())
                .init();
        }
        _ => {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(tracing_subscriber::fmt::layer().pretty())
                .init();
        }
    }
}

#[tokio::main]
async fn main() {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/default.toml".to_string());

    let config = load_config(&config_path).expect("Failed to load config");

    init_tracing(&config.logging);

    // Build symbol registry and engines
    let mut symbol_registry: HashMap<String, Symbol> = HashMap::new();
    let mut symbol_configs: HashMap<String, SymbolConfig> = HashMap::new();
    let mut engines: HashMap<u32, SymbolEngine> = HashMap::new();

    for (idx, (name, section)) in config.symbols.iter().enumerate() {
        let symbol = Symbol::new(idx as u32);
        let sym_config = section
            .to_symbol_config(symbol)
            .unwrap_or_else(|e| panic!("Invalid config for symbol {}: {}", name, e));

        let risk_config = RiskConfig {
            price_band_pct: 0,
            max_order_qty: sym_config.max_order_qty,
        };

        let engine = SymbolEngine::new(symbol, sym_config.clone(), StpMode::None, risk_config);

        symbol_registry.insert(name.clone(), symbol);
        symbol_configs.insert(name.clone(), sym_config);
        engines.insert(symbol.raw(), engine);

        tracing::info!(symbol = %name, id = idx, "Registered symbol");
    }

    // Initialize persistence (optional)
    let persist_config = config.persistence.as_ref().map(|p| {
        p.to_persist_config()
            .unwrap_or_else(|e| panic!("Invalid persistence config: {}", e))
    });

    // Attempt recovery if persistence is configured
    if let Some(ref pc) = persist_config {
        let recovery_mgr = tachyon_persist::RecoveryManager::new(&pc.snapshot_dir, &pc.wal_dir);
        match recovery_mgr.recover() {
            Ok(state) => {
                if state.last_sequence > 0 {
                    tracing::info!(
                        last_sequence = state.last_sequence,
                        events_replayed = state.events_replayed,
                        "Recovery complete"
                    );
                } else {
                    tracing::info!("No previous state to recover");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Recovery failed, starting fresh");
            }
        }
    }

    // Create WAL writer if persistence is configured (shared across engine threads via Mutex)
    let wal_writer = if let Some(ref pc) = persist_config {
        match tachyon_persist::WalWriter::new(
            &pc.wal_dir,
            pc.max_wal_size,
            pc.fsync_strategy.clone(),
        ) {
            Ok(w) => {
                tracing::info!(wal_dir = %pc.wal_dir.display(), "WAL writer initialized");
                Some(Arc::new(std::sync::Mutex::new(w)))
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to create WAL writer");
                None
            }
        }
    } else {
        None
    };

    // Initialize metrics
    let metrics = Arc::new(Metrics::new());

    // Collect symbol IDs for bridge creation
    let symbol_ids: Vec<u32> = engines.keys().copied().collect();

    // Create engine bridge with per-symbol SPSC queues
    let (bridge, engine_queues) = EngineBridge::new(&symbol_ids, 4096);

    // Shared state between engine threads and REST handlers (std::sync::RwLock for sync engine threads)
    let recent_trades: Arc<RwLock<HashMap<String, Vec<TradeResponse>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let book_snapshots: Arc<RwLock<HashMap<String, OrderBookResponse>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let order_registry: Arc<RwLock<HashMap<u64, Symbol>>> = Arc::new(RwLock::new(HashMap::new()));

    // Build reverse map: symbol_id -> (symbol_name, config)
    let mut symbol_info: HashMap<u32, (String, SymbolConfig)> = HashMap::new();
    for (name, config) in &symbol_configs {
        if let Some(&sym) = symbol_registry.get(name) {
            symbol_info.insert(sym.raw(), (name.clone(), config.clone()));
        }
    }

    let snapshot_dir = persist_config.as_ref().map(|pc| pc.snapshot_dir.clone());
    let snapshot_interval_events = persist_config
        .as_ref()
        .map(|pc| pc.snapshot_interval_events)
        .unwrap_or(0);

    // Global shutdown flag for engine threads
    let shutdown = Arc::new(AtomicBool::new(false));

    // Create WebSocket state (broadcast channel for real-time market data push).
    // Must be created before engine threads so they can publish events.
    let ws_state = WsState::new(4096);

    // Get available CPU cores for affinity pinning
    let core_ids = core_affinity::get_core_ids().unwrap_or_default();

    // Spawn a dedicated OS thread per symbol engine
    let mut engine_threads = Vec::new();
    for (idx, (symbol_id, engine)) in engines.into_iter().enumerate() {
        let queue = engine_queues
            .get(&symbol_id)
            .expect("missing SPSC queue for symbol")
            .clone();

        let thread_metrics = metrics.clone();
        let thread_wal = wal_writer.clone();
        let thread_snapshot_dir = snapshot_dir.clone();
        let thread_recent_trades = recent_trades.clone();
        let thread_book_snapshots = book_snapshots.clone();
        let thread_order_registry = order_registry.clone();
        let thread_shutdown = shutdown.clone();
        let thread_symbol_info = symbol_info.get(&symbol_id).cloned();
        let thread_ws_tx = ws_state.event_tx.clone();

        // Pick a core for CPU pinning (round-robin across available cores, skip core 0 for OS)
        let pin_core = if core_ids.len() > 1 {
            Some(core_ids[1 + (idx % (core_ids.len() - 1))])
        } else {
            None
        };

        let handle = std::thread::Builder::new()
            .name(format!("engine-{}", symbol_id))
            .spawn(move || {
                // Pin to CPU core
                if let Some(core_id) = pin_core {
                    if core_affinity::set_for_current(core_id) {
                        tracing::info!(
                            symbol_id,
                            core = core_id.id,
                            "Engine thread pinned to core"
                        );
                    } else {
                        tracing::warn!(
                            symbol_id,
                            core = core_id.id,
                            "Failed to pin engine thread to core"
                        );
                    }
                }

                engine_thread_loop(EngineThreadCtx {
                    engine,
                    queue,
                    metrics: thread_metrics,
                    wal_writer: thread_wal,
                    snapshot_dir: thread_snapshot_dir,
                    snapshot_interval_events,
                    symbol_info: thread_symbol_info,
                    recent_trades: thread_recent_trades,
                    book_snapshots: thread_book_snapshots,
                    order_registry: thread_order_registry,
                    shutdown: thread_shutdown,
                    ws_event_tx: thread_ws_tx,
                });
            })
            .unwrap_or_else(|e| {
                panic!(
                    "Failed to spawn engine thread for symbol {}: {}",
                    symbol_id, e
                )
            });

        engine_threads.push(handle);
    }

    let bridge = Arc::new(bridge);
    let start_time = Instant::now();

    // Create REST app state
    let app_state = AppState {
        bridge: bridge.clone(),
        symbol_registry: Arc::new(symbol_registry),
        symbol_configs: Arc::new(symbol_configs),
        recent_trades,
        book_snapshots,
        order_registry,
        start_time,
    };

    // Build REST router with metrics endpoint
    let metrics_handle = metrics.clone();
    let rest_app = rest_router(app_state).route(
        "/metrics",
        axum::routing::get(move || {
            let m = metrics_handle.clone();
            async move {
                let body = m.encode();
                (
                    axum::http::StatusCode::OK,
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/plain; version=0.0.4",
                    )],
                    body,
                )
            }
        }),
    );

    // Build WebSocket router
    let ws_app = axum::Router::new()
        .route("/ws", axum::routing::get(ws_handler))
        .with_state(ws_state);

    // Bind REST server
    let rest_addr = format!("{}:{}", config.server.bind_address, config.server.rest_port);
    let rest_listener = tokio::net::TcpListener::bind(&rest_addr)
        .await
        .unwrap_or_else(|e| panic!("Failed to bind REST on {}: {}", rest_addr, e));
    tracing::info!(address = %rest_addr, "REST server listening");

    // Bind WebSocket server
    let ws_addr = format!(
        "{}:{}",
        config.server.bind_address, config.server.websocket_port
    );
    let ws_listener = tokio::net::TcpListener::bind(&ws_addr)
        .await
        .unwrap_or_else(|e| panic!("Failed to bind WebSocket on {}: {}", ws_addr, e));
    tracing::info!(address = %ws_addr, "WebSocket server listening");

    tracing::info!("Tachyon engine ready");

    // Run both servers and wait for shutdown signal
    tokio::select! {
        result = axum::serve(rest_listener, rest_app) => {
            if let Err(e) = result {
                tracing::error!(error = %e, "REST server error");
            }
        }
        result = axum::serve(ws_listener, ws_app) => {
            if let Err(e) = result {
                tracing::error!(error = %e, "WebSocket server error");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Shutting down...");
        }
    }

    // Signal engine threads to stop and wait for them
    shutdown.store(true, Ordering::Release);
    for handle in engine_threads {
        let _ = handle.join();
    }

    // Graceful shutdown: flush WAL
    if let Some(ref wal) = wal_writer {
        if let Ok(ref mut writer) = wal.lock() {
            if let Err(e) = writer.flush() {
                tracing::error!(error = %e, "Failed to flush WAL on shutdown");
            } else {
                tracing::info!("WAL flushed on shutdown");
            }
        }
    }
}

/// Per-symbol engine thread context.
struct EngineThreadCtx {
    engine: SymbolEngine,
    queue: Arc<SpscQueue<EngineCommand>>,
    metrics: Arc<Metrics>,
    wal_writer: Option<Arc<std::sync::Mutex<tachyon_persist::WalWriter>>>,
    snapshot_dir: Option<std::path::PathBuf>,
    snapshot_interval_events: u64,
    symbol_info: Option<(String, SymbolConfig)>,
    recent_trades: Arc<RwLock<HashMap<String, Vec<TradeResponse>>>>,
    book_snapshots: Arc<RwLock<HashMap<String, OrderBookResponse>>>,
    order_registry: Arc<RwLock<HashMap<u64, Symbol>>>,
    shutdown: Arc<AtomicBool>,
    /// Broadcast sender for real-time WebSocket market data push.
    ws_event_tx: broadcast::Sender<WsServerMessage>,
}

/// Maximum number of commands to drain per batch iteration.
const BATCH_SIZE: usize = 64;
const MAX_RECENT_TRADES: usize = 500;
const BOOK_DEPTH: usize = 20;

/// Tight poll loop for a single symbol engine thread.
fn engine_thread_loop(mut ctx: EngineThreadCtx) {
    let mut sequence: u64 = 0;
    let mut events_since_snapshot: u64 = 0;
    let mut spin_count: u32 = 0;

    loop {
        let mut batch = ArrayVec::<EngineCommand, BATCH_SIZE>::new();
        ctx.queue.drain_batch(&mut batch);

        if batch.is_empty() {
            if ctx.shutdown.load(Ordering::Acquire) {
                break;
            }
            // Progressive backoff: spin briefly, then yield to OS
            spin_count += 1;
            if spin_count < 1000 {
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
                spin_count = 0;
            }
            continue;
        }

        spin_count = 0;

        for cmd in batch {
            let events = {
                let result = ctx
                    .engine
                    .process_command(cmd.command, cmd.account_id, cmd.timestamp);
                result.to_vec()
            };

            // Update metrics, trades, order registry, and push WS market data
            let mut has_book_change = false;
            let mut last_trade_price: Option<(String, String)> = None; // (price, qty)
            for event in &events {
                match event {
                    EngineEvent::OrderAccepted {
                        order_id, symbol, ..
                    } => {
                        ctx.metrics.orders_total.inc();
                        has_book_change = true;
                        ctx.order_registry
                            .write()
                            .unwrap()
                            .insert(order_id.raw(), *symbol);
                    }
                    EngineEvent::Trade { trade } => {
                        ctx.metrics.trades_total.inc();
                        has_book_change = true;
                        if let Some((ref name, ref config)) = ctx.symbol_info {
                            let trade_resp = TradeResponse {
                                trade_id: trade.trade_id,
                                symbol: name.clone(),
                                price: trade.price.format_with_scale(config.price_scale),
                                quantity: format_qty_raw(trade.quantity.raw(), config.qty_scale),
                                maker_side: match trade.maker_side {
                                    Side::Buy => "buy".to_string(),
                                    Side::Sell => "sell".to_string(),
                                },
                                timestamp: trade.timestamp,
                            };

                            // Track last trade price for ticker update
                            last_trade_price =
                                Some((trade_resp.price.clone(), trade_resp.quantity.clone()));

                            // Publish trade to WebSocket broadcast (ignore error if no subscribers)
                            let _ = ctx.ws_event_tx.send(WsServerMessage::Trade {
                                data: trade_resp.clone(),
                            });

                            let mut trades = ctx.recent_trades.write().unwrap();
                            let list = trades.entry(name.clone()).or_default();
                            list.push(trade_resp);
                            if list.len() > MAX_RECENT_TRADES {
                                let drain_count = list.len() - MAX_RECENT_TRADES;
                                list.drain(..drain_count);
                            }
                        }
                    }
                    EngineEvent::OrderCancelled { order_id, .. } => {
                        has_book_change = true;
                        ctx.order_registry.write().unwrap().remove(&order_id.raw());
                    }
                    EngineEvent::BookUpdate { .. } => {
                        has_book_change = true;
                    }
                    _ => {}
                }
            }

            // Update book snapshot and push depth + ticker to WebSocket if the book changed
            if has_book_change {
                if let Some((ref name, ref config)) = ctx.symbol_info {
                    let (bids_raw, asks_raw) = ctx.engine.book().get_depth(BOOK_DEPTH);
                    let bids: Vec<OrderBookLevel> = bids_raw
                        .into_iter()
                        .map(|(price, qty)| OrderBookLevel {
                            price: price.format_with_scale(config.price_scale),
                            quantity: format_qty_raw(qty.raw(), config.qty_scale),
                        })
                        .collect();
                    let asks: Vec<OrderBookLevel> = asks_raw
                        .into_iter()
                        .map(|(price, qty)| OrderBookLevel {
                            price: price.format_with_scale(config.price_scale),
                            quantity: format_qty_raw(qty.raw(), config.qty_scale),
                        })
                        .collect();
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    let snapshot = OrderBookResponse {
                        symbol: name.clone(),
                        bids: bids.clone(),
                        asks: asks.clone(),
                        timestamp: ts,
                    };

                    // Publish depth snapshot to WebSocket broadcast
                    let _ = ctx.ws_event_tx.send(WsServerMessage::Depth {
                        data: snapshot.clone(),
                    });

                    // Publish ticker update to WebSocket broadcast
                    let best_bid = bids.first();
                    let best_ask = asks.first();
                    let ticker = TickerResponse {
                        symbol: name.clone(),
                        best_bid: best_bid.map(|l| l.price.clone()),
                        best_bid_qty: best_bid.map(|l| l.quantity.clone()),
                        best_ask: best_ask.map(|l| l.price.clone()),
                        best_ask_qty: best_ask.map(|l| l.quantity.clone()),
                        last_price: last_trade_price.as_ref().map(|(p, _)| p.clone()),
                        last_qty: last_trade_price.as_ref().map(|(_, q)| q.clone()),
                        timestamp: ts,
                    };
                    let _ = ctx
                        .ws_event_tx
                        .send(WsServerMessage::Ticker { data: ticker });

                    ctx.book_snapshots
                        .write()
                        .unwrap()
                        .insert(name.clone(), snapshot);
                }
            }

            // Append events to WAL if persistence is configured
            if let Some(ref wal) = ctx.wal_writer {
                if let Ok(ref mut writer) = wal.lock() {
                    for event in &events {
                        sequence += 1;
                        if let Err(e) = writer.append(sequence, event) {
                            tracing::error!(error = %e, sequence, "Failed to write WAL entry");
                        }
                        events_since_snapshot += 1;
                    }

                    if ctx.snapshot_interval_events > 0
                        && events_since_snapshot >= ctx.snapshot_interval_events
                    {
                        if let Some(ref snap_dir) = ctx.snapshot_dir {
                            let snapshot = tachyon_persist::Snapshot {
                                sequence,
                                timestamp: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_nanos() as u64)
                                    .unwrap_or(0),
                                symbols: Vec::new(),
                            };
                            match tachyon_persist::SnapshotWriter::write(snap_dir, &snapshot) {
                                Ok(path) => {
                                    tracing::info!(path = %path.display(), sequence, "Snapshot created");
                                    events_since_snapshot = 0;
                                }
                                Err(e) => {
                                    tracing::error!(error = %e, "Failed to create snapshot");
                                }
                            }
                        }
                    }
                }
            }

            // Send response back to the gateway (ignore errors if caller dropped)
            let _ = cmd.response_tx.send(events);
        }
    }
}

fn format_qty_raw(raw: u64, scale: u8) -> String {
    if scale == 0 {
        return raw.to_string();
    }
    let divisor = 10u64.pow(scale as u32);
    let whole = raw / divisor;
    let frac = raw % divisor;
    format!("{whole}.{frac:0>width$}", width = scale as usize)
}
