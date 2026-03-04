//! Frame codec for TCP transport using `tokio_util::codec`.
//!
//! Implements length-prefixed framing over a byte stream. The frame format is:
//!
//! ```text
//! +--------+--------+--------+------------------+
//! | Length  | MsgType| SeqNum | Payload          |
//! | 4 bytes | 1 byte | 8 bytes| variable         |
//! +--------+--------+--------+------------------+
//! ```
//!
//! The Length field contains the byte count of everything after it
//! (MsgType + SeqNum + Payload).

use bytes::{Buf, BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use crate::error::ProtoError;
use crate::msg::{ClientMessage, Frame, ServerMessage, MAX_PAYLOAD_SIZE};

// ---------------------------------------------------------------------------
// ClientFrameCodec: decodes ClientMessage frames, encodes ServerMessage frames.
// This is what the server side uses.
// ---------------------------------------------------------------------------

/// Codec for the server side of a TCP connection.
///
/// - Decodes incoming bytes into `Frame<ClientMessage>`
/// - Encodes outgoing `Frame<ServerMessage>` into bytes
pub struct ServerCodec;

impl Decoder for ServerCodec {
    type Item = Frame<ClientMessage>;
    type Error = ProtoError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // Need at least 4 bytes for the length prefix
        if src.len() < 4 {
            return Ok(None);
        }

        // Peek at the length without consuming
        let length = u32::from_le_bytes([src[0], src[1], src[2], src[3]]);

        // Guard against allocation bombs
        if length > MAX_PAYLOAD_SIZE {
            return Err(ProtoError::FrameTooLarge(length, MAX_PAYLOAD_SIZE));
        }

        let total_frame_len = 4 + length as usize;

        // Check if we have the complete frame
        if src.len() < total_frame_len {
            // Reserve space for the rest of the frame to avoid repeated allocations
            src.reserve(total_frame_len - src.len());
            return Ok(None);
        }

        // Consume the length prefix
        src.advance(4);

        // Take the frame body (msg_type + seq_num + payload)
        let body = src.split_to(length as usize);
        let frame = Frame::<ClientMessage>::decode_body(&body)?;

        Ok(Some(frame))
    }
}

impl Encoder<Frame<ServerMessage>> for ServerCodec {
    type Error = ProtoError;

    fn encode(
        &mut self,
        item: Frame<ServerMessage>,
        dst: &mut BytesMut,
    ) -> Result<(), Self::Error> {
        let encoded = item.encode_to_vec();
        dst.reserve(encoded.len());
        dst.put_slice(&encoded);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ClientCodec: decodes ServerMessage frames, encodes ClientMessage frames.
// This is what the client side uses.
// ---------------------------------------------------------------------------

/// Codec for the client side of a TCP connection.
///
/// - Decodes incoming bytes into `Frame<ServerMessage>`
/// - Encodes outgoing `Frame<ClientMessage>` into bytes
pub struct ClientCodec;

impl Decoder for ClientCodec {
    type Item = Frame<ServerMessage>;
    type Error = ProtoError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < 4 {
            return Ok(None);
        }

        let length = u32::from_le_bytes([src[0], src[1], src[2], src[3]]);

        if length > MAX_PAYLOAD_SIZE {
            return Err(ProtoError::FrameTooLarge(length, MAX_PAYLOAD_SIZE));
        }

        let total_frame_len = 4 + length as usize;

        if src.len() < total_frame_len {
            src.reserve(total_frame_len - src.len());
            return Ok(None);
        }

        src.advance(4);
        let body = src.split_to(length as usize);
        let frame = Frame::<ServerMessage>::decode_body(&body)?;

        Ok(Some(frame))
    }
}

impl Encoder<Frame<ClientMessage>> for ClientCodec {
    type Error = ProtoError;

