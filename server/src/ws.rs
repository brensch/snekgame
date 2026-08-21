//! Websocket endpoint. Connection model: the first frame must be a `Join`;
//! after that the socket is a dumb pipe — decoded client messages go to the
//! session actor, pre-encoded frames come back out. Read inactivity (no
//! frames at all, including app-level pings the client sends every 15s) kills
//! the connection; the client reconnects and rejoins into a full snapshot.

use crate::session::{conn_channel, Cmd, Registry};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use snek_shared::protocol::{decode, encode, ClientMsg, ServerMsg, PROTOCOL_VERSION};
use tokio::time::{timeout, Duration};

const JOIN_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn ws_handler(ws: WebSocketUpgrade, State(registry): State<Registry>) -> Response {
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = handle_socket(socket, registry).await {
            tracing::debug!("ws closed: {e}");
        }
    })
}

async fn handle_socket(mut socket: WebSocket, registry: Registry) -> Result<(), &'static str> {
    // First frame must be Join.
    let join = timeout(JOIN_TIMEOUT, socket.recv())
        .await
        .map_err(|_| "join timeout")?
        .ok_or("closed before join")?
        .map_err(|_| "socket error")?;

    let msg = match join {
        Message::Binary(bytes) => decode::<ClientMsg>(&bytes).ok_or("bad join frame")?,
        _ => return Err("expected binary join frame"),
    };

    let (version, session, secret, info, display_name) = match msg {
        ClientMsg::Join {
            version,
            session,
            secret,
            profile,
            display_name,
        } => (version, session, secret, profile, display_name),
        _ => return Err("first frame was not join"),
    };

    if version != PROTOCOL_VERSION {
        let _ = socket
            .send(Message::Binary(
                encode(&ServerMsg::Error("protocol version mismatch — refresh the page".into()))
                    .into(),
            ))
            .await;
        return Err("version mismatch");
    }

    let session = normalize_session(&session).ok_or("bad session name")?;
    if !(8..=128).contains(&secret.len()) {
        return Err("bad secret");
    }
    // The public identity is a one-way hash of the client's private secret:
    // knowing someone's public id (broadcast in lobbies and games) gives you
    // no way to act as them.
    let profile = snek_shared::PlayerProfile {
        id: public_id(&secret),
        name: info.name,
        emoji: info.emoji,
        colour: info.colour,
    };
    if !valid_profile(&profile) {
        // Player setup is mandatory: no name, no entry — tell them why.
        let _ = socket
            .send(Message::Binary(
                encode(&ServerMsg::Error("set up your player first".into())).into(),
            ))
            .await;
        return Err("bad profile");
    }

    let conn_id = registry.new_conn_id();
    let (tx, mut rx) = conn_channel();
    // Tell the client who it is before any lobby traffic.
    let _ = tx
        .try_send(encode(&ServerMsg::Welcome { your_id: profile.id.clone() }).into());
    let actor = registry
        .connect(&session, conn_id, profile, display_name, tx)
        .await;

    let (mut ws_tx, mut ws_rx) = {
        use futures_util::StreamExt;
        socket.split()
    };

    // Writer: session frames -> socket.
    let writer = tokio::spawn(async move {
        use futures_util::SinkExt;
        while let Some(frame) = rx.recv().await {
            if ws_tx
                .send(Message::Binary(frame.to_vec().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Reader: socket -> session, with inactivity timeout.
    loop {
        use futures_util::StreamExt;
        let frame = match timeout(READ_TIMEOUT, ws_rx.next()).await {
            Err(_) => break, // silent for too long
            Ok(None) => break,
            Ok(Some(Err(_))) => break,
            Ok(Some(Ok(f))) => f,
        };
        match frame {
            Message::Binary(bytes) => {
                if let Some(msg) = decode::<ClientMsg>(&bytes) {
                    if actor.send(Cmd::Msg { conn_id, msg }).await.is_err() {
                        break;
                    }
                }
            }
            Message::Close(_) => break,
            // Axum answers ws-level pings itself.
            _ => {}
        }
    }

    let _ = actor.send(Cmd::Disconnect { conn_id }).await;
    writer.abort();
    Ok(())
}

/// Public player id: first 12 hex chars of sha256(secret).
pub fn public_id(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(secret.as_bytes());
    digest[..6].iter().map(|b| format!("{b:02x}")).collect()
}

/// Session words: lowercase letters/digits/dashes, 1..=24 chars.
pub fn normalize_session(raw: &str) -> Option<String> {
    let s: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(24)
        .collect();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn valid_profile(p: &snek_shared::PlayerProfile) -> bool {
    !p.id.is_empty()
        && p.id.len() <= 64
        && !p.name.trim().is_empty()
        && p.name.len() <= 32
        && p.emoji.len() <= 16
        && p.colour.len() <= 32
}
