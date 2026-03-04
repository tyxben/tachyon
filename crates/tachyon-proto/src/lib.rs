//! tachyon-proto: Compact binary wire protocol for the Tachyon matching engine.
//!
//! Provides a zero-copy, little-endian binary framing protocol optimized for
//! low-latency TCP communication. The protocol uses length-prefixed frames:
//!
//! ```text
//! +--------+--------+--------+------------------+
//! | Length  | MsgType| SeqNum | Payload          |
//! | 4 bytes | 1 byte | 8 bytes| variable         |
//! +--------+--------+--------+------------------+
//! ```
//!
//! # Message types
//!
//! **Client -> Server:** NewOrder (0x01), CancelOrder (0x02), ModifyOrder (0x03), Heartbeat (0x10)
//!
//! **Server -> Client:** OrderAccepted (0x81), OrderRejected (0x82), OrderCancelled (0x83),
//! Trade (0x84), Heartbeat (0x90)

pub mod codec;
pub mod error;
pub mod msg;

pub use codec::{ClientCodec, ServerCodec};
pub use error::ProtoError;
pub use msg::{
    engine_event_to_server_msg, new_order_to_core, CancelOrder, ClientMessage, Frame, ModifyOrder,
    NewOrder, OrderAccepted, OrderCancelled, OrderRejected, ServerMessage, TradeMsg,
};
