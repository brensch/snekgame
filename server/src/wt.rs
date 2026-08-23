//! WebTransport (QUIC) endpoint — the only game transport.
//!
//! Connection model: the client opens one bidirectional control stream and
//! sends `Join` (session traffic) or `WatchGames` (home-screen list) as its
//! first frame. After that:
//! - control frames (lobby, game start/end, catch-ups) ride the stream,
//!   reliable and ordered;
//! - `Turn` snapshots ride datagrams, sent twice ~30ms apart; `Move`/`Ping`
//!   come back the same way. Loss is harmless: snapshots are full state and
//!   inputs are re-sent until reflected.
//!
//! QUIC gives us no TCP head-of-line blocking and connection migration
//! (wifi↔cellular) for free.

use crate::session::{conn_channel, Cmd, Frame, Registry};
use snek_shared::protocol::{decode, encode, ClientMsg, ServerMsg, PROTOCOL_VERSION};
use std::sync::Arc;
use tokio::time::{timeout, Duration};
use wtransport::endpoint::endpoint_side::Server;
use wtransport::{Connection, Endpoint, Identity, RecvStream, SendStream, ServerConfig};

const JOIN_TIMEOUT: Duration = Duration::from_secs(10);
/// Second copy of each snapshot goes out this much later, so a single burst
/// of loss doesn't eat both.
const SNAPSHOT_ECHO_DELAY: Duration = Duration::from_millis(30);
/// Frames on the control stream are u32-le length-prefixed; cap them.
const MAX_CONTROL_FRAME: u32 = 1 << 20;

pub fn build_endpoint(port: u16, identity: &Identity) -> anyhow::Result<Endpoint<Server>> {
    let config = ServerConfig::builder()
        .with_bind_default(port)
        .with_identity(identity.clone_identity())
        .keep_alive_interval(Some(Duration::from_secs(3)))
        .max_idle_timeout(Some(Duration::from_secs(10)))?
        .build();
    Ok(Endpoint::server(config)?)
}

pub async fn serve(endpoint: Endpoint<Server>, registry: Registry) {
    loop {
        let incoming = endpoint.accept().await;
        let registry = registry.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_session(incoming, registry).await {
                tracing::debug!("wt session ended: {e:#}");
            }
        });
    }
}

async fn handle_session(
    incoming: wtransport::endpoint::IncomingSession,
    registry: Registry,
) -> anyhow::Result<()> {
    let request = incoming.await?;
    let connection = request.accept().await?;

    // The client opens the control stream and speaks first.
    let (send, mut recv) = timeout(JOIN_TIMEOUT, connection.accept_bi())
        .await
        .map_err(|_| anyhow::anyhow!("no control stream"))??;
    let first = timeout(JOIN_TIMEOUT, read_frame(&mut recv))
        .await
        .map_err(|_| anyhow::anyhow!("no first frame"))??
        .ok_or_else(|| anyhow::anyhow!("stream closed before first frame"))?;
    let msg: ClientMsg =
        decode(&first).ok_or_else(|| anyhow::anyhow!("undecodable first frame"))?;

    match msg {
        ClientMsg::WatchGames => watch_games(connection, send, recv, registry).await,
        ClientMsg::Join {
            version,
            session,
            secret,
            profile,
        } => {
            join_session(
                connection, send, recv, registry, version, session, secret, profile,
            )
            .await
        }
        _ => Err(anyhow::anyhow!("first frame was not join/watch")),
    }
}

// ---------------------------------------------------------------------------
// Home screen: live games list
// ---------------------------------------------------------------------------

async fn watch_games(
    connection: Connection,
    mut send: SendStream,
    mut recv: RecvStream,
    registry: Registry,
) -> anyhow::Result<()> {
    // Answer datagram pings so watchers can measure liveness.
    let echo_conn = connection.clone();
    let echo = tokio::spawn(async move {
        while let Ok(datagram) = echo_conn.receive_datagram().await {
            if let Some(ClientMsg::Ping(n)) = decode::<ClientMsg>(&datagram) {
                let _ = echo_conn.send_datagram(encode(&ServerMsg::Pong(n)));
            }
        }
    });
    let _guard = scopeguard(echo);
    let mut rx = registry.watch_games();
    loop {
        let list = rx.borrow_and_update().clone();
        write_frame(&mut send, &encode(&ServerMsg::Sessions(list))).await?;
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
            }
            frame = read_frame(&mut recv) => {
                if frame?.is_none() {
                    return Ok(()); // client left
                }
            }
        }
    }
}

