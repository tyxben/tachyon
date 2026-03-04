use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;

use crate::types::{
    WsClientBatchMessage, WsClientMessage, WsServerMessage, VALID_CHANNEL_PREFIXES,
};

/// Shared state for the WebSocket handler.
#[derive(Clone)]
pub struct WsState {
    pub event_tx: broadcast::Sender<WsServerMessage>,
    /// Current number of active WebSocket connections.
    pub active_connections: Arc<AtomicU64>,
    /// Maximum allowed WebSocket connections (0 = unlimited).
    pub max_connections: u64,
}

impl WsState {
    /// Creates a new WsState with a broadcast channel of the given capacity.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        WsState {
            event_tx: tx,
            active_connections: Arc::new(AtomicU64::new(0)),
            max_connections: 0,
        }
    }

    /// Creates a new WsState with a broadcast channel and connection limit.
    pub fn with_limit(capacity: usize, max_connections: u64) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        WsState {
            event_tx: tx,
            active_connections: Arc::new(AtomicU64::new(0)),
            max_connections,
        }
    }
}

/// Axum handler for upgrading an HTTP connection to WebSocket.
pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<WsState>) -> impl IntoResponse {
    // Check connection limit before upgrading
    if state.max_connections > 0 {
        let current = state.active_connections.load(Ordering::Relaxed);
        if current >= state.max_connections {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    }

    state.active_connections.fetch_add(1, Ordering::Relaxed);
    ws.on_upgrade(move |socket| handle_socket(socket, state))
        .into_response()
}

/// Handles a single WebSocket connection.
async fn handle_socket(socket: WebSocket, state: WsState) {
    let (mut sender, mut receiver) = socket.split();
    let mut subscriptions: HashSet<String> = HashSet::new();
    let mut event_rx = state.event_tx.subscribe();

    loop {
        tokio::select! {
            // Incoming message from the client
            msg = receiver.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let responses = handle_client_message(&text, &mut subscriptions);
                        for resp in responses {
                            let json = match serde_json::to_string(&resp) {
                                Ok(j) => j,
                                Err(_) => continue,
                            };
                            if sender.send(Message::Text(json)).await.is_err() {
                                state.active_connections.fetch_sub(1, Ordering::Relaxed);
                                return;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    Some(Ok(Message::Ping(data))) => {
                        if sender.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(_)) => {} // Ignore binary and other messages
                }
            }
            // Broadcast event from the engine
            event = event_rx.recv() => {
                match event {
                    Ok(msg) => {
                        if should_deliver(&msg, &subscriptions) {
                            let json = match serde_json::to_string(&msg) {
                                Ok(j) => j,
                                Err(_) => continue,
                            };
                            if sender.send(Message::Text(json)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Client is too slow; skip missed messages
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    // Send a close frame before disconnecting
    let _ = sender.send(Message::Close(None)).await;
    state.active_connections.fetch_sub(1, Ordering::Relaxed);
}

/// Validates that a channel string uses a known prefix format.
fn is_valid_channel(channel: &str) -> bool {
    VALID_CHANNEL_PREFIXES
        .iter()
        .any(|prefix| channel.starts_with(prefix))
}

/// Normalizes a channel name: converts `orderbook@` to `depth@` for internal
/// consistency while accepting both from clients.
fn normalize_channel(channel: &str) -> String {
    if let Some(symbol) = channel.strip_prefix("orderbook@") {
        format!("depth@{}", symbol)
    } else {
        channel.to_string()
    }
}

/// Processes an incoming client message and returns response(s).
///
/// Supports two formats:
/// - Legacy single-channel: `{"type":"subscribe","channel":"trades@BTCUSDT"}`
/// - Batch: `{"method":"subscribe","params":["orderbook@BTCUSDT","trades@BTCUSDT"]}`
fn handle_client_message(text: &str, subscriptions: &mut HashSet<String>) -> Vec<WsServerMessage> {
    // Try batch format first (uses "method" tag)
    if let Ok(batch) = serde_json::from_str::<WsClientBatchMessage>(text) {
        return handle_batch_message(batch, subscriptions);
    }

    // Fall back to legacy single-channel format (uses "type" tag)
    match serde_json::from_str::<WsClientMessage>(text) {
        Ok(msg) => handle_legacy_message(msg, subscriptions),
        Err(e) => {
            vec![WsServerMessage::Error {
                message: format!("invalid message: {}", e),
            }]
        }
    }
}

/// Handles a legacy single-channel client message.
fn handle_legacy_message(
    msg: WsClientMessage,
    subscriptions: &mut HashSet<String>,
) -> Vec<WsServerMessage> {
    match msg {
        WsClientMessage::Subscribe { channel } => {
            if !is_valid_channel(&channel) {
                return vec![WsServerMessage::Error {
                    message: format!("invalid channel: {}", channel),
                }];
            }
            let normalized = normalize_channel(&channel);
            subscriptions.insert(normalized);
            vec![WsServerMessage::Subscribed { channel }]
        }
        WsClientMessage::Unsubscribe { channel } => {
            let normalized = normalize_channel(&channel);
            subscriptions.remove(&normalized);
            vec![WsServerMessage::Unsubscribed { channel }]
        }
        WsClientMessage::Ping => vec![WsServerMessage::Pong],
    }
}

/// Handles a batch client message with multiple channels.
fn handle_batch_message(
    msg: WsClientBatchMessage,
    subscriptions: &mut HashSet<String>,
) -> Vec<WsServerMessage> {
    match msg {
        WsClientBatchMessage::Subscribe { params } => {
            let mut responses = Vec::with_capacity(params.len());
            for channel in params {
                if !is_valid_channel(&channel) {
                    responses.push(WsServerMessage::Error {
                        message: format!("invalid channel: {}", channel),
                    });
                    continue;
                }
                let normalized = normalize_channel(&channel);
                subscriptions.insert(normalized);
                responses.push(WsServerMessage::Subscribed { channel });
            }
            responses
        }
        WsClientBatchMessage::Unsubscribe { params } => {
            let mut responses = Vec::with_capacity(params.len());
            for channel in params {
                let normalized = normalize_channel(&channel);
                subscriptions.remove(&normalized);
                responses.push(WsServerMessage::Unsubscribed { channel });
            }
            responses
        }
        WsClientBatchMessage::Ping => vec![WsServerMessage::Pong],
    }
}

/// Determines if a server message should be delivered to a connection
/// based on its active subscriptions.
fn should_deliver(msg: &WsServerMessage, subscriptions: &HashSet<String>) -> bool {
    match msg {
        WsServerMessage::Trade { data } => {
            let channel = format!("trades@{}", data.symbol);
            subscriptions.contains(&channel)
        }
        WsServerMessage::Depth { data } => {
            // Internally stored as "depth@SYMBOL"
            let channel = format!("depth@{}", data.symbol);
            subscriptions.contains(&channel)
        }
        WsServerMessage::Ticker { data } => {
            let channel = format!("ticker@{}", data.symbol);
            subscriptions.contains(&channel)
        }
        // Control messages (subscribed, unsubscribed, pong, error) are always delivered
        // via direct send, not through the broadcast channel.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Legacy single-channel format tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_handle_subscribe() {
        let mut subs = HashSet::new();
        let result = handle_client_message(
            r#"{"type":"subscribe","channel":"trades@BTCUSDT"}"#,
            &mut subs,
        );
        assert!(subs.contains("trades@BTCUSDT"));
        assert_eq!(result.len(), 1);
        match &result[0] {
            WsServerMessage::Subscribed { channel } => {
                assert_eq!(channel, "trades@BTCUSDT");
            }
            _ => panic!("Expected Subscribed"),
        }
    }

    #[test]
    fn test_handle_unsubscribe() {
        let mut subs = HashSet::new();
        subs.insert("trades@BTCUSDT".to_string());
        let result = handle_client_message(
            r#"{"type":"unsubscribe","channel":"trades@BTCUSDT"}"#,
            &mut subs,
        );
        assert!(!subs.contains("trades@BTCUSDT"));
        assert_eq!(result.len(), 1);
        match &result[0] {
            WsServerMessage::Unsubscribed { channel } => {
                assert_eq!(channel, "trades@BTCUSDT");
            }
            _ => panic!("Expected Unsubscribed"),
        }
    }

    #[test]
    fn test_handle_ping() {
        let mut subs = HashSet::new();
        let result = handle_client_message(r#"{"type":"ping"}"#, &mut subs);
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], WsServerMessage::Pong));
    }

    #[test]
    fn test_handle_invalid_json() {
        let mut subs = HashSet::new();
        let result = handle_client_message("not json", &mut subs);
        assert_eq!(result.len(), 1);
        match &result[0] {
            WsServerMessage::Error { message } => {
                assert!(message.contains("invalid message"));
            }
            _ => panic!("Expected Error"),
        }
    }

    #[test]
    fn test_handle_unknown_type() {
        let mut subs = HashSet::new();
        let result = handle_client_message(r#"{"type":"invalid_type"}"#, &mut subs);
        assert_eq!(result.len(), 1);
        match &result[0] {
            WsServerMessage::Error { message } => {
                assert!(message.contains("invalid message"));
            }
            _ => panic!("Expected Error"),
        }
    }

    // -----------------------------------------------------------------------
    // Batch format tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_batch_subscribe_single() {
        let mut subs = HashSet::new();
        let result = handle_client_message(
            r#"{"method":"subscribe","params":["trades@BTCUSDT"]}"#,
            &mut subs,
        );
        assert!(subs.contains("trades@BTCUSDT"));
        assert_eq!(result.len(), 1);
        match &result[0] {
            WsServerMessage::Subscribed { channel } => {
                assert_eq!(channel, "trades@BTCUSDT");
            }
            _ => panic!("Expected Subscribed"),
        }
    }

    #[test]
    fn test_batch_subscribe_multiple() {
        let mut subs = HashSet::new();
        let result = handle_client_message(
            r#"{"method":"subscribe","params":["orderbook@BTCUSDT","trades@BTCUSDT","ticker@BTCUSDT"]}"#,
            &mut subs,
        );
        // orderbook@ is normalized to depth@
        assert!(subs.contains("depth@BTCUSDT"));
        assert!(subs.contains("trades@BTCUSDT"));
        assert!(subs.contains("ticker@BTCUSDT"));
        assert_eq!(result.len(), 3);
        match &result[0] {
            WsServerMessage::Subscribed { channel } => {
                // The response channel keeps the original name the client sent
                assert_eq!(channel, "orderbook@BTCUSDT");
            }
            _ => panic!("Expected Subscribed"),
        }
    }

    #[test]
    fn test_batch_unsubscribe() {
        let mut subs = HashSet::new();
        subs.insert("depth@BTCUSDT".to_string());
        subs.insert("trades@BTCUSDT".to_string());
        let result = handle_client_message(
            r#"{"method":"unsubscribe","params":["orderbook@BTCUSDT"]}"#,
            &mut subs,
        );
        // orderbook@ normalizes to depth@ which should be removed
        assert!(!subs.contains("depth@BTCUSDT"));
        assert!(subs.contains("trades@BTCUSDT"));
        assert_eq!(result.len(), 1);
        match &result[0] {
            WsServerMessage::Unsubscribed { channel } => {
                assert_eq!(channel, "orderbook@BTCUSDT");
            }
            _ => panic!("Expected Unsubscribed"),
        }
    }

    #[test]
    fn test_batch_ping() {
        let mut subs = HashSet::new();
        let result = handle_client_message(r#"{"method":"ping"}"#, &mut subs);
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], WsServerMessage::Pong));
    }

    #[test]
    fn test_batch_subscribe_invalid_channel() {
        let mut subs = HashSet::new();
        let result = handle_client_message(
            r#"{"method":"subscribe","params":["invalid_channel"]}"#,
            &mut subs,
        );
        assert!(subs.is_empty());
        assert_eq!(result.len(), 1);
        match &result[0] {
            WsServerMessage::Error { message } => {
                assert!(message.contains("invalid channel"));
            }
            _ => panic!("Expected Error"),
        }
    }

    #[test]
    fn test_batch_subscribe_mixed_valid_invalid() {
        let mut subs = HashSet::new();
        let result = handle_client_message(
            r#"{"method":"subscribe","params":["trades@BTCUSDT","bad_channel","ticker@ETHUSDT"]}"#,
            &mut subs,
        );
        assert!(subs.contains("trades@BTCUSDT"));
        assert!(subs.contains("ticker@ETHUSDT"));
        assert_eq!(subs.len(), 2);
        assert_eq!(result.len(), 3);
        assert!(matches!(&result[0], WsServerMessage::Subscribed { .. }));
        assert!(matches!(&result[1], WsServerMessage::Error { .. }));
        assert!(matches!(&result[2], WsServerMessage::Subscribed { .. }));
    }

    // -----------------------------------------------------------------------
    // Channel normalization tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_normalize_channel_orderbook_to_depth() {
        assert_eq!(normalize_channel("orderbook@BTCUSDT"), "depth@BTCUSDT");
    }

    #[test]
    fn test_normalize_channel_depth_unchanged() {
        assert_eq!(normalize_channel("depth@BTCUSDT"), "depth@BTCUSDT");
    }

    #[test]
    fn test_normalize_channel_trades_unchanged() {
        assert_eq!(normalize_channel("trades@ETHUSDT"), "trades@ETHUSDT");
    }

    #[test]
    fn test_normalize_channel_ticker_unchanged() {
        assert_eq!(normalize_channel("ticker@BTCUSDT"), "ticker@BTCUSDT");
    }

    // -----------------------------------------------------------------------
    // Channel validation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_valid_channel() {
        assert!(is_valid_channel("trades@BTCUSDT"));
        assert!(is_valid_channel("orderbook@BTCUSDT"));
        assert!(is_valid_channel("depth@BTCUSDT"));
        assert!(is_valid_channel("ticker@ETHUSDT"));
        assert!(!is_valid_channel("invalid"));
        assert!(!is_valid_channel("unknown@BTCUSDT"));
        assert!(!is_valid_channel(""));
    }

    // -----------------------------------------------------------------------
    // should_deliver tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_should_deliver_trade() {
        let mut subs = HashSet::new();
        subs.insert("trades@BTCUSDT".to_string());

        let msg = WsServerMessage::Trade {
            data: crate::types::TradeResponse {
                trade_id: 1,
                symbol: "BTCUSDT".to_string(),
                price: "50000.00".to_string(),
                quantity: "1.0".to_string(),
                maker_side: "sell".to_string(),
                timestamp: 1000,
            },
        };
        assert!(should_deliver(&msg, &subs));
    }

    #[test]
    fn test_should_not_deliver_unsubscribed() {
        let subs = HashSet::new();

        let msg = WsServerMessage::Trade {
            data: crate::types::TradeResponse {
                trade_id: 1,
                symbol: "BTCUSDT".to_string(),
                price: "50000.00".to_string(),
                quantity: "1.0".to_string(),
                maker_side: "sell".to_string(),
                timestamp: 1000,
            },
        };
        assert!(!should_deliver(&msg, &subs));
    }

    #[test]
    fn test_should_deliver_depth() {
        let mut subs = HashSet::new();
        subs.insert("depth@ETHUSDT".to_string());

        let msg = WsServerMessage::Depth {
            data: crate::types::OrderBookResponse {
                symbol: "ETHUSDT".to_string(),
                bids: vec![],
                asks: vec![],
                timestamp: 1000,
            },
        };
        assert!(should_deliver(&msg, &subs));
    }

    #[test]
    fn test_should_deliver_depth_via_orderbook_subscription() {
        // Client subscribes via "orderbook@" which is normalized to "depth@"
        let mut subs = HashSet::new();
        let result = handle_client_message(
            r#"{"method":"subscribe","params":["orderbook@ETHUSDT"]}"#,
            &mut subs,
        );
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WsServerMessage::Subscribed { .. }));

        // Depth messages should be delivered since orderbook@ normalized to depth@
        let msg = WsServerMessage::Depth {
            data: crate::types::OrderBookResponse {
                symbol: "ETHUSDT".to_string(),
                bids: vec![],
                asks: vec![],
                timestamp: 1000,
            },
        };
        assert!(should_deliver(&msg, &subs));
    }

    #[test]
    fn test_should_deliver_ticker() {
        let mut subs = HashSet::new();
        subs.insert("ticker@BTCUSDT".to_string());

        let msg = WsServerMessage::Ticker {
            data: crate::types::TickerResponse {
                symbol: "BTCUSDT".to_string(),
                best_bid: Some("49999.00".to_string()),
                best_bid_qty: Some("5.0".to_string()),
                best_ask: Some("50001.00".to_string()),
                best_ask_qty: Some("3.0".to_string()),
                last_price: Some("50000.00".to_string()),
                last_qty: Some("1.0".to_string()),
                timestamp: 1000,
            },
        };
        assert!(should_deliver(&msg, &subs));
    }

    #[test]
    fn test_should_not_deliver_ticker_wrong_symbol() {
        let mut subs = HashSet::new();
        subs.insert("ticker@ETHUSDT".to_string());

        let msg = WsServerMessage::Ticker {
            data: crate::types::TickerResponse {
                symbol: "BTCUSDT".to_string(),
                best_bid: None,
                best_bid_qty: None,
                best_ask: None,
                best_ask_qty: None,
                last_price: None,
                last_qty: None,
                timestamp: 1000,
            },
        };
        assert!(!should_deliver(&msg, &subs));
    }

    #[test]
    fn test_should_not_deliver_control_messages() {
        let mut subs = HashSet::new();
        subs.insert("trades@BTCUSDT".to_string());

        assert!(!should_deliver(
            &WsServerMessage::Subscribed {
                channel: "trades@BTCUSDT".to_string()
            },
            &subs
        ));
        assert!(!should_deliver(
            &WsServerMessage::Unsubscribed {
                channel: "trades@BTCUSDT".to_string()
            },
            &subs
        ));
        assert!(!should_deliver(&WsServerMessage::Pong, &subs));
        assert!(!should_deliver(
            &WsServerMessage::Error {
                message: "test".to_string()
            },
            &subs
        ));
    }

    #[test]
    fn test_ws_state_new() {
        let state = WsState::new(100);
        // Should be able to subscribe
        let _rx = state.event_tx.subscribe();
    }

    // -----------------------------------------------------------------------
    // Subscription lifecycle tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_subscribe_then_unsubscribe_then_resubscribe() {
        let mut subs = HashSet::new();

        // Subscribe
        let _ = handle_client_message(
            r#"{"method":"subscribe","params":["trades@BTCUSDT"]}"#,
            &mut subs,
        );
        assert!(subs.contains("trades@BTCUSDT"));

        // Unsubscribe
        let _ = handle_client_message(
            r#"{"method":"unsubscribe","params":["trades@BTCUSDT"]}"#,
            &mut subs,
        );
        assert!(!subs.contains("trades@BTCUSDT"));

        // Resubscribe
        let _ = handle_client_message(
            r#"{"method":"subscribe","params":["trades@BTCUSDT"]}"#,
            &mut subs,
        );
        assert!(subs.contains("trades@BTCUSDT"));
    }

    #[test]
    fn test_duplicate_subscribe_is_idempotent() {
        let mut subs = HashSet::new();
        let _ = handle_client_message(
            r#"{"method":"subscribe","params":["trades@BTCUSDT"]}"#,
            &mut subs,
        );
        let _ = handle_client_message(
            r#"{"method":"subscribe","params":["trades@BTCUSDT"]}"#,
            &mut subs,
        );
        assert_eq!(subs.len(), 1);
        assert!(subs.contains("trades@BTCUSDT"));
    }

    #[test]
    fn test_unsubscribe_nonexistent_channel() {
        let mut subs = HashSet::new();
        let result = handle_client_message(
            r#"{"method":"unsubscribe","params":["trades@BTCUSDT"]}"#,
            &mut subs,
        );
        // Should still respond with Unsubscribed even if not previously subscribed
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WsServerMessage::Unsubscribed { .. }));
    }

    #[test]
    fn test_legacy_subscribe_with_invalid_channel() {
        let mut subs = HashSet::new();
        let result = handle_client_message(
            r#"{"type":"subscribe","channel":"unknown_channel"}"#,
            &mut subs,
        );
        assert!(subs.is_empty());
        assert_eq!(result.len(), 1);
        match &result[0] {
            WsServerMessage::Error { message } => {
                assert!(message.contains("invalid channel"));
            }
            _ => panic!("Expected Error"),
        }
    }

    #[test]
    fn test_legacy_subscribe_orderbook_normalizes() {
        let mut subs = HashSet::new();
        let _ = handle_client_message(
            r#"{"type":"subscribe","channel":"orderbook@BTCUSDT"}"#,
            &mut subs,
        );
        // Internally stored as depth@
        assert!(subs.contains("depth@BTCUSDT"));
        assert!(!subs.contains("orderbook@BTCUSDT"));
    }

    // -----------------------------------------------------------------------
    // Broadcast delivery tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_broadcast_channel_send_and_filter() {
        let state = WsState::new(16);
        let mut rx = state.event_tx.subscribe();

        let trade_msg = WsServerMessage::Trade {
            data: crate::types::TradeResponse {
                trade_id: 42,
                symbol: "BTCUSDT".to_string(),
                price: "50000.00".to_string(),
                quantity: "1.0".to_string(),
                maker_side: "buy".to_string(),
                timestamp: 999,
            },
        };

        // Send on broadcast
        state.event_tx.send(trade_msg.clone()).unwrap();

        // Receive
        let received = rx.try_recv().unwrap();
        match received {
            WsServerMessage::Trade { data } => {
                assert_eq!(data.trade_id, 42);
                assert_eq!(data.symbol, "BTCUSDT");
            }
            _ => panic!("Expected Trade"),
        }

        // Verify filtering: subscribed to ETHUSDT, message is BTCUSDT
        let mut eth_subs = HashSet::new();
        eth_subs.insert("trades@ETHUSDT".to_string());
        assert!(!should_deliver(&trade_msg, &eth_subs));

        // Subscribed to BTCUSDT, should deliver
        let mut btc_subs = HashSet::new();
        btc_subs.insert("trades@BTCUSDT".to_string());
        assert!(should_deliver(&trade_msg, &btc_subs));
    }

    #[test]
    fn test_batch_subscribe_empty_params() {
        let mut subs = HashSet::new();
        let result = handle_client_message(r#"{"method":"subscribe","params":[]}"#, &mut subs);
        assert!(subs.is_empty());
        assert!(result.is_empty());
    }
}