    fn encode(
        &mut self,
        item: Frame<ClientMessage>,
        dst: &mut BytesMut,
    ) -> Result<(), Self::Error> {
        let encoded = item.encode_to_vec();
        dst.reserve(encoded.len());
        dst.put_slice(&encoded);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg::*;
    use bytes::BytesMut;

    // -----------------------------------------------------------------------
    // ServerCodec: decode client messages
    // -----------------------------------------------------------------------

    #[test]
    fn test_server_codec_decode_new_order() {
        let mut codec = ServerCodec;
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
        let encoded = frame.encode_to_vec();
        let mut buf = BytesMut::from(&encoded[..]);
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(frame, decoded);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_server_codec_decode_heartbeat() {
        let mut codec = ServerCodec;
        let frame = Frame {
            seq_num: 0,
            message: ClientMessage::Heartbeat,
        };
        let encoded = frame.encode_to_vec();
        let mut buf = BytesMut::from(&encoded[..]);
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(frame, decoded);
    }

    #[test]
    fn test_server_codec_partial_read() {
        let mut codec = ServerCodec;
        let frame = Frame {
            seq_num: 5,
            message: ClientMessage::NewOrder(NewOrder {
                side: SIDE_SELL,
                order_type: ORDER_TYPE_LIMIT,
                time_in_force: TIF_IOC,
                gtd_timestamp: 0,
                symbol_id: 1,
                price: 50000,
                quantity: 200,
                account_id: 42,
            }),
        };
        let encoded = frame.encode_to_vec();

        // Feed bytes one at a time
        let mut buf = BytesMut::new();
        for (i, &byte) in encoded.iter().enumerate() {
            buf.put_u8(byte);
            let result = codec.decode(&mut buf).unwrap();
            if i < encoded.len() - 1 {
                // Should not have a complete frame yet
                assert!(result.is_none(), "unexpected frame at byte {}", i);
            } else {
                // Last byte completes the frame
                let decoded = result.expect("should have frame on last byte");
                assert_eq!(frame, decoded);
            }
        }
    }

    #[test]
    fn test_server_codec_multiple_frames() {
        let mut codec = ServerCodec;

        let frame1 = Frame {
            seq_num: 1,
            message: ClientMessage::Heartbeat,
        };
        let frame2 = Frame {
            seq_num: 2,
            message: ClientMessage::CancelOrder(CancelOrder {
                order_id: 99,
                symbol_id: 0,
            }),
        };

        let mut combined = BytesMut::new();
        combined.extend_from_slice(&frame1.encode_to_vec());
        combined.extend_from_slice(&frame2.encode_to_vec());

        let decoded1 = codec.decode(&mut combined).unwrap().unwrap();
        assert_eq!(frame1, decoded1);

        let decoded2 = codec.decode(&mut combined).unwrap().unwrap();
        assert_eq!(frame2, decoded2);

        // Buffer should be empty now
        assert!(combined.is_empty());
    }

    #[test]
    fn test_server_codec_empty_buffer() {
        let mut codec = ServerCodec;
        let mut buf = BytesMut::new();
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn test_server_codec_incomplete_length() {
        let mut codec = ServerCodec;
        let mut buf = BytesMut::from(&[0x0A, 0x00][..]);
        // Only 2 bytes, need 4 for length
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn test_server_codec_frame_too_large() {
        let mut codec = ServerCodec;
        // Encode a length value larger than MAX_PAYLOAD_SIZE
        let bad_length = MAX_PAYLOAD_SIZE + 1;
        let mut buf = BytesMut::new();
        buf.put_slice(&bad_length.to_le_bytes());
        buf.put_slice(&[0u8; 100]);
        let result = codec.decode(&mut buf);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // ServerCodec: encode server messages
    // -----------------------------------------------------------------------

    #[test]
    fn test_server_codec_encode_accepted() {
        let mut codec = ServerCodec;
        let frame = Frame {
            seq_num: 10,
            message: ServerMessage::OrderAccepted(OrderAccepted {
                order_id: 42,
                symbol_id: 0,
                side: SIDE_BUY,
                price: 10000,
                quantity: 100,
                timestamp: 99999,
            }),
        };
        let mut buf = BytesMut::new();
        codec.encode(frame.clone(), &mut buf).unwrap();

        // Decode it back with ClientCodec
        let mut client_codec = ClientCodec;
        let decoded = client_codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(frame, decoded);
    }

    // -----------------------------------------------------------------------
    // ClientCodec: decode server messages
    // -----------------------------------------------------------------------

    #[test]
    fn test_client_codec_decode_trade() {
        let mut codec = ClientCodec;
        let frame = Frame {
            seq_num: 50,
            message: ServerMessage::Trade(TradeMsg {
                trade_id: 100,
                symbol_id: 2,
                price: 50000,
                quantity: 10,
                maker_order_id: 1,
                taker_order_id: 2,
                maker_side: SIDE_SELL,
                timestamp: 1_700_000_000_000,
            }),
        };
        let encoded = frame.encode_to_vec();
        let mut buf = BytesMut::from(&encoded[..]);
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(frame, decoded);
    }

    // -----------------------------------------------------------------------
    // ClientCodec: encode client messages
    // -----------------------------------------------------------------------

    #[test]
    fn test_client_codec_encode_new_order() {
        let mut client_codec = ClientCodec;
        let frame = Frame {
            seq_num: 7,
            message: ClientMessage::NewOrder(NewOrder {
                side: SIDE_BUY,
                order_type: ORDER_TYPE_LIMIT,
                time_in_force: TIF_POST_ONLY,
                gtd_timestamp: 0,
                symbol_id: 3,
                price: 12345,
                quantity: 500,
                account_id: 99,
            }),
        };
        let mut buf = BytesMut::new();
        client_codec.encode(frame.clone(), &mut buf).unwrap();

        // Decode it back with ServerCodec
        let mut server_codec = ServerCodec;
        let decoded = server_codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(frame, decoded);
    }

    // -----------------------------------------------------------------------
    // Partial streaming: feed chunks of various sizes
    // -----------------------------------------------------------------------

    #[test]
    fn test_streaming_multiple_chunk_sizes() {
        let mut codec = ServerCodec;
        let frame = Frame {
            seq_num: 42,
            message: ClientMessage::ModifyOrder(ModifyOrder {
                order_id: 100,
                symbol_id: 1,
                new_price: 55555,
                new_quantity: 200,
            }),
        };
        let encoded = frame.encode_to_vec();

        // Try feeding in chunks of size 3
        let mut buf = BytesMut::new();
        let mut decoded_frame = None;
        for chunk in encoded.chunks(3) {
            buf.extend_from_slice(chunk);
            if let Some(f) = codec.decode(&mut buf).unwrap() {
                decoded_frame = Some(f);
                break;
            }
        }
        assert_eq!(frame, decoded_frame.unwrap());
    }

    // -----------------------------------------------------------------------
    // Round trip: encode then decode through client/server codec pair
    // -----------------------------------------------------------------------

    #[test]
    fn test_full_roundtrip_client_to_server() {
        let client_frame = Frame {
            seq_num: 777,
            message: ClientMessage::NewOrder(NewOrder {
                side: SIDE_SELL,
                order_type: ORDER_TYPE_MARKET,
                time_in_force: TIF_IOC,
                gtd_timestamp: 0,
                symbol_id: 5,
                price: 0,
                quantity: 1000,
                account_id: 42,
            }),
        };

        // Client encodes
        let mut client_codec = ClientCodec;
        let mut wire = BytesMut::new();
        client_codec
            .encode(client_frame.clone(), &mut wire)
            .unwrap();

        // Server decodes
        let mut server_codec = ServerCodec;
        let decoded = server_codec.decode(&mut wire).unwrap().unwrap();
        assert_eq!(client_frame, decoded);
    }

    #[test]
    fn test_full_roundtrip_server_to_client() {
        let server_frame = Frame {
            seq_num: 888,
            message: ServerMessage::OrderRejected(OrderRejected {
                order_id: 42,
                reason: REJECT_PRICE_OUT_OF_RANGE,
                timestamp: 1_700_000_000_000,
            }),
        };

        // Server encodes
        let mut server_codec = ServerCodec;
        let mut wire = BytesMut::new();
        server_codec
            .encode(server_frame.clone(), &mut wire)
            .unwrap();

        // Client decodes
        let mut client_codec = ClientCodec;
        let decoded = client_codec.decode(&mut wire).unwrap().unwrap();
        assert_eq!(server_frame, decoded);
    }
}
