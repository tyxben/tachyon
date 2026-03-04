//! TCP server for the Tachyon binary wire protocol.
//!
//! Accepts TCP connections, decodes binary-framed messages using `tachyon_proto::ServerCodec`,
//! routes commands through the existing `EngineBridge`, and sends encoded responses back.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_util::codec::Framed;

use tachyon_engine::Command;
use tachyon_proto::msg::{engine_event_to_server_msg, new_order_to_core};
use tachyon_proto::{
    CancelOrder as ProtoCancelOrder, ClientMessage, Frame, ModifyOrder as ProtoModifyOrder,
    ServerCodec, ServerMessage,
};

use crate::bridge::EngineBridge;

/// Shared state for the TCP server.
#[derive(Clone)]
pub struct TcpState {
    pub bridge: Arc<EngineBridge>,
    /// Atomic sequence counter for outbound server messages.
    pub seq_counter: Arc<AtomicU64>,
    /// Current number of active TCP connections.
    pub active_connections: Arc<AtomicU64>,
    /// Maximum allowed TCP connections (0 = unlimited).
    pub max_connections: u64,
    /// Maps order_id -> symbol for cancel/modify routing (shared with REST).
    pub order_registry: Arc<std::sync::RwLock<HashMap<u64, tachyon_core::Symbol>>>,
}

/// Starts the TCP binary protocol server.
///
/// Listens on the given address and spawns a task per connection.
/// Each connection decodes client frames, routes them through the
/// `EngineBridge`, and sends encoded server frames back.
pub async fn run_tcp_server(listener: TcpListener, state: TcpState) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                // Atomic increment-then-check to avoid TOCTOU race on connection limits.
                if state.max_connections > 0 {
                    let prev = state.active_connections.fetch_add(1, Ordering::AcqRel);
                    if prev >= state.max_connections {
                        state.active_connections.fetch_sub(1, Ordering::AcqRel);
                        tracing::warn!(
                            peer = %addr,
                            current = prev,
                            max = state.max_connections,
                            "TCP connection rejected: limit reached"
                        );
                        drop(stream);
                        continue;
                    }
                } else {
                    state.active_connections.fetch_add(1, Ordering::Relaxed);
                }
                tracing::info!(peer = %addr, "TCP binary client connected");

                let conn_state = state.clone();
                tokio::spawn(async move {
                    handle_connection(stream, conn_state.clone()).await;
                    conn_state
                        .active_connections
                        .fetch_sub(1, Ordering::Relaxed);
                    tracing::info!(peer = %addr, "TCP binary client disconnected");
                });
            }
            Err(e) => {
                tracing::error!(error = %e, "TCP accept error");
            }
        }
    }
}

