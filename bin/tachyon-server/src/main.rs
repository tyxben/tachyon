//! Tachyon matching engine server entry point.

mod config;
mod metrics;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use mimalloc::MiMalloc;
use tachyon_core::*;
use tachyon_engine::{RiskConfig, StpMode, SymbolEngine};
use tachyon_gateway::bridge::{EngineBridge, OrderRequest};
use tachyon_gateway::rest::{rest_router, AppState};
use tachyon_gateway::ws::{ws_handler, WsState};
use tokio::sync::RwLock;
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

    // Create WAL writer if persistence is configured
    let wal_writer = if let Some(ref pc) = persist_config {
        match tachyon_persist::WalWriter::new(
            &pc.wal_dir,
            pc.max_wal_size,
            pc.fsync_strategy.clone(),
        ) {
            Ok(w) => {
                tracing::info!(wal_dir = %pc.wal_dir.display(), "WAL writer initialized");
                Some(w)
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

    // Create engine bridge
    let (bridge, mut engine_rx) = EngineBridge::new(4096);

    // Shared state between dispatch loop and REST handlers
    let recent_trades: Arc<RwLock<HashMap<String, Vec<TradeResponse>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let book_snapshots: Arc<RwLock<HashMap<String, OrderBookResponse>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let order_registry: Arc<RwLock<HashMap<u64, Symbol>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Build reverse map: symbol_id -> (symbol_name, config)
    let mut symbol_info: HashMap<u32, (String, SymbolConfig)> = HashMap::new();
    for (name, config) in &symbol_configs {
        if let Some(&sym) = symbol_registry.get(name) {
            symbol_info.insert(sym.raw(), (name.clone(), config.clone()));
        }
    }

    // Spawn engine dispatch loop
    let engine_metrics = metrics.clone();
    let snapshot_dir = persist_config.as_ref().map(|pc| pc.snapshot_dir.clone());
    let snapshot_interval_events = persist_config
        .as_ref()
        .map(|pc| pc.snapshot_interval_events)
        .unwrap_or(0);
    let loop_trades = recent_trades.clone();
    let loop_snapshots = book_snapshots.clone();
    let loop_order_registry = order_registry.clone();

    tokio::spawn(async move {
        engine_dispatch_loop(
            &mut engine_rx,
            &mut engines,
            wal_writer,
            engine_metrics,
            snapshot_dir,
            snapshot_interval_events,
            symbol_info,
            loop_trades,
            loop_snapshots,
            loop_order_registry,
        )
        .await;
    });

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

    // Create WebSocket state
    let ws_state = WsState::new(4096);

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
}

use tachyon_gateway::types::{OrderBookLevel, OrderBookResponse, TradeResponse};

async fn engine_dispatch_loop(
    rx: &mut tokio::sync::mpsc::Receiver<OrderRequest>,
    engines: &mut HashMap<u32, SymbolEngine>,
    mut wal_writer: Option<tachyon_persist::WalWriter>,
    metrics: Arc<Metrics>,
    snapshot_dir: Option<std::path::PathBuf>,
    snapshot_interval_events: u64,
    symbol_info: HashMap<u32, (String, SymbolConfig)>,
    recent_trades: Arc<RwLock<HashMap<String, Vec<TradeResponse>>>>,
    book_snapshots: Arc<RwLock<HashMap<String, OrderBookResponse>>>,
    order_registry: Arc<RwLock<HashMap<u64, Symbol>>>,
) {
    let mut sequence: u64 = 0;
    let mut events_since_snapshot: u64 = 0;
    const MAX_RECENT_TRADES: usize = 500;
    const BOOK_DEPTH: usize = 20;

    while let Some(request) = rx.recv().await {
        let symbol_id = request.symbol.raw();
        let events = if let Some(engine) = engines.get_mut(&symbol_id) {
            let result =
                engine.process_command(request.command, request.account_id, request.timestamp);
            result.to_vec()
        } else {
            vec![EngineEvent::OrderRejected {
                order_id: OrderId::new(0),
                reason: RejectReason::InvalidPrice,
                timestamp: request.timestamp,
            }]
        };

        // Update metrics, trades, and order registry
        let mut has_book_change = false;
        for event in &events {
            match event {
                EngineEvent::OrderAccepted {
                    order_id, symbol, ..
                } => {
                    metrics.orders_total.inc();
                    has_book_change = true;
                    order_registry.write().await.insert(order_id.raw(), *symbol);
                }
                EngineEvent::Trade { trade } => {
                    metrics.trades_total.inc();
                    has_book_change = true;
                    if let Some((name, config)) = symbol_info.get(&symbol_id) {
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
                        let mut trades = recent_trades.write().await;
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
                    order_registry.write().await.remove(&order_id.raw());
                }
                EngineEvent::BookUpdate { .. } => {
                    has_book_change = true;
                }
                _ => {}
            }
        }

        // Update book snapshot if the book changed
        if has_book_change {
            if let Some(engine) = engines.get(&symbol_id) {
                if let Some((name, config)) = symbol_info.get(&symbol_id) {
                    let (bids_raw, asks_raw) = engine.book().get_depth(BOOK_DEPTH);
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
                        bids,
                        asks,
                        timestamp: ts,
                    };
                    book_snapshots.write().await.insert(name.clone(), snapshot);
                }
            }
        }

        // Append events to WAL if persistence is configured
        if let Some(ref mut writer) = wal_writer {
            for event in &events {
                sequence += 1;
                if let Err(e) = writer.append(sequence, event) {
                    tracing::error!(error = %e, sequence, "Failed to write WAL entry");
                }
                events_since_snapshot += 1;
            }

            // Check if we need to take a snapshot
            if snapshot_interval_events > 0 && events_since_snapshot >= snapshot_interval_events {
                if let Some(ref snap_dir) = snapshot_dir {
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

        // Ignore send errors (caller dropped the receiver)
        let _ = request.response_tx.send(events);
    }

    // Graceful shutdown: flush WAL and create final snapshot
    if let Some(ref mut writer) = wal_writer {
        if let Err(e) = writer.flush() {
            tracing::error!(error = %e, "Failed to flush WAL on shutdown");
        } else {
            tracing::info!("WAL flushed on shutdown");
        }

        if let Some(ref snap_dir) = snapshot_dir {
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
                    tracing::info!(path = %path.display(), sequence, "Final snapshot created on shutdown");
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to create final snapshot on shutdown");
                }
            }
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
