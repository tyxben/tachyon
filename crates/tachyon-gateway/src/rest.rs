use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use std::sync::RwLock;

use tachyon_core::*;
use tachyon_engine::Command;

use crate::bridge::EngineBridge;
use crate::types::*;
use crate::MetricsProvider;

/// Shared application state for all REST handlers.
#[derive(Clone)]
pub struct AppState {
    pub bridge: Arc<EngineBridge>,
    pub symbol_registry: Arc<HashMap<String, Symbol>>,
    pub symbol_configs: Arc<HashMap<String, SymbolConfig>>,
    pub recent_trades: Arc<RwLock<HashMap<String, Vec<TradeResponse>>>>,
    /// Live order book snapshots updated by the engine dispatch loop.
    pub book_snapshots: Arc<RwLock<HashMap<String, OrderBookResponse>>>,
    /// Maps order_id -> symbol for cancel routing.
    pub order_registry: Arc<RwLock<HashMap<u64, Symbol>>>,
    pub start_time: Instant,
    /// Per-symbol engine alive flags (set to false if engine thread exits).
    pub engine_alive: Arc<HashMap<String, Arc<AtomicBool>>>,
    /// Global request ID counter for tracing.
    pub request_id_counter: Arc<AtomicU64>,
    /// Metrics provider for `/metrics` and `/api/v1/metrics` endpoints.
    pub metrics: Option<Arc<dyn MetricsProvider>>,
    /// List of configured symbol names for the status endpoint.
    pub symbol_names: Arc<Vec<String>>,
}

/// Maximum number of recent trades returned per query.
const MAX_RECENT_TRADES: usize = 100;

/// Maximum request body size (1 MB).
const MAX_BODY_SIZE: usize = 1024 * 1024;

/// Creates the REST API router.
pub fn rest_router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/orderbook/:symbol", get(get_orderbook))
        .route("/api/v1/trades/:symbol", get(get_trades))
        .route("/api/v1/order", post(place_order))
        .route("/api/v1/order/:id", delete(cancel_order))
        .route("/api/v1/symbols", get(list_symbols))
        .route("/api/v1/metrics", get(get_metrics_json))
        .route("/api/v1/status", get(get_status))
        .route("/metrics", get(get_metrics_prometheus))
        .route("/health", get(health_check))
        .layer(DefaultBodyLimit::max(MAX_BODY_SIZE))
        .with_state(state)
}

