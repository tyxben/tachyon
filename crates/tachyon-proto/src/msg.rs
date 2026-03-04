//! Binary message types for the Tachyon wire protocol.
//!
//! All multi-byte integers are little-endian (matching x86 native byte order).

use crate::error::ProtoError;

// ---------------------------------------------------------------------------
// Message type discriminants
// ---------------------------------------------------------------------------

/// Client -> Server message types.
pub const MSG_NEW_ORDER: u8 = 0x01;
pub const MSG_CANCEL_ORDER: u8 = 0x02;
pub const MSG_MODIFY_ORDER: u8 = 0x03;
pub const MSG_CLIENT_HEARTBEAT: u8 = 0x10;

/// Server -> Client message types.
pub const MSG_ORDER_ACCEPTED: u8 = 0x81;
pub const MSG_ORDER_REJECTED: u8 = 0x82;
pub const MSG_ORDER_CANCELLED: u8 = 0x83;
pub const MSG_TRADE: u8 = 0x84;
pub const MSG_SERVER_HEARTBEAT: u8 = 0x90;

// ---------------------------------------------------------------------------
// Side / OrderType / TimeInForce wire encodings
// ---------------------------------------------------------------------------

pub const SIDE_BUY: u8 = 0;
pub const SIDE_SELL: u8 = 1;

pub const ORDER_TYPE_LIMIT: u8 = 0;
pub const ORDER_TYPE_MARKET: u8 = 1;

pub const TIF_GTC: u8 = 0;
pub const TIF_IOC: u8 = 1;
pub const TIF_FOK: u8 = 2;
pub const TIF_POST_ONLY: u8 = 3;

// ---------------------------------------------------------------------------
// Reject reason wire encoding
// ---------------------------------------------------------------------------

pub const REJECT_INSUFFICIENT_LIQUIDITY: u8 = 0;
pub const REJECT_PRICE_OUT_OF_RANGE: u8 = 1;
pub const REJECT_INVALID_QUANTITY: u8 = 2;
pub const REJECT_INVALID_PRICE: u8 = 3;
pub const REJECT_BOOK_FULL: u8 = 4;
pub const REJECT_RATE_LIMIT: u8 = 5;
pub const REJECT_SELF_TRADE: u8 = 6;
pub const REJECT_POST_ONLY_WOULD_TAKE: u8 = 7;
pub const REJECT_DUPLICATE_ORDER_ID: u8 = 8;
pub const REJECT_UNKNOWN: u8 = 0xFF;

// ---------------------------------------------------------------------------
// Client -> Server messages
// ---------------------------------------------------------------------------

/// NewOrder: side(1B) + order_type(1B) + tif(1B) + symbol_id(4B) + price(8B) + qty(8B) + account_id(8B) = 31 bytes
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOrder {
    pub side: u8,
    pub order_type: u8,
    pub time_in_force: u8,
    pub symbol_id: u32,
    pub price: i64,
    pub quantity: u64,
    pub account_id: u64,
}

impl NewOrder {
    pub const WIRE_SIZE: usize = 1 + 1 + 1 + 4 + 8 + 8 + 8; // 31

    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, ProtoError> {
        if buf.len() < Self::WIRE_SIZE {
            return Err(ProtoError::BufferTooSmall);
        }
        buf[0] = self.side;
        buf[1] = self.order_type;
        buf[2] = self.time_in_force;
        buf[3..7].copy_from_slice(&self.symbol_id.to_le_bytes());
        buf[7..15].copy_from_slice(&self.price.to_le_bytes());
        buf[15..23].copy_from_slice(&self.quantity.to_le_bytes());
        buf[23..31].copy_from_slice(&self.account_id.to_le_bytes());
        Ok(Self::WIRE_SIZE)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < Self::WIRE_SIZE {
            return Err(ProtoError::BufferTooSmall);
        }
        Ok(NewOrder {
            side: buf[0],
            order_type: buf[1],
            time_in_force: buf[2],
            symbol_id: u32::from_le_bytes([buf[3], buf[4], buf[5], buf[6]]),
            price: i64::from_le_bytes([
                buf[7], buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14],
            ]),
            quantity: u64::from_le_bytes([
                buf[15], buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22],
            ]),
            account_id: u64::from_le_bytes([
                buf[23], buf[24], buf[25], buf[26], buf[27], buf[28], buf[29], buf[30],
            ]),
        })
    }
}

/// CancelOrder: order_id(8B) + symbol_id(4B) = 12 bytes
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelOrder {
    pub order_id: u64,
    pub symbol_id: u32,
}

impl CancelOrder {
    pub const WIRE_SIZE: usize = 8 + 4; // 12

    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, ProtoError> {
        if buf.len() < Self::WIRE_SIZE {
            return Err(ProtoError::BufferTooSmall);
        }
        buf[0..8].copy_from_slice(&self.order_id.to_le_bytes());
        buf[8..12].copy_from_slice(&self.symbol_id.to_le_bytes());
        Ok(Self::WIRE_SIZE)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < Self::WIRE_SIZE {
            return Err(ProtoError::BufferTooSmall);
        }
        Ok(CancelOrder {
            order_id: u64::from_le_bytes([
                buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
            ]),
            symbol_id: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
        })
    }
}

/// ModifyOrder: order_id(8B) + symbol_id(4B) + new_price(8B) + new_qty(8B) = 28 bytes
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModifyOrder {
    pub order_id: u64,
    pub symbol_id: u32,
    pub new_price: i64,
    pub new_quantity: u64,
}

