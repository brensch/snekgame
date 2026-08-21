//! Full-stack test: two websocket clients join a session, ready up, start a
//! game, play until it ends, and the finished game lands in sqlite and is
//! served by the history API.

use futures_util::{SinkExt, StreamExt};
use snek_shared::protocol::*;
use snek_shared::{Dir, GameConfig};
use tokio_tungstenite::tungstenite::Message;

type Ws = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

fn info(name: &str) -> ProfileInfo {
    ProfileInfo {
        name: name.to_string(),
        emoji: "🐍".to_string(),
        colour: "hsl(120, 85%, 60%)".to_string(),
    }
}

async fn connect(port: u16, session: &str, secret: &str, profile: ProfileInfo) -> Ws {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .expect("ws connect");
    ws.send(Message::Binary(encode(&ClientMsg::Join {
        version: PROTOCOL_VERSION,
        session: session.to_string(),
        secret: secret.to_string(),
        profile,
    })))
    .await
    .unwrap();
    ws
}

async fn send(ws: &mut Ws, msg: &ClientMsg) {
    ws.send(Message::Binary(encode(msg))).await.unwrap();
}

async fn next_msg(ws: &mut Ws) -> ServerMsg {
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(15), ws.next())
            .await
            .expect("timed out waiting for server message")
            .expect("stream ended")
            .expect("ws error");
        if let Message::Binary(bytes) = frame {
            if let Some(msg) = decode::<ServerMsg>(&bytes) {
                return msg;
            }
        }
    }
}

#[tokio::test]
async fn two_players_play_a_full_game() {
    let dir = std::env::temp_dir().join(format!("snek-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let app = snek_server::build_app(&db_path);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut alice = connect(port, "e2e-test", "alice-secret-0000", info("alice")).await;
    let mut bob = connect(port, "e2e-test", "bob-secret-000000", info("bob")).await;

    // Identity: the server assigns a public id that is a hash of the secret,
    // so the secret itself is never visible to other players.
    let alice_id = match next_msg(&mut alice).await {
        ServerMsg::Welcome { your_id } => your_id,
        other => panic!("expected welcome, got {other:?}"),
    };
    assert_eq!(alice_id.len(), 12, "public id is a short hash");
    assert!(!alice_id.contains("secret"), "secret must not leak");

    // Both must appear in the lobby.
    let mut seen_both = false;
    for _ in 0..10 {
        if let ServerMsg::Lobby(l) = next_msg(&mut bob).await {
            if l.players.len() == 2 {
                seen_both = true;
                break;
            }
        }
    }
    assert!(seen_both, "lobby never showed both players");

    // Configure a fast small game, ready up, start.
    send(
        &mut alice,
        &ClientMsg::Configure(GameConfig {
            board_width: 11,
            board_height: 11,
            tick_ms: 250,
        }),
    )
    .await;
    send(&mut alice, &ClientMsg::SetReady(true)).await;
    send(&mut bob, &ClientMsg::SetReady(true)).await;
    send(&mut alice, &ClientMsg::Start).await;

    // Alice should see the game start.
    let mut started = false;
    for _ in 0..10 {
        if let ServerMsg::GameStart { turn, .. } = next_msg(&mut alice).await {
            assert_eq!(turn.alive.len(), 2);
            started = true;
            break;
        }
    }
    assert!(started, "game never started");

    // Steer sneks up and down and wait for the game to finish (they will hit
    // walls and the game resolves).
    send(&mut alice, &ClientMsg::Move(Dir::Up)).await;
    send(&mut bob, &ClientMsg::Move(Dir::Down)).await;

    let mut saved_id: Option<i64> = None;
    let mut game_over = false;
    for _ in 0..200 {
        match next_msg(&mut alice).await {
            ServerMsg::Turn(turn) => {
                if turn.game_over() {
                    game_over = true;
                    assert_eq!(turn.winners.len(), 2, "all players ranked");
                }
            }
            ServerMsg::GameSaved { id } => {
                saved_id = Some(id);
                break;
            }
            _ => {}
        }
    }
    assert!(game_over, "game never ended");
    let saved_id = saved_id.expect("game was not saved");

    // History API serves the finished game.
    let body = http_get(port, "/api/history").await;
    assert!(body.contains("e2e-test"), "history list missing the game: {body}");
    assert!(body.contains("alice"), "history list missing players");

    let blob = http_get_bytes(port, &format!("/api/history/{saved_id}")).await;
    let record: snek_shared::GameRecord = postcard::from_bytes(&blob).expect("decode record");
    assert_eq!(record.players.len(), 2);
    assert!(record.turns.last().unwrap().game_over());

    // Reconnect model: a fresh connection with the same secret gets the same
    // public identity back.
    let mut alice2 = connect(port, "e2e-test", "alice-secret-0000", info("alice")).await;
    match next_msg(&mut alice2).await {
        ServerMsg::Welcome { your_id } => assert_eq!(your_id, alice_id),
        other => panic!("expected welcome, got {other:?}"),
    }
    match next_msg(&mut alice2).await {
        ServerMsg::Lobby(l) => assert_eq!(l.session, "e2e-test"),
        other => panic!("expected lobby for rejoiner, got {other:?}"),
    }
}

async fn http_get(port: u16, path: &str) -> String {
    String::from_utf8_lossy(&http_get_bytes(port, path).await).to_string()
}

async fn http_get_bytes(port: u16, path: &str) -> Vec<u8> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    assert!(head.contains("200 OK"), "bad response: {head}");
    buf[split + 4..].to_vec()
}
