use serde::{Deserialize, Serialize};

/// Request to place a new order.
///
/// All monetary values (price, quantity) are serialized as strings to preserve precision.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PlaceOrderRequest {
    pub symbol: String,
    pub side: String,
    pub order_type: String,
    pub time_in_force: String,
    pub price: Option<String>,
    pub quantity: String,
}

/// Response after placing an order.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PlaceOrderResponse {
    pub order_id: u64,
    pub status: String,
}

/// Response after cancelling an order.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CancelOrderResponse {
    pub order_id: u64,
    pub status: String,
}

/// A single price level in the order book.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OrderBookLevel {
    pub price: String,
    pub quantity: String,
}

/// Full order book snapshot response.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OrderBookResponse {
    pub symbol: String,
    pub bids: Vec<OrderBookLevel>,
    pub asks: Vec<OrderBookLevel>,
    pub timestamp: u64,
}

/// A single trade.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TradeResponse {
    pub trade_id: u64,
    pub symbol: String,
    pub price: String,
    pub quantity: String,
    pub maker_side: String,
    pub timestamp: u64,
}

/// Information about a tradeable symbol.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SymbolInfo {
    pub symbol: String,
    pub tick_size: String,
    pub lot_size: String,
    pub min_price: String,
    pub max_price: String,
    pub min_order_qty: String,
    pub max_order_qty: String,
}

/// Ticker (best bid/ask + last price) response.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TickerResponse {
    pub symbol: String,
    pub best_bid: Option<String>,
    pub best_bid_qty: Option<String>,
    pub best_ask: Option<String>,
    pub best_ask_qty: Option<String>,
    pub last_price: Option<String>,
    pub last_qty: Option<String>,
    pub timestamp: u64,
}

/// Health check response.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HealthResponse {
    pub status: String,
    pub uptime_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engines: Option<Vec<EngineStatus>>,
}

/// Status of a single symbol engine thread.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EngineStatus {
    pub symbol: String,
    pub alive: bool,
}

/// Server status response for `/api/v1/status`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StatusResponse {
    pub version: String,
    pub uptime_secs: u64,
    pub symbols: Vec<String>,
    pub ws_connections_active: i64,
    pub tcp_connections_active: i64,
}

/// Messages sent from WebSocket clients (single-channel legacy format).
///
/// Format: `{"type":"subscribe","channel":"trades@BTCUSDT"}`
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum WsClientMessage {
    #[serde(rename = "subscribe")]
    Subscribe { channel: String },
    #[serde(rename = "unsubscribe")]
    Unsubscribe { channel: String },
    #[serde(rename = "ping")]
    Ping,
}

/// Messages sent from WebSocket clients (batch format).
///
/// Format: `{"method":"subscribe","params":["orderbook@BTCUSDT","trades@BTCUSDT"]}`
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "method")]
pub enum WsClientBatchMessage {
    #[serde(rename = "subscribe")]
    Subscribe { params: Vec<String> },
    #[serde(rename = "unsubscribe")]
    Unsubscribe { params: Vec<String> },
    #[serde(rename = "ping")]
    Ping,
}

/// Messages sent from the server to WebSocket clients.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum WsServerMessage {
    #[serde(rename = "subscribed")]
    Subscribed { channel: String },
    #[serde(rename = "unsubscribed")]
    Unsubscribed { channel: String },
    #[serde(rename = "pong")]
    Pong,
    #[serde(rename = "trade")]
    Trade { data: TradeResponse },
    #[serde(rename = "depth")]
    Depth { data: OrderBookResponse },
    #[serde(rename = "ticker")]
    Ticker { data: TickerResponse },
    #[serde(rename = "error")]
    Error { message: String },
}