/// Handles a single TCP connection.
async fn handle_connection(stream: tokio::net::TcpStream, state: TcpState) {
    // Disable Nagle's algorithm for low latency
    let _ = stream.set_nodelay(true);

    let mut framed = Framed::new(stream, ServerCodec);

    while let Some(result) = framed.next().await {
        match result {
            Ok(client_frame) => {
                let responses = process_client_frame(client_frame, &state).await;
                for resp_frame in responses {
                    if framed.send(resp_frame).await.is_err() {
                        return; // Connection broken
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "TCP frame decode error");
                return; // Protocol error, close connection
            }
        }
    }
}

/// Process a single client frame and return server response frames.
async fn process_client_frame(
    client_frame: Frame<ClientMessage>,
    state: &TcpState,
) -> Vec<Frame<ServerMessage>> {
    match client_frame.message {
        ClientMessage::NewOrder(ref new_order) => {
            let order = match new_order_to_core(new_order) {
                Ok(o) => o,
                Err(e) => {
                    tracing::warn!(error = %e, "Invalid NewOrder from TCP client");
                    return vec![Frame {
                        seq_num: next_seq(&state.seq_counter),
                        message: ServerMessage::OrderRejected(tachyon_proto::OrderRejected {
                            order_id: 0,
                            reason: tachyon_proto::msg::REJECT_INVALID_PRICE,
                            timestamp: current_timestamp(),
                        }),
                    }];
                }
            };

            let symbol = order.symbol;
            let account_id = order.account_id;
            let command = Command::PlaceOrder(order);

            match state.bridge.send_order(command, symbol, account_id).await {
                Ok(events) => events_to_frames(events, &state.seq_counter),
                Err(e) => {
                    tracing::warn!(error = %e, "Bridge error for NewOrder");
                    vec![Frame {
                        seq_num: next_seq(&state.seq_counter),
                        message: ServerMessage::OrderRejected(tachyon_proto::OrderRejected {
                            order_id: 0,
                            reason: tachyon_proto::msg::REJECT_UNKNOWN,
                            timestamp: current_timestamp(),
                        }),
                    }]
                }
            }
        }

        ClientMessage::CancelOrder(ProtoCancelOrder {
            order_id,
            symbol_id: _,
        }) => {
            let oid = tachyon_core::OrderId::new(order_id);

            // Look up the correct symbol from order_registry instead of
            // trusting the client-supplied symbol_id.
            let symbol = {
                let registry = state.order_registry.read().unwrap();
                registry.get(&order_id).copied()
            };

            let symbol = match symbol {
                Some(s) => s,
                None => {
                    return vec![Frame {
                        seq_num: next_seq(&state.seq_counter),
                        message: ServerMessage::OrderRejected(tachyon_proto::OrderRejected {
                            order_id,
                            reason: tachyon_proto::msg::REJECT_UNKNOWN,
                            timestamp: current_timestamp(),
                        }),
                    }];
                }
            };

            match state.bridge.send_cancel(oid, symbol, 0).await {
                Ok(events) => events_to_frames(events, &state.seq_counter),
                Err(e) => {
                    tracing::warn!(error = %e, order_id, "Bridge error for CancelOrder");
                    Vec::new()
                }
            }
        }

        ClientMessage::ModifyOrder(ProtoModifyOrder {
            order_id,
            symbol_id: _,
            new_price,
            new_quantity,
        }) => {
            // Look up the correct symbol from order_registry instead of
            // trusting the client-supplied symbol_id.
            let symbol = {
                let registry = state.order_registry.read().unwrap();
                registry.get(&order_id).copied()
            };

            let symbol = match symbol {
                Some(s) => s,
                None => {
                    return vec![Frame {
                        seq_num: next_seq(&state.seq_counter),
                        message: ServerMessage::OrderRejected(tachyon_proto::OrderRejected {
                            order_id,
                            reason: tachyon_proto::msg::REJECT_UNKNOWN,
                            timestamp: current_timestamp(),
                        }),
                    }];
                }
            };

            let command = Command::ModifyOrder {
                order_id: tachyon_core::OrderId::new(order_id),
                new_price: Some(tachyon_core::Price::new(new_price)),
                new_qty: Some(tachyon_core::Quantity::new(new_quantity)),
            };

            match state.bridge.send_order(command, symbol, 0).await {
                Ok(events) => events_to_frames(events, &state.seq_counter),
                Err(e) => {
                    tracing::warn!(error = %e, order_id, "Bridge error for ModifyOrder");
                    Vec::new()
                }
            }
        }

        ClientMessage::Heartbeat => {
            vec![Frame {
                seq_num: next_seq(&state.seq_counter),
                message: ServerMessage::Heartbeat,
            }]
        }
    }
}

/// Convert engine events to server message frames.
fn events_to_frames(
    events: Vec<tachyon_core::EngineEvent>,
    seq_counter: &AtomicU64,
) -> Vec<Frame<ServerMessage>> {
    let mut frames = Vec::new();
    for event in &events {
        if let Some(msg) = engine_event_to_server_msg(event) {
            frames.push(Frame {
                seq_num: next_seq(seq_counter),
                message: msg,
            });
        }
    }
    frames
}

/// Get the next sequence number.
fn next_seq(counter: &AtomicU64) -> u64 {
    counter.fetch_add(1, Ordering::Relaxed)
}

/// Current timestamp in nanoseconds since epoch.
fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::RwLock;
    use tachyon_proto::msg::*;
    use tachyon_proto::ClientCodec;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_util::bytes::BytesMut;
    use tokio_util::codec::{Decoder, Encoder};

    fn empty_order_registry() -> Arc<RwLock<HashMap<u64, tachyon_core::Symbol>>> {
        Arc::new(RwLock::new(HashMap::new()))
    }

    /// Integration test: send a binary NewOrder over TCP, receive OrderAccepted/Trade responses.
    #[tokio::test]
    async fn test_tcp_binary_new_order_integration() {
        // Set up a mock engine bridge with one symbol
        let (bridge, engine_queues) = EngineBridge::new(&[0], 16);
        let q = engine_queues.get(&0).unwrap().clone();
        let state = TcpState {
            bridge: Arc::new(bridge),
            seq_counter: Arc::new(AtomicU64::new(1)),
            active_connections: Arc::new(AtomicU64::new(0)),
            max_connections: 0,
            order_registry: empty_order_registry(),
        };

        // Bind TCP listener on an ephemeral port
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Spawn the TCP server
        let server_state = state.clone();
        tokio::spawn(async move {
            run_tcp_server(listener, server_state).await;
        });

        // Connect a client
        let mut client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let _ = client_stream.set_nodelay(true);

        // Encode a NewOrder frame
        let client_frame = Frame {
            seq_num: 1,
            message: ClientMessage::NewOrder(NewOrder {
                side: SIDE_BUY,
                order_type: ORDER_TYPE_LIMIT,
                time_in_force: TIF_GTC,
                gtd_timestamp: 0,
                symbol_id: 0,
                price: 10000,
                quantity: 100,
                account_id: 7,
            }),
        };
        let mut client_codec = ClientCodec;
        let mut wire_buf = BytesMut::new();
        client_codec.encode(client_frame, &mut wire_buf).unwrap();
        client_stream.write_all(&wire_buf).await.unwrap();

        // Simulate the engine thread: read the command from the SPSC queue
        // and send back an OrderAccepted event
        let cmd = loop {
            if let Some(cmd) = q.try_pop() {
                break cmd;
            }
            tokio::task::yield_now().await;
        };

        assert!(matches!(cmd.command, Command::PlaceOrder(_)));
        let events = vec![tachyon_core::EngineEvent::OrderAccepted {
            order_id: tachyon_core::OrderId::new(42),
            symbol: tachyon_core::Symbol::new(0),
            side: tachyon_core::Side::Buy,
            price: tachyon_core::Price::new(10000),
            qty: tachyon_core::Quantity::new(100),
            timestamp: 1234567890,
        }];
        cmd.response_tx.send(events).unwrap();

        // Read the response from the server
        let mut resp_buf = vec![0u8; 256];
        let n = client_stream.read(&mut resp_buf).await.unwrap();
        assert!(n > 0, "should receive response bytes");

        // Decode the response
        let mut decode_buf = BytesMut::from(&resp_buf[..n]);
        let mut server_codec_for_client = ClientCodec;
        let server_frame = server_codec_for_client
            .decode(&mut decode_buf)
            .unwrap()
            .expect("should decode a server frame");

        match server_frame.message {
            ServerMessage::OrderAccepted(accepted) => {
                assert_eq!(accepted.order_id, 42);
                assert_eq!(accepted.symbol_id, 0);
                assert_eq!(accepted.side, SIDE_BUY);
                assert_eq!(accepted.price, 10000);
                assert_eq!(accepted.quantity, 100);
            }
            other => panic!("Expected OrderAccepted, got {:?}", other),
        }
    }

    /// Integration test: send a heartbeat, receive a heartbeat back.
    #[tokio::test]
    async fn test_tcp_binary_heartbeat() {
        let (bridge, _queues) = EngineBridge::new(&[0], 16);
        let state = TcpState {
            bridge: Arc::new(bridge),
            seq_counter: Arc::new(AtomicU64::new(1)),
            active_connections: Arc::new(AtomicU64::new(0)),
            max_connections: 0,
            order_registry: empty_order_registry(),
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            run_tcp_server(listener, state).await;
        });

        let mut client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();

        // Send heartbeat
        let heartbeat_frame = Frame {
            seq_num: 0,
            message: ClientMessage::Heartbeat,
        };
        let mut client_codec = ClientCodec;
        let mut wire_buf = BytesMut::new();
        client_codec.encode(heartbeat_frame, &mut wire_buf).unwrap();
        client_stream.write_all(&wire_buf).await.unwrap();

        // Read response
        let mut resp_buf = vec![0u8; 256];
        let n = client_stream.read(&mut resp_buf).await.unwrap();
        assert!(n > 0);

        let mut decode_buf = BytesMut::from(&resp_buf[..n]);
        let mut client_decoder = ClientCodec;
        let server_frame = client_decoder
            .decode(&mut decode_buf)
            .unwrap()
            .expect("should decode heartbeat response");

        assert_eq!(server_frame.message, ServerMessage::Heartbeat);
    }

    /// Integration test: send a CancelOrder for a known symbol.
    #[tokio::test]
    async fn test_tcp_binary_cancel_order_integration() {
        let (bridge, engine_queues) = EngineBridge::new(&[0], 16);
        let q = engine_queues.get(&0).unwrap().clone();
        let order_registry = empty_order_registry();
        // Pre-register order 99 with symbol 0 so the cancel handler can find it.
        order_registry
            .write()
            .unwrap()
            .insert(99, tachyon_core::Symbol::new(0));
        let state = TcpState {
            bridge: Arc::new(bridge),
            seq_counter: Arc::new(AtomicU64::new(1)),
            active_connections: Arc::new(AtomicU64::new(0)),
            max_connections: 0,
            order_registry,
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            run_tcp_server(listener, state).await;
        });

        let mut client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();

        // Send CancelOrder
        let cancel_frame = Frame {
            seq_num: 5,
            message: ClientMessage::CancelOrder(ProtoCancelOrder {
                order_id: 99,
                symbol_id: 0,
            }),
        };
        let mut client_codec = ClientCodec;
        let mut wire_buf = BytesMut::new();
        client_codec.encode(cancel_frame, &mut wire_buf).unwrap();
        client_stream.write_all(&wire_buf).await.unwrap();

        // Simulate engine: receive command and reply with OrderCancelled
        let cmd = loop {
            if let Some(cmd) = q.try_pop() {
                break cmd;
            }
            tokio::task::yield_now().await;
        };
        assert!(matches!(cmd.command, Command::CancelOrder(_)));
        let events = vec![tachyon_core::EngineEvent::OrderCancelled {
            order_id: tachyon_core::OrderId::new(99),
            remaining_qty: tachyon_core::Quantity::new(50),
            timestamp: 12345,
        }];
        cmd.response_tx.send(events).unwrap();

        // Read response
        let mut resp_buf = vec![0u8; 256];
        let n = client_stream.read(&mut resp_buf).await.unwrap();
        assert!(n > 0);

        let mut decode_buf = BytesMut::from(&resp_buf[..n]);
        let mut client_decoder = ClientCodec;
        let frame = client_decoder
            .decode(&mut decode_buf)
            .unwrap()
            .expect("should decode response");

        match frame.message {
            ServerMessage::OrderCancelled(cancelled) => {
                assert_eq!(cancelled.order_id, 99);
                assert_eq!(cancelled.remaining_qty, 50);
            }
            other => panic!("Expected OrderCancelled, got {:?}", other),
        }
    }

    /// Integration test: NewOrder that generates both an Accept and a Trade.
    #[tokio::test]
    async fn test_tcp_binary_accept_and_trade() {
        let (bridge, engine_queues) = EngineBridge::new(&[0], 16);
        let q = engine_queues.get(&0).unwrap().clone();
        let state = TcpState {
            bridge: Arc::new(bridge),
            seq_counter: Arc::new(AtomicU64::new(1)),
            active_connections: Arc::new(AtomicU64::new(0)),
            max_connections: 0,
            order_registry: empty_order_registry(),
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            run_tcp_server(listener, state).await;
        });

        let mut client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let _ = client_stream.set_nodelay(true);

        // Send NewOrder
        let frame = Frame {
            seq_num: 1,
            message: ClientMessage::NewOrder(NewOrder {
                side: SIDE_BUY,
                order_type: ORDER_TYPE_LIMIT,
                time_in_force: TIF_GTC,
                gtd_timestamp: 0,
                symbol_id: 0,
                price: 10000,
                quantity: 100,
                account_id: 1,
            }),
        };
        let mut client_codec = ClientCodec;
        let mut wire_buf = BytesMut::new();
        client_codec.encode(frame, &mut wire_buf).unwrap();
        client_stream.write_all(&wire_buf).await.unwrap();

        // Simulate engine: accept + trade
        let cmd = loop {
            if let Some(cmd) = q.try_pop() {
                break cmd;
            }
            tokio::task::yield_now().await;
        };

        let events = vec![
            tachyon_core::EngineEvent::OrderAccepted {
                order_id: tachyon_core::OrderId::new(1),
                symbol: tachyon_core::Symbol::new(0),
                side: tachyon_core::Side::Buy,
                price: tachyon_core::Price::new(10000),
                qty: tachyon_core::Quantity::new(100),
                timestamp: 1000,
            },
            tachyon_core::EngineEvent::Trade {
                trade: tachyon_core::Trade {
                    trade_id: 500,
                    symbol: tachyon_core::Symbol::new(0),
                    price: tachyon_core::Price::new(10000),
                    quantity: tachyon_core::Quantity::new(100),
                    maker_order_id: tachyon_core::OrderId::new(10),
                    taker_order_id: tachyon_core::OrderId::new(1),
                    maker_side: tachyon_core::Side::Sell,
                    timestamp: 1000,
                },
            },
        ];
        cmd.response_tx.send(events).unwrap();

        // Read responses -- may need multiple reads to get both frames
        let mut decode_buf = BytesMut::new();
        let mut client_decoder = ClientCodec;
        let mut frames: Vec<Frame<ServerMessage>> = Vec::new();

        while frames.len() < 2 {
            let mut tmp = vec![0u8; 1024];
            let n = client_stream.read(&mut tmp).await.unwrap();
            assert!(n > 0, "connection closed before all frames received");
            decode_buf.extend_from_slice(&tmp[..n]);

            while let Some(frame) = client_decoder.decode(&mut decode_buf).unwrap() {
                frames.push(frame);
            }
        }

        assert_eq!(frames.len(), 2);

        // First frame: OrderAccepted
        match &frames[0].message {
            ServerMessage::OrderAccepted(a) => {
                assert_eq!(a.order_id, 1);
            }
            other => panic!("Expected OrderAccepted, got {:?}", other),
        }

        // Second frame: Trade
        match &frames[1].message {
            ServerMessage::Trade(t) => {
                assert_eq!(t.trade_id, 500);
                assert_eq!(t.maker_order_id, 10);
                assert_eq!(t.taker_order_id, 1);
                assert_eq!(t.maker_side, SIDE_SELL);
            }
            other => panic!("Expected Trade, got {:?}", other),
        }
    }

    /// Test connection limits: reject when at max.
    #[tokio::test]
    async fn test_tcp_connection_limit() {
        let (bridge, _queues) = EngineBridge::new(&[0], 16);
        let state = TcpState {
            bridge: Arc::new(bridge),
            seq_counter: Arc::new(AtomicU64::new(1)),
            active_connections: Arc::new(AtomicU64::new(0)),
            max_connections: 1,
            order_registry: empty_order_registry(),
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            run_tcp_server(listener, state).await;
        });

        // First connection should succeed
        let conn1 = tokio::net::TcpStream::connect(addr).await.unwrap();

        // Give the server a moment to register the connection
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Second connection: the server should drop it immediately
        let conn2 = tokio::net::TcpStream::connect(addr).await;
        if let Ok(mut stream2) = conn2 {
            // The server accepted the TCP socket at OS level but drops it immediately.
            // We should see the stream close (read returns 0 or error).
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let mut buf = vec![0u8; 16];
            let result = stream2.read(&mut buf).await;
            match result {
                Ok(0) => {} // Connection closed as expected
                Ok(_) => panic!("Expected connection to be closed by server"),
                Err(_) => {} // Also acceptable -- connection reset
            }
        }

        // Drop first connection to free up the slot
        drop(conn1);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Third connection should now succeed since the slot is free
        let conn3 = tokio::net::TcpStream::connect(addr).await;
        assert!(
            conn3.is_ok(),
            "Third connection should succeed after first was dropped"
        );
    }

    /// Test that TCP cancel for an order not in the registry gets rejected.
    #[tokio::test]
    async fn test_tcp_cancel_unknown_order_rejected() {
        let (bridge, _engine_queues) = EngineBridge::new(&[0], 16);
        // Order registry is empty -- order 999 is not registered
        let state = TcpState {
            bridge: Arc::new(bridge),
            seq_counter: Arc::new(AtomicU64::new(1)),
            active_connections: Arc::new(AtomicU64::new(0)),
            max_connections: 0,
            order_registry: empty_order_registry(),
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            run_tcp_server(listener, state).await;
        });

        let mut client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let _ = client_stream.set_nodelay(true);

        // Send CancelOrder for an order that does not exist in the registry
        let cancel_frame = Frame {
            seq_num: 1,
            message: ClientMessage::CancelOrder(ProtoCancelOrder {
                order_id: 999,
                symbol_id: 0, // Client claims symbol 0, but order is not registered
            }),
        };
        let mut client_codec = ClientCodec;
        let mut wire_buf = BytesMut::new();
        client_codec.encode(cancel_frame, &mut wire_buf).unwrap();
        client_stream.write_all(&wire_buf).await.unwrap();

        // Read the response -- should be OrderRejected
        let mut resp_buf = vec![0u8; 256];
        let n = client_stream.read(&mut resp_buf).await.unwrap();
        assert!(n > 0);

        let mut decode_buf = BytesMut::from(&resp_buf[..n]);
        let mut client_decoder = ClientCodec;
        let frame = client_decoder
            .decode(&mut decode_buf)
            .unwrap()
            .expect("should decode response");

        match frame.message {
            ServerMessage::OrderRejected(rejected) => {
                assert_eq!(rejected.order_id, 999);
                assert_eq!(rejected.reason, REJECT_UNKNOWN);
            }
            other => panic!("Expected OrderRejected, got {:?}", other),
        }
    }

    /// Test that TCP modify for an order not in the registry gets rejected.
    #[tokio::test]
    async fn test_tcp_modify_unknown_order_rejected() {
        let (bridge, _engine_queues) = EngineBridge::new(&[0], 16);
        let state = TcpState {
            bridge: Arc::new(bridge),
            seq_counter: Arc::new(AtomicU64::new(1)),
            active_connections: Arc::new(AtomicU64::new(0)),
            max_connections: 0,
            order_registry: empty_order_registry(),
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            run_tcp_server(listener, state).await;
        });

        let mut client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let _ = client_stream.set_nodelay(true);

        // Send ModifyOrder for an order not in the registry
        let modify_frame = Frame {
            seq_num: 1,
            message: ClientMessage::ModifyOrder(ProtoModifyOrder {
                order_id: 888,
                symbol_id: 0,
                new_price: 12345,
                new_quantity: 50,
            }),
        };
        let mut client_codec = ClientCodec;
        let mut wire_buf = BytesMut::new();
        client_codec.encode(modify_frame, &mut wire_buf).unwrap();
        client_stream.write_all(&wire_buf).await.unwrap();

        // Read the response -- should be OrderRejected
        let mut resp_buf = vec![0u8; 256];
        let n = client_stream.read(&mut resp_buf).await.unwrap();
        assert!(n > 0);

        let mut decode_buf = BytesMut::from(&resp_buf[..n]);
        let mut client_decoder = ClientCodec;
        let frame = client_decoder
            .decode(&mut decode_buf)
            .unwrap()
            .expect("should decode response");

        match frame.message {
            ServerMessage::OrderRejected(rejected) => {
                assert_eq!(rejected.order_id, 888);
                assert_eq!(rejected.reason, REJECT_UNKNOWN);
            }
            other => panic!("Expected OrderRejected, got {:?}", other),
        }
    }
}
