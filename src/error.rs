use thiserror::Error;

#[derive(Error, Debug)]
pub enum H3Error {
    #[error("QUIC error: {0}")]
    Quic(#[from] quiche::Error),

    #[error("HTTP/3 error: {0}")]
    Http3(#[from] quiche::h3::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Connection closed")]
    ConnectionClosed,

    #[error("Stream {0} not found")]
    StreamNotFound(u64),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Handshake timeout")]
    HandshakeTimeout,

    #[error("Invalid frame: {0}")]
    InvalidFrame(String),

    #[error("Channel {0} not found")]
    ChannelNotFound(String),

    #[error("Address initialized error {0}")]
    AddrErr(String),
}

pub type Result<T> = std::result::Result<T, H3Error>;