/// Known channel prefixes for subscription validation.
pub const VALID_CHANNEL_PREFIXES: &[&str] = &["trades@", "orderbook@", "depth@", "ticker@"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_place_order_request_roundtrip() {
        let req = PlaceOrderRequest {
            symbol: "BTCUSDT".to_string(),
            side: "buy".to_string(),
            order_type: "limit".to_string(),
            time_in_force: "GTC".to_string(),
            price: Some("50000.00".to_string()),
            quantity: "1.5".to_string(),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let parsed: PlaceOrderRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.symbol, "BTCUSDT");
        assert_eq!(parsed.price, Some("50000.00".to_string()));
        assert_eq!(parsed.quantity, "1.5");
    }

    #[test]
    fn test_place_order_request_market_no_price() {
        let req = PlaceOrderRequest {
            symbol: "ETHUSDT".to_string(),
            side: "sell".to_string(),
            order_type: "market".to_string(),
            time_in_force: "IOC".to_string(),
            price: None,
            quantity: "10.0".to_string(),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let parsed: PlaceOrderRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.price, None);
    }

    #[test]
    fn test_place_order_response_roundtrip() {
        let resp = PlaceOrderResponse {
            order_id: 42,
            status: "accepted".to_string(),
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        let parsed: PlaceOrderResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.order_id, 42);
        assert_eq!(parsed.status, "accepted");
    }

    #[test]
    fn test_cancel_order_response_roundtrip() {
        let resp = CancelOrderResponse {
            order_id: 99,
            status: "cancelled".to_string(),
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        let parsed: CancelOrderResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.order_id, 99);
    }

    #[test]
    fn test_orderbook_level_price_as_string() {
        let level = OrderBookLevel {
            price: "50001.50".to_string(),
            quantity: "1.23456789".to_string(),
        };
        let json = serde_json::to_string(&level).expect("serialize");
        assert!(json.contains("\"50001.50\""));
        assert!(json.contains("\"1.23456789\""));
        let parsed: OrderBookLevel = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.price, "50001.50");
    }

    #[test]
    fn test_orderbook_response_roundtrip() {
        let resp = OrderBookResponse {
            symbol: "BTCUSDT".to_string(),
            bids: vec![OrderBookLevel {
                price: "49999.00".to_string(),
                quantity: "5.0".to_string(),
            }],
            asks: vec![OrderBookLevel {
                price: "50001.00".to_string(),
                quantity: "3.0".to_string(),
            }],
            timestamp: 1234567890,
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        let parsed: OrderBookResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.bids.len(), 1);
        assert_eq!(parsed.asks.len(), 1);
        assert_eq!(parsed.timestamp, 1234567890);
    }

    #[test]
    fn test_trade_response_roundtrip() {
        let trade = TradeResponse {
            trade_id: 1,
            symbol: "BTCUSDT".to_string(),
            price: "50000.00".to_string(),
            quantity: "0.5".to_string(),
            maker_side: "sell".to_string(),
            timestamp: 1000,
        };
        let json = serde_json::to_string(&trade).expect("serialize");
        let parsed: TradeResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.trade_id, 1);
        assert_eq!(parsed.price, "50000.00");
    }

    #[test]
    fn test_symbol_info_roundtrip() {
        let info = SymbolInfo {
            symbol: "BTCUSDT".to_string(),
            tick_size: "0.01".to_string(),
            lot_size: "0.001".to_string(),
            min_price: "0.01".to_string(),
            max_price: "1000000.00".to_string(),
            min_order_qty: "0.001".to_string(),
            max_order_qty: "1000.000".to_string(),
        };
        let json = serde_json::to_string(&info).expect("serialize");
        let parsed: SymbolInfo = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.tick_size, "0.01");
    }

    #[test]
    fn test_health_response_roundtrip() {
        let resp = HealthResponse {
            status: "ok".to_string(),
            uptime_secs: 3600,
            engines: None,
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        let parsed: HealthResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.uptime_secs, 3600);
    }

    #[test]
    fn test_status_response_roundtrip() {
        let resp = StatusResponse {
            version: "0.1.0".to_string(),
            uptime_secs: 120,
            symbols: vec!["BTCUSDT".to_string(), "ETHUSDT".to_string()],
            ws_connections_active: 5,
            tcp_connections_active: 2,
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        let parsed: StatusResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.version, "0.1.0");
        assert_eq!(parsed.symbols.len(), 2);
        assert_eq!(parsed.ws_connections_active, 5);
    }

    #[test]
    fn test_ws_client_message_subscribe() {
        let json = r#"{"type":"subscribe","channel":"trades@BTCUSDT"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).expect("deserialize");
        match msg {
            WsClientMessage::Subscribe { channel } => {
                assert_eq!(channel, "trades@BTCUSDT");
            }
            _ => panic!("Expected Subscribe"),
        }
    }

    #[test]
    fn test_ws_client_message_unsubscribe() {
        let json = r#"{"type":"unsubscribe","channel":"depth@ETHUSDT"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).expect("deserialize");
        match msg {
            WsClientMessage::Unsubscribe { channel } => {
                assert_eq!(channel, "depth@ETHUSDT");
            }
            _ => panic!("Expected Unsubscribe"),
        }
    }

    #[test]
    fn test_ws_client_message_ping() {
        let json = r#"{"type":"ping"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).expect("deserialize");
        assert!(matches!(msg, WsClientMessage::Ping));
    }

    #[test]
    fn test_ws_server_message_subscribed() {
        let msg = WsServerMessage::Subscribed {
            channel: "trades@BTCUSDT".to_string(),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(json.contains("\"subscribed\""));
        let parsed: WsServerMessage = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            WsServerMessage::Subscribed { channel } => {
                assert_eq!(channel, "trades@BTCUSDT");
            }
            _ => panic!("Expected Subscribed"),
        }
    }

    #[test]
    fn test_ws_server_message_pong() {
        let msg = WsServerMessage::Pong;
        let json = serde_json::to_string(&msg).expect("serialize");
        let parsed: WsServerMessage = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(parsed, WsServerMessage::Pong));
    }

    #[test]
    fn test_ws_server_message_trade() {
        let msg = WsServerMessage::Trade {
            data: TradeResponse {
                trade_id: 1,
                symbol: "BTCUSDT".to_string(),
                price: "50000.00".to_string(),
                quantity: "0.5".to_string(),
                maker_side: "sell".to_string(),
                timestamp: 1000,
            },
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(json.contains("\"trade\""));
        let parsed: WsServerMessage = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            WsServerMessage::Trade { data } => {
                assert_eq!(data.trade_id, 1);
            }
            _ => panic!("Expected Trade"),
        }
    }

    #[test]
    fn test_ws_server_message_error() {
        let msg = WsServerMessage::Error {
            message: "invalid channel".to_string(),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let parsed: WsServerMessage = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            WsServerMessage::Error { message } => {
                assert_eq!(message, "invalid channel");
            }
            _ => panic!("Expected Error"),
        }
    }

    #[test]
    fn test_ws_client_message_invalid_type() {
        let json = r#"{"type":"invalid"}"#;
        let result = serde_json::from_str::<WsClientMessage>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_ws_client_batch_subscribe() {
        let json = r#"{"method":"subscribe","params":["orderbook@BTCUSDT","trades@BTCUSDT"]}"#;
        let msg: WsClientBatchMessage = serde_json::from_str(json).expect("deserialize");
        match msg {
            WsClientBatchMessage::Subscribe { params } => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0], "orderbook@BTCUSDT");
                assert_eq!(params[1], "trades@BTCUSDT");
            }
            _ => panic!("Expected Subscribe"),
        }
    }

    #[test]
    fn test_ws_client_batch_unsubscribe() {
        let json = r#"{"method":"unsubscribe","params":["orderbook@BTCUSDT"]}"#;
        let msg: WsClientBatchMessage = serde_json::from_str(json).expect("deserialize");
        match msg {
            WsClientBatchMessage::Unsubscribe { params } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0], "orderbook@BTCUSDT");
            }
            _ => panic!("Expected Unsubscribe"),
        }
    }

    #[test]
    fn test_ws_client_batch_ping() {
        let json = r#"{"method":"ping"}"#;
        let msg: WsClientBatchMessage = serde_json::from_str(json).expect("deserialize");
        assert!(matches!(msg, WsClientBatchMessage::Ping));
    }

    #[test]
    fn test_ws_server_message_ticker() {
        let msg = WsServerMessage::Ticker {
            data: TickerResponse {
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
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(json.contains("\"ticker\""));
        let parsed: WsServerMessage = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            WsServerMessage::Ticker { data } => {
                assert_eq!(data.symbol, "BTCUSDT");
                assert_eq!(data.best_bid, Some("49999.00".to_string()));
                assert_eq!(data.best_ask, Some("50001.00".to_string()));
                assert_eq!(data.last_price, Some("50000.00".to_string()));
            }
            _ => panic!("Expected Ticker"),
        }
    }

    #[test]
    fn test_ticker_response_roundtrip() {
        let ticker = TickerResponse {
            symbol: "ETHUSDT".to_string(),
            best_bid: None,
            best_bid_qty: None,
            best_ask: None,
            best_ask_qty: None,
            last_price: None,
            last_qty: None,
            timestamp: 2000,
        };
        let json = serde_json::to_string(&ticker).expect("serialize");
        let parsed: TickerResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.symbol, "ETHUSDT");
        assert!(parsed.best_bid.is_none());
        assert!(parsed.last_price.is_none());
    }
}
