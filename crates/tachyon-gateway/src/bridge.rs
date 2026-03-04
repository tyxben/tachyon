use std::time::{SystemTime, UNIX_EPOCH};

use tachyon_core::{EngineEvent, OrderId, Symbol};
use tachyon_engine::Command;
use tokio::sync::{mpsc, oneshot};

/// A request sent from the gateway to the engine thread.
pub struct OrderRequest {
    pub command: Command,
    pub symbol: Symbol,
    pub account_id: u64,
    pub timestamp: u64,
    pub response_tx: oneshot::Sender<Vec<EngineEvent>>,
}

/// Async bridge between the gateway (tokio) and the engine (sync thread-per-symbol).
///
/// Commands are sent via an mpsc channel. The engine processes them
/// and returns results through a oneshot channel.
#[derive(Clone)]
pub struct EngineBridge {
    command_tx: mpsc::Sender<OrderRequest>,
}

impl EngineBridge {
    /// Creates a new bridge and returns the receiver end for the engine thread.
    pub fn new(capacity: usize) -> (Self, mpsc::Receiver<OrderRequest>) {
        let (tx, rx) = mpsc::channel(capacity);
        (EngineBridge { command_tx: tx }, rx)
    }

    /// Sends a place-order or modify-order command and awaits the engine response.
    pub async fn send_order(
        &self,
        command: Command,
        symbol: Symbol,
        account_id: u64,
    ) -> Result<Vec<EngineEvent>, BridgeError> {
        let (tx, rx) = oneshot::channel();
        let timestamp = current_timestamp();
        let request = OrderRequest {
            command,
            symbol,
            account_id,
            timestamp,
            response_tx: tx,
        };
        self.command_tx
            .send(request)
            .await
            .map_err(|_| BridgeError::EngineStopped)?;
        rx.await.map_err(|_| BridgeError::ResponseDropped)
    }

    /// Sends a cancel-order command and awaits the engine response.
    pub async fn send_cancel(
        &self,
        order_id: OrderId,
        symbol: Symbol,
        account_id: u64,
    ) -> Result<Vec<EngineEvent>, BridgeError> {
        self.send_order(Command::CancelOrder(order_id), symbol, account_id)
            .await
    }
}

/// Errors that can occur when communicating through the bridge.
#[derive(Debug)]
pub enum BridgeError {
    /// The engine thread has stopped and is no longer accepting commands.
    EngineStopped,
    /// The engine dropped the response channel without sending a response.
    ResponseDropped,
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::EngineStopped => write!(f, "engine has stopped"),
            BridgeError::ResponseDropped => write!(f, "engine response was dropped"),
        }
    }
}

impl std::error::Error for BridgeError {}

fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tachyon_core::*;

    #[tokio::test]
    async fn test_bridge_send_and_receive() {
        let (bridge, mut rx) = EngineBridge::new(16);

        let order = Order {
            id: OrderId::new(1),
            symbol: Symbol::new(0),
            side: Side::Buy,
            price: Price::new(100),
            quantity: Quantity::new(10),
            remaining_qty: Quantity::new(10),
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            timestamp: 0,
            account_id: 0,
            prev: NO_LINK,
            next: NO_LINK,
        };

        let bridge_clone = bridge.clone();
        let handle = tokio::spawn(async move {
            bridge_clone
                .send_order(Command::PlaceOrder(order), Symbol::new(0), 1)
                .await
        });

        // Receive on the engine side
        let request = rx.recv().await.expect("should receive request");
        assert!(matches!(request.command, Command::PlaceOrder(_)));
        assert_eq!(request.symbol, Symbol::new(0));
        assert_eq!(request.account_id, 1);

        // Send response back
        let events = vec![EngineEvent::OrderAccepted {
            order_id: OrderId::new(1),
            symbol: Symbol::new(0),
            side: Side::Buy,
            price: Price::new(100),
            qty: Quantity::new(10),
            timestamp: request.timestamp,
        }];
        request.response_tx.send(events).expect("send response");

        let result = handle.await.expect("join").expect("bridge result");
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], EngineEvent::OrderAccepted { .. }));
    }

    #[tokio::test]
    async fn test_bridge_cancel() {
        let (bridge, mut rx) = EngineBridge::new(16);

        let bridge_clone = bridge.clone();
        let handle = tokio::spawn(async move {
            bridge_clone
                .send_cancel(OrderId::new(42), Symbol::new(0), 1)
                .await
        });

        let request = rx.recv().await.expect("should receive request");
        assert!(matches!(request.command, Command::CancelOrder(_)));

        let events = vec![EngineEvent::OrderCancelled {
            order_id: OrderId::new(42),
            remaining_qty: Quantity::new(5),
            timestamp: request.timestamp,
        }];
        request.response_tx.send(events).expect("send response");

        let result = handle.await.expect("join").expect("bridge result");
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], EngineEvent::OrderCancelled { .. }));
    }

    #[tokio::test]
    async fn test_bridge_engine_stopped() {
        let (bridge, rx) = EngineBridge::new(16);
        drop(rx); // Simulate engine stopping

        let order = Order {
            id: OrderId::new(1),
            symbol: Symbol::new(0),
            side: Side::Buy,
            price: Price::new(100),
            quantity: Quantity::new(10),
            remaining_qty: Quantity::new(10),
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            timestamp: 0,
            account_id: 0,
            prev: NO_LINK,
            next: NO_LINK,
        };

        let result = bridge
            .send_order(Command::PlaceOrder(order), Symbol::new(0), 1)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_bridge_response_dropped() {
        let (bridge, mut rx) = EngineBridge::new(16);

        let bridge_clone = bridge.clone();
        let handle = tokio::spawn(async move {
            bridge_clone
                .send_cancel(OrderId::new(1), Symbol::new(0), 1)
                .await
        });

        // Receive but drop the response sender without responding
        let request = rx.recv().await.expect("should receive request");
        drop(request.response_tx);

        let result = handle.await.expect("join");
        assert!(result.is_err());
    }
}
