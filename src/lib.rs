//! # quic-h3
//!
//! HTTP/3 library with realtime support built on top of Cloudflare's Quiche.
//! Supports standard HTTP/3 requests AND realtime bidirectional streaming
//! (similar to WebSocket but over native QUIC streams).
//!
//! ## Architecture
//!
//! ```text
//!  ┌──────────────────────────────────────────────────┐
//!  │                   quic-h3 API                    │
//!  ├───────────────┬──────────────────────────────────┤
//!  │  HTTP/3 Layer │     Realtime Stream Layer         │
//!  │  (request /   │  (bidirectional QUIC streams,    │
//!  │   response)   │   pub/sub, events)               │
//!  ├───────────────┴──────────────────────────────────┤
//!  │              QUIC Transport (quiche)              │
//!  └──────────────────────────────────────────────────┘
//! ```

pub mod client;
pub mod config;
pub mod connection;
pub mod error;
pub mod realtime;
pub mod server;

pub use client::H3Client;
pub use config::QuicConfig;
pub use connection::QuicConnection;
pub use realtime::{RealtimeChannel, RealtimeEvent, RealtimeMessage};
pub use server::H3Server;

/// Re-export quiche untuk access tingkat rendah bila diperlukan
pub use quiche;