impl ModifyOrder {
    pub const WIRE_SIZE: usize = 8 + 4 + 8 + 8; // 28

    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, ProtoError> {
        if buf.len() < Self::WIRE_SIZE {
            return Err(ProtoError::BufferTooSmall);
        }
        buf[0..8].copy_from_slice(&self.order_id.to_le_bytes());
        buf[8..12].copy_from_slice(&self.symbol_id.to_le_bytes());
        buf[12..20].copy_from_slice(&self.new_price.to_le_bytes());
        buf[20..28].copy_from_slice(&self.new_quantity.to_le_bytes());
        Ok(Self::WIRE_SIZE)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < Self::WIRE_SIZE {
            return Err(ProtoError::BufferTooSmall);
        }
        Ok(ModifyOrder {
            order_id: u64::from_le_bytes([
                buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
            ]),
            symbol_id: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            new_price: i64::from_le_bytes([
                buf[12], buf[13], buf[14], buf[15], buf[16], buf[17], buf[18], buf[19],
            ]),
            new_quantity: u64::from_le_bytes([
                buf[20], buf[21], buf[22], buf[23], buf[24], buf[25], buf[26], buf[27],
            ]),
        })
    }
}

// ---------------------------------------------------------------------------
// Server -> Client messages
// ---------------------------------------------------------------------------

/// OrderAccepted: order_id(8B) + symbol_id(4B) + side(1B) + price(8B) + qty(8B) + timestamp(8B) = 37 bytes
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderAccepted {
    pub order_id: u64,
    pub symbol_id: u32,
    pub side: u8,
    pub price: i64,
    pub quantity: u64,
    pub timestamp: u64,
}

impl OrderAccepted {
    pub const WIRE_SIZE: usize = 8 + 4 + 1 + 8 + 8 + 8; // 37

    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, ProtoError> {
        if buf.len() < Self::WIRE_SIZE {
            return Err(ProtoError::BufferTooSmall);
        }
        buf[0..8].copy_from_slice(&self.order_id.to_le_bytes());
        buf[8..12].copy_from_slice(&self.symbol_id.to_le_bytes());
        buf[12] = self.side;
        buf[13..21].copy_from_slice(&self.price.to_le_bytes());
        buf[21..29].copy_from_slice(&self.quantity.to_le_bytes());
        buf[29..37].copy_from_slice(&self.timestamp.to_le_bytes());
        Ok(Self::WIRE_SIZE)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < Self::WIRE_SIZE {
            return Err(ProtoError::BufferTooSmall);
        }
        Ok(OrderAccepted {
            order_id: u64::from_le_bytes([
                buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
            ]),
            symbol_id: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            side: buf[12],
            price: i64::from_le_bytes([
                buf[13], buf[14], buf[15], buf[16], buf[17], buf[18], buf[19], buf[20],
            ]),
            quantity: u64::from_le_bytes([
                buf[21], buf[22], buf[23], buf[24], buf[25], buf[26], buf[27], buf[28],
            ]),
            timestamp: u64::from_le_bytes([
                buf[29], buf[30], buf[31], buf[32], buf[33], buf[34], buf[35], buf[36],
            ]),
        })
    }
}

/// OrderRejected: order_id(8B) + reason(1B) + timestamp(8B) = 17 bytes
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderRejected {
    pub order_id: u64,
    pub reason: u8,
    pub timestamp: u64,
}

impl OrderRejected {
    pub const WIRE_SIZE: usize = 8 + 1 + 8; // 17

    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, ProtoError> {
        if buf.len() < Self::WIRE_SIZE {
            return Err(ProtoError::BufferTooSmall);
        }
        buf[0..8].copy_from_slice(&self.order_id.to_le_bytes());
        buf[8] = self.reason;
        buf[9..17].copy_from_slice(&self.timestamp.to_le_bytes());
        Ok(Self::WIRE_SIZE)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < Self::WIRE_SIZE {
            return Err(ProtoError::BufferTooSmall);
        }
        Ok(OrderRejected {
            order_id: u64::from_le_bytes([
                buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
            ]),
            reason: buf[8],
            timestamp: u64::from_le_bytes([
                buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15], buf[16],
            ]),
        })
    }
}

/// OrderCancelled: order_id(8B) + remaining_qty(8B) + timestamp(8B) = 24 bytes
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderCancelled {
    pub order_id: u64,
    pub remaining_qty: u64,
    pub timestamp: u64,
}

impl OrderCancelled {
    pub const WIRE_SIZE: usize = 8 + 8 + 8; // 24

    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, ProtoError> {
        if buf.len() < Self::WIRE_SIZE {
            return Err(ProtoError::BufferTooSmall);
        }
        buf[0..8].copy_from_slice(&self.order_id.to_le_bytes());
        buf[8..16].copy_from_slice(&self.remaining_qty.to_le_bytes());
        buf[16..24].copy_from_slice(&self.timestamp.to_le_bytes());
        Ok(Self::WIRE_SIZE)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < Self::WIRE_SIZE {
            return Err(ProtoError::BufferTooSmall);
        }
        Ok(OrderCancelled {
            order_id: u64::from_le_bytes([
                buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
            ]),
            remaining_qty: u64::from_le_bytes([
                buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
            ]),
            timestamp: u64::from_le_bytes([
                buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23],
            ]),
        })
    }
}

/// Trade: trade_id(8B) + symbol_id(4B) + price(8B) + qty(8B) + maker_id(8B) + taker_id(8B) + maker_side(1B) + timestamp(8B) = 53 bytes
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TradeMsg {
    pub trade_id: u64,
    pub symbol_id: u32,
    pub price: i64,
    pub quantity: u64,
    pub maker_order_id: u64,
    pub taker_order_id: u64,
    pub maker_side: u8,
    pub timestamp: u64,
}

