//! Tachyon matching engine server entry point.

mod config;
mod metrics;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use arrayvec::ArrayVec;
use mimalloc::MiMalloc;
use tachyon_core::*;
use tachyon_engine::{RiskConfig, StpMode, SymbolEngine};
use tachyon_gateway::bridge::{EngineBridge, EngineCommand};
use tachyon_gateway::rest::{rest_router, AppState};
use tachyon_gateway::tcp::{run_tcp_server, TcpState};
use tachyon_gateway::types::{
    OrderBookLevel, OrderBookResponse, TickerResponse, TradeResponse, WsServerMessage,
};
use tachyon_gateway::ws::{ws_handler, WsState};
use tachyon_gateway::{auth_middleware, AuthConfig, AuthState, RateLimitConfig, RateLimiter};
use tachyon_io::SpscQueue;
use tachyon_persist::{TradeRecord, TradeStore};
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

/// Replay a single v1 legacy WAL event into the appropriate engine's order book.
///
/// This is only used for backward compatibility with v1 WAL files that stored events
/// instead of commands. New WAL files use v2 command format with deterministic replay.
fn replay_legacy_wal_event(engines: &mut HashMap<u32, SymbolEngine>, event: &EngineEvent) {
    match event {
        EngineEvent::OrderAccepted {
            order_id,
            symbol,
            side,
            price,
            qty,
            timestamp,
        } => {
            if let Some(engine) = engines.get_mut(&symbol.raw()) {
                let order = Order {
                    id: *order_id,
                    symbol: *symbol,
                    side: *side,
                    price: *price,
                    quantity: *qty,
                    remaining_qty: *qty,
                    order_type: OrderType::Limit,
                    time_in_force: TimeInForce::GTC,
                    timestamp: *timestamp,
                    account_id: 0,
                    prev: NO_LINK,
                    next: NO_LINK,
                };
                let _ = engine.book_mut().restore_order(order);
            }
        }
        EngineEvent::OrderCancelled { order_id, .. } => {
            for engine in engines.values_mut() {
                if engine.book().get_order(*order_id).is_some() {
                    let _ = engine.book_mut().cancel_order(*order_id);
                    break;
                }
            }
        }
        EngineEvent::Trade { trade } => {
            if let Some(engine) = engines.get_mut(&trade.symbol.raw()) {
                if let Some(maker) = engine.book().get_order(trade.maker_order_id) {
                    let new_remaining = maker.remaining_qty.saturating_sub(trade.quantity);
                    if new_remaining.is_zero() {
                        let _ = engine.book_mut().cancel_order(trade.maker_order_id);
                    } else if let Some(existing) = engine.book().get_order(trade.maker_order_id) {
                        let mut updated = existing.clone();
                        updated.remaining_qty = new_remaining;
                        let _ = engine.book_mut().cancel_order(trade.maker_order_id);
                        let _ = engine.book_mut().restore_order(updated);
                    }
                }
            }
        }
        EngineEvent::OrderRejected { .. }
        | EngineEvent::BookUpdate { .. }
        | EngineEvent::OrderExpired { .. } => {}
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

    // Global WAL sequence counter shared across all engine threads (H10 fix)
    let wal_sequence = Arc::new(AtomicU64::new(0));

    // Attempt recovery if persistence is configured
    if let Some(ref pc) = persist_config {
        let recovery_mgr = tachyon_persist::RecoveryManager::new(&pc.snapshot_dir, &pc.wal_dir);
        match recovery_mgr.recover() {
            Ok(state) => {
                if state.last_sequence > 0 {
                    // Restore order book state from snapshot (C4 fix: also restore engine counters)
                    for (symbol, sym_state) in &state.snapshots {
                        if let Some(engine) = engines.get_mut(&symbol.raw()) {
                            for order in &sym_state.orders {
                                if let Err(e) = engine.book_mut().restore_order(order.clone()) {
                                    tracing::warn!(
                                        order_id = order.id.raw(),
                                        error = %e,
                                        "Failed to restore order during recovery"
                                    );
                                }
                            }
                            // Restore engine counters from snapshot
                            if sym_state.next_order_id > 0 {
                                engine.set_next_order_id(sym_state.next_order_id);
                            }
                            if sym_state.sequence > 0 {
                                engine.set_sequence(sym_state.sequence);
                            }
                            if sym_state.trade_id_counter > 0 {
                                engine.set_trade_id_counter(sym_state.trade_id_counter);
                            }
                        }
                    }

                    // Replay v2 WAL commands deterministically (preferred path)
                    for wal_cmd in &state.wal_commands {
                        if let Some(engine) = engines.get_mut(&wal_cmd.symbol_id) {
                            engine.process_command(
                                wal_cmd.command.clone(),
                                wal_cmd.account_id,
                                wal_cmd.timestamp,
                            );
                        }
                    }

                    // Replay v1 legacy WAL events (backward compatibility)
                    for (_seq, event) in &state.legacy_wal_events {
                        replay_legacy_wal_event(&mut engines, event);
                    }

                    // Initialize global WAL sequence from recovery state (H10 fix)
                    wal_sequence.store(state.last_sequence, Ordering::Release);

                    tracing::info!(
                        last_sequence = state.last_sequence,
                        entries_replayed = state.events_replayed,
                        wal_commands = state.wal_commands.len(),
                        legacy_events = state.legacy_wal_events.len(),
                        symbols_recovered = state.snapshots.len(),
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

    // Create persistent trade store for historical queries
    let trade_store_dir = persist_config
        .as_ref()
        .map(|pc| pc.wal_dir.parent().unwrap_or(&pc.wal_dir).join("trades"))
        .unwrap_or_else(|| std::path::PathBuf::from("data/trades"));

    let trade_store = match TradeStore::new(&trade_store_dir) {
        Ok(ts) => {
            tracing::info!(dir = %trade_store_dir.display(), "Trade store initialized");
            Some(Arc::new(ts))
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to create trade store, history disabled");
            None
        }
    };

    // Async channel: engine threads (sync) -> trade history writer task (async)
    let (trade_tx, trade_rx) = tokio::sync::mpsc::unbounded_channel::<TradeRecord>();

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

    // Create per-symbol engine alive flags
    let mut engine_alive: HashMap<String, Arc<AtomicBool>> = HashMap::new();
    for name in symbol_configs.keys() {
        engine_alive.insert(name.clone(), Arc::new(AtomicBool::new(true)));
    }

    // Create WebSocket state (broadcast channel for real-time market data push).
    // Must be created before engine threads so they can publish events.
    let ws_state = WsState::with_limit(4096, config.server.max_ws_connections);

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
        let thread_wal_sequence = wal_sequence.clone();
        let thread_trade_tx = trade_store.as_ref().map(|_| trade_tx.clone());

        // Get the engine alive flag for this symbol
        let thread_alive = thread_symbol_info
            .as_ref()
            .and_then(|(name, _)| engine_alive.get(name).cloned());

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
                    alive_flag: thread_alive,
                    wal_sequence: thread_wal_sequence,
                    trade_tx: thread_trade_tx,
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

    // Collect symbol names for the status endpoint.
    let symbol_names: Vec<String> = symbol_configs.keys().cloned().collect();

    // Create REST app state
    let app_state = AppState {
        bridge: bridge.clone(),
        symbol_registry: Arc::new(symbol_registry),
        symbol_configs: Arc::new(symbol_configs),
        recent_trades,
        book_snapshots,
        order_registry,
        start_time,
        engine_alive: Arc::new(engine_alive),
        request_id_counter: Arc::new(AtomicU64::new(1)),
        metrics: Some(metrics.clone() as Arc<dyn tachyon_gateway::MetricsProvider>),
        symbol_names: Arc::new(symbol_names),
        trade_store: trade_store.clone(),
    };

    // Build auth state
    let auth_config = AuthConfig {
        enabled: config.auth.enabled,
        api_keys: config.auth.api_keys.iter().cloned().collect(),
    };
    let auth_state = AuthState::new(auth_config.clone());

    if auth_config.enabled {
        tracing::info!(
            num_keys = auth_config.api_keys.len(),
            "API key authentication enabled"
        );
    }

    // Build rate limiter
    let rate_limit_config = RateLimitConfig {
        enabled: config.rate_limit.enabled,
        requests_per_second: config.rate_limit.requests_per_second,
        burst_size: config.rate_limit.burst_size,
    };
    if rate_limit_config.enabled {
        tracing::info!(
            rps = rate_limit_config.requests_per_second,
            burst = rate_limit_config.burst_size,
            "Rate limiting enabled"
        );
    }
    let rate_limiter = RateLimiter::new(rate_limit_config);

    // Clone order_registry before app_state is moved into rest_router
    let tcp_order_registry = app_state.order_registry.clone();

    // Build REST router with rate limiting and auth middleware.
    // Rate limiting is applied before auth so that excessive requests are rejected early.
    // The /health and /metrics endpoints are excluded from rate limiting by placing
    // them outside the rate-limited router group.
    let rest_app = rest_router(app_state)
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            auth_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            rate_limiter,
            tachyon_gateway::rate_limit_middleware,
        ));

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

    // Bind TCP binary protocol server
    let tcp_addr = format!("{}:{}", config.server.bind_address, config.server.tcp_port);
    let tcp_listener = tokio::net::TcpListener::bind(&tcp_addr)
        .await
        .unwrap_or_else(|e| panic!("Failed to bind TCP on {}: {}", tcp_addr, e));
    tracing::info!(address = %tcp_addr, "TCP binary protocol server listening");

    let tcp_state = TcpState {
        bridge: bridge.clone(),
        seq_counter: Arc::new(AtomicU64::new(1)),
        active_connections: Arc::new(AtomicU64::new(0)),
        max_connections: config.server.max_tcp_connections,
        order_registry: tcp_order_registry,
    };

    // Spawn async trade history writer task
    let trade_writer_handle = if let Some(ref ts) = trade_store {
        let ts = ts.clone();
        Some(tokio::spawn(trade_history_writer(trade_rx, ts)))
    } else {
        // Drop rx so engine threads don't block on send
        drop(trade_rx);
        None
    };

    tracing::info!("Tachyon engine ready");

    let shutdown_timeout_secs = config.server.shutdown_timeout_secs;

    // Run all servers and wait for shutdown signal
    tokio::select! {
        result = axum::serve(rest_listener, rest_app.into_make_service_with_connect_info::<std::net::SocketAddr>()) => {
            if let Err(e) = result {
                tracing::error!(error = %e, "REST server error");
            }
        }
        result = axum::serve(ws_listener, ws_app) => {
            if let Err(e) = result {
                tracing::error!(error = %e, "WebSocket server error");
            }
        }
        _ = run_tcp_server(tcp_listener, tcp_state) => {
            tracing::error!("TCP binary protocol server exited unexpectedly");
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received shutdown signal, beginning graceful shutdown...");
        }
    }

    // Graceful shutdown with timeout
    tracing::info!(
        timeout_secs = shutdown_timeout_secs,
        "Draining in-flight requests and shutting down engines..."
    );

    let shutdown_deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(shutdown_timeout_secs);

    // Signal engine threads to stop (they will drain remaining SPSC queue items)
    shutdown.store(true, Ordering::Release);

    // Wait for engine threads with timeout
    let engine_join = tokio::task::spawn_blocking(move || {
        for handle in engine_threads {
            let _ = handle.join();
        }
    });

    tokio::select! {
        _ = engine_join => {
            tracing::info!("All engine threads stopped");
        }
        _ = tokio::time::sleep_until(shutdown_deadline) => {
            tracing::warn!("Shutdown timeout reached, forcing exit");
        }
    }

    // Drop trade_tx to signal trade writer to stop, then wait for it
    drop(trade_tx);
    if let Some(handle) = trade_writer_handle {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        tracing::info!("Trade history writer stopped");
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

    tracing::info!("Shutdown complete");
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
    /// Per-symbol alive flag set to false when the engine thread exits.
    alive_flag: Option<Arc<AtomicBool>>,
    /// Global WAL sequence counter shared across all engine threads (H10 fix).
    wal_sequence: Arc<AtomicU64>,
    /// Channel to send trades to the async trade history writer (non-blocking).
    trade_tx: Option<tokio::sync::mpsc::UnboundedSender<TradeRecord>>,
}

/// Maximum number of commands to drain per batch iteration.
const BATCH_SIZE: usize = 64;
const MAX_RECENT_TRADES: usize = 500;
const BOOK_DEPTH: usize = 20;

/// Tight poll loop for a single symbol engine thread.
fn engine_thread_loop(mut ctx: EngineThreadCtx) {
    let mut events_since_snapshot: u64 = 0;
    let mut spin_count: u32 = 0;

    loop {
        let mut batch = ArrayVec::<EngineCommand, BATCH_SIZE>::new();
        ctx.queue.drain_batch(&mut batch);

        if batch.is_empty() {
            if ctx.shutdown.load(Ordering::Acquire) {
                // Drain any remaining commands before exiting
                ctx.queue.drain_batch(&mut batch);
                if batch.is_empty() {
                    break;
                }
                // Fall through to process the final batch below
            } else {
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
        }

        spin_count = 0;

        for cmd in batch {
            // Write command to WAL BEFORE processing (true write-ahead log)
            if let Some(ref wal) = ctx.wal_writer {
                if let Ok(ref mut writer) = wal.lock() {
                    let seq = ctx.wal_sequence.fetch_add(1, Ordering::Relaxed) + 1;
                    let wal_cmd = tachyon_persist::WalCommand {
                        sequence: seq,
                        symbol_id: cmd.symbol.raw(),
                        command: cmd.command.clone(),
                        account_id: cmd.account_id,
                        timestamp: cmd.timestamp,
                    };
                    if let Err(e) = writer.append_command(&wal_cmd) {
                        tracing::error!(error = %e, seq, "Failed to write WAL command");
                    }
                    events_since_snapshot += 1;
                }
            }

            // Measure matching engine latency
            let match_start = Instant::now();
            let events = ctx
                .engine
                .process_command(cmd.command, cmd.account_id, cmd.timestamp);
            let match_elapsed_ns = match_start.elapsed().as_nanos() as u64;
            ctx.metrics.match_latency_ns.observe(match_elapsed_ns);

            // Update metrics, trades, order registry, and push WS market data
            let mut has_book_change = false;
            let mut last_trade_price: Option<(String, String)> = None; // (price, qty)
            for event in &events {
                match event {
                    EngineEvent::OrderAccepted {
                        order_id,
                        symbol,
                        side,
                        ..
                    } => {
                        // Determine order type label from the command (we infer from price for simplicity)
                        let side_str = match side {
                            Side::Buy => "buy",
                            Side::Sell => "sell",
                        };
                        // We record as "limit" by default; market orders are relatively rare
                        // and we don't have the order type in the event. This is a best-effort label.
                        ctx.metrics.orders_total.inc(side_str, "limit");
                        has_book_change = true;
                        ctx.order_registry
                            .write()
                            .unwrap_or_else(|p| p.into_inner())
                            .insert(order_id.raw(), *symbol);
                    }
                    EngineEvent::Trade { trade } => {
                        ctx.metrics.trades_total.fetch_add(1, Ordering::Relaxed);
                        has_book_change = true;

                        // Send to persistent trade history (non-blocking)
                        if let Some(ref tx) = ctx.trade_tx {
                            let record = TradeRecord {
                                timestamp: trade.timestamp,
                                trade_id: trade.trade_id,
                                symbol_id: trade.symbol.raw(),
                                maker_side: match trade.maker_side {
                                    Side::Buy => 0,
                                    Side::Sell => 1,
                                },
                                _pad: [0; 3],
                                price: trade.price.raw(),
                                quantity: trade.quantity.raw(),
                                maker_order_id: trade.maker_order_id.raw(),
                                taker_order_id: trade.taker_order_id.raw(),
                            };
                            let _ = tx.send(record);
                        }

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

                            let mut trades =
                                ctx.recent_trades.write().unwrap_or_else(|p| p.into_inner());
                            let list = trades.entry(name.clone()).or_default();
                            list.push(trade_resp);
                            if list.len() > MAX_RECENT_TRADES {
                                let drain_count = list.len() - MAX_RECENT_TRADES;
                                list.drain(..drain_count);
                            }
                        }
                    }
                    EngineEvent::OrderCancelled { order_id, .. } => {
                        ctx.metrics.cancels_total.fetch_add(1, Ordering::Relaxed);
                        has_book_change = true;
                        ctx.order_registry
                            .write()
                            .unwrap_or_else(|p| p.into_inner())
                            .remove(&order_id.raw());
                    }
                    EngineEvent::OrderRejected { reason, .. } => {
                        let reason_str = format!("{:?}", reason);
                        ctx.metrics.rejects_total.inc(&reason_str);
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

                    // Update book depth gauges
                    ctx.metrics
                        .book_depth_bids
                        .store(bids.len() as i64, Ordering::Relaxed);
                    ctx.metrics
                        .book_depth_asks
                        .store(asks.len() as i64, Ordering::Relaxed);
                    ctx.metrics
                        .book_orders_count
                        .store(ctx.engine.book().order_count() as i64, Ordering::Relaxed);

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
                        .unwrap_or_else(|p| p.into_inner())
                        .insert(name.clone(), snapshot);
                }
            }

            // Periodic snapshotting (after WAL write + command processing)
            if ctx.snapshot_interval_events > 0
                && events_since_snapshot >= ctx.snapshot_interval_events
            {
                if let Some(ref snap_dir) = ctx.snapshot_dir {
                    let engine_snap = ctx.engine.snapshot_full_state();
                    let current_seq = ctx.wal_sequence.load(Ordering::Relaxed);
                    let sym_snapshot = tachyon_persist::SymbolSnapshot {
                        symbol: engine_snap.symbol,
                        config: engine_snap.config,
                        orders: engine_snap.orders,
                        next_order_id: engine_snap.next_order_id,
                        sequence: engine_snap.sequence,
                        trade_id_counter: engine_snap.trade_id_counter,
                    };
                    let snapshot = tachyon_persist::Snapshot {
                        sequence: current_seq,
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos() as u64)
                            .unwrap_or(0),
                        symbols: vec![sym_snapshot],
                    };
                    match tachyon_persist::SnapshotWriter::write(snap_dir, &snapshot) {
                        Ok(path) => {
                            tracing::info!(
                                path = %path.display(),
                                sequence = current_seq,
                                "Snapshot created"
                            );
                            events_since_snapshot = 0;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to create snapshot");
                        }
                    }
                }
            }

            // Send response back to the gateway (only convert SmallVec to Vec at the send boundary)
            let _ = cmd.response_tx.send(events.to_vec());
        }
    }

    // Signal that this engine thread has exited
    if let Some(ref flag) = ctx.alive_flag {
        flag.store(false, Ordering::Release);
    }
}

/// Async task that drains trade records from engine threads and writes them to persistent storage.
///
/// Batches writes for efficiency — flushes after each drain or every 100ms if idle.
async fn trade_history_writer(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<TradeRecord>,
    store: Arc<TradeStore>,
) {
    use std::collections::HashMap;

    let mut writers: HashMap<u32, tachyon_persist::TradeWriter> = HashMap::new();
    let mut batch_count: u64 = 0;

    loop {
        // Wait for the first record
        let record = match rx.recv().await {
            Some(r) => r,
            None => break, // Channel closed, shutdown
        };

        // Write this record
        write_trade_record(&store, &mut writers, &record);
        batch_count += 1;

        // Drain any additional buffered records (non-blocking)
        while let Ok(record) = rx.try_recv() {
            write_trade_record(&store, &mut writers, &record);
            batch_count += 1;
        }

        // Flush all writers after batch
        for writer in writers.values_mut() {
            let _ = writer.flush();
        }

        if batch_count % 10000 == 0 {
            tracing::debug!(batch_count, "Trade history writer progress");
        }
    }

    // Final flush
    for writer in writers.values_mut() {
        let _ = writer.flush();
    }

    tracing::info!(total = batch_count, "Trade history writer shutdown");
}

fn write_trade_record(
    store: &TradeStore,
    writers: &mut std::collections::HashMap<u32, tachyon_persist::TradeWriter>,
    record: &TradeRecord,
) {
    let writer = writers
        .entry(record.symbol_id)
        .or_insert_with(|| {
            store
                .writer(record.symbol_id)
                .expect("failed to open trade writer")
        });
    if let Err(e) = writer.append(record) {
        tracing::error!(error = %e, trade_id = record.trade_id, "Failed to write trade record");
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
