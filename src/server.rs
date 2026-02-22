use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use quiche::h3::NameValue;
use tokio::net::UdpSocket;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, error, info, warn};

use crate::config::QuicConfig;
use crate::connection::{QuicConnection, StreamMeta, StreamType};
use crate::error::Result;
use crate::realtime::{
    RealtimeChannel, RealtimeEvent, encode_realtime_frame, parse_realtime_frame,
};

pub type RequestHandler = Arc<
    dyn Fn(
            H3Request,
            Arc<QuicConnection>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = H3Response> + Send>>
        + Send
        + Sync,
>;

pub type RealtimeHandler = Arc<
    dyn Fn(
            RealtimeEvent,
            Arc<RealtimeChannel>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone)]
pub struct H3Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub stream_id: u64,
    pub conn_id: String,
}

#[derive(Debug, Clone)]
pub struct H3Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl H3Response {
    pub fn json(status: u16, body: serde_json::Value) -> Self {
        let body_bytes = body.to_string().into_bytes();
        Self {
            status,
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("content-length".to_string(), body_bytes.len().to_string()),
            ],
            body: body_bytes,
        }
    }

    pub fn ok(body: impl Into<Vec<u8>>) -> Self {
        let body = body.into();
        Self {
            status: 200,
            headers: vec![("content-length".to_string(), body.len().to_string())],
            body,
        }
    }

    pub fn not_found() -> Self {
        Self::json(404, serde_json::json!({ "error": "not found" }))
    }
}

pub struct H3Server {
    bind_addr: SocketAddr,
    config: QuicConfig,
    request_handler: Option<RequestHandler>,
    realtime_handler: Option<RealtimeHandler>,
    pub channel: RealtimeChannel,
}

impl H3Server {
    pub fn new(bind_addr: SocketAddr, config: QuicConfig) -> Self {
        Self {
            bind_addr,
            config,
            request_handler: None,
            realtime_handler: None,
            channel: RealtimeChannel::new(),
        }
    }

