//! Full-stack test over real QUIC: two WebTransport clients join a session,
//! ready up, play a game via datagram snapshots, and the finished game lands
//! in sqlite and the history API.

use snek_shared::protocol::*;
use snek_shared::{Dir, GameConfig};
use std::time::Duration;
use wtransport::{ClientConfig, Connection, Endpoint, Identity, RecvStream, SendStream};

struct Client {
    conn: Connection,
    send: SendStream,
    /// Stream + datagram messages merged by reader tasks (cancel-safe).
    rx: tokio::sync::mpsc::UnboundedReceiver<ServerMsg>,
}

fn info(name: &str) -> ProfileInfo {
    ProfileInfo {
        name: name.to_string(),
        emoji: "🐍".to_string(),
        colour: "hsl(120, 85%, 60%)".to_string(),
    }
}

async fn write_frame(send: &mut SendStream, msg: &ClientMsg) {
    let bytes = encode(msg);
    send.write_all(&(bytes.len() as u32).to_le_bytes()).await.unwrap();
    send.write_all(&bytes).await.unwrap();
}

async fn read_stream_msg(recv: &mut RecvStream) -> ServerMsg {
    let mut len_buf = [0u8; 4];
    read_exact(recv, &mut len_buf).await;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    read_exact(recv, &mut buf).await;
    decode(&buf).expect("bad stream frame")
}

async fn read_exact(recv: &mut RecvStream, buf: &mut [u8]) {
    let mut filled = 0;
    while filled < buf.len() {
        let n = recv.read(&mut buf[filled..]).await.unwrap().expect("eof");
        filled += n;
    }
}

impl Client {
    async fn connect(port: u16, session: &str, secret: &str, profile: ProfileInfo) -> Client {
        let config = ClientConfig::builder()
            .with_bind_default()
            .with_no_cert_validation()
            .build();
        let conn = Endpoint::client(config)
            .unwrap()
            .connect(format!("https://127.0.0.1:{port}"))
            .await
            .expect("wt connect");
        let (mut send, mut recv) = conn.open_bi().await.unwrap().await.unwrap();
        write_frame(
            &mut send,
            &ClientMsg::Join {
                version: PROTOCOL_VERSION,
                session: session.to_string(),
                secret: secret.to_string(),
                profile,
            },
        )
        .await;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let stream_tx = tx.clone();
        tokio::spawn(async move {
            loop {
                let msg = read_stream_msg(&mut recv).await;
                if stream_tx.send(msg).is_err() {
                    break;
                }
            }
        });
        let dgram_conn = conn.clone();
        tokio::spawn(async move {
            while let Ok(dgram) = dgram_conn.receive_datagram().await {
                if let Some(msg) = decode::<ServerMsg>(&dgram) {
                    if tx.send(msg).is_err() {
                        break;
                    }
                }
            }
        });
        Client { conn, send, rx }
    }

    async fn control(&mut self, msg: &ClientMsg) {
        write_frame(&mut self.send, msg).await;
    }

    /// Next message from either the control stream or a datagram.
    async fn next_msg(&mut self) -> ServerMsg {
        tokio::time::timeout(Duration::from_secs(15), self.rx.recv())
            .await
            .expect("timed out waiting for server message")
            .expect("connection closed")
    }
}

