//! Quick live-server probe: connect, join, print the first few messages.
//! Usage: cargo run -p snek-server --example probe [ws://host:port/ws]

use futures_util::{SinkExt, StreamExt};
use snek_shared::protocol::*;
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ws://127.0.0.1:8080/ws".to_string());
    let (mut ws, resp) = tokio_tungstenite::connect_async(&url).await.expect("connect");
    println!("connected: HTTP {}", resp.status());

    ws.send(Message::Binary(encode(&ClientMsg::Join {
        version: PROTOCOL_VERSION,
        session: "probe-test".to_string(),
        secret: "probe-secret-000000".to_string(),
        profile: ProfileInfo {
            name: "probe".to_string(),
            emoji: "🧪".to_string(),
            colour: "hsl(200, 85%, 60%)".to_string(),
        },
        display_name: Some("probe game".to_string()),
    })))
    .await
    .unwrap();

    for _ in 0..3 {
        match tokio::time::timeout(std::time::Duration::from_secs(5), ws.next()).await {
            Ok(Some(Ok(Message::Binary(bytes)))) => {
                println!("<- {:?}", decode::<ServerMsg>(&bytes));
            }
            Ok(Some(Ok(other))) => println!("<- non-binary: {other:?}"),
            Ok(Some(Err(e))) => {
                println!("!! ws error: {e}");
                break;
            }
            Ok(None) => {
                println!("!! closed by server");
                break;
            }
            Err(_) => {
                println!("!! timeout waiting for message");
                break;
            }
        }
    }
}