impl TradeMsg {
    pub const WIRE_SIZE: usize = 8 + 4 + 8 + 8 + 8 + 8 + 1 + 8; // 53

    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, ProtoError> {
        if buf.len() < Self::WIRE_SIZE {
            return Err(ProtoError::BufferTooSmall);
        }
        buf[0..8].copy_from_slice(&self.trade_id.to_le_bytes());
        buf[8..12].copy_from_slice(&self.symbol_id.to_le_bytes());
        buf[12..20].copy_from_slice(&self.price.to_le_bytes());
        buf[20..28].copy_from_slice(&self.quantity.to_le_bytes());
        buf[28..36].copy_from_slice(&self.maker_order_id.to_le_bytes());
        buf[36..44].copy_from_slice(&self.taker_order_id.to_le_bytes());
        buf[44] = self.maker_side;
        buf[45..53].copy_from_slice(&self.timestamp.to_le_bytes());
        Ok(Self::WIRE_SIZE)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < Self::WIRE_SIZE {
            return Err(ProtoError::BufferTooSmall);
        }
        Ok(TradeMsg {
            trade_id: u64::from_le_bytes([
                buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
            ]),
            symbol_id: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            price: i64::from_le_bytes([
                buf[12], buf[13], buf[14], buf[15], buf[16], buf[17], buf[18], buf[19],
            ]),
            quantity: u64::from_le_bytes([
                buf[20], buf[21], buf[22], buf[23], buf[24], buf[25], buf[26], buf[27],
            ]),
            maker_order_id: u64::from_le_bytes([
                buf[28], buf[29], buf[30], buf[31], buf[32], buf[33], buf[34], buf[35],
            ]),
            taker_order_id: u64::from_le_bytes([
                buf[36], buf[37], buf[38], buf[39], buf[40], buf[41], buf[42], buf[43],
            ]),
            maker_side: buf[44],
            timestamp: u64::from_le_bytes([
                buf[45], buf[46], buf[47], buf[48], buf[49], buf[50], buf[51], buf[52],
            ]),
        })
    }
}

// ---------------------------------------------------------------------------
// Envelope: framed message with header
// ---------------------------------------------------------------------------

/// Header size: length(4B) + msg_type(1B) + seq_num(8B) = 13 bytes
pub const HEADER_SIZE: usize = 4 + 1 + 8;

/// Maximum payload size (4 MiB). Prevents allocation bombs from malformed frames.
pub const MAX_PAYLOAD_SIZE: u32 = 4 * 1024 * 1024;

/// Client -> Server message enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMessage {
    NewOrder(NewOrder),
    CancelOrder(CancelOrder),
    ModifyOrder(ModifyOrder),
    Heartbeat,
}

/// Server -> Client message enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMessage {
    OrderAccepted(OrderAccepted),
    OrderRejected(OrderRejected),
    OrderCancelled(OrderCancelled),
    Trade(TradeMsg),
    Heartbeat,
}

/// A framed message with sequence number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame<M> {
    pub seq_num: u64,
    pub message: M,
}

impl Frame<ClientMessage> {
    /// Encode a client frame into a buffer. Returns the total number of bytes written.
    pub fn encode_to_vec(&self) -> Vec<u8> {
        let (msg_type, payload_size) = match &self.message {
            ClientMessage::NewOrder(_) => (MSG_NEW_ORDER, NewOrder::WIRE_SIZE),
            ClientMessage::CancelOrder(_) => (MSG_CANCEL_ORDER, CancelOrder::WIRE_SIZE),
            ClientMessage::ModifyOrder(_) => (MSG_MODIFY_ORDER, ModifyOrder::WIRE_SIZE),
            ClientMessage::Heartbeat => (MSG_CLIENT_HEARTBEAT, 0),
        };

        let total_len = HEADER_SIZE + payload_size;
        let mut buf = vec![0u8; total_len];

        // Write header: length field covers msg_type + seq_num + payload
        let length = (1 + 8 + payload_size) as u32;
        buf[0..4].copy_from_slice(&length.to_le_bytes());
        buf[4] = msg_type;
        buf[5..13].copy_from_slice(&self.seq_num.to_le_bytes());

        // Write payload
        match &self.message {
            ClientMessage::NewOrder(m) => {
                m.encode(&mut buf[HEADER_SIZE..]).unwrap();
            }
            ClientMessage::CancelOrder(m) => {
                m.encode(&mut buf[HEADER_SIZE..]).unwrap();
            }
            ClientMessage::ModifyOrder(m) => {
                m.encode(&mut buf[HEADER_SIZE..]).unwrap();
            }
            ClientMessage::Heartbeat => {}
        }

        buf
    }

    /// Decode a client frame from a buffer that starts AFTER the length field.
    /// The `payload` slice must contain: msg_type(1B) + seq_num(8B) + payload.
    pub fn decode_body(payload: &[u8]) -> Result<Self, ProtoError> {
        if payload.len() < 9 {
            return Err(ProtoError::IncompleteMessage);
        }
        let msg_type = payload[0];
        let seq_num = u64::from_le_bytes([
            payload[1], payload[2], payload[3], payload[4], payload[5], payload[6], payload[7],
            payload[8],
        ]);
        let body = &payload[9..];

        let message = match msg_type {
            MSG_NEW_ORDER => ClientMessage::NewOrder(NewOrder::decode(body)?),
            MSG_CANCEL_ORDER => ClientMessage::CancelOrder(CancelOrder::decode(body)?),
            MSG_MODIFY_ORDER => ClientMessage::ModifyOrder(ModifyOrder::decode(body)?),
            MSG_CLIENT_HEARTBEAT => ClientMessage::Heartbeat,
            _ => return Err(ProtoError::UnknownMessageType(msg_type)),
        };

        Ok(Frame { seq_num, message })
    }
}