/// Place a new order.
async fn place_order(
    State(state): State<AppState>,
    Json(req): Json<PlaceOrderRequest>,
) -> impl IntoResponse {
    // Generate request ID for tracing
    let request_id = state.request_id_counter.fetch_add(1, Ordering::Relaxed);

    // Resolve symbol
    let symbol = match state.symbol_registry.get(&req.symbol) {
        Some(s) => *s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                [("x-request-id", request_id.to_string())],
                Json(serde_json::json!({"error": "unknown symbol", "request_id": request_id})),
            )
                .into_response();
        }
    };

    let config = match state.symbol_configs.get(&req.symbol) {
        Some(c) => c,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("x-request-id", request_id.to_string())],
                Json(serde_json::json!({"error": "symbol config not found"})),
            )
                .into_response();
        }
    };

    // Parse side
    let side = match req.side.as_str() {
        "buy" => Side::Buy,
        "sell" => Side::Sell,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                [("x-request-id", request_id.to_string())],
                Json(serde_json::json!({"error": "invalid side, expected 'buy' or 'sell'"})),
            )
                .into_response();
        }
    };

    // Parse order type
    let order_type = match req.order_type.as_str() {
        "limit" => OrderType::Limit,
        "market" => OrderType::Market,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                [("x-request-id", request_id.to_string())],
                Json(serde_json::json!({"error": "invalid order_type, expected 'limit' or 'market'"})),
            )
                .into_response();
        }
    };

    // Parse time-in-force
    let time_in_force = match req.time_in_force.as_str() {
        "GTC" => TimeInForce::GTC,
        "IOC" => TimeInForce::IOC,
        "FOK" => TimeInForce::FOK,
        "PostOnly" => TimeInForce::PostOnly,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                [("x-request-id", request_id.to_string())],
                Json(serde_json::json!({"error": "invalid time_in_force"})),
            )
                .into_response();
        }
    };

    // Parse quantity
    let qty_raw = match parse_scaled_value(&req.quantity, config.qty_scale) {
        Some(0) => {
            return (
                StatusCode::BAD_REQUEST,
                [("x-request-id", request_id.to_string())],
                Json(serde_json::json!({"error": "quantity must be greater than zero"})),
            )
                .into_response();
        }
        Some(v) => v,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                [("x-request-id", request_id.to_string())],
                Json(serde_json::json!({"error": "invalid quantity"})),
            )
                .into_response();
        }
    };

    // Validate quantity against symbol limits
    let qty = Quantity::new(qty_raw);
    if qty < config.min_order_qty {
        return (
            StatusCode::BAD_REQUEST,
            [("x-request-id", request_id.to_string())],
            Json(serde_json::json!({
                "error": format!("quantity below minimum ({})", format_qty(config.min_order_qty, config.qty_scale))
            })),
        )
            .into_response();
    }
    if qty > config.max_order_qty {
        return (
            StatusCode::BAD_REQUEST,
            [("x-request-id", request_id.to_string())],
            Json(serde_json::json!({
                "error": format!("quantity exceeds maximum ({})", format_qty(config.max_order_qty, config.qty_scale))
            })),
        )
            .into_response();
    }

    // Parse price
    let price_raw = if order_type == OrderType::Market {
        match side {
            Side::Buy => i64::MAX,
            Side::Sell => 1,
        }
    } else {
        match &req.price {
            Some(p) => match parse_scaled_value(p, config.price_scale) {
                Some(0) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        [("x-request-id", request_id.to_string())],
                        Json(serde_json::json!({"error": "price must be greater than zero"})),
                    )
                        .into_response();
                }
                Some(v) => match i64::try_from(v) {
                    Ok(v) => {
                        // Validate price against symbol limits
                        let price = Price::new(v);
                        if price < config.min_price {
                            return (
                                StatusCode::BAD_REQUEST,
                                [("x-request-id", request_id.to_string())],
                                Json(serde_json::json!({
                                    "error": format!("price below minimum ({})", format_price(config.min_price, config.price_scale))
                                })),
                            )
                                .into_response();
                        }
                        if price > config.max_price {
                            return (
                                StatusCode::BAD_REQUEST,
                                [("x-request-id", request_id.to_string())],
                                Json(serde_json::json!({
                                    "error": format!("price exceeds maximum ({})", format_price(config.max_price, config.price_scale))
                                })),
                            )
                                .into_response();
                        }
                        v
                    }
                    Err(_) => {
                        return (
                            StatusCode::BAD_REQUEST,
                            [("x-request-id", request_id.to_string())],
                            Json(serde_json::json!({"error": "price value overflow"})),
                        )
                            .into_response();
                    }
                },
                None => {
                    return (
                        StatusCode::BAD_REQUEST,
                        [("x-request-id", request_id.to_string())],
                        Json(serde_json::json!({"error": "invalid price"})),
                    )
                        .into_response();
                }
            },
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    [("x-request-id", request_id.to_string())],
                    Json(serde_json::json!({"error": "price required for limit orders"})),
                )
                    .into_response();
            }
        }
    };

    let account_id = req.account_id;

    let order = Order {
        id: OrderId::new(0), // Engine assigns the real ID
        symbol,
        side,
        price: Price::new(price_raw),
        quantity: Quantity::new(qty_raw),
        remaining_qty: Quantity::new(qty_raw),
        order_type,
        time_in_force,
        timestamp: 0,
        account_id,
        prev: NO_LINK,
        next: NO_LINK,
    };

    let command = Command::PlaceOrder(order);
    match state.bridge.send_order(command, symbol, account_id).await {
        Ok(events) => {
            let mut order_id = 0u64;
            let mut status = "unknown".to_string();

            for event in &events {
                match event {
                    EngineEvent::OrderAccepted { order_id: oid, .. } => {
                        order_id = oid.raw();
                        status = "accepted".to_string();
                    }
                    EngineEvent::OrderRejected {
                        order_id: oid,
                        reason,
                        ..
                    } => {
                        order_id = oid.raw();
                        status = format!("rejected: {:?}", reason);
                    }
                    _ => {}
                }
            }

            (
                StatusCode::OK,
                [("x-request-id", request_id.to_string())],
                Json(serde_json::json!(PlaceOrderResponse { order_id, status })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [("x-request-id", request_id.to_string())],
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Cancel an existing order.
async fn cancel_order(State(state): State<AppState>, Path(id): Path<u64>) -> impl IntoResponse {
    let order_id = OrderId::new(id);
    // Look up the symbol this order belongs to.
    let symbol = {
        let registry = state
            .order_registry
            .read()
            .unwrap_or_else(|p| p.into_inner());
        match registry.get(&id) {
            Some(&sym) => sym,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "order not found"})),
                )
                    .into_response();
            }
        }
    };

    match state.bridge.send_cancel(order_id, symbol, 1).await {
        Ok(events) => {
            let has_cancel = events
                .iter()
                .any(|e| matches!(e, EngineEvent::OrderCancelled { .. }));

            if has_cancel {
                (
                    StatusCode::OK,
                    Json(serde_json::json!(CancelOrderResponse {
                        order_id: id,
                        status: "cancelled".to_string(),
                    })),
                )
                    .into_response()
            } else {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "order not found"})),
                )
                    .into_response()
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Get the order book depth for a symbol.
async fn get_orderbook(
    State(state): State<AppState>,
    Path(symbol_name): Path<String>,
) -> impl IntoResponse {
    if !state.symbol_registry.contains_key(&symbol_name) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "unknown symbol"})),
        )
            .into_response();
    }

    let snapshots = state
        .book_snapshots
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let response = match snapshots.get(&symbol_name) {
        Some(snapshot) => snapshot.clone(),
        None => OrderBookResponse {
            symbol: symbol_name,
            bids: Vec::new(),
            asks: Vec::new(),
            timestamp: current_timestamp_millis(),
        },
    };

    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

