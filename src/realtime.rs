use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, info};

use crate::error::{H3Error, Result};

/// Pesan yang dikirim/diterima via realtime stream
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealtimeMessage {
    pub event: String,
    pub channel: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

impl RealtimeMessage {
    pub fn new(
        event: impl Into<String>,
        channel: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            event: event.into(),
            channel: channel.into(),
            payload,
        }
    }
}

/// Event yang dikirim ke layer aplikasi
#[derive(Debug, Clone)]
pub enum RealtimeEvent {
    /// Client baru bergabung ke channel
    Join {
        conn_id: String,
        channel: String,
        stream_id: u64,
    },
    /// Client meninggalkan channel
    Leave { conn_id: String, channel: String },
    /// Pesan masuk dari client
    Message {
        conn_id: String,
        stream_id: u64,
        msg: RealtimeMessage,
    },
    /// Koneksi ditutup
    Disconnected { conn_id: String },
}

/// Subscriber: setiap subscriber mendapat cloned sender
type Subscriber = mpsc::Sender<RealtimeMessage>;

/// RealtimeChannel mengelola pub/sub di sisi server.
///
/// Setiap "channel" bisa punya banyak subscriber (connections).
/// Broadcast message ke semua subscriber dalam satu channel.
pub struct RealtimeChannel {
    /// channel_name -> HashMap<conn_id, Sender>
    channels: Arc<RwLock<HashMap<String, HashMap<String, Subscriber>>>>,
}

impl RealtimeChannel {
    pub fn new() -> Self {
        Self {
            channels: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Subscribe conn ke channel tertentu, kembalikan receiver
    pub async fn subscribe(&self, channel: &str, conn_id: &str) -> mpsc::Receiver<RealtimeMessage> {
        let (tx, rx) = mpsc::channel(256);
        let mut chans = self.channels.write().await;
        chans
            .entry(channel.to_string())
            .or_default()
            .insert(conn_id.to_string(), tx);
        info!("Conn '{}' subscribed to channel '{}'", conn_id, channel);
        rx
    }

    /// Unsubscribe conn dari channel
    pub async fn unsubscribe(&self, channel: &str, conn_id: &str) {
        let mut chans = self.channels.write().await;
        if let Some(subs) = chans.get_mut(channel) {
            subs.remove(conn_id);
            if subs.is_empty() {
                chans.remove(channel);
            }
        }
        debug!("Conn '{}' unsubscribed from channel '{}'", conn_id, channel);
    }

    /// Broadcast pesan ke semua subscriber di channel (kecuali sender)
    pub async fn broadcast(
        &self,
        channel: &str,
        msg: RealtimeMessage,
        exclude_conn: Option<&str>,
    ) -> usize {
        let chans = self.channels.read().await;
        let Some(subs) = chans.get(channel) else {
            return 0;
        };

        let mut sent = 0;
        for (conn_id, tx) in subs.iter() {
            if let Some(exc) = exclude_conn {
                if conn_id == exc {
                    continue;
                }
            }
            if tx.send(msg.clone()).await.is_ok() {
                sent += 1;
            }
        }
        sent
    }

    /// Kirim pesan ke satu subscriber spesifik
    pub async fn send_to(&self, channel: &str, conn_id: &str, msg: RealtimeMessage) -> Result<()> {
        let chans = self.channels.read().await;
        let subs = chans
            .get(channel)
            .ok_or_else(|| H3Error::ChannelNotFound(channel.to_string()))?;
        let tx = subs
            .get(conn_id)
            .ok_or_else(|| H3Error::ChannelNotFound(conn_id.to_string()))?;
        tx.send(msg).await.map_err(|_| H3Error::ConnectionClosed)?;
        Ok(())
    }

    /// List semua subscriber di channel
    pub async fn subscribers(&self, channel: &str) -> Vec<String> {
        let chans = self.channels.read().await;
        chans
            .get(channel)
            .map(|s| s.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// List semua active channels
    pub async fn active_channels(&self) -> Vec<String> {
        self.channels.read().await.keys().cloned().collect()
    }

    /// Arc clone untuk share antar tasks
    pub fn clone_handle(&self) -> Self {
        Self {
            channels: Arc::clone(&self.channels),
        }
    }
}

impl Default for RealtimeChannel {
    fn default() -> Self {
        Self::new()
    }
}

/// Helper: parse realtime frame dari raw bytes.
/// Format: [4 bytes big-endian length][JSON payload]
pub fn parse_realtime_frame(buf: &[u8]) -> Option<(RealtimeMessage, usize)> {
    if buf.len() < 4 {
        return None;
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < 4 + len {
        return None;
    }
    let json = &buf[4..4 + len];
    let msg = serde_json::from_slice(json).ok()?;
    Some((msg, 4 + len))
}

/// Helper: encode realtime message ke bytes
pub fn encode_realtime_frame(msg: &RealtimeMessage) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(msg)?;
    let mut out = Vec::with_capacity(4 + json.len());
    out.extend_from_slice(&(json.len() as u32).to_be_bytes());
    out.extend_from_slice(&json);
    Ok(out)
}
