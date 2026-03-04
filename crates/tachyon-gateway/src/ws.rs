use std::collections::HashSet;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;

use crate::types::{WsClientMessage, WsServerMessage};

/// Shared state for the WebSocket handler.
#[derive(Clone)]
pub struct WsState {
    pub event_tx: broadcast::Sender<WsServerMessage>,
}

impl WsState {
    /// Creates a new WsState with a broadcast channel of the given capacity.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        WsState { event_tx: tx }
    }
}

/// Axum handler for upgrading an HTTP connection to WebSocket.
pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<WsState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
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
                        let response = handle_client_message(&text, &mut subscriptions);
                        if let Some(resp) = response {
                            let json = match serde_json::to_string(&resp) {
                                Ok(j) => j,
                                Err(_) => continue,
                            };
                            if sender.send(Message::Text(json)).await.is_err() {
                                break;
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
}

/// Processes an incoming client message and returns an optional response.
fn handle_client_message(
    text: &str,
    subscriptions: &mut HashSet<String>,
) -> Option<WsServerMessage> {
    let msg: WsClientMessage = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(e) => {
            return Some(WsServerMessage::Error {
                message: format!("invalid message: {}", e),
            });
        }
    };

    match msg {
        WsClientMessage::Subscribe { channel } => {
            subscriptions.insert(channel.clone());
            Some(WsServerMessage::Subscribed { channel })
        }
        WsClientMessage::Unsubscribe { channel } => {
            subscriptions.remove(&channel);
            Some(WsServerMessage::Unsubscribed { channel })
        }
        WsClientMessage::Ping => Some(WsServerMessage::Pong),
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
            let channel = format!("depth@{}", data.symbol);
            subscriptions.contains(&channel)
        }
        // Control messages (subscribed, unsubscribed, pong, error) are always delivered
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_handle_subscribe() {
        let mut subs = HashSet::new();
        let result = handle_client_message(
            r#"{"type":"subscribe","channel":"trades@BTCUSDT"}"#,
            &mut subs,
        );
        assert!(subs.contains("trades@BTCUSDT"));
        match result {
            Some(WsServerMessage::Subscribed { channel }) => {
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
        match result {
            Some(WsServerMessage::Unsubscribed { channel }) => {
                assert_eq!(channel, "trades@BTCUSDT");
            }
            _ => panic!("Expected Unsubscribed"),
        }
    }

    #[test]
    fn test_handle_ping() {
        let mut subs = HashSet::new();
        let result = handle_client_message(r#"{"type":"ping"}"#, &mut subs);
        assert!(matches!(result, Some(WsServerMessage::Pong)));
    }

    #[test]
    fn test_handle_invalid_json() {
        let mut subs = HashSet::new();
        let result = handle_client_message("not json", &mut subs);
        match result {
            Some(WsServerMessage::Error { message }) => {
                assert!(message.contains("invalid message"));
            }
            _ => panic!("Expected Error"),
        }
    }

    #[test]
    fn test_handle_unknown_type() {
        let mut subs = HashSet::new();
        let result = handle_client_message(r#"{"type":"invalid_type"}"#, &mut subs);
        match result {
            Some(WsServerMessage::Error { message }) => {
                assert!(message.contains("invalid message"));
            }
            _ => panic!("Expected Error"),
        }
    }

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
    fn test_ws_state_new() {
        let state = WsState::new(100);
        // Should be able to subscribe
        let _rx = state.event_tx.subscribe();
    }
}