/// Get recent trades for a symbol.
async fn get_trades(
    State(state): State<AppState>,
    Path(symbol_name): Path<String>,
) -> impl IntoResponse {
    if !state.symbol_registry.contains_key(&symbol_name) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "unknown symbol"})),
        )
            .into_response();
    }

    let trades = state
        .recent_trades
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let symbol_trades = trades
        .get(&symbol_name)
        .map(|t| {
            let start = t.len().saturating_sub(MAX_RECENT_TRADES);
            t[start..].to_vec()
        })
        .unwrap_or_default();

    (StatusCode::OK, Json(serde_json::json!(symbol_trades))).into_response()
}

/// List all configured symbols.
async fn list_symbols(State(state): State<AppState>) -> impl IntoResponse {
    let symbols: Vec<SymbolInfo> = state
        .symbol_configs
        .iter()
        .map(|(name, config)| SymbolInfo {
            symbol: name.clone(),
            tick_size: format_price(config.tick_size, config.price_scale),
            lot_size: format_qty(config.lot_size, config.qty_scale),
            min_price: format_price(config.min_price, config.price_scale),
            max_price: format_price(config.max_price, config.price_scale),
            min_order_qty: format_qty(config.min_order_qty, config.qty_scale),
            max_order_qty: format_qty(config.max_order_qty, config.qty_scale),
        })
        .collect();

    (StatusCode::OK, Json(serde_json::json!(symbols))).into_response()
}

/// Health check endpoint.
///
/// Returns "ok" if all engine threads are alive, "degraded" if any have stopped.
async fn health_check(State(state): State<AppState>) -> impl IntoResponse {
    let uptime = state.start_time.elapsed().as_secs();

    let mut all_alive = true;
    let mut engine_statuses = Vec::new();

    for (name, alive_flag) in state.engine_alive.iter() {
        let alive = alive_flag.load(Ordering::Relaxed);
        if !alive {
            all_alive = false;
        }
        engine_statuses.push(EngineStatus {
            symbol: name.clone(),
            alive,
        });
    }
    // Sort for deterministic output
    engine_statuses.sort_by(|a, b| a.symbol.cmp(&b.symbol));

    let status = if all_alive { "ok" } else { "degraded" };

    let engines = if engine_statuses.is_empty() {
        None
    } else {
        Some(engine_statuses)
    };

    let response = HealthResponse {
        status: status.to_string(),
        uptime_secs: uptime,
        engines,
    };

    let status_code = if all_alive {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (status_code, Json(serde_json::json!(response))).into_response()
}

/// `GET /metrics` -- Prometheus text exposition format.
async fn get_metrics_prometheus(State(state): State<AppState>) -> impl IntoResponse {
    match &state.metrics {
        Some(provider) => {
            let body = provider.encode_prometheus();
            (
                StatusCode::OK,
                [(
                    axum::http::header::CONTENT_TYPE,
                    "text/plain; version=0.0.4; charset=utf-8",
                )],
                body,
            )
                .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "metrics not configured"})),
        )
            .into_response(),
    }
}