#[tokio::test]
async fn two_players_play_a_full_game() {
    let dir = std::env::temp_dir().join(format!("snek-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let (app, registry) = snek_server::build(&db_path);

    // HTTP on an ephemeral port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // WebTransport on an ephemeral UDP port with a self-signed identity.
    let identity = Identity::self_signed(["localhost", "127.0.0.1"]).unwrap();
    let endpoint = snek_server::wt::build_endpoint(0, &identity).unwrap();
    let wt_port = endpoint.local_addr().unwrap().port();
    tokio::spawn(snek_server::wt::serve(endpoint, registry));

    let mut alice = Client::connect(wt_port, "e2e-test", "alice-secret-0000", info("alice")).await;
    let mut bob = Client::connect(wt_port, "e2e-test", "bob-secret-000000", info("bob")).await;

    // Identity: hash of the secret, never the secret.
    let alice_id = match alice.next_msg().await {
        ServerMsg::Welcome { your_id } => your_id,
        other => panic!("expected welcome, got {other:?}"),
    };
    assert_eq!(alice_id.len(), 12);
    assert!(!alice_id.contains("secret"));

    // Both must appear in the lobby.
    let mut seen_both = false;
    for _ in 0..10 {
        if let ServerMsg::Lobby(l) = bob.next_msg().await {
            if l.players.len() == 2 {
                seen_both = true;
                break;
            }
        }
    }
    assert!(seen_both, "lobby never showed both players");

    alice
        .control(&ClientMsg::Configure(GameConfig {
            board_width: 11,
            board_height: 11,
            tick_ms: 250,
        }))
        .await;
    alice.control(&ClientMsg::SetReady(true)).await;
    bob.control(&ClientMsg::SetReady(true)).await;
    // Different connections have no cross-ordering: wait until the lobby
    // echoes both ready before starting.
    for _ in 0..10 {
        if let ServerMsg::Lobby(l) = alice.next_msg().await {
            if l.ready.len() == 2 {
                break;
            }
        }
    }
    alice.control(&ClientMsg::Start).await;

    let mut started = false;
    for _ in 0..10 {
        if let ServerMsg::GameStart { turn, .. } = alice.next_msg().await {
            assert_eq!(turn.alive.len(), 2);
            started = true;
            break;
        }
    }
    assert!(started, "game never started");

    // Moves go up as datagrams.
    let _ = alice.conn.send_datagram(encode(&ClientMsg::Move(Dir::Up)));
    let _ = bob.conn.send_datagram(encode(&ClientMsg::Move(Dir::Down)));
    // Datagram pings get datagram pongs.
    let _ = alice.conn.send_datagram(encode(&ClientMsg::Ping(7)));

    let mut saved_id: Option<i64> = None;
    let mut game_over = false;
    let mut got_pong = false;
    let mut turn_count = 0u32;
    for _ in 0..400 {
        match alice.next_msg().await {
            ServerMsg::Turn(turn) => {
                turn_count += 1;
                if turn.game_over() {
                    game_over = true;
                    assert_eq!(turn.winners.len(), 2, "all players ranked");
                }
            }
            ServerMsg::Pong(7) => got_pong = true,
            ServerMsg::GameSaved { id, .. } => {
                saved_id = Some(id);
                break;
            }
            _ => {}
        }
    }
    assert!(game_over, "game never ended");
    assert!(got_pong, "datagram ping never answered");
    assert!(turn_count > 0, "no turn snapshots arrived");
    let saved_id = saved_id.expect("game was not saved");

    // History API serves the finished game.
    let body = http_get(http_port, "/api/history").await;
    assert!(body.contains("alice"), "history list missing players");
    let blob = http_get_bytes(http_port, &format!("/api/history/{saved_id}")).await;
    let record: snek_shared::GameRecord = postcard::from_bytes(&blob).expect("decode record");
    assert_eq!(record.players.len(), 2);
    assert!(record.turns.last().unwrap().game_over());

    // Sessions API lists the (post-game) lobby.
    let body = http_get(http_port, "/api/sessions").await;
    assert!(body.contains("e2e-test"), "sessions list missing id: {body}");

    // Reconnect with the same secret -> same public identity.
    let mut alice2 =
        Client::connect(wt_port, "e2e-test", "alice-secret-0000", info("alice")).await;
    match alice2.next_msg().await {
        ServerMsg::Welcome { your_id } => assert_eq!(your_id, alice_id),
        other => panic!("expected welcome, got {other:?}"),
    }
}

#[tokio::test]
async fn watch_games_pushes_list() {
    let dir = std::env::temp_dir().join(format!("snek-watch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("watch.db");
    let _ = std::fs::remove_file(&db_path);

    let (_, registry) = snek_server::build(&db_path);
    let identity = Identity::self_signed(["localhost", "127.0.0.1"]).unwrap();
    let endpoint = snek_server::wt::build_endpoint(0, &identity).unwrap();
    let wt_port = endpoint.local_addr().unwrap().port();
    tokio::spawn(snek_server::wt::serve(endpoint, registry));

    // Watcher connects first and gets an (empty) list.
    let config = ClientConfig::builder()
        .with_bind_default()
        .with_no_cert_validation()
        .build();
    let conn = Endpoint::client(config)
        .unwrap()
        .connect(format!("https://127.0.0.1:{wt_port}"))
        .await
        .unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap().await.unwrap();
    write_frame(&mut send, &ClientMsg::WatchGames).await;
    match tokio::time::timeout(Duration::from_secs(5), read_stream_msg(&mut recv))
        .await
        .unwrap()
    {
        ServerMsg::Sessions(list) => assert!(list.is_empty()),
        other => panic!("expected sessions, got {other:?}"),
    }

    // A player joins somewhere -> the watcher gets a push.
    let mut player = Client::connect(wt_port, "watch-me", "carol-secret-0000", info("carol")).await;
    let _ = player.next_msg().await; // welcome
    match tokio::time::timeout(Duration::from_secs(5), read_stream_msg(&mut recv))
        .await
        .unwrap()
    {
        ServerMsg::Sessions(list) => {
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].id, "watch-me");
            assert_eq!(list[0].host_name, "carol", "host is the first human");
        }
        other => panic!("expected sessions push, got {other:?}"),
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

#[tokio::test]
async fn distributed_compute_round_trip() {
    let dir = std::env::temp_dir().join(format!("snek-dist-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("dist.db");
    let _ = std::fs::remove_file(&db_path);

    let (_, registry) = snek_server::build(&db_path);
    let identity = wtransport::Identity::self_signed(["localhost", "127.0.0.1"]).unwrap();
    let endpoint = snek_server::wt::build_endpoint(0, &identity).unwrap();
    let wt_port = endpoint.local_addr().unwrap().port();
    tokio::spawn(snek_server::wt::serve(endpoint, registry));

    let mut alice = Client::connect(wt_port, "dist-test", "alice-secret-0000", info("alice")).await;
    // Fast board so the game resolves quickly.
    alice
        .control(&ClientMsg::Configure(GameConfig {
            board_width: 11,
            board_height: 11,
            tick_ms: 250,
        }))
        .await;
    // Become a compute host, add a bot, go.
    alice
        .control(&ClientMsg::Benchmark {
            sims_per_sec: 50_000,
            device: "e2e test rig".to_string(),
            cores: 8,
        })
        .await;
    alice.control(&ClientMsg::AddBot(snek_shared::BotDifficulty::Medium)).await;
    alice.control(&ClientMsg::SetReady(true)).await;
    alice.control(&ClientMsg::Start).await;

    let mut compute_requests = 0u32;
    let mut game_over = false;
    for _ in 0..600 {
        match alice.next_msg().await {
            ServerMsg::ComputeRequest {
                request_id,
                bot_id,
                config,
                turn,
                think_ms,
                nonce,
                ..
            } => {
                compute_requests += 1;
                // What the browser does: search until the deadline, then
                // answer with the nonce+request binding checksum.
                let deadline =
                    std::time::Instant::now() + Duration::from_millis(think_ms as u64);
                let mut search =
                    snek_shared::bot::begin(&config, &turn, &bot_id, u32::MAX, nonce);
                let mut sims = 0u32;
                while !search.step(128) {
                    sims += 128;
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                }
                let (dir, _work) = search.result();
                let proof = snek_shared::bot::bind_proof(nonce, request_id, dir);
                alice
                    .control(&ClientMsg::BotMove {
                        request_id,
                        dir,
                        proof,
                        tm: snek_shared::protocol::ComputeTelemetry {
                            queue_ms: 0,
                            search_ms: think_ms.max(1),
                            total_ms: think_ms.max(1),
                            think_ms,
                            sims,
                            slices: sims / 128,
                            fallback: false, loop_lag_ms: 0,
                        },
                    })
                    .await;
            }
            ServerMsg::Turn(t) if t.game_over() => {
                game_over = true;
            }
            ServerMsg::GameSaved { compute, .. } => {
                assert!(
                    compute.iter().any(|c| c.device == "e2e test rig" && c.moves > 0),
                    "compute stats missing our device: {compute:?}"
                );
                break;
            }
            _ => {}
        }
    }
    assert!(game_over, "game never finished");
    assert!(
        compute_requests > 0,
        "server never delegated compute to the benchmarked client"
    );
}

/// Cadence measurement (ignored by default; run with --ignored): turn
/// snapshots must arrive on a steady beat, not "period + processing time".
#[tokio::test]
#[ignore]
async fn tick_cadence_is_steady() {
    let dir = std::env::temp_dir().join(format!("snek-cadence-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("cadence.db");
    let _ = std::fs::remove_file(&db_path);

    let (_, registry) = snek_server::build(&db_path);
    let identity = Identity::self_signed(["localhost", "127.0.0.1"]).unwrap();
    let endpoint = snek_server::wt::build_endpoint(0, &identity).unwrap();
    let wt_port = endpoint.local_addr().unwrap().port();
    tokio::spawn(snek_server::wt::serve(endpoint, registry));

    let mut alice = Client::connect(wt_port, "cad-test", "alice-secret-0000", info("alice")).await;
    alice
        .control(&ClientMsg::Configure(GameConfig {
            board_width: 17,
            board_height: 17,
            tick_ms: 500,
        }))
        .await;
    // A bot adds per-tick work (request issuance) without needing a host.
    alice.control(&ClientMsg::AddBot(snek_shared::BotDifficulty::Hard)).await;
    alice.control(&ClientMsg::SetReady(true)).await;
    alice.control(&ClientMsg::Start).await;

    let mut arrivals: Vec<std::time::Instant> = Vec::new();
    let mut last_turn = 0u32;
    while arrivals.len() < 14 {
        match alice.next_msg().await {
            ServerMsg::Turn(t) => {
                // Dedup the redundant snapshot echoes, like the real client.
                if t.turn_number <= last_turn {
                    continue;
                }
                last_turn = t.turn_number;
                arrivals.push(std::time::Instant::now());
                if t.game_over() {
                    break;
                }
            }
            _ => {}
        }
        // Steer in a circle to stay alive long enough.
        let dir = match arrivals.len() % 20 {
            0..=4 => Dir::Right,
            5..=9 => Dir::Down,
            10..=14 => Dir::Left,
            _ => Dir::Up,
        };
        let _ = alice.conn.send_datagram(encode(&ClientMsg::Move(dir)));
    }
    let deltas: Vec<f64> = arrivals
        .windows(2)
        .map(|w| w[1].duration_since(w[0]).as_secs_f64() * 1000.0)
        .collect();
    let mean = deltas.iter().sum::<f64>() / deltas.len() as f64;
    let max_dev = deltas
        .iter()
        .map(|d| (d - 500.0).abs())
        .fold(0.0f64, f64::max);
    println!("deltas: {deltas:?}\nmean {mean:.1}ms, max deviation {max_dev:.1}ms");
    assert!(
        max_dev < 60.0,
        "cadence wobble too big: {max_dev:.1}ms (deltas {deltas:?})"
    );
}

/// A phone whose screen goes off suspends its workers. The game must stop
/// rather than march every bot into a wall, and pick straight back up when
/// the device wakes.
#[tokio::test]
async fn sleeping_host_pauses_the_game() {
    let dir = std::env::temp_dir().join(format!("snek-sleep-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("sleep.db");
    let _ = std::fs::remove_file(&db_path);

    let (_, registry) = snek_server::build(&db_path);
    let identity = wtransport::Identity::self_signed(["localhost", "127.0.0.1"]).unwrap();
    let endpoint = snek_server::wt::build_endpoint(0, &identity).unwrap();
    let wt_port = endpoint.local_addr().unwrap().port();
    tokio::spawn(snek_server::wt::serve(endpoint, registry));

    let mut alice =
        Client::connect(wt_port, "sleep-test", "alice-secret-0000", info("alice")).await;
    alice
        .control(&ClientMsg::Configure(GameConfig {
            board_width: 11,
            board_height: 11,
            tick_ms: 200,
        }))
        .await;
    alice
        .control(&ClientMsg::Benchmark {
            sims_per_sec: 50_000,
            device: "sleepy phone".to_string(),
            cores: 8,
        })
        .await;
    alice.control(&ClientMsg::AddBot(snek_shared::BotDifficulty::Easy)).await;
    alice.control(&ClientMsg::SetReady(true)).await;
    alice.control(&ClientMsg::Start).await;

    // Play normally for a few turns, answering compute like a browser would.
    let mut turns = 0u32;
    while turns < 3 {
        match tokio::time::timeout(Duration::from_secs(5), alice.next_msg()).await {
            Ok(ServerMsg::Turn(_)) => turns += 1,
            Ok(ServerMsg::ComputeRequest { request_id, bot_id, config, turn, nonce, .. }) => {
                let mut search =
                    snek_shared::bot::begin(&config, &turn, &bot_id, 200, nonce);
                while !search.step(64) {}
                let (dir, _) = search.result();
                let proof = snek_shared::bot::bind_proof(nonce, request_id, dir);
                alice
                    .control(&ClientMsg::BotMove {
                        request_id,
                        dir,
                        proof,
                        tm: snek_shared::protocol::ComputeTelemetry {
                            queue_ms: 0, search_ms: 10, total_ms: 10,
                            think_ms: 100, sims: 200, slices: 3, fallback: false, loop_lag_ms: 0,
                        },
                    })
                    .await;
            }
            Ok(_) => {}
            Err(_) => panic!("game never got going"),
        }
    }

    // Screen off.
    alice.control(&ClientMsg::HostAvailable { can_compute: false }).await;

    // At most the tick already in flight may land; after that, silence.
    // The pings matter: a hidden page still gets the odd message out, and
    // treating any traffic as proof of life un-paused the game the moment
    // it paused.
    let mut after_sleep = 0u32;
    let deadline = std::time::Instant::now() + Duration::from_millis(1_500);
    while std::time::Instant::now() < deadline {
        alice.control(&ClientMsg::Ping(1)).await;
        match tokio::time::timeout(Duration::from_millis(400), alice.next_msg()).await {
            Ok(ServerMsg::Turn(_)) => after_sleep += 1,
            Ok(_) => {}
            Err(_) => {}
        }
    }
    assert!(
        after_sleep <= 1,
        "game kept ticking with no device able to think: {after_sleep} turns while asleep"
    );

    // Screen on: the game resumes.
    alice.control(&ClientMsg::HostAvailable { can_compute: true }).await;
    let mut resumed = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), alice.next_msg()).await {
            Ok(ServerMsg::Turn(_)) => {
                resumed = true;
                break;
            }
            Ok(ServerMsg::ComputeRequest { request_id, bot_id, config, turn, nonce, .. }) => {
                let mut search =
                    snek_shared::bot::begin(&config, &turn, &bot_id, 200, nonce);
                while !search.step(64) {}
                let (dir, _) = search.result();
                let proof = snek_shared::bot::bind_proof(nonce, request_id, dir);
                alice
                    .control(&ClientMsg::BotMove {
                        request_id, dir, proof,
                        tm: snek_shared::protocol::ComputeTelemetry {
                            queue_ms: 0, search_ms: 10, total_ms: 10,
                            think_ms: 100, sims: 200, slices: 3, fallback: false, loop_lag_ms: 0,
                        },
                    })
                    .await;
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
    assert!(resumed, "game never resumed after the device woke up");
}

/// A host we *guess* has gone to sleep must be able to prove us wrong. A
/// device that is only watching sends nothing but keepalives, so if those
/// don't count as evidence the guess is unfalsifiable: it gets no compute
/// requests, produces no other traffic, and the game stays paused forever.
#[tokio::test]
async fn a_silent_host_recovers_on_keepalives_alone() {
    let dir = std::env::temp_dir().join(format!("snek-revive-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("revive.db");
    let _ = std::fs::remove_file(&db_path);

    let (_, registry) = snek_server::build(&db_path);
    let identity = wtransport::Identity::self_signed(["localhost", "127.0.0.1"]).unwrap();
    let endpoint = snek_server::wt::build_endpoint(0, &identity).unwrap();
    let wt_port = endpoint.local_addr().unwrap().port();
    tokio::spawn(snek_server::wt::serve(endpoint, registry));

    let mut alice =
        Client::connect(wt_port, "revive-test", "alice-secret-0000", info("alice")).await;
    alice
        .control(&ClientMsg::Configure(GameConfig {
            board_width: 11,
            board_height: 11,
            tick_ms: 200,
        }))
        .await;
    alice
        .control(&ClientMsg::Benchmark {
            sims_per_sec: 50_000,
            device: "stalling phone".to_string(),
            cores: 8,
        })
        .await;
    alice.control(&ClientMsg::AddBot(snek_shared::BotDifficulty::Easy)).await;
    alice.control(&ClientMsg::SetReady(true)).await;
    alice.control(&ClientMsg::Start).await;

    // Never answer a single compute request: the server should give up on
    // this host after two ticks of silence and pause the game.
    let mut turns_seen = 0u32;
    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(300), alice.next_msg()).await {
            Ok(ServerMsg::Turn(_)) => turns_seen += 1,
            Ok(_) => {}
            Err(_) => {}
        }
    }
    let paused_at = turns_seen;
    assert!(
        paused_at < 25,
        "server kept ticking a game nobody could think for: {paused_at} turns"
    );

    // The only traffic a watching device produces. This must revive it.
    for _ in 0..6 {
        alice.conn.send_datagram(encode(&ClientMsg::Ping(7))).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let mut resumed = 0u32;
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    while std::time::Instant::now() < deadline && resumed == 0 {
        match tokio::time::timeout(Duration::from_millis(300), alice.next_msg()).await {
            Ok(ServerMsg::Turn(_)) => resumed += 1,
            Ok(ServerMsg::ComputeRequest { .. }) => resumed += 1,
            Ok(_) => {}
            Err(_) => {}
        }
    }
    assert!(
        resumed > 0,
        "keepalives did not revive the host — the game is stuck for good"
    );
}

/// Lateness is the only clean signal that a device is being asked for more
/// thinking than it can deliver in a tick, so a late answer has to be
/// recognised when it arrives. It wasn't: requests were forgotten the
/// instant their tick resolved, so the answer looked like noise, the window
/// never shrank, and the device kept being overcommitted every single turn.
#[tokio::test]
async fn answering_late_shrinks_the_thinking_window() {
    let dir = std::env::temp_dir().join(format!("snek-late-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("late.db");
    let _ = std::fs::remove_file(&db_path);

    let (_, registry) = snek_server::build(&db_path);
    let identity = wtransport::Identity::self_signed(["localhost", "127.0.0.1"]).unwrap();
    let endpoint = snek_server::wt::build_endpoint(0, &identity).unwrap();
    let wt_port = endpoint.local_addr().unwrap().port();
    tokio::spawn(snek_server::wt::serve(endpoint, registry));

    let mut alice =
        Client::connect(wt_port, "late-test", "alice-secret-0000", info("alice")).await;
    alice
        .control(&ClientMsg::Configure(GameConfig {
            board_width: 11,
            board_height: 11,
            tick_ms: 200,
        }))
        .await;
    alice
        .control(&ClientMsg::Benchmark {
            sims_per_sec: 50_000,
            device: "struggling phone".to_string(),
            cores: 8,
        })
        .await;
    alice.control(&ClientMsg::AddBot(snek_shared::BotDifficulty::Hard)).await;
    alice.control(&ClientMsg::SetReady(true)).await;
    alice.control(&ClientMsg::Start).await;

    let mut first_window = 0u32;
    let mut last_window = 0u32;
    let mut answered = 0u32;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline && answered < 6 {
        match tokio::time::timeout(Duration::from_millis(500), alice.next_msg()).await {
            Ok(ServerMsg::ComputeRequest { request_id, bot_id, config, turn, think_ms, nonce, .. }) => {
                if first_window == 0 {
                    first_window = think_ms;
                }
                last_window = think_ms;
                answered += 1;
                // A phone that overshoots its deadline every time: the
                // answer is correct but lands after the tick has gone.
                let mut search = snek_shared::bot::begin(&config, &turn, &bot_id, 64, nonce);
                while !search.step(64) {}
                let (dir, _) = search.result();
                let proof = snek_shared::bot::bind_proof(nonce, request_id, dir);
                // Overshoot the window by enough that the tick it belonged
                // to has certainly resolved, whatever cadence the server
                // settled on.
                tokio::time::sleep(Duration::from_millis(think_ms as u64 + 700)).await;
                alice
                    .control(&ClientMsg::BotMove {
                        request_id, dir, proof,
                        tm: snek_shared::protocol::ComputeTelemetry {
                            queue_ms: 300, search_ms: 50, total_ms: think_ms + 700,
                            think_ms, sims: 64, slices: 1, fallback: false, loop_lag_ms: 0,
                        },
                    })
                    .await;
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
    assert!(answered >= 4, "never got enough compute requests: {answered}");
    // One backoff is the whole point: it proves lateness is heard at all.
    // Repeated lateness keeps compounding it down to the 0.45 floor, but a
    // bot that isn't being steered dies quickly, which ends the game before
    // many rounds of it can land.
    assert!(
        last_window < first_window,
        "window never backed off for a device answering late: {first_window}ms -> {last_window}ms"
    );
}

/// Half the tick is the device's, the other half belongs to the two network
/// legs. A bot that answers correctly but a tick late is defaulted onto its
/// old heading, which is how one walked into a wall with a good move in
/// flight.
#[tokio::test]
async fn the_thinking_window_is_at_most_half_the_tick() {
    let dir = std::env::temp_dir().join(format!("snek-window-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("window.db");
    let _ = std::fs::remove_file(&db_path);

    let (_, registry) = snek_server::build(&db_path);
    let identity = wtransport::Identity::self_signed(["localhost", "127.0.0.1"]).unwrap();
    let endpoint = snek_server::wt::build_endpoint(0, &identity).unwrap();
    let wt_port = endpoint.local_addr().unwrap().port();
    tokio::spawn(snek_server::wt::serve(endpoint, registry));

    let mut alice =
        Client::connect(wt_port, "window-test", "alice-secret-0000", info("alice")).await;
    let tick_ms = 500u32;
    alice
        .control(&ClientMsg::Configure(GameConfig {
            board_width: 11,
            board_height: 11,
            tick_ms,
        }))
        .await;
    alice
        .control(&ClientMsg::Benchmark {
            sims_per_sec: 500_000,
            device: "fast phone".to_string(),
            cores: 16,
        })
        .await;
    // Hard is the greediest setting, and one bot on a 16-core device is the
    // case where nothing else trims the window.
    alice.control(&ClientMsg::AddBot(snek_shared::BotDifficulty::Hard)).await;
    alice.control(&ClientMsg::SetReady(true)).await;
    alice.control(&ClientMsg::Start).await;

    let mut saw = 0u32;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline && saw < 3 {
        if let Ok(ServerMsg::ComputeRequest { think_ms, .. }) =
            tokio::time::timeout(Duration::from_millis(800), alice.next_msg()).await
        {
            saw += 1;
            assert!(
                think_ms * 2 <= tick_ms,
                "asked a device for {think_ms}ms of thinking out of a {tick_ms}ms tick, \
                 leaving too little for the round trip"
            );
        }
    }
    assert!(saw > 0, "never saw a compute request");
}
