use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tachyon_core::{EngineEvent, OrderId, Symbol};
use tachyon_engine::Command;
use tachyon_io::SpscQueue;
use tokio::sync::oneshot;

/// A command sent through the SPSC queue to the engine thread.
pub struct EngineCommand {
    pub command: Command,
    pub symbol: Symbol,
    pub account_id: u64,
    pub timestamp: u64,
    pub response_tx: oneshot::Sender<Vec<EngineEvent>>,
}

/// Async bridge between the gateway (tokio) and the engine (dedicated OS thread per symbol).
///
/// Commands are sent via per-symbol SPSC queues. Each engine thread drains
/// its SPSC queue and returns results through a tokio oneshot channel.
#[derive(Clone)]
pub struct EngineBridge {
    /// Per-symbol SPSC queue: gateway is the producer, engine thread is the consumer.
    queues: Arc<HashMap<u32, Arc<SpscQueue<EngineCommand>>>>,
}

impl EngineBridge {
    /// Creates a new bridge with one SPSC queue per symbol.
    ///
    /// `symbol_ids` is the list of symbol IDs to create queues for.
    /// `capacity` must be a power of two.
    ///
    /// Returns the bridge and a map of symbol_id -> Arc<SpscQueue> for the engine threads.
    pub fn new(
        symbol_ids: &[u32],
        capacity: usize,
    ) -> (Self, HashMap<u32, Arc<SpscQueue<EngineCommand>>>) {
        let mut queues = HashMap::new();
        let mut engine_queues = HashMap::new();

        for &id in symbol_ids {
            let q = Arc::new(SpscQueue::new(capacity));
            queues.insert(id, Arc::clone(&q));
            engine_queues.insert(id, q);
        }

        (
            EngineBridge {
                queues: Arc::new(queues),
            },
            engine_queues,
        )
    }

    /// Sends a place-order or modify-order command and awaits the engine response.
    pub async fn send_order(
        &self,
        command: Command,
        symbol: Symbol,
        account_id: u64,
    ) -> Result<Vec<EngineEvent>, BridgeError> {
        let queue = self
            .queues
            .get(&symbol.raw())
            .ok_or(BridgeError::UnknownSymbol)?;

        let (tx, rx) = oneshot::channel();
        let timestamp = current_timestamp();
        let cmd = EngineCommand {
            command,
            symbol,
            account_id,
            timestamp,
            response_tx: tx,
        };

        // Spin-push with timeout: back-pressure if the SPSC queue is full.
        // In production the queue should be sized large enough that this rarely happens.
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(100);
        let mut cmd = cmd;
        loop {
            match queue.try_push(cmd) {
                Ok(()) => break,
                Err(returned) => {
                    cmd = returned;
                    if tokio::time::Instant::now() >= deadline {
                        return Err(BridgeError::QueueFull);
                    }
                    // Yield to the tokio runtime so we don't block the executor.
                    tokio::task::yield_now().await;
                }
            }
        }

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
    /// The target symbol has no engine queue (unknown symbol).
    UnknownSymbol,
    /// The engine dropped the response channel without sending a response.
    ResponseDropped,
    /// The SPSC queue is full and the push timed out.
    QueueFull,
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::UnknownSymbol => write!(f, "unknown symbol (no engine queue)"),
            BridgeError::ResponseDropped => write!(f, "engine response was dropped"),
            BridgeError::QueueFull => write!(f, "engine queue full (backpressure timeout)"),
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
        let (bridge, engine_queues) = EngineBridge::new(&[0], 16);
        let q = engine_queues.get(&0).unwrap().clone();

        let bridge_clone = bridge.clone();
        let handle = tokio::spawn(async move {
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
            bridge_clone
                .send_order(Command::PlaceOrder(order), Symbol::new(0), 1)
                .await
        });

        // Spin-wait for the command to arrive
        let cmd = loop {
            if let Some(cmd) = q.try_pop() {
                break cmd;
            }
            tokio::task::yield_now().await;
        };

        assert!(matches!(cmd.command, Command::PlaceOrder(_)));
        assert_eq!(cmd.symbol, Symbol::new(0));
        assert_eq!(cmd.account_id, 1);

        // Send response back
        let events = vec![EngineEvent::OrderAccepted {
            order_id: OrderId::new(1),
            symbol: Symbol::new(0),
            side: Side::Buy,
            price: Price::new(100),
            qty: Quantity::new(10),
            timestamp: cmd.timestamp,
        }];
        cmd.response_tx.send(events).expect("send response");

        let result = handle.await.expect("join").expect("bridge result");
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], EngineEvent::OrderAccepted { .. }));
    }

    #[tokio::test]
    async fn test_bridge_cancel() {
        let (bridge, engine_queues) = EngineBridge::new(&[0], 16);
        let q = engine_queues.get(&0).unwrap().clone();

        let bridge_clone = bridge.clone();
        let handle = tokio::spawn(async move {
            bridge_clone
                .send_cancel(OrderId::new(42), Symbol::new(0), 1)
                .await
        });

        let cmd = loop {
            if let Some(cmd) = q.try_pop() {
                break cmd;
            }
            tokio::task::yield_now().await;
        };

        assert!(matches!(cmd.command, Command::CancelOrder(_)));

        let events = vec![EngineEvent::OrderCancelled {
            order_id: OrderId::new(42),
            remaining_qty: Quantity::new(5),
            timestamp: cmd.timestamp,
        }];
        cmd.response_tx.send(events).expect("send response");

        let result = handle.await.expect("join").expect("bridge result");
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], EngineEvent::OrderCancelled { .. }));
    }

    #[tokio::test]
    async fn test_bridge_unknown_symbol() {
        let (bridge, _) = EngineBridge::new(&[0], 16);

        let order = Order {
            id: OrderId::new(1),
            symbol: Symbol::new(99),
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
            .send_order(Command::PlaceOrder(order), Symbol::new(99), 1)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_bridge_response_dropped() {
        let (bridge, engine_queues) = EngineBridge::new(&[0], 16);
        let q = engine_queues.get(&0).unwrap().clone();

        let bridge_clone = bridge.clone();
        let handle = tokio::spawn(async move {
            bridge_clone
                .send_cancel(OrderId::new(1), Symbol::new(0), 1)
                .await
        });

        // Receive but drop the response sender without responding
        let cmd = loop {
            if let Some(cmd) = q.try_pop() {
                break cmd;
            }
            tokio::task::yield_now().await;
        };
        drop(cmd.response_tx);

        let result = handle.await.expect("join");
        assert!(result.is_err());
    }
}
