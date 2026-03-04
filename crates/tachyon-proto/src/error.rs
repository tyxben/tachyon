//! Error types for the Tachyon binary protocol.

/// Errors that can occur during binary protocol encoding/decoding.
#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("buffer too small for message encoding/decoding")]
    BufferTooSmall,

    #[error("incomplete message: not enough bytes")]
    IncompleteMessage,

    #[error("unknown message type: 0x{0:02x}")]
    UnknownMessageType(u8),

    #[error("invalid field value for {0}: {1}")]
    InvalidFieldValue(&'static str, u8),

    #[error("frame too large: {0} bytes (max {1})")]
    FrameTooLarge(u32, u32),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
