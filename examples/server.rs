use std::net::{AddrParseError, SocketAddr};
use std::sync::Arc;

use airnomads::error::H3Error;
use airnomads::{
    H3Server, QuicConfig,
    error::Result,
    realtime::{RealtimeChannel, RealtimeEvent, RealtimeMessage},
    server::{H3Request, H3Response},
};
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info,quic_h3=debug")
        .init();

    let bind: SocketAddr = "0.0.0.0:4433"
        .parse()
        .map_err(|e: AddrParseError| H3Error::AddrErr(e.to_string()))?;

    let config = QuicConfig::new().with_cert("./cert.pem", "./key.pem");

    let server = H3Server::new(bind, config)
        .on_request(
            |req: H3Request, conn: Arc<airnomads::QuicConnection>| async move {
                info!(
                    "[HTTP/3] {} {} from {}",
                    req.method, req.path, conn.peer_addr
                );

                match (req.method.as_str(), req.path.as_str()) {
                    ("GET", "/") => H3Response::json(
                        200,
                        serde_json::json!({
                            "message": "Hello from HTTP/3!",
                            "conn_id": conn.conn_id,
                        }),
                    ),

                    ("GET", "/health") => H3Response::ok(r#"{"status":"ok"}"#),

                    ("POST", "/echo") => {
                        let body = req.body.clone();
                        H3Response {
                            status: 200,
                            headers: vec![(
                                "content-type".into(),
                                "application/octet-stream".into(),
                            )],
                            body,
                        }
                    }

                    _ => H3Response::not_found(),
                }
            },
        )
        .on_realtime(
            |event: RealtimeEvent, channel: Arc<RealtimeChannel>| async move {
                match event {
                    RealtimeEvent::Join {
                        conn_id,
                        channel: ch,
                        stream_id,
                    } => {
                        info!(
                            "[Realtime] {} joined channel '{}' stream_id {}",
                            conn_id, ch, stream_id
                        );

                        channel
                            .broadcast(
                                &ch,
                                RealtimeMessage::new(
                                    "system",
                                    &ch,
                                    serde_json::json!({
                                        "text": format!("User {} joined", conn_id),
                                    }),
                                ),
                                None,
                            )
                            .await;
                    }

                    RealtimeEvent::Leave {
                        conn_id,
                        channel: ch,
                    } => {
                        info!("[Realtime] {} left channel '{}'", conn_id, ch);
                    }

                    RealtimeEvent::Message {
                        conn_id,
                        stream_id,
                        msg,
                    } => {
                        info!(
                            "[Realtime] msg from {} stream_id {} on '{}': {:?}",
                            conn_id, stream_id, msg.channel, msg.payload
                        );

                        channel.broadcast(&msg.channel, msg.clone(), None).await;
                    }

                    RealtimeEvent::Disconnected { conn_id } => {
                        info!("[Realtime] {} disconnected", conn_id);
                    }
                }
            },
        );

    info!("Server starting on {bind}");
    server.run().await?;

    Ok(())
}