impl Frame<ServerMessage> {
    /// Encode a server frame into a buffer.
    pub fn encode_to_vec(&self) -> Vec<u8> {
        let (msg_type, payload_size) = match &self.message {
            ServerMessage::OrderAccepted(_) => (MSG_ORDER_ACCEPTED, OrderAccepted::WIRE_SIZE),
            ServerMessage::OrderRejected(_) => (MSG_ORDER_REJECTED, OrderRejected::WIRE_SIZE),
            ServerMessage::OrderCancelled(_) => (MSG_ORDER_CANCELLED, OrderCancelled::WIRE_SIZE),
            ServerMessage::Trade(_) => (MSG_TRADE, TradeMsg::WIRE_SIZE),
            ServerMessage::Heartbeat => (MSG_SERVER_HEARTBEAT, 0),
        };

        let total_len = HEADER_SIZE + payload_size;
        let mut buf = vec![0u8; total_len];

        let length = (1 + 8 + payload_size) as u32;
        buf[0..4].copy_from_slice(&length.to_le_bytes());
        buf[4] = msg_type;
        buf[5..13].copy_from_slice(&self.seq_num.to_le_bytes());

        match &self.message {
            ServerMessage::OrderAccepted(m) => {
                m.encode(&mut buf[HEADER_SIZE..]).unwrap();
            }
            ServerMessage::OrderRejected(m) => {
                m.encode(&mut buf[HEADER_SIZE..]).unwrap();
            }
            ServerMessage::OrderCancelled(m) => {
                m.encode(&mut buf[HEADER_SIZE..]).unwrap();
            }
            ServerMessage::Trade(m) => {
                m.encode(&mut buf[HEADER_SIZE..]).unwrap();
            }
            ServerMessage::Heartbeat => {}
        }

        buf
    }

    /// Decode a server frame from a buffer that starts AFTER the length field.
    pub fn decode_body(payload: &[u8]) -> Result<Self, ProtoError> {
        if payload.len() < 9 {
            return Err(ProtoError::IncompleteMessage);
        }
        let msg_type = payload[0];
        let seq_num = u64::from_le_bytes([
            payload[1], payload[2], payload[3], payload[4], payload[5], payload[6], payload[7],
            payload[8],
        ]);
        let body = &payload[9..];

        let message = match msg_type {
            MSG_ORDER_ACCEPTED => ServerMessage::OrderAccepted(OrderAccepted::decode(body)?),
            MSG_ORDER_REJECTED => ServerMessage::OrderRejected(OrderRejected::decode(body)?),
            MSG_ORDER_CANCELLED => ServerMessage::OrderCancelled(OrderCancelled::decode(body)?),
            MSG_TRADE => ServerMessage::Trade(TradeMsg::decode(body)?),
            MSG_SERVER_HEARTBEAT => ServerMessage::Heartbeat,
            _ => return Err(ProtoError::UnknownMessageType(msg_type)),
        };

        Ok(Frame { seq_num, message })
    }
}

// ---------------------------------------------------------------------------
// Conversion helpers: tachyon_core <-> wire protocol
// ---------------------------------------------------------------------------

use tachyon_core::{EngineEvent, OrderId, OrderType, Price, Quantity, RejectReason, Side, Symbol};

/// Convert a `Side` to its wire byte.
pub fn side_to_wire(side: Side) -> u8 {
    match side {
        Side::Buy => SIDE_BUY,
        Side::Sell => SIDE_SELL,
    }
}

/// Convert a wire byte to a `Side`.
pub fn side_from_wire(b: u8) -> Result<Side, ProtoError> {
    match b {
        SIDE_BUY => Ok(Side::Buy),
        SIDE_SELL => Ok(Side::Sell),
        _ => Err(ProtoError::InvalidFieldValue("side", b)),
    }
}

/// Convert an `OrderType` to its wire byte.
pub fn order_type_to_wire(ot: OrderType) -> u8 {
    match ot {
        OrderType::Limit => ORDER_TYPE_LIMIT,
        OrderType::Market => ORDER_TYPE_MARKET,
    }
}

/// Convert a wire byte to an `OrderType`.
pub fn order_type_from_wire(b: u8) -> Result<OrderType, ProtoError> {
    match b {
        ORDER_TYPE_LIMIT => Ok(OrderType::Limit),
        ORDER_TYPE_MARKET => Ok(OrderType::Market),
        _ => Err(ProtoError::InvalidFieldValue("order_type", b)),
    }
}

/// Convert a `TimeInForce` to its wire byte.
pub fn tif_to_wire(tif: tachyon_core::TimeInForce) -> u8 {
    match tif {
        tachyon_core::TimeInForce::GTC => TIF_GTC,
        tachyon_core::TimeInForce::IOC => TIF_IOC,
        tachyon_core::TimeInForce::FOK => TIF_FOK,
        tachyon_core::TimeInForce::PostOnly => TIF_POST_ONLY,
        tachyon_core::TimeInForce::GTD(_) => TIF_GTC, // GTD not supported over binary proto, default GTC
    }
}

/// Convert a wire byte to a `TimeInForce`.
pub fn tif_from_wire(b: u8) -> Result<tachyon_core::TimeInForce, ProtoError> {
    match b {
        TIF_GTC => Ok(tachyon_core::TimeInForce::GTC),
        TIF_IOC => Ok(tachyon_core::TimeInForce::IOC),
        TIF_FOK => Ok(tachyon_core::TimeInForce::FOK),
        TIF_POST_ONLY => Ok(tachyon_core::TimeInForce::PostOnly),
        _ => Err(ProtoError::InvalidFieldValue("time_in_force", b)),
    }
}

