use quiche::h3::NameValue;
use ring::rand::SecureRandom;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::config::QuicConfig;
use crate::connection::QuicConnection;
use crate::error::{H3Error, Result};
use crate::realtime::{RealtimeEvent, RealtimeMessage, parse_realtime_frame};

pub struct H3Client {
    server_addr: SocketAddr,
    local_addr: SocketAddr,
    config: QuicConfig,
    server_name: String,
}

#[derive(Debug)]
pub struct H3ClientResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub struct RealtimeStreamHandle {
    pub stream_id: u64,
    conn: Arc<QuicConnection>,
    pub rx: mpsc::Receiver<RealtimeMessage>,
}

impl RealtimeStreamHandle {
    pub async fn send(&self, event: &str, payload: serde_json::Value) -> Result<()> {
        let msg = RealtimeMessage::new(event, "", payload);
        self.conn.send_realtime(self.stream_id, &msg).await
    }

    pub async fn recv(&mut self) -> Option<RealtimeMessage> {
        self.rx.recv().await
    }
}

impl H3Client {
    pub fn new(
        server_addr: SocketAddr,
        local_addr: SocketAddr,
        config: QuicConfig,
        server_name: impl Into<String>,
    ) -> Self {
        Self {
            server_addr,
            local_addr,
            config,
            server_name: server_name.into(),
        }
    }

    pub async fn connect(self) -> Result<H3ClientConn> {
        let socket = Arc::new(UdpSocket::bind(self.local_addr).await?);
        socket.connect(self.server_addr).await?;

        let actual_local_addr = socket.local_addr()?;

        let mut quiche_config = self.config.build_quiche_config()?;

        let mut scid_bytes = [0u8; quiche::MAX_CONN_ID_LEN];
        ring::rand::SystemRandom::new()
            .fill(&mut scid_bytes)
            .map_err(|_| H3Error::InvalidFrame("rng failed".into()))?;
        let scid = quiche::ConnectionId::from_ref(&scid_bytes);

        let quic_conn = quiche::connect(
            Some(&self.server_name),
            &scid,
            actual_local_addr,
            self.server_addr,
            &mut quiche_config,
        )?;

        let (rt_tx, rt_rx) = mpsc::channel(256);
        let conn = Arc::new(QuicConnection::new(
            QuicConnection::generate_conn_id(),
            self.server_addr,
            quic_conn,
            rt_tx,
        ));

        info!(
            "Connecting to {} (local {}) ...",
            self.server_addr, actual_local_addr
        );

        Ok(H3ClientConn {
            socket,
            conn,
            local_addr: actual_local_addr,
            server_addr: self.server_addr,
            rt_rx,
        })
    }
}

pub struct H3ClientConn {
    socket: Arc<UdpSocket>,
    pub conn: Arc<QuicConnection>,
    local_addr: SocketAddr,
    server_addr: SocketAddr,
    rt_rx: mpsc::Receiver<RealtimeEvent>,
}