/// Abort a task when dropped.
fn scopeguard(task: tokio::task::JoinHandle<()>) -> impl Drop {
    struct Guard(tokio::task::JoinHandle<()>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    Guard(task)
}

// ---------------------------------------------------------------------------
// Session traffic
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn join_session(
    connection: Connection,
    mut send: SendStream,
    mut recv: RecvStream,
    registry: Registry,
    version: u16,
    session: String,
    secret: String,
    info: snek_shared::protocol::ProfileInfo,
) -> anyhow::Result<()> {
    if version != PROTOCOL_VERSION {
        let msg = ServerMsg::Error("protocol version mismatch — refresh the page".into());
        let _ = write_frame(&mut send, &encode(&msg)).await;
        anyhow::bail!("version mismatch");
    }
    let session = normalize_session(&session).ok_or_else(|| anyhow::anyhow!("bad session"))?;
    if !(8..=128).contains(&secret.len()) {
        anyhow::bail!("bad secret");
    }
    // The public identity is a one-way hash of the client's private secret:
    // knowing someone's public id gives you no way to act as them.
    let profile = snek_shared::PlayerProfile {
        id: public_id(&secret),
        name: info.name,
        emoji: info.emoji,
        colour: info.colour,
    };
    if !valid_profile(&profile) {
        let msg = ServerMsg::Error("set up your player first".into());
        let _ = write_frame(&mut send, &encode(&msg)).await;
        anyhow::bail!("bad profile");
    }

    write_frame(&mut send, &encode(&ServerMsg::Welcome { your_id: profile.id.clone() })).await?;

    let conn_id = registry.new_conn_id();
    let (tx, mut rx) = conn_channel();
    let ip = connection.remote_address().ip().to_string();
    let actor = registry.connect(&session, conn_id, profile, ip, tx).await;

    // Outbound: control frames -> stream; snapshots -> datagram x2.
    let out_conn = connection.clone();
    let outbound = tokio::spawn(async move {
        // A snapshot awaiting its delayed second send.
        let mut echo: Option<(Arc<[u8]>, tokio::time::Instant)> = None;
        loop {
            let echo_sleep = async {
                match &echo {
                    Some((_, at)) => tokio::time::sleep_until(*at).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                frame = rx.recv() => match frame {
                    Some(Frame::Control(bytes)) => {
                        if write_frame(&mut send, &bytes).await.is_err() {
                            break;
                        }
                    }
                    Some(Frame::Snapshot(bytes)) => {
                        // Datagrams must fit one packet; oversized snapshots
                        // (freak clash pile-ups) fall back to the stream.
                        let fits = out_conn
                            .max_datagram_size()
                            .map(|m| bytes.len() <= m)
                            .unwrap_or(false);
                        if fits {
                            let _ = out_conn.send_datagram(bytes.to_vec());
                            echo = Some((
                                bytes,
                                tokio::time::Instant::now() + SNAPSHOT_ECHO_DELAY,
                            ));
                        } else if write_frame(&mut send, &bytes).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                _ = echo_sleep => {
                    if let Some((bytes, _)) = echo.take() {
                        let _ = out_conn.send_datagram(bytes.to_vec());
                    }
                }
            }
        }
    });

    // Inbound datagrams: moves + pings (pong answered directly).
    let dgram_conn = connection.clone();
    let dgram_actor = actor.clone();
    let inbound_dgrams = tokio::spawn(async move {
        while let Ok(datagram) = dgram_conn.receive_datagram().await {
            match decode::<ClientMsg>(&datagram) {
                Some(ClientMsg::Ping(n)) => {
                    // Pong here, where it is cheapest. The actor still needs
                    // to hear about it: a keepalive is the only traffic a
                    // spectating device produces, so it is the only thing
                    // that can prove a host we wrote off as asleep is in
                    // fact fine. Without this, that guess is unfalsifiable
                    // and the game stays paused forever.
                    let _ = dgram_conn.send_datagram(encode(&ServerMsg::Pong(n)));
                    if dgram_actor.send(Cmd::Alive { conn_id }).await.is_err() {
                        break;
                    }
                }
                // Everything latency-critical arrives here: player input
                // and bot answers alike. Duplicates are harmless — the
                // session drops any request id it has already retired.
                Some(msg @ (ClientMsg::Move(_)
                    | ClientMsg::BotMove { .. }
                    | ClientMsg::ComputeFailed { .. })) => {
                    if dgram_actor.send(Cmd::Msg { conn_id, msg }).await.is_err() {
                        break;
                    }
                }
                _ => {}
            }
        }
    });

    // Inbound control frames until the client goes away.
    loop {
        match read_frame(&mut recv).await {
            Ok(Some(bytes)) => {
                if let Some(msg) = decode::<ClientMsg>(&bytes) {
                    if actor.send(Cmd::Msg { conn_id, msg }).await.is_err() {
                        break;
                    }
                }
            }
            _ => break,
        }
    }

    let _ = actor.send(Cmd::Disconnect { conn_id }).await;
    outbound.abort();
    inbound_dgrams.abort();
    Ok(())
}

// ---------------------------------------------------------------------------
// Stream framing: u32-le length prefix + postcard bytes
// ---------------------------------------------------------------------------

async fn read_frame(recv: &mut RecvStream) -> anyhow::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match read_exact(recv, &mut len_buf).await? {
        false => return Ok(None),
        true => {}
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_CONTROL_FRAME {
        anyhow::bail!("oversized control frame");
    }
    let mut buf = vec![0u8; len as usize];
    if !read_exact(recv, &mut buf).await? {
        return Ok(None);
    }
    Ok(Some(buf))
}

async fn read_exact(recv: &mut RecvStream, buf: &mut [u8]) -> anyhow::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match recv.read(&mut buf[filled..]).await? {
            Some(n) => filled += n,
            None => return Ok(false), // clean EOF
        }
    }
    Ok(true)
}

async fn write_frame(send: &mut SendStream, bytes: &[u8]) -> anyhow::Result<()> {
    send.write_all(&(bytes.len() as u32).to_le_bytes()).await?;
    send.write_all(bytes).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Identity helpers
// ---------------------------------------------------------------------------

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

/// Load the TLS identity: real certs if the env points at them, otherwise a
/// self-signed one (dev / tests).
pub async fn load_identity() -> anyhow::Result<Identity> {
    match (std::env::var("SNEK_CERT"), std::env::var("SNEK_KEY")) {
        (Ok(cert), Ok(key)) => {
            let identity = Identity::load_pemfiles(&cert, &key).await?;
            tracing::info!("loaded TLS identity from {cert}");
            Ok(identity)
        }
        _ => {
            tracing::warn!("SNEK_CERT/SNEK_KEY unset — using a self-signed identity");
            Ok(Identity::self_signed(["localhost", "127.0.0.1"])?)
        }
    }
}