/// Convert a `RejectReason` to its wire byte.
pub fn reject_reason_to_wire(reason: RejectReason) -> u8 {
    match reason {
        RejectReason::InsufficientLiquidity => REJECT_INSUFFICIENT_LIQUIDITY,
        RejectReason::PriceOutOfRange => REJECT_PRICE_OUT_OF_RANGE,
        RejectReason::InvalidQuantity => REJECT_INVALID_QUANTITY,
        RejectReason::InvalidPrice => REJECT_INVALID_PRICE,
        RejectReason::BookFull => REJECT_BOOK_FULL,
        RejectReason::RateLimitExceeded => REJECT_RATE_LIMIT,
        RejectReason::SelfTradePrevented => REJECT_SELF_TRADE,
        RejectReason::PostOnlyWouldTake => REJECT_POST_ONLY_WOULD_TAKE,
        RejectReason::DuplicateOrderId => REJECT_DUPLICATE_ORDER_ID,
    }
}

/// Convert a wire byte to a `RejectReason`.
pub fn reject_reason_from_wire(b: u8) -> RejectReason {
    match b {
        REJECT_INSUFFICIENT_LIQUIDITY => RejectReason::InsufficientLiquidity,
        REJECT_PRICE_OUT_OF_RANGE => RejectReason::PriceOutOfRange,
        REJECT_INVALID_QUANTITY => RejectReason::InvalidQuantity,
        REJECT_INVALID_PRICE => RejectReason::InvalidPrice,
        REJECT_BOOK_FULL => RejectReason::BookFull,
        REJECT_RATE_LIMIT => RejectReason::RateLimitExceeded,
        REJECT_SELF_TRADE => RejectReason::SelfTradePrevented,
        REJECT_POST_ONLY_WOULD_TAKE => RejectReason::PostOnlyWouldTake,
        REJECT_DUPLICATE_ORDER_ID => RejectReason::DuplicateOrderId,
        _ => RejectReason::InsufficientLiquidity, // fallback
    }
}

/// Convert an `EngineEvent` to a `ServerMessage`.
pub fn engine_event_to_server_msg(event: &EngineEvent) -> Option<ServerMessage> {
    match event {
        EngineEvent::OrderAccepted {
            order_id,
            symbol,
            side,
            price,
            qty,
            timestamp,
        } => Some(ServerMessage::OrderAccepted(OrderAccepted {
            order_id: order_id.raw(),
            symbol_id: symbol.raw(),
            side: side_to_wire(*side),
            price: price.raw(),
            quantity: qty.raw(),
            timestamp: *timestamp,
        })),
        EngineEvent::OrderRejected {
            order_id,
            reason,
            timestamp,
        } => Some(ServerMessage::OrderRejected(OrderRejected {
            order_id: order_id.raw(),
            reason: reject_reason_to_wire(*reason),
            timestamp: *timestamp,
        })),
        EngineEvent::OrderCancelled {
            order_id,
            remaining_qty,
            timestamp,
        } => Some(ServerMessage::OrderCancelled(OrderCancelled {
            order_id: order_id.raw(),
            remaining_qty: remaining_qty.raw(),
            timestamp: *timestamp,
        })),
        EngineEvent::Trade { trade } => Some(ServerMessage::Trade(TradeMsg {
            trade_id: trade.trade_id,
            symbol_id: trade.symbol.raw(),
            price: trade.price.raw(),
            quantity: trade.quantity.raw(),
            maker_order_id: trade.maker_order_id.raw(),
            taker_order_id: trade.taker_order_id.raw(),
            maker_side: side_to_wire(trade.maker_side),
            timestamp: trade.timestamp,
        })),
        // BookUpdate and OrderExpired are internal events, not sent over wire
        EngineEvent::BookUpdate { .. } | EngineEvent::OrderExpired { .. } => None,
    }
}