impl H3ClientConn {
    pub async fn handshake(&self) -> Result<()> {
        let mut out = [0u8; 1350];
        let mut buf = [0u8; 65535];
        let timeout = std::time::Duration::from_secs(10);
        let start = Instant::now();

        loop {
            if start.elapsed() > timeout {
                return Err(H3Error::HandshakeTimeout);
            }

            loop {
                let (write, _) = {
                    let mut quic = self.conn.quic.lock().await;
                    match quic.send(&mut out) {
                        Ok(v) => v,
                        Err(quiche::Error::Done) => break,
                        Err(e) => return Err(e.into()),
                    }
                };
                self.socket.send(&out[..write]).await?;
            }

            if self.conn.is_established().await {
                break;
            }

            let recv_timeout = tokio::time::timeout(
                std::time::Duration::from_millis(100),
                self.socket.recv(&mut buf),
            );

            match recv_timeout.await {
                Ok(Ok(len)) => {
                    let recv_info = quiche::RecvInfo {
                        to: self.local_addr,
                        from: self.server_addr,
                    };
                    let mut quic = self.conn.quic.lock().await;
                    if let Err(e) = quic.recv(&mut buf[..len], recv_info) {
                        warn!("handshake recv error: {e}");
                    }
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => {}
            }
        }

        self.conn.init_h3_client().await?;
        info!("Connected and H3 initialized!");
        Ok(())
    }

    pub async fn get(&self, path: &str) -> Result<H3ClientResponse> {
        self.request("GET", path, &[], None).await
    }

    pub async fn post_json(&self, path: &str, body: serde_json::Value) -> Result<H3ClientResponse> {
        let body_bytes = serde_json::to_vec(&body)?;
        self.request(
            "POST",
            path,
            &[("content-type", "application/json")],
            Some(&body_bytes),
        )
        .await
    }

    pub async fn request(
        &self,
        method: &str,
        path: &str,
        extra_headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<H3ClientResponse> {
        let mut headers = vec![
            quiche::h3::Header::new(b":method", method.as_bytes()),
            quiche::h3::Header::new(b":scheme", b"https"),
            quiche::h3::Header::new(b":path", path.as_bytes()),
            quiche::h3::Header::new(b":authority", b"localhost"),
            quiche::h3::Header::new(b"user-agent", b"quic-h3/0.1"),
        ];

        for (k, v) in extra_headers {
            headers.push(quiche::h3::Header::new(k.as_bytes(), v.as_bytes()));
        }

        let stream_id = {
            let mut quic = self.conn.quic.lock().await;
            let mut h3_guard = self.conn.h3.lock().await;
            let h3 = h3_guard.as_mut().ok_or(H3Error::ConnectionClosed)?;
            h3.send_request(&mut *quic, &headers, body.is_none())?
        };

        if let Some(data) = body {
            let mut quic = self.conn.quic.lock().await;
            let mut h3_guard = self.conn.h3.lock().await;
            let h3 = h3_guard.as_mut().ok_or(H3Error::ConnectionClosed)?;
            h3.send_body(&mut *quic, stream_id, data, true)?;
        }

        self.flush().await?;

        let mut response_status = 0u16;
        let mut response_headers = Vec::new();
        let mut response_body = Vec::new();
        let mut buf = [0u8; 65535];

        loop {
            let recv_result = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                self.socket.recv(&mut buf),
            )
            .await;

            match recv_result {
                Ok(Ok(len)) => {
                    let recv_info = quiche::RecvInfo {
                        to: self.local_addr,
                        from: self.server_addr,
                    };
                    let mut quic = self.conn.quic.lock().await;
                    if let Err(e) = quic.recv(&mut buf[..len], recv_info) {
                        warn!("request recv error: {e}");
                    }
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => return Err(H3Error::HandshakeTimeout), // response timeout
            }

            self.flush().await?;

            loop {
                let event = {
                    let mut quic = self.conn.quic.lock().await;
                    let mut h3_guard = self.conn.h3.lock().await;
                    let h3 = match h3_guard.as_mut() {
                        Some(h) => h,
                        None => return Err(H3Error::ConnectionClosed),
                    };
                    match h3.poll(&mut *quic) {
                        Ok(ev) => ev,
                        Err(quiche::h3::Error::Done) => break,
                        Err(e) => return Err(e.into()),
                    }
                };

                match event {
                    (sid, quiche::h3::Event::Headers { list, .. }) if sid == stream_id => {
                        for hdr in &list {
                            let k = std::str::from_utf8(hdr.name()).unwrap_or("").to_string();
                            let v = std::str::from_utf8(hdr.value()).unwrap_or("").to_string();
                            if k == ":status" {
                                response_status = v.parse().unwrap_or(0);
                            } else {
                                response_headers.push((k, v));
                            }
                        }
                    }

                    (sid, quiche::h3::Event::Data) if sid == stream_id => {
                        let mut body_buf = [0u8; 65535];
                        let read = {
                            let mut quic = self.conn.quic.lock().await;
                            let mut h3_guard = self.conn.h3.lock().await;
                            if let Some(h3) = h3_guard.as_mut() {
                                h3.recv_body(&mut *quic, stream_id, &mut body_buf)
                                    .unwrap_or(0)
                            } else {
                                0
                            }
                        };
                        response_body.extend_from_slice(&body_buf[..read]);
                    }

                    (sid, quiche::h3::Event::Finished) if sid == stream_id => {
                        return Ok(H3ClientResponse {
                            status: response_status,
                            headers: response_headers,
                            body: response_body,
                        });
                    }

                    _ => {}
                }
            }
        }
    }

    pub async fn realtime_connect(&self, channel: &str) -> Result<RealtimeStreamHandle> {
        let stream_id = self.conn.open_realtime_stream(channel).await?;
        self.flush().await?;

        let (tx, rx) = mpsc::channel(256);

        let socket = Arc::clone(&self.socket);
        let conn = Arc::clone(&self.conn);
        let local_addr = self.local_addr;
        let server_addr = self.server_addr;

        tokio::spawn(async move {
            let mut recv_buf = [0u8; 65535];
            let mut stream_buf: Vec<u8> = Vec::new();
            let mut out = [0u8; 1350];

            loop {
                let Ok(len) = socket.recv(&mut recv_buf).await else {
                    break;
                };

                let recv_info = quiche::RecvInfo {
                    to: local_addr,
                    from: server_addr,
                };
                {
                    let mut quic = conn.quic.lock().await;
                    if let Err(e) = quic.recv(&mut recv_buf[..len], recv_info) {
                        warn!("realtime recv error: {e}");
                    }
                }

                {
                    let mut quic = conn.quic.lock().await;
                    loop {
                        match quic.send(&mut out) {
                            Ok((w, _)) => {
                                let _ = socket.send(&out[..w]).await;
                            }
                            Err(quiche::Error::Done) => break,
                            Err(_) => break,
                        }
                    }
                }

                let data = {
                    let mut quic = conn.quic.lock().await;
                    match quic.stream_recv(stream_id, &mut recv_buf) {
                        Ok((n, _)) => recv_buf[..n].to_vec(),
                        Err(quiche::Error::Done) => continue,
                        Err(_) => break,
                    }
                };

                stream_buf.extend_from_slice(&data);

                while let Some((msg, consumed)) = parse_realtime_frame(&stream_buf) {
                    stream_buf.drain(..consumed);
                    if tx.send(msg).await.is_err() {
                        return;
                    }
                }
            }
        });

        Ok(RealtimeStreamHandle {
            stream_id,
            conn: Arc::clone(&self.conn),
            rx,
        })
    }

    pub async fn flush(&self) -> Result<()> {
        let mut out = [0u8; 1350];
        loop {
            let (write, _) = {
                let mut quic = self.conn.quic.lock().await;
                match quic.send(&mut out) {
                    Ok(v) => v,
                    Err(quiche::Error::Done) => break,
                    Err(e) => return Err(e.into()),
                }
            };
            self.socket.send(&out[..write]).await?;
        }
        Ok(())
    }
}