    pub fn on_request<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(H3Request, Arc<QuicConnection>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = H3Response> + Send + 'static,
    {
        self.request_handler = Some(Arc::new(move |req, conn| Box::pin(handler(req, conn))));
        self
    }

    pub fn on_realtime<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(RealtimeEvent, Arc<RealtimeChannel>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.realtime_handler = Some(Arc::new(move |ev, ch| Box::pin(handler(ev, ch))));
        self
    }

    pub async fn run(self) -> Result<()> {
        let socket = Arc::new(UdpSocket::bind(self.bind_addr).await?);
        info!("HTTP/3 server listening on {}", self.bind_addr);

        let mut quiche_config = self.config.build_quiche_config()?;

        let connections: Arc<RwLock<HashMap<String, Arc<QuicConnection>>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let channel = Arc::new(self.channel.clone_handle());
        let req_handler = self.request_handler.clone();
        let rt_handler = self.realtime_handler.clone();

        let mut buf = [0u8; 65535];
        let mut out = [0u8; 1350];

        loop {
            let (len, from) = match socket.recv_from(&mut buf).await {
                Ok(v) => v,
                Err(e) => {
                    error!("recv_from error: {e}");
                    continue;
                }
            };

            let mut pkt = &mut buf[..len];

            let hdr = match quiche::Header::from_slice(pkt, quiche::MAX_CONN_ID_LEN) {
                Ok(h) => h,
                Err(e) => {
                    warn!("Failed to parse QUIC header from {from}: {e}");
                    continue;
                }
            };

            let conn_id = format!("{:?}", hdr.dcid);

            let conn_arc = {
                let conns = connections.read().await;
                conns.get(&conn_id).cloned()
            };

            let conn_arc = if let Some(c) = conn_arc {
                c
            } else {
                if hdr.ty != quiche::Type::Initial {
                    warn!("Non-initial packet from unknown connection, dropping");
                    continue;
                }

                let scid = quiche::ConnectionId::from_ref(&hdr.dcid);
                let (rt_tx, mut rt_rx) = mpsc::channel::<RealtimeEvent>(256);

                let quic_conn =
                    quiche::accept(&scid, None, self.bind_addr, from, &mut quiche_config)?;

                let conn = Arc::new(QuicConnection::new(conn_id.clone(), from, quic_conn, rt_tx));

                connections
                    .write()
                    .await
                    .insert(conn_id.clone(), conn.clone());

                {
                    let ch = Arc::clone(&channel);
                    let rt_handler = rt_handler.clone();
                    tokio::spawn(async move {
                        while let Some(ev) = rt_rx.recv().await {
                            if let Some(h) = &rt_handler {
                                h(ev, Arc::clone(&ch)).await;
                            }
                        }
                    });
                }

                info!("New QUIC connection from {from} id={conn_id}");
                conn
            };

            {
                let recv_info = quiche::RecvInfo {
                    to: self.bind_addr,
                    from,
                };
                let mut quic = conn_arc.quic.lock().await;
                if let Err(e) = quic.recv(&mut pkt, recv_info) {
                    warn!("quic.recv error: {e}");
                }
            }

            if conn_arc.is_established().await && conn_arc.h3.lock().await.is_none() {
                conn_arc.init_h3_server().await?;
            }

            if conn_arc.h3.lock().await.is_some() {
                Self::process_h3_events(&conn_arc, &channel, req_handler.as_ref()).await;
            }

            Self::process_realtime_streams(&conn_arc, &channel).await;

            {
                let mut quic = conn_arc.quic.lock().await;
                loop {
                    let (write, _) = match quic.send(&mut out) {
                        Ok(v) => v,
                        Err(quiche::Error::Done) => break,
                        Err(e) => {
                            error!("quic.send error: {e}");
                            break;
                        }
                    };
                    if let Err(e) = socket.send_to(&out[..write], from).await {
                        error!("send_to error: {e}");
                    }
                }
            }

            if conn_arc.is_closed().await {
                connections.write().await.remove(&conn_id);
                info!("Connection {conn_id} closed");
            }
        }
    }

    async fn process_h3_events(
        conn: &Arc<QuicConnection>,
        channel: &Arc<RealtimeChannel>,
        handler: Option<&RequestHandler>,
    ) {
        let mut pending_requests: HashMap<u64, H3Request> = HashMap::new();

        loop {
            let event = {
                let mut quic = conn.quic.lock().await;
                let mut h3_guard = conn.h3.lock().await;
                let h3 = match h3_guard.as_mut() {
                    Some(h) => h,
                    None => break,
                };
                match h3.poll(&mut *quic) {
                    Ok(ev) => ev,
                    Err(quiche::h3::Error::Done) => break,
                    Err(e) => {
                        warn!("H3 poll error: {e}");
                        break;
                    }
                }
            };

            match event {
                (stream_id, quiche::h3::Event::Headers { list, .. }) => {
                    let mut method = String::new();
                    let mut path = String::new();
                    let mut headers = Vec::new();

                    for hdr in &list {
                        let k = std::str::from_utf8(hdr.name()).unwrap_or("").to_string();
                        let v = std::str::from_utf8(hdr.value()).unwrap_or("").to_string();
                        match k.as_str() {
                            ":method" => method = v,
                            ":path" => path = v,
                            _ => headers.push((k, v)),
                        }
                    }

                    debug!("H3 request: {method} {path} on stream {stream_id}");

                    pending_requests.insert(
                        stream_id,
                        H3Request {
                            method,
                            path,
                            headers,
                            body: Vec::new(),
                            stream_id,
                            conn_id: conn.conn_id.clone(),
                        },
                    );
                }

                (stream_id, quiche::h3::Event::Data) => {
                    let mut body_buf = [0u8; 65535];
                    let read = {
                        let mut quic = conn.quic.lock().await;
                        let mut h3_guard = conn.h3.lock().await;
                        if let Some(h3) = h3_guard.as_mut() {
                            h3.recv_body(&mut *quic, stream_id, &mut body_buf)
                                .unwrap_or(0)
                        } else {
                            0
                        }
                    };

                    if let Some(req) = pending_requests.get_mut(&stream_id) {
                        req.body.extend_from_slice(&body_buf[..read]);
                    }
                }

                (stream_id, quiche::h3::Event::Finished) => {
                    if let Some(req) = pending_requests.remove(&stream_id) {
                        // Dispatch ke request handler
                        if let Some(h) = handler {
                            let conn_clone = Arc::clone(conn);
                            let h_clone = Arc::clone(h);
                            let conn_for_resp = Arc::clone(conn);
                            tokio::spawn(async move {
                                let response = h_clone(req, conn_clone).await;
                                // Kirim response
                                let status_str = response.status.to_string();
                                let mut h3_headers = vec![
                                    quiche::h3::Header::new(b":status", status_str.as_bytes()),
                                    quiche::h3::Header::new(b"server", b"quic-h3/0.1"),
                                ];
                                for (k, v) in &response.headers {
                                    h3_headers
                                        .push(quiche::h3::Header::new(k.as_bytes(), v.as_bytes()));
                                }
                                if let Err(e) = conn_for_resp
                                    .send_h3_response(stream_id, &h3_headers, Some(&response.body))
                                    .await
                                {
                                    error!("Failed to send H3 response: {e}");
                                }
                            });
                        }
                    }
                }

                _ => {}
            }
        }
    }

    async fn process_realtime_streams(conn: &Arc<QuicConnection>, channel: &Arc<RealtimeChannel>) {
        let mut buf = [0u8; 65535];
        let mut stream_buf: HashMap<u64, Vec<u8>> = HashMap::new();

        let readable: Vec<u64> = {
            let quic = conn.quic.lock().await;
            quic.readable().collect()
        };

        for stream_id in readable {
            {
                let streams = conn.streams.lock().await;
                if let Some(meta) = streams.get(&stream_id) {
                    if meta.stream_type != StreamType::Realtime {
                        continue;
                    }
                }
            }

            let read = {
                let mut quic = conn.quic.lock().await;
                match quic.stream_recv(stream_id, &mut buf) {
                    Ok((n, _)) => n,
                    Err(quiche::Error::Done) => continue,
                    Err(e) => {
                        warn!("stream_recv error on {stream_id}: {e}");
                        continue;
                    }
                }
            };

            let entry = stream_buf.entry(stream_id).or_default();
            entry.extend_from_slice(&buf[..read]);

            while let Some((msg, consumed)) = parse_realtime_frame(entry) {
                entry.drain(..consumed);

                if msg.event == "join" {
                    let ch = msg.channel.clone();
                    let cid = conn.conn_id.clone();
                    let mut rx = channel.subscribe(&ch, &cid).await;

                    conn.streams.lock().await.insert(
                        stream_id,
                        StreamMeta {
                            stream_id,
                            stream_type: StreamType::Realtime,
                            channel: Some(ch.clone()),
                        },
                    );

                    let _ = conn
                        .realtime_tx
                        .send(RealtimeEvent::Join {
                            conn_id: cid.clone(),
                            channel: ch.clone(),
                            stream_id,
                        })
                        .await;

                    let conn_clone = Arc::clone(conn);
                    tokio::spawn(async move {
                        while let Some(broadcast_msg) = rx.recv().await {
                            if let Ok(frame) = encode_realtime_frame(&broadcast_msg) {
                                let mut quic = conn_clone.quic.lock().await;
                                let _ = quic.stream_send(stream_id, &frame, false);
                            }
                        }
                    });
                } else {
                    let channel_name = {
                        conn.streams
                            .lock()
                            .await
                            .get(&stream_id)
                            .and_then(|m| m.channel.clone())
                    };

                    if let Some(ch) = channel_name {
                        channel
                            .broadcast(&ch, msg.clone(), Some(&conn.conn_id))
                            .await;

                        let _ = conn
                            .realtime_tx
                            .send(RealtimeEvent::Message {
                                conn_id: conn.conn_id.clone(),
                                stream_id,
                                msg,
                            })
                            .await;
                    }
                }
            }
        }
    }
}