/// Convert a `NewOrder` wire message to a `tachyon_core::Order` (with placeholder fields).
pub fn new_order_to_core(msg: &NewOrder) -> Result<tachyon_core::Order, ProtoError> {
    let side = side_from_wire(msg.side)?;
    let order_type = order_type_from_wire(msg.order_type)?;
    let time_in_force = tif_from_wire(msg.time_in_force)?;

    Ok(tachyon_core::Order {
        id: OrderId::new(0), // Engine assigns the real ID
        symbol: Symbol::new(msg.symbol_id),
        side,
        price: Price::new(msg.price),
        quantity: Quantity::new(msg.quantity),
        remaining_qty: Quantity::new(msg.quantity),
        order_type,
        time_in_force,
        timestamp: 0,
        account_id: msg.account_id,
        prev: tachyon_core::NO_LINK,
        next: tachyon_core::NO_LINK,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // NewOrder roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn test_new_order_roundtrip() {
        let msg = NewOrder {
            side: SIDE_BUY,
            order_type: ORDER_TYPE_LIMIT,
            time_in_force: TIF_GTC,
            symbol_id: 42,
            price: 5000150,
            quantity: 100_000_000,
            account_id: 7,
        };
        let mut buf = [0u8; NewOrder::WIRE_SIZE];
        let n = msg.encode(&mut buf).unwrap();
        assert_eq!(n, NewOrder::WIRE_SIZE);
        let decoded = NewOrder::decode(&buf).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_new_order_buffer_too_small() {
        let msg = NewOrder {
            side: SIDE_BUY,
            order_type: ORDER_TYPE_LIMIT,
            time_in_force: TIF_GTC,
            symbol_id: 0,
            price: 100,
            quantity: 10,
            account_id: 1,
        };
        let mut buf = [0u8; 10]; // too small
        assert!(msg.encode(&mut buf).is_err());
    }

    #[test]
    fn test_new_order_decode_too_small() {
        let buf = [0u8; 10];
        assert!(NewOrder::decode(&buf).is_err());
    }

    // -----------------------------------------------------------------------
    // CancelOrder roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn test_cancel_order_roundtrip() {
        let msg = CancelOrder {
            order_id: 123456789,
            symbol_id: 5,
        };
        let mut buf = [0u8; CancelOrder::WIRE_SIZE];
        let n = msg.encode(&mut buf).unwrap();
        assert_eq!(n, CancelOrder::WIRE_SIZE);
        let decoded = CancelOrder::decode(&buf).unwrap();
        assert_eq!(msg, decoded);
    }

    // -----------------------------------------------------------------------
    // ModifyOrder roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn test_modify_order_roundtrip() {
        let msg = ModifyOrder {
            order_id: 999,
            symbol_id: 3,
            new_price: 50001,
            new_quantity: 200,
        };
        let mut buf = [0u8; ModifyOrder::WIRE_SIZE];
        let n = msg.encode(&mut buf).unwrap();
        assert_eq!(n, ModifyOrder::WIRE_SIZE);
        let decoded = ModifyOrder::decode(&buf).unwrap();
        assert_eq!(msg, decoded);
    }

    // -----------------------------------------------------------------------
    // OrderAccepted roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn test_order_accepted_roundtrip() {
        let msg = OrderAccepted {
            order_id: 42,
            symbol_id: 0,
            side: SIDE_SELL,
            price: 100_00,
            quantity: 10_000,
            timestamp: 1_700_000_000_000,
        };
        let mut buf = [0u8; OrderAccepted::WIRE_SIZE];
        let n = msg.encode(&mut buf).unwrap();
        assert_eq!(n, OrderAccepted::WIRE_SIZE);
        let decoded = OrderAccepted::decode(&buf).unwrap();
        assert_eq!(msg, decoded);
    }

    // -----------------------------------------------------------------------
    // OrderRejected roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn test_order_rejected_roundtrip() {
        let msg = OrderRejected {
            order_id: 99,
            reason: REJECT_POST_ONLY_WOULD_TAKE,
            timestamp: 12345,
        };
        let mut buf = [0u8; OrderRejected::WIRE_SIZE];
        let n = msg.encode(&mut buf).unwrap();
        assert_eq!(n, OrderRejected::WIRE_SIZE);
        let decoded = OrderRejected::decode(&buf).unwrap();
        assert_eq!(msg, decoded);
    }

    // -----------------------------------------------------------------------
    // OrderCancelled roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn test_order_cancelled_roundtrip() {
        let msg = OrderCancelled {
            order_id: 77,
            remaining_qty: 50,
            timestamp: 99999,
        };
        let mut buf = [0u8; OrderCancelled::WIRE_SIZE];
        let n = msg.encode(&mut buf).unwrap();
        assert_eq!(n, OrderCancelled::WIRE_SIZE);
        let decoded = OrderCancelled::decode(&buf).unwrap();
        assert_eq!(msg, decoded);
    }

    // -----------------------------------------------------------------------
    // TradeMsg roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn test_trade_msg_roundtrip() {
        let msg = TradeMsg {
            trade_id: 1001,
            symbol_id: 2,
            price: 50000_00,
            quantity: 500,
            maker_order_id: 10,
            taker_order_id: 20,
            maker_side: SIDE_BUY,
            timestamp: 1_700_000_000_001,
        };
        let mut buf = [0u8; TradeMsg::WIRE_SIZE];
        let n = msg.encode(&mut buf).unwrap();
        assert_eq!(n, TradeMsg::WIRE_SIZE);
        let decoded = TradeMsg::decode(&buf).unwrap();
        assert_eq!(msg, decoded);
    }

    // -----------------------------------------------------------------------
    // Client frame roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn test_client_frame_new_order_roundtrip() {
        let frame = Frame {
            seq_num: 42,
            message: ClientMessage::NewOrder(NewOrder {
                side: SIDE_SELL,
                order_type: ORDER_TYPE_MARKET,
                time_in_force: TIF_IOC,
                symbol_id: 1,
                price: 0,
                quantity: 500,
                account_id: 100,
            }),
        };
        let buf = frame.encode_to_vec();
        // Skip the 4-byte length prefix for decode_body
        let decoded = Frame::<ClientMessage>::decode_body(&buf[4..]).unwrap();
        assert_eq!(frame, decoded);
    }

    #[test]
    fn test_client_frame_cancel_order_roundtrip() {
        let frame = Frame {
            seq_num: 7,
            message: ClientMessage::CancelOrder(CancelOrder {
                order_id: 555,
                symbol_id: 3,
            }),
        };
        let buf = frame.encode_to_vec();
        let decoded = Frame::<ClientMessage>::decode_body(&buf[4..]).unwrap();
        assert_eq!(frame, decoded);
    }

    #[test]
    fn test_client_frame_modify_order_roundtrip() {
        let frame = Frame {
            seq_num: 99,
            message: ClientMessage::ModifyOrder(ModifyOrder {
                order_id: 888,
                symbol_id: 0,
                new_price: 12345,
                new_quantity: 50,
            }),
        };
        let buf = frame.encode_to_vec();
        let decoded = Frame::<ClientMessage>::decode_body(&buf[4..]).unwrap();
        assert_eq!(frame, decoded);
    }

    #[test]
    fn test_client_frame_heartbeat_roundtrip() {
        let frame = Frame {
            seq_num: 0,
            message: ClientMessage::Heartbeat,
        };
        let buf = frame.encode_to_vec();
        let decoded = Frame::<ClientMessage>::decode_body(&buf[4..]).unwrap();
        assert_eq!(frame, decoded);
    }

    // -----------------------------------------------------------------------
    // Server frame roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn test_server_frame_accepted_roundtrip() {
        let frame = Frame {
            seq_num: 1,
            message: ServerMessage::OrderAccepted(OrderAccepted {
                order_id: 42,
                symbol_id: 0,
                side: SIDE_BUY,
                price: 10000,
                quantity: 100,
                timestamp: 1234567890,
            }),
        };
        let buf = frame.encode_to_vec();
        let decoded = Frame::<ServerMessage>::decode_body(&buf[4..]).unwrap();
        assert_eq!(frame, decoded);
    }

    #[test]
    fn test_server_frame_rejected_roundtrip() {
        let frame = Frame {
            seq_num: 2,
            message: ServerMessage::OrderRejected(OrderRejected {
                order_id: 99,
                reason: REJECT_INVALID_QUANTITY,
                timestamp: 5555,
            }),
        };
        let buf = frame.encode_to_vec();
        let decoded = Frame::<ServerMessage>::decode_body(&buf[4..]).unwrap();
        assert_eq!(frame, decoded);
    }

    #[test]
    fn test_server_frame_cancelled_roundtrip() {
        let frame = Frame {
            seq_num: 3,
            message: ServerMessage::OrderCancelled(OrderCancelled {
                order_id: 77,
                remaining_qty: 25,
                timestamp: 9999,
            }),
        };
        let buf = frame.encode_to_vec();
        let decoded = Frame::<ServerMessage>::decode_body(&buf[4..]).unwrap();
        assert_eq!(frame, decoded);
    }

    #[test]
    fn test_server_frame_trade_roundtrip() {
        let frame = Frame {
            seq_num: 100,
            message: ServerMessage::Trade(TradeMsg {
                trade_id: 500,
                symbol_id: 1,
                price: 50000,
                quantity: 10,
                maker_order_id: 1,
                taker_order_id: 2,
                maker_side: SIDE_SELL,
                timestamp: 1_700_000_000_000,
            }),
        };
        let buf = frame.encode_to_vec();
        let decoded = Frame::<ServerMessage>::decode_body(&buf[4..]).unwrap();
        assert_eq!(frame, decoded);
    }

    #[test]
    fn test_server_frame_heartbeat_roundtrip() {
        let frame = Frame {
            seq_num: 0,
            message: ServerMessage::Heartbeat,
        };
        let buf = frame.encode_to_vec();
        let decoded = Frame::<ServerMessage>::decode_body(&buf[4..]).unwrap();
        assert_eq!(frame, decoded);
    }

    // -----------------------------------------------------------------------
    // Unknown message type
    // -----------------------------------------------------------------------

    #[test]
    fn test_unknown_client_message_type() {
        let mut buf = vec![0u8; 20];
        buf[0] = 0xFF; // unknown type
        buf[1..9].copy_from_slice(&1u64.to_le_bytes());
        let result = Frame::<ClientMessage>::decode_body(&buf);
        assert!(result.is_err());
        match result.unwrap_err() {
            ProtoError::UnknownMessageType(t) => assert_eq!(t, 0xFF),
            other => panic!("Expected UnknownMessageType, got {:?}", other),
        }
    }

    #[test]
    fn test_unknown_server_message_type() {
        let mut buf = vec![0u8; 20];
        buf[0] = 0x50; // unknown type
        buf[1..9].copy_from_slice(&1u64.to_le_bytes());
        let result = Frame::<ServerMessage>::decode_body(&buf);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Wire conversion helpers
    // -----------------------------------------------------------------------

    #[test]
    fn test_side_roundtrip() {
        assert_eq!(side_from_wire(side_to_wire(Side::Buy)).unwrap(), Side::Buy);
        assert_eq!(
            side_from_wire(side_to_wire(Side::Sell)).unwrap(),
            Side::Sell
        );
        assert!(side_from_wire(99).is_err());
    }

    #[test]
    fn test_order_type_roundtrip() {
        assert_eq!(
            order_type_from_wire(order_type_to_wire(OrderType::Limit)).unwrap(),
            OrderType::Limit
        );
        assert_eq!(
            order_type_from_wire(order_type_to_wire(OrderType::Market)).unwrap(),
            OrderType::Market
        );
        assert!(order_type_from_wire(99).is_err());
    }

    #[test]
    fn test_tif_roundtrip() {
        use tachyon_core::TimeInForce;
        assert_eq!(
            tif_from_wire(tif_to_wire(TimeInForce::GTC)).unwrap(),
            TimeInForce::GTC
        );
        assert_eq!(
            tif_from_wire(tif_to_wire(TimeInForce::IOC)).unwrap(),
            TimeInForce::IOC
        );
        assert_eq!(
            tif_from_wire(tif_to_wire(TimeInForce::FOK)).unwrap(),
            TimeInForce::FOK
        );
        assert_eq!(
            tif_from_wire(tif_to_wire(TimeInForce::PostOnly)).unwrap(),
            TimeInForce::PostOnly
        );
        assert!(tif_from_wire(99).is_err());
    }

    #[test]
    fn test_reject_reason_roundtrip() {
        let reasons = [
            RejectReason::InsufficientLiquidity,
            RejectReason::PriceOutOfRange,
            RejectReason::InvalidQuantity,
            RejectReason::InvalidPrice,
            RejectReason::BookFull,
            RejectReason::RateLimitExceeded,
            RejectReason::SelfTradePrevented,
            RejectReason::PostOnlyWouldTake,
            RejectReason::DuplicateOrderId,
        ];
        for reason in &reasons {
            let wire = reject_reason_to_wire(*reason);
            let back = reject_reason_from_wire(wire);
            assert_eq!(*reason, back);
        }
    }

    // -----------------------------------------------------------------------
    // engine_event_to_server_msg
    // -----------------------------------------------------------------------

    #[test]
    fn test_engine_event_accepted_to_server_msg() {
        let event = EngineEvent::OrderAccepted {
            order_id: OrderId::new(42),
            symbol: Symbol::new(1),
            side: Side::Buy,
            price: Price::new(10000),
            qty: Quantity::new(100),
            timestamp: 999,
        };
        let msg = engine_event_to_server_msg(&event).unwrap();
        match msg {
            ServerMessage::OrderAccepted(a) => {
                assert_eq!(a.order_id, 42);
                assert_eq!(a.symbol_id, 1);
                assert_eq!(a.side, SIDE_BUY);
                assert_eq!(a.price, 10000);
                assert_eq!(a.quantity, 100);
                assert_eq!(a.timestamp, 999);
            }
            _ => panic!("Expected OrderAccepted"),
        }
    }

    #[test]
    fn test_engine_event_rejected_to_server_msg() {
        let event = EngineEvent::OrderRejected {
            order_id: OrderId::new(5),
            reason: RejectReason::PostOnlyWouldTake,
            timestamp: 123,
        };
        let msg = engine_event_to_server_msg(&event).unwrap();
        match msg {
            ServerMessage::OrderRejected(r) => {
                assert_eq!(r.order_id, 5);
                assert_eq!(r.reason, REJECT_POST_ONLY_WOULD_TAKE);
            }
            _ => panic!("Expected OrderRejected"),
        }
    }

    #[test]
    fn test_engine_event_trade_to_server_msg() {
        let event = EngineEvent::Trade {
            trade: tachyon_core::Trade {
                trade_id: 1,
                symbol: Symbol::new(0),
                price: Price::new(50000),
                quantity: Quantity::new(100),
                maker_order_id: OrderId::new(10),
                taker_order_id: OrderId::new(20),
                maker_side: Side::Sell,
                timestamp: 1000,
            },
        };
        let msg = engine_event_to_server_msg(&event).unwrap();
        match msg {
            ServerMessage::Trade(t) => {
                assert_eq!(t.trade_id, 1);
                assert_eq!(t.maker_side, SIDE_SELL);
                assert_eq!(t.maker_order_id, 10);
                assert_eq!(t.taker_order_id, 20);
            }
            _ => panic!("Expected Trade"),
        }
    }

    #[test]
    fn test_engine_event_book_update_returns_none() {
        let event = EngineEvent::BookUpdate {
            symbol: Symbol::new(0),
            side: Side::Buy,
            price: Price::new(100),
            new_total_qty: Quantity::new(50),
            timestamp: 1000,
        };
        assert!(engine_event_to_server_msg(&event).is_none());
    }

    // -----------------------------------------------------------------------
    // new_order_to_core
    // -----------------------------------------------------------------------

    #[test]
    fn test_new_order_to_core() {
        let msg = NewOrder {
            side: SIDE_BUY,
            order_type: ORDER_TYPE_LIMIT,
            time_in_force: TIF_GTC,
            symbol_id: 0,
            price: 10000,
            quantity: 100,
            account_id: 7,
        };
        let order = new_order_to_core(&msg).unwrap();
        assert_eq!(order.side, Side::Buy);
        assert_eq!(order.order_type, OrderType::Limit);
        assert_eq!(order.time_in_force, tachyon_core::TimeInForce::GTC);
        assert_eq!(order.price.raw(), 10000);
        assert_eq!(order.quantity.raw(), 100);
        assert_eq!(order.account_id, 7);
        assert_eq!(order.symbol.raw(), 0);
    }

    #[test]
    fn test_new_order_to_core_invalid_side() {
        let msg = NewOrder {
            side: 99,
            order_type: ORDER_TYPE_LIMIT,
            time_in_force: TIF_GTC,
            symbol_id: 0,
            price: 100,
            quantity: 10,
            account_id: 1,
        };
        assert!(new_order_to_core(&msg).is_err());
    }

    // -----------------------------------------------------------------------
    // Edge cases: extreme values
    // -----------------------------------------------------------------------

    #[test]
    fn test_extreme_values_roundtrip() {
        let msg = NewOrder {
            side: SIDE_SELL,
            order_type: ORDER_TYPE_MARKET,
            time_in_force: TIF_FOK,
            symbol_id: u32::MAX,
            price: i64::MIN,
            quantity: u64::MAX,
            account_id: u64::MAX,
        };
        let mut buf = [0u8; NewOrder::WIRE_SIZE];
        msg.encode(&mut buf).unwrap();
        let decoded = NewOrder::decode(&buf).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_zero_values_roundtrip() {
        let msg = NewOrder {
            side: 0,
            order_type: 0,
            time_in_force: 0,
            symbol_id: 0,
            price: 0,
            quantity: 0,
            account_id: 0,
        };
        let mut buf = [0u8; NewOrder::WIRE_SIZE];
        msg.encode(&mut buf).unwrap();
        let decoded = NewOrder::decode(&buf).unwrap();
        assert_eq!(msg, decoded);
    }

    // -----------------------------------------------------------------------
    // Frame length field
    // -----------------------------------------------------------------------

    #[test]
    fn test_frame_length_field_correct() {
        let frame = Frame {
            seq_num: 1,
            message: ClientMessage::NewOrder(NewOrder {
                side: SIDE_BUY,
                order_type: ORDER_TYPE_LIMIT,
                time_in_force: TIF_GTC,
                symbol_id: 0,
                price: 100,
                quantity: 10,
                account_id: 1,
            }),
        };
        let buf = frame.encode_to_vec();
        let length = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        // length = msg_type(1) + seq_num(8) + payload(31) = 40
        assert_eq!(length, 1 + 8 + NewOrder::WIRE_SIZE as u32);
        assert_eq!(buf.len(), 4 + length as usize);
    }
}
