use airnomads::{
    H3Client, QuicConfig,
    error::{H3Error, Result},
};
use std::net::{AddrParseError, SocketAddr};
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info,quic_h3=debug")
        .init();

    let server_addr: SocketAddr = "0.0.0.0:4433"
        .parse()
        .map_err(|e: AddrParseError| H3Error::AddrErr(e.to_string()))?;

    let local_addr: SocketAddr = "0.0.0.0:0"
        .parse()
        .map_err(|e: AddrParseError| H3Error::AddrErr(e.to_string()))?;

    let config = QuicConfig::new().with_no_verify();

    let client = H3Client::new(server_addr, local_addr, config, "localhost");
    let conn = client.connect().await?;
    conn.handshake().await?;

    info!("=== HTTP/3 GET / ===");
    let resp = conn.get("/").await?;
    info!("Status: {}", resp.status);
    info!("Body: {}", String::from_utf8_lossy(&resp.body));

    info!("=== HTTP/3 POST /echo ===");
    let resp = conn
        .post_json("/echo", serde_json::json!({ "hello": "world" }))
        .await?;
    info!("Echo body: {}", String::from_utf8_lossy(&resp.body));

    info!("=== Realtime: joining channel 'chat' ===");
    let mut stream = conn.realtime_connect("chat").await?;

    for i in 0..3 {
        stream
            .send(
                "message",
                serde_json::json!({ "text": format!("Hello #{}", i) }),
            )
            .await?;
        info!("Sent message #{}", i);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    info!("=== Listening for incoming realtime messages... ===");
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(2), stream.recv()).await {
            Ok(Some(msg)) => {
                info!("[Realtime] event='{}' payload={:?}", msg.event, msg.payload);
            }
            Ok(None) | Err(_) => {
                info!("No more messages, done.");
                break;
            }
        }
    }

    Ok(())
}