/// `GET /api/v1/metrics` -- JSON format with all metrics.
async fn get_metrics_json(State(state): State<AppState>) -> impl IntoResponse {
    match &state.metrics {
        Some(provider) => {
            let body = provider.encode_json();
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                body,
            )
                .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "metrics not configured"})),
        )
            .into_response(),
    }
}

/// `GET /api/v1/status` -- Server status: uptime, version, symbols, connections.
async fn get_status(State(state): State<AppState>) -> impl IntoResponse {
    let uptime = match &state.metrics {
        Some(provider) => provider.uptime_secs(),
        None => state.start_time.elapsed().as_secs(),
    };

    let (ws_active, tcp_active) = match &state.metrics {
        Some(provider) => {
            let json_str = provider.encode_json();
            let val: serde_json::Value = serde_json::from_str(&json_str).unwrap_or_default();
            let ws = val["ws_connections_active"].as_i64().unwrap_or(0);
            let tcp = val["tcp_connections_active"].as_i64().unwrap_or(0);
            (ws, tcp)
        }
        None => (0, 0),
    };

    let mut symbols = state.symbol_names.as_ref().clone();
    symbols.sort();

    let response = StatusResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_secs: uptime,
        symbols,
        ws_connections_active: ws_active,
        tcp_connections_active: tcp_active,
    };

    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

/// Parse a decimal string (e.g., "50001.50") into a scaled integer value.
fn parse_scaled_value(s: &str, scale: u8) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.split('.').collect();
    match parts.len() {
        1 => {
            let whole: u64 = parts[0].parse().ok()?;
            let factor = 10u64.checked_pow(scale as u32)?;
            whole.checked_mul(factor)
        }
        2 => {
            let whole: u64 = parts[0].parse().ok()?;
            let frac_str = parts[1];
            let frac_len = frac_str.len();
            if frac_len > scale as usize {
                return None; // Too many decimal places
            }
            let frac: u64 = if frac_str.is_empty() {
                0
            } else {
                frac_str.parse().ok()?
            };
            let factor = 10u64.checked_pow(scale as u32)?;
            let frac_factor = 10u64.checked_pow((scale as usize - frac_len) as u32)?;
            let whole_scaled = whole.checked_mul(factor)?;
            let frac_scaled = frac.checked_mul(frac_factor)?;
            whole_scaled.checked_add(frac_scaled)
        }
        _ => None,
    }
}

fn format_price(price: Price, scale: u8) -> String {
    price.format_with_scale(scale)
}

fn format_qty(qty: Quantity, scale: u8) -> String {
    if scale == 0 {
        return qty.raw().to_string();
    }
    let divisor = 10u64.pow(scale as u32);
    let raw = qty.raw();
    let whole = raw / divisor;
    let frac = raw % divisor;
    format!("{whole}.{frac:0>width$}", width = scale as usize)
}

fn current_timestamp_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        let (bridge, _queues) = EngineBridge::new(&[0], 16);
        let mut symbol_registry = HashMap::new();
        symbol_registry.insert("BTCUSDT".to_string(), Symbol::new(0));

        let mut symbol_configs = HashMap::new();
        symbol_configs.insert(
            "BTCUSDT".to_string(),
            SymbolConfig {
                symbol: Symbol::new(0),
                tick_size: Price::new(1),
                lot_size: Quantity::new(1),
                price_scale: 2,
                qty_scale: 8,
                max_price: Price::new(1_000_000_00),
                min_price: Price::new(1),
                max_order_qty: Quantity::new(1_000_000_00000000),
                min_order_qty: Quantity::new(1),
            },
        );

        let mut engine_alive = HashMap::new();
        engine_alive.insert("BTCUSDT".to_string(), Arc::new(AtomicBool::new(true)));

        AppState {
            bridge: Arc::new(bridge),
            symbol_registry: Arc::new(symbol_registry),
            symbol_configs: Arc::new(symbol_configs),
            recent_trades: Arc::new(RwLock::new(HashMap::new())),
            book_snapshots: Arc::new(RwLock::new(HashMap::new())),
            order_registry: Arc::new(RwLock::new(HashMap::new())),
            start_time: Instant::now(),
            engine_alive: Arc::new(engine_alive),
            request_id_counter: Arc::new(AtomicU64::new(1)),
            metrics: None,
            symbol_names: Arc::new(vec!["BTCUSDT".to_string()]),
        }
    }

    /// A simple test metrics provider for unit tests.
    struct TestMetricsProvider;

    impl MetricsProvider for TestMetricsProvider {
        fn encode_prometheus(&self) -> String {
            "# HELP tachyon_trades_total Total trades\n# TYPE tachyon_trades_total counter\ntachyon_trades_total 42\n".to_string()
        }

        fn encode_json(&self) -> String {
            r#"{"trades_total":42,"ws_connections_active":3,"tcp_connections_active":1}"#
                .to_string()
        }

        fn uptime_secs(&self) -> u64 {
            120
        }
    }

    #[tokio::test]
    async fn test_health_check() {
        let state = test_state();
        let app = rest_router(state);

        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .expect("build request");

        let resp = app.oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body().collect().await.expect("body").to_bytes();
        let parsed: HealthResponse = serde_json::from_slice(&body).expect("parse");
        assert_eq!(parsed.status, "ok");
        // Should include engine status
        assert!(parsed.engines.is_some());
        let engines = parsed.engines.unwrap();
        assert_eq!(engines.len(), 1);
        assert_eq!(engines[0].symbol, "BTCUSDT");
        assert!(engines[0].alive);
    }

    #[tokio::test]
    async fn test_health_check_degraded() {
        let state = test_state();
        // Simulate engine thread death
        if let Some(alive) = state.engine_alive.get("BTCUSDT") {
            alive.store(false, Ordering::Relaxed);
        }
        let app = rest_router(state);

        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .expect("build request");

        let resp = app.oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        let body = resp.into_body().collect().await.expect("body").to_bytes();
        let parsed: HealthResponse = serde_json::from_slice(&body).expect("parse");
        assert_eq!(parsed.status, "degraded");
    }

    #[tokio::test]
    async fn test_list_symbols() {
        let state = test_state();
        let app = rest_router(state);

        let req = Request::builder()
            .uri("/api/v1/symbols")
            .body(Body::empty())
            .expect("build request");

        let resp = app.oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body().collect().await.expect("body").to_bytes();
        let parsed: Vec<SymbolInfo> = serde_json::from_slice(&body).expect("parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].symbol, "BTCUSDT");
    }

    #[tokio::test]
    async fn test_get_orderbook_known_symbol() {
        let state = test_state();
        let app = rest_router(state);

        let req = Request::builder()
            .uri("/api/v1/orderbook/BTCUSDT")
            .body(Body::empty())
            .expect("build request");

        let resp = app.oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body().collect().await.expect("body").to_bytes();
        let parsed: OrderBookResponse = serde_json::from_slice(&body).expect("parse");
        assert_eq!(parsed.symbol, "BTCUSDT");
    }

    #[tokio::test]
    async fn test_get_orderbook_unknown_symbol() {
        let state = test_state();
        let app = rest_router(state);

        let req = Request::builder()
            .uri("/api/v1/orderbook/UNKNOWN")
            .body(Body::empty())
            .expect("build request");

        let resp = app.oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_get_trades_known_symbol() {
        let state = test_state();
        let app = rest_router(state);

        let req = Request::builder()
            .uri("/api/v1/trades/BTCUSDT")
            .body(Body::empty())
            .expect("build request");

        let resp = app.oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_get_trades_unknown_symbol() {
        let state = test_state();
        let app = rest_router(state);

        let req = Request::builder()
            .uri("/api/v1/trades/UNKNOWN")
            .body(Body::empty())
            .expect("build request");

        let resp = app.oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_parse_scaled_value_integer() {
        assert_eq!(parse_scaled_value("100", 2), Some(10000));
        assert_eq!(parse_scaled_value("0", 2), Some(0));
    }

    #[test]
    fn test_parse_scaled_value_decimal() {
        assert_eq!(parse_scaled_value("50001.50", 2), Some(5000150));
        assert_eq!(parse_scaled_value("1.5", 2), Some(150));
        assert_eq!(parse_scaled_value("0.01", 2), Some(1));
    }

    #[test]
    fn test_parse_scaled_value_too_many_decimals() {
        assert_eq!(parse_scaled_value("1.234", 2), None);
    }

    #[test]
    fn test_parse_scaled_value_empty() {
        assert_eq!(parse_scaled_value("", 2), None);
    }

    #[test]
    fn test_parse_scaled_value_invalid() {
        assert_eq!(parse_scaled_value("abc", 2), None);
    }

    #[test]
    fn test_parse_scaled_value_negative() {
        assert_eq!(parse_scaled_value("-1.00", 2), None);
        assert_eq!(parse_scaled_value("-100", 2), None);
    }

    #[test]
    fn test_parse_scaled_value_overflow() {
        // u64::MAX / 100 + 1 would overflow when multiplied by 100
        assert_eq!(parse_scaled_value("184467440737095516.16", 2), None);
    }

    #[test]
    fn test_parse_scaled_value_whitespace() {
        assert_eq!(parse_scaled_value("  100  ", 2), Some(10000));
    }

    #[test]
    fn test_parse_scaled_value_trailing_dot() {
        // "100." has an empty fractional part
        assert_eq!(parse_scaled_value("100.", 2), Some(10000));
    }

    #[test]
    fn test_format_qty() {
        assert_eq!(format_qty(Quantity::new(150), 2), "1.50");
        assert_eq!(format_qty(Quantity::new(100), 0), "100");
        assert_eq!(format_qty(Quantity::new(1), 4), "0.0001");
    }

    #[tokio::test]
    async fn test_get_metrics_prometheus() {
        let mut state = test_state();
        state.metrics = Some(Arc::new(TestMetricsProvider));
        let app = rest_router(state);

        let req = Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .expect("build request");

        let resp = app.oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body().collect().await.expect("body").to_bytes();
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("tachyon_trades_total 42"));
        assert!(text.contains("# TYPE tachyon_trades_total counter"));
    }

    #[tokio::test]
    async fn test_get_metrics_prometheus_not_configured() {
        let state = test_state();
        let app = rest_router(state);

        let req = Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .expect("build request");

        let resp = app.oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_get_metrics_json() {
        let mut state = test_state();
        state.metrics = Some(Arc::new(TestMetricsProvider));
        let app = rest_router(state);

        let req = Request::builder()
            .uri("/api/v1/metrics")
            .body(Body::empty())
            .expect("build request");

        let resp = app.oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body().collect().await.expect("body").to_bytes();
        let val: serde_json::Value = serde_json::from_slice(&body).expect("parse json");
        assert_eq!(val["trades_total"], 42);
    }

    #[tokio::test]
    async fn test_get_status() {
        let mut state = test_state();
        state.metrics = Some(Arc::new(TestMetricsProvider));
        let app = rest_router(state);

        let req = Request::builder()
            .uri("/api/v1/status")
            .body(Body::empty())
            .expect("build request");

        let resp = app.oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body().collect().await.expect("body").to_bytes();
        let parsed: StatusResponse = serde_json::from_slice(&body).expect("parse");
        assert_eq!(parsed.uptime_secs, 120);
        assert_eq!(parsed.symbols, vec!["BTCUSDT"]);
        assert_eq!(parsed.ws_connections_active, 3);
        assert_eq!(parsed.tcp_connections_active, 1);
    }

    #[tokio::test]
    async fn test_get_status_without_metrics() {
        let state = test_state();
        let app = rest_router(state);

        let req = Request::builder()
            .uri("/api/v1/status")
            .body(Body::empty())
            .expect("build request");

        let resp = app.oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body().collect().await.expect("body").to_bytes();
        let parsed: StatusResponse = serde_json::from_slice(&body).expect("parse");
        assert_eq!(parsed.symbols, vec!["BTCUSDT"]);
        assert_eq!(parsed.ws_connections_active, 0);
        assert_eq!(parsed.tcp_connections_active, 0);
    }
}
