//! Full-stack test over real QUIC: two WebTransport clients join a session,
//! ready up, play a game via datagram snapshots, and the finished game lands
//! in sqlite and the history API.

// All of this file is test code, rig helpers included, so it gets the test
// rules from clippy.toml: a failed assumption anywhere here should end the
// test, loudly.
#![cfg(test)]

use snek_shared::protocol::*;
use snek_shared::{Dir, GameConfig, PlayerId, Turn};
use std::time::Duration;
use wtransport::{ClientConfig, Connection, Endpoint, Identity, RecvStream, SendStream};

struct Client {
    conn: Connection,
    seen_requests: std::collections::HashSet<u64>,
    send: SendStream,
    /// Stream + datagram messages merged by reader tasks (cancel-safe).
    rx: tokio::sync::mpsc::UnboundedReceiver<ServerMsg>,
}

fn info(name: &str) -> ProfileInfo {
    ProfileInfo {
        name: name.to_owned(),
        emoji: "🐍".to_owned(),
        colour: "hsl(120, 85%, 60%)".to_owned(),
    }
}

async fn write_frame(send: &mut SendStream, msg: &ClientMsg) {
    let bytes = encode(msg);
    let len = u32::try_from(bytes.len()).unwrap();
    send.write_all(&len.to_le_bytes()).await.unwrap();
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
    async fn connect(port: u16, session: &str, secret: &str, profile: ProfileInfo) -> Self {
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
                session: session.to_owned(),
                secret: secret.to_owned(),
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
                if let Some(msg) = decode::<ServerMsg>(&dgram)
                    && tx.send(msg).is_err()
                {
                    break;
                }
            }
        });
        Self {
            conn,
            seen_requests: std::collections::HashSet::new(),
            send,
            rx,
        }
    }

    async fn control(&mut self, msg: &ClientMsg) {
        write_frame(&mut self.send, msg).await;
    }

    /// Answer a compute request the way a healthy phone would: a real (if
    /// small) search, reported as quick and unremarkable.
    async fn answer_promptly(
        &mut self,
        request_id: u64,
        bot_id: &PlayerId,
        config: &GameConfig,
        turn: &Turn,
        nonce: u64,
    ) {
        let mut search = snek_shared::bot::begin(config, turn, bot_id, 200, nonce);
        while !search.step(64) {}
        let (dir, _) = search.result();
        let proof = snek_shared::bot::bind_proof(nonce, request_id, dir);
        self.control(&ClientMsg::BotMove {
            request_id,
            dir,
            proof,
            tm: ComputeTelemetry {
                queue_ms: 0,
                search_ms: 10,
                total_ms: 10,
                think_ms: 100,
                sims: 200,
                slices: 3,
                fallback: false,
                loop_lag_ms: 0,
            },
        })
        .await;
    }

    /// Next message from either the control stream or a datagram.
    /// Next message, with repeat copies of a compute request dropped. The
    /// server deliberately sends each request three times — twice as a
    /// datagram and once on the stream — so that a lost packet cannot delay
    /// it. A real client takes up the first copy and ignores the rest; a
    /// test that answered all three would be answering a request that is no
    /// longer outstanding and reading its own echo as a fresh one.
    async fn next_msg(&mut self) -> ServerMsg {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(15), self.rx.recv())
                .await
                .expect("timed out waiting for server message")
                .expect("connection closed");
            if let ServerMsg::ComputeRequest { request_id, .. } = &msg
                && !self.seen_requests.insert(*request_id)
            {
                continue;
            }
            return msg;
        }
    }
}

/// A fresh server over its own empty database (`snek-<name>-<pid>` in the
/// temp dir), serving HTTP and WebTransport on ephemeral ports under a
/// self-signed identity. Returns the HTTP and WebTransport ports.
async fn start_server(name: &str) -> (u16, u16) {
    let server = snek_server::Server::bind(&test_config(name)).await.unwrap();
    let ports = (server.http_addr().port(), server.wt_port());
    tokio::spawn(server.serve(std::future::pending()));
    ports
}

fn test_config(name: &str) -> snek_server::Config {
    let dir = std::env::temp_dir().join(format!("snek-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join(format!("{name}.db"));
    let _ = std::fs::remove_file(&db);
    snek_server::Config {
        http_port: 0,
        wt_port: 0,
        db,
        tls: None,
    }
}

#[tokio::test]
async fn two_players_play_a_full_game() {
    let (http_port, wt_port) = start_server("e2e").await;

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
        if let ServerMsg::Lobby(l) = bob.next_msg().await
            && l.players.len() == 2
        {
            seen_both = true;
            break;
        }
    }
    assert!(seen_both, "lobby never showed both players");

    alice
        .control(&ClientMsg::Configure(GameConfig {
            board_width: 11,
            board_height: 11,
            tick_ms: 250,
            food_emoji: "🍕".to_owned(),
            cerebral: false,
        }))
        .await;
    alice.control(&ClientMsg::SetReady(true)).await;
    bob.control(&ClientMsg::SetReady(true)).await;
    // Different connections have no cross-ordering: wait until the lobby
    // echoes both ready before starting.
    for _ in 0..10 {
        if let ServerMsg::Lobby(l) = alice.next_msg().await
            && l.ready.len() == 2
        {
            break;
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
    let _ = alice.conn.send_datagram(encode(&ClientMsg::Move {
        dir: Dir::Up,
        after: 0,
    }));
    let _ = bob.conn.send_datagram(encode(&ClientMsg::Move {
        dir: Dir::Down,
        after: 0,
    }));
    // Datagram pings get datagram pongs.
    let _ = alice.conn.send_datagram(encode(&ClientMsg::Ping(7)));

    let mut saved_id: Option<i64> = None;
    let mut got_pong = false;
    let mut turn_count = 0u32;
    for _ in 0..400 {
        match alice.next_msg().await {
            ServerMsg::Turn(_) => turn_count += 1,
            ServerMsg::Pong(7) => got_pong = true,
            ServerMsg::GameSaved { id, .. } => {
                saved_id = Some(id);
                break;
            }
            _ => {}
        }
    }
    // Datagrams and the stream are read by separate tasks, so the save
    // notice can be handed over before the snapshot that preceded it (the
    // game can end on its first tick, leaving just that one). Give any
    // stragglers a moment.
    while let Ok(Some(msg)) =
        tokio::time::timeout(Duration::from_millis(500), alice.rx.recv()).await
    {
        if let ServerMsg::Turn(_) = msg {
            turn_count += 1;
        }
    }
    // How it ended is checked on the saved record (below): it is the
    // reliable copy, where any one snapshot datagram may be lost.
    assert!(got_pong, "datagram ping never answered");
    assert!(turn_count > 0, "no turn snapshots arrived");
    let saved_id = saved_id.expect("game was not saved");

    // History API serves the finished game.
    let body = http_get(http_port, "/api/history").await;
    assert!(body.contains("alice"), "history list missing players");
    let record_bytes = http_get_bytes(http_port, &format!("/api/history/{saved_id}")).await;
    let record: snek_shared::GameRecord =
        postcard::from_bytes(&record_bytes).expect("decode record");
    assert_eq!(record.players.len(), 2);
    let last = record.turns.last().expect("record has turns");
    assert!(last.game_over(), "game never ended");
    assert_eq!(last.winners.len(), 2, "all players ranked");

    // Sessions API lists the (post-game) lobby.
    let body = http_get(http_port, "/api/sessions").await;
    assert!(
        body.contains("e2e-test"),
        "sessions list missing id: {body}"
    );

    // Reconnect with the same secret -> same public identity.
    let mut alice2 = Client::connect(wt_port, "e2e-test", "alice-secret-0000", info("alice")).await;
    match alice2.next_msg().await {
        ServerMsg::Welcome { your_id } => assert_eq!(your_id, alice_id),
        other => panic!("expected welcome, got {other:?}"),
    }
}

#[tokio::test]
async fn watch_games_pushes_list() {
    let (_, wt_port) = start_server("watch").await;

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
        ServerMsg::Sessions(list) => assert_eq!(list, Vec::<SessionSummary>::new()),
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
            assert_eq!(list[0].players, 1, "carol isn't counted in it");
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
    let (_, wt_port) = start_server("dist").await;

    let mut alice = Client::connect(wt_port, "dist-test", "alice-secret-0000", info("alice")).await;
    // Fast board so the game resolves quickly.
    alice
        .control(&ClientMsg::Configure(GameConfig {
            board_width: 11,
            board_height: 11,
            tick_ms: 250,
            food_emoji: "🍕".to_owned(),
            cerebral: false,
        }))
        .await;
    // Become a compute host, add a bot, go.
    alice
        .control(&ClientMsg::Benchmark {
            sims_per_sec: 50_000,
            device: "e2e test rig".to_owned(),
            cores: 8,
        })
        .await;
    alice
        .control(&ClientMsg::AddBot(snek_shared::BotDifficulty::Medium))
        .await;
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
                    std::time::Instant::now() + Duration::from_millis(u64::from(think_ms));
                let mut search = snek_shared::bot::begin(&config, &turn, &bot_id, u32::MAX, nonce);
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
                        tm: ComputeTelemetry {
                            queue_ms: 0,
                            search_ms: think_ms.max(1),
                            total_ms: think_ms.max(1),
                            think_ms,
                            sims,
                            slices: sims / 128,
                            fallback: false,
                            loop_lag_ms: 0,
                        },
                    })
                    .await;
            }
            ServerMsg::Turn(t) if t.game_over() => {
                game_over = true;
            }
            ServerMsg::GameSaved { compute, .. } => {
                assert!(
                    compute
                        .iter()
                        .any(|c| c.device == "e2e test rig" && c.moves > 0),
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
#[ignore = "timing measurement, too sensitive to machine load to gate on; run with --ignored"]
async fn tick_cadence_is_steady() {
    let (_, wt_port) = start_server("cadence").await;

    let mut alice = Client::connect(wt_port, "cad-test", "alice-secret-0000", info("alice")).await;
    alice
        .control(&ClientMsg::Configure(GameConfig {
            board_width: 17,
            board_height: 17,
            tick_ms: 500,
            food_emoji: "🍕".to_owned(),
            cerebral: false,
        }))
        .await;
    // A bot adds per-tick work (request issuance) without needing a host.
    alice
        .control(&ClientMsg::AddBot(snek_shared::BotDifficulty::Hard))
        .await;
    alice.control(&ClientMsg::SetReady(true)).await;
    alice.control(&ClientMsg::Start).await;

    let mut arrivals: Vec<std::time::Instant> = Vec::new();
    let mut last_turn = 0u32;
    while arrivals.len() < 14 {
        if let ServerMsg::Turn(t) = alice.next_msg().await {
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
        // Steer in a circle to stay alive long enough.
        let dir = match arrivals.len() % 20 {
            0..=4 => Dir::Right,
            5..=9 => Dir::Down,
            10..=14 => Dir::Left,
            _ => Dir::Up,
        };
        let _ = alice
            .conn
            .send_datagram(encode(&ClientMsg::Move { dir, after: 0 }));
    }
    let deltas: Vec<f64> = arrivals
        .windows(2)
        .map(|w| w[1].duration_since(w[0]).as_secs_f64() * 1000.0)
        .collect();
    let mean = deltas.iter().sum::<f64>() / f64::from(u32::try_from(deltas.len()).unwrap());
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
    let (_, wt_port) = start_server("sleep").await;

    let mut alice =
        Client::connect(wt_port, "sleep-test", "alice-secret-0000", info("alice")).await;
    alice
        .control(&ClientMsg::Configure(GameConfig {
            board_width: 11,
            board_height: 11,
            tick_ms: 200,
            food_emoji: "🍕".to_owned(),
            cerebral: false,
        }))
        .await;
    alice
        .control(&ClientMsg::Benchmark {
            sims_per_sec: 50_000,
            device: "sleepy phone".to_owned(),
            cores: 8,
        })
        .await;
    alice
        .control(&ClientMsg::AddBot(snek_shared::BotDifficulty::Easy))
        .await;
    alice.control(&ClientMsg::SetReady(true)).await;
    alice.control(&ClientMsg::Start).await;

    // Play normally for a few turns, answering compute like a browser would.
    let mut turns = 0u32;
    while turns < 3 {
        match tokio::time::timeout(Duration::from_secs(10), alice.next_msg()).await {
            Ok(ServerMsg::Turn(_)) => turns += 1,
            Ok(ServerMsg::ComputeRequest {
                request_id,
                bot_id,
                config,
                turn,
                nonce,
                ..
            }) => {
                alice
                    .answer_promptly(request_id, &bot_id, &config, &turn, nonce)
                    .await;
            }
            Ok(_) => {}
            Err(tokio::time::error::Elapsed { .. }) => panic!("game never got going"),
        }
    }

    // Screen off.
    alice
        .control(&ClientMsg::HostAvailable { can_compute: false })
        .await;

    // At most the tick already in flight may land; after that, silence.
    // The pings matter: a hidden page still gets the odd message out, and
    // treating any traffic as proof of life un-paused the game the moment
    // it paused.
    let mut after_sleep = 0u32;
    let deadline = std::time::Instant::now() + Duration::from_millis(1_500);
    while std::time::Instant::now() < deadline {
        alice.control(&ClientMsg::Ping(1)).await;
        if let Ok(ServerMsg::Turn(_)) =
            tokio::time::timeout(Duration::from_millis(400), alice.next_msg()).await
        {
            after_sleep += 1;
        }
    }
    assert!(
        after_sleep <= 1,
        "game kept ticking with no device able to think: {after_sleep} turns while asleep"
    );

    // Screen on: the game resumes.
    alice
        .control(&ClientMsg::HostAvailable { can_compute: true })
        .await;
    let mut resumed = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), alice.next_msg()).await {
            Ok(ServerMsg::Turn(_)) => {
                resumed = true;
                break;
            }
            Ok(ServerMsg::ComputeRequest {
                request_id,
                bot_id,
                config,
                turn,
                nonce,
                ..
            }) => {
                alice
                    .answer_promptly(request_id, &bot_id, &config, &turn, nonce)
                    .await;
            }
            _ => {}
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
    let (_, wt_port) = start_server("revive").await;

    let mut alice =
        Client::connect(wt_port, "revive-test", "alice-secret-0000", info("alice")).await;
    alice
        .control(&ClientMsg::Configure(GameConfig {
            board_width: 11,
            board_height: 11,
            tick_ms: 200,
            food_emoji: "🍕".to_owned(),
            cerebral: false,
        }))
        .await;
    alice
        .control(&ClientMsg::Benchmark {
            sims_per_sec: 50_000,
            device: "stalling phone".to_owned(),
            cores: 8,
        })
        .await;
    alice
        .control(&ClientMsg::AddBot(snek_shared::BotDifficulty::Easy))
        .await;
    alice.control(&ClientMsg::SetReady(true)).await;
    alice.control(&ClientMsg::Start).await;

    // Never answer a single compute request: the server should give up on
    // this host after two ticks of silence and pause the game.
    let mut turns_seen = 0u32;
    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    while std::time::Instant::now() < deadline {
        if let Ok(ServerMsg::Turn(_)) =
            tokio::time::timeout(Duration::from_millis(300), alice.next_msg()).await
        {
            turns_seen += 1;
        }
    }
    let paused_at = turns_seen;
    assert!(
        paused_at < 25,
        "server kept ticking a game nobody could think for: {paused_at} turns"
    );

    // The only traffic a watching device produces. This must revive it.
    for _ in 0..6 {
        alice
            .conn
            .send_datagram(encode(&ClientMsg::Ping(7)))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let mut resumed = 0u32;
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    while std::time::Instant::now() < deadline && resumed == 0 {
        if let Ok(ServerMsg::Turn(_) | ServerMsg::ComputeRequest { .. }) =
            tokio::time::timeout(Duration::from_millis(300), alice.next_msg()).await
        {
            resumed += 1;
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
    let (_, wt_port) = start_server("late").await;

    let mut alice = Client::connect(wt_port, "late-test", "alice-secret-0000", info("alice")).await;
    alice
        .control(&ClientMsg::Configure(GameConfig {
            board_width: 11,
            board_height: 11,
            tick_ms: 200,
            food_emoji: "🍕".to_owned(),
            cerebral: false,
        }))
        .await;
    alice
        .control(&ClientMsg::Benchmark {
            sims_per_sec: 50_000,
            device: "struggling phone".to_owned(),
            cores: 8,
        })
        .await;
    alice
        .control(&ClientMsg::AddBot(snek_shared::BotDifficulty::Hard))
        .await;
    alice.control(&ClientMsg::SetReady(true)).await;
    alice.control(&ClientMsg::Start).await;

    let mut first_window = 0u32;
    let mut last_window = 0u32;
    let mut answered = 0u32;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline && answered < 6 {
        if let Ok(ServerMsg::ComputeRequest {
            request_id,
            bot_id,
            config,
            turn,
            think_ms,
            nonce,
            ..
        }) = tokio::time::timeout(Duration::from_millis(500), alice.next_msg()).await
        {
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
            tokio::time::sleep(Duration::from_millis(u64::from(think_ms) + 700)).await;
            alice
                .control(&ClientMsg::BotMove {
                    request_id,
                    dir,
                    proof,
                    tm: ComputeTelemetry {
                        queue_ms: 300,
                        search_ms: 50,
                        total_ms: think_ms + 700,
                        think_ms,
                        sims: 64,
                        slices: 1,
                        fallback: false,
                        loop_lag_ms: 0,
                    },
                })
                .await;
        }
    }
    assert!(
        answered >= 4,
        "never got enough compute requests: {answered}"
    );
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
    let (_, wt_port) = start_server("window").await;

    let mut alice =
        Client::connect(wt_port, "window-test", "alice-secret-0000", info("alice")).await;
    let tick_ms = 500u32;
    alice
        .control(&ClientMsg::Configure(GameConfig {
            board_width: 11,
            board_height: 11,
            tick_ms,
            food_emoji: "🍕".to_owned(),
            cerebral: false,
        }))
        .await;
    alice
        .control(&ClientMsg::Benchmark {
            sims_per_sec: 500_000,
            device: "fast phone".to_owned(),
            cores: 16,
        })
        .await;
    // Hard is the greediest setting, and one bot on a 16-core device is the
    // case where nothing else trims the window.
    alice
        .control(&ClientMsg::AddBot(snek_shared::BotDifficulty::Hard))
        .await;
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

/// Pause holds the game where it is, and the food choice made in the lobby
/// is what the game is played and recorded with.
#[tokio::test]
async fn pausing_holds_the_game_and_food_is_recorded() {
    let (_, wt_port) = start_server("pause").await;

    let mut alice =
        Client::connect(wt_port, "pause-test", "alice-secret-0000", info("alice")).await;
    alice
        .control(&ClientMsg::Configure(GameConfig {
            board_width: 11,
            board_height: 11,
            tick_ms: 250,
            food_emoji: "🌮".to_owned(),
            cerebral: false,
        }))
        .await;
    alice.control(&ClientMsg::SetReady(true)).await;
    let mut bob = Client::connect(wt_port, "pause-test", "bob-secret-000000", info("bob")).await;
    bob.control(&ClientMsg::SetReady(true)).await;
    alice.control(&ClientMsg::Start).await;

    // Let it get going, and check the food we chose is what is in play.
    let mut turns = 0u32;
    let mut saw_food = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while std::time::Instant::now() < deadline && turns < 3 {
        match tokio::time::timeout(Duration::from_millis(600), alice.next_msg()).await {
            Ok(ServerMsg::Turn(_)) => turns += 1,
            Ok(ServerMsg::GameStart { config, .. }) => saw_food = Some(config.food_emoji),
            _ => {}
        }
    }
    assert_eq!(
        saw_food.as_deref(),
        Some("🌮"),
        "game started with the wrong food"
    );
    assert!(turns >= 1, "game never got going");

    alice.control(&ClientMsg::SetPaused(true)).await;
    let mut while_held = 0u32;
    let deadline = std::time::Instant::now() + Duration::from_millis(1_500);
    while std::time::Instant::now() < deadline {
        // Traffic while held must not sneak the clock back on.
        alice.control(&ClientMsg::Ping(3)).await;
        if let Ok(ServerMsg::Turn(_)) =
            tokio::time::timeout(Duration::from_millis(300), alice.next_msg()).await
        {
            while_held += 1;
        }
    }
    assert!(
        while_held <= 1,
        "game kept playing while held: {while_held} turns"
    );

    // Bob sees the hold too — it is the game's state, not one screen's.
    let mut bob_saw_pause = false;
    for _ in 0..40 {
        match tokio::time::timeout(Duration::from_millis(100), bob.next_msg()).await {
            Ok(ServerMsg::Hold(hold)) if hold.by.is_some() => bob_saw_pause = true,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(
        bob_saw_pause,
        "the other player was never told the game was held"
    );

    alice.control(&ClientMsg::SetPaused(false)).await;
    let mut resumed = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline && !resumed {
        if let Ok(ServerMsg::Turn(_)) =
            tokio::time::timeout(Duration::from_millis(400), alice.next_msg()).await
        {
            resumed = true;
        }
    }
    assert!(resumed, "game never resumed after the hold was lifted");
}

/// A compute request only escapes head-of-line blocking if it fits in one
/// datagram — oversized ones fall back to the stream, which is the thing
/// being avoided. A late-game board is the case that matters: long snakes
/// are what make the message big, and are exactly when misses hurt.
#[test]
fn a_compute_request_fits_in_one_datagram() {
    use snek_shared::protocol::ServerMsg;

    // The shape the real games have: 13x13, four snakes, long ones. Game
    // #203 ended with a snake 26 segments long.
    let config = GameConfig {
        board_width: 13,
        board_height: 13,
        tick_ms: 500,
        food_emoji: "🍩".to_owned(),
        cerebral: false,
    };
    let ids: Vec<PlayerId> = (0..4).map(|i| format!("bot-{i}")).collect();
    let mut engine = snek_shared::Engine::new(config.clone(), ids.clone(), 7);
    let mut turn = engine.first_turn();
    // Grow everyone to a realistic late-game length.
    for snake in turn.snakes.values_mut() {
        let head = snake[0];
        *snake = (0..30).map(|i| head.saturating_sub(i)).collect();
    }

    let msg = ServerMsg::ComputeRequest {
        request_id: u64::MAX,
        bot_id: ids[0].clone(),
        bot_name: "a reasonably long bot name".to_owned(),
        config,
        turn,
        think_ms: 250,
        nonce: u64::MAX,
    };
    let size = postcard::to_allocvec(&msg).unwrap().len();
    // QUIC datagrams over IPv6 with room for headers: ~1200 bytes is the
    // conservative floor any path should manage.
    println!("late-game compute request: {size} bytes");
    assert!(
        size <= 1_100,
        "compute request is {size} bytes and will fall back to the stream, \
         where one lost packet delays every request behind it"
    );
}

/// A backgrounded tab stops reading its socket and the server times it out
/// within ten seconds. Losing your seat over that is what made coming back
/// to a tab feel broken: the roster entry went, the ready you had given
/// went with it, and you rejoined as somebody new.
#[tokio::test]
async fn a_dropped_connection_keeps_its_seat() {
    let (_, wt_port) = start_server("seat").await;

    let alice_secret = "alice-secret-0000";
    let mut alice = Client::connect(wt_port, "seat-test", alice_secret, info("alice")).await;
    let mut bob = Client::connect(wt_port, "seat-test", "bob-secret-000000", info("bob")).await;
    alice.control(&ClientMsg::SetReady(true)).await;

    // Wait until bob can see alice ready, so we know the state we are about
    // to test the survival of actually existed.
    let alice_id = {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut found = None;
        while std::time::Instant::now() < deadline && found.is_none() {
            if let Ok(ServerMsg::Lobby(lobby)) =
                tokio::time::timeout(Duration::from_millis(500), bob.next_msg()).await
                && let Some(p) = lobby.players.iter().find(|p| p.name == "alice")
                && lobby.ready.contains(&p.id)
            {
                found = Some(p.id.clone());
            }
        }
        found.expect("alice never showed up ready")
    };

    // Her tab goes away.
    drop(alice);

    // Bob keeps watching: alice must still be there, and still ready.
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    let mut last: Option<LobbyState> = None;
    while std::time::Instant::now() < deadline {
        if let Ok(ServerMsg::Lobby(lobby)) =
            tokio::time::timeout(Duration::from_millis(500), bob.next_msg()).await
        {
            last = Some(lobby);
        }
    }
    if let Some(lobby) = last {
        assert!(
            lobby.players.iter().any(|p| p.id == alice_id),
            "alice lost her seat the moment her connection dropped"
        );
        assert!(
            lobby.ready.contains(&alice_id),
            "alice's seat survived but the ready she had already given did not"
        );
    }

    // And coming back is the same person in the same seat, still ready.
    let mut alice = Client::connect(wt_port, "seat-test", alice_secret, info("alice")).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut still_ready = false;
    while std::time::Instant::now() < deadline && !still_ready {
        if let Ok(ServerMsg::Lobby(lobby)) =
            tokio::time::timeout(Duration::from_millis(500), alice.next_msg()).await
        {
            still_ready =
                lobby.ready.contains(&alice_id) && lobby.players.iter().any(|p| p.id == alice_id);
        }
    }
    assert!(
        still_ready,
        "alice came back and had to ready up all over again"
    );
}

/// A client running code the server has moved past must be told so, and
/// told loudly enough that it can act. Retrying forever behind "Joining
/// session…" is what a stale page did instead.
#[tokio::test]
async fn a_stale_client_is_told_to_refresh() {
    let (_, wt_port) = start_server("stale").await;

    let conn = Endpoint::client(
        ClientConfig::builder()
            .with_bind_default()
            .with_no_cert_validation()
            .build(),
    )
    .unwrap()
    .connect(format!("https://127.0.0.1:{wt_port}"))
    .await
    .unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap().await.unwrap();

    write_frame(
        &mut send,
        &ClientMsg::Join {
            version: PROTOCOL_VERSION - 1,
            session: "stale-test".to_owned(),
            secret: "stale-secret-0000".to_owned(),
            profile: info("stale"),
        },
    )
    .await;

    let msg = tokio::time::timeout(Duration::from_secs(5), read_stream_msg(&mut recv))
        .await
        .expect("server said nothing to a stale client — it would just hang");
    match msg {
        ServerMsg::Error(e) => assert!(
            e.contains("version"),
            "the refusal must name the reason so the page knows to reload: {e}"
        ),
        other => panic!("expected a version refusal, got {other:?}"),
    }
}

/// Whether `c` is dealt into a game within ten seconds.
async fn started(c: &mut Client) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if let Ok(ServerMsg::GameStart { .. }) =
            tokio::time::timeout(Duration::from_millis(500), c.next_msg()).await
        {
            return true;
        }
    }
    false
}

/// Readying up after a game starts the next one, exactly as it does in the
/// lobby — the end screen offers the same decision, so it must work the
/// same way.
#[tokio::test]
async fn readying_up_after_a_game_starts_another() {
    let (_, wt_port) = start_server("again").await;

    let mut alice =
        Client::connect(wt_port, "again-test", "alice-secret-0000", info("alice")).await;
    let mut bob = Client::connect(wt_port, "again-test", "bob-secret-000000", info("bob")).await;
    alice
        .control(&ClientMsg::Configure(GameConfig {
            board_width: 7,
            board_height: 7,
            tick_ms: 250,
            food_emoji: "🍕".to_owned(),
            cerebral: false,
        }))
        .await;
    alice.control(&ClientMsg::SetReady(true)).await;
    bob.control(&ClientMsg::SetReady(true)).await;
    alice.control(&ClientMsg::Start).await;

    // Play it out.
    let deadline = std::time::Instant::now() + Duration::from_secs(40);
    let mut over = false;
    while std::time::Instant::now() < deadline && !over {
        if let Ok(ServerMsg::Turn(t)) =
            tokio::time::timeout(Duration::from_millis(800), alice.next_msg()).await
        {
            over = t.game_over();
        }
    }
    assert!(over, "first game never finished");

    // A game clears everyone's ready, so both agree again and it goes.
    alice.control(&ClientMsg::SetReady(true)).await;
    bob.control(&ClientMsg::SetReady(true)).await;
    alice.control(&ClientMsg::Start).await;

    // Both must be dealt into it.
    assert!(
        started(&mut alice).await,
        "the player who started it wasn't dealt in"
    );
    assert!(
        started(&mut bob).await,
        "the other player was left behind in the lobby"
    );
}

/// Waits for the first lobby that passes `want`.
async fn lobby_where(c: &mut Client, want: impl Fn(&LobbyState) -> bool) -> LobbyState {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if let Ok(ServerMsg::Lobby(l)) =
            tokio::time::timeout(Duration::from_millis(500), c.next_msg()).await
            && want(&l)
        {
            return l;
        }
    }
    panic!("no such lobby arrived");
}

/// The lobby says who last changed the settings, so every phone can say who
/// did it. Picking what is already picked is not a change and claims
/// nothing, and a real change leaves everyone's ready standing.
#[tokio::test]
async fn the_lobby_says_who_changed_the_settings() {
    let (_, wt_port) = start_server("cfg").await;

    let mut alice = Client::connect(wt_port, "cfg-test", "alice-secret-0000", info("alice")).await;
    let mut bob = Client::connect(wt_port, "cfg-test", "bob-secret-000000", info("bob")).await;
    let bob_id = match bob.next_msg().await {
        ServerMsg::Welcome { your_id } => your_id,
        other => panic!("expected welcome, got {other:?}"),
    };

    let fresh = lobby_where(&mut alice, |l| l.players.len() == 2).await;
    assert_eq!(fresh.config_changed_by, None);
    let config = fresh.config.clone();

    alice.control(&ClientMsg::SetReady(true)).await;
    lobby_where(&mut alice, |l| l.ready.len() == 1).await;

    // Bob re-picks what is already picked: nothing happens.
    bob.control(&ClientMsg::Configure(config.clone())).await;
    // Bob changes the food: his name goes on it, and alice stays ready.
    let changed = GameConfig {
        food_emoji: "🍕".to_owned(),
        ..config
    };
    bob.control(&ClientMsg::Configure(changed.clone())).await;
    let after = lobby_where(&mut alice, |l| l.config == changed).await;
    assert_eq!(after.config_changed_by.as_ref(), Some(&bob_id));
    assert_eq!(after.config_rev, fresh.config_rev + 1, "the no-op counted");
    assert_eq!(after.ready.len(), 1, "a settings change un-readied someone");
}

/// A raw WebTransport connection with its control stream open and nothing
/// said yet. `hashes` are the certificate hashes to accept, as a browser
/// is told them; with none, the certificate isn't checked at all.
async fn raw_connect(
    wt_port: u16,
    hashes: &[wtransport::tls::Sha256Digest],
) -> (Connection, SendStream, RecvStream) {
    let builder = ClientConfig::builder().with_bind_default();
    let config = if hashes.is_empty() {
        builder.with_no_cert_validation().build()
    } else {
        builder
            .with_server_certificate_hashes(hashes.iter().cloned())
            .build()
    };
    let conn = Endpoint::client(config)
        .unwrap()
        .connect(format!("https://127.0.0.1:{wt_port}"))
        .await
        .expect("wt connect");
    let (send, recv) = conn.open_bi().await.unwrap().await.unwrap();
    (conn, send, recv)
}

fn join_msg(session: &str, secret: &str, name: &str) -> ClientMsg {
    ClientMsg::Join {
        version: PROTOCOL_VERSION,
        session: session.to_owned(),
        secret: secret.to_owned(),
        profile: info(name),
    }
}

/// Whether the server ends the connection within a few seconds.
async fn closed_by_server(conn: &Connection) -> bool {
    tokio::time::timeout(Duration::from_secs(5), conn.closed())
        .await
        .is_ok()
}

/// Status line and body of a GET.
async fn http_get_any(port: u16, path: &str) -> (String, Vec<u8>) {
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
    let status = head.lines().next().unwrap().to_owned();
    (status, buf[split + 4..].to_vec())
}

/// With no real certificate, a page served from localhost asks for the
/// self-signed one's hash, and with it the browser's own check passes.
#[tokio::test]
async fn a_local_browser_is_told_the_dev_certificate() {
    let (http_port, wt_port) = start_server("devcert").await;
    let body = http_get(http_port, "/api/dev/wt").await;
    let dev: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(dev["port"], wt_port);
    let hash: Vec<u8> = serde_json::from_value(dev["certHash"].clone()).unwrap();
    let hash = wtransport::tls::Sha256Digest::new(hash.try_into().unwrap());

    let (_conn, mut send, mut recv) = raw_connect(wt_port, &[hash]).await;
    write_frame(&mut send, &join_msg("dev", "dev-secret-0000", "dev")).await;
    let msg = tokio::time::timeout(Duration::from_secs(5), read_stream_msg(&mut recv))
        .await
        .unwrap();
    assert!(matches!(msg, ServerMsg::Welcome { .. }), "{msg:?}");
}

/// Real certificates come from the PEM files the config names, and a
/// server with them never offers the dev route.
#[tokio::test]
async fn certificates_are_loaded_from_their_files() {
    let identity = Identity::self_signed(["localhost", "127.0.0.1"]).unwrap();
    let mut config = test_config("certs");
    let dir = config.db.parent().unwrap().to_owned();
    let tls = snek_server::TlsFiles {
        cert: dir.join("cert.pem"),
        key: dir.join("key.pem"),
    };
    identity
        .certificate_chain()
        .store_pemfile(&tls.cert)
        .await
        .unwrap();
    identity
        .private_key()
        .store_secret_pemfile(&tls.key)
        .await
        .unwrap();
    config.tls = Some(tls);

    let server = snek_server::Server::bind(&config).await.unwrap();
    let (http_port, wt_port) = (server.http_addr().port(), server.wt_port());
    tokio::spawn(server.serve(std::future::pending()));

    let (_, body) = http_get_any(http_port, "/api/dev/wt").await;
    assert!(
        !String::from_utf8_lossy(&body).contains("certHash"),
        "a server with real certificates handed out a dev hash"
    );
    // The browser check passes against exactly the certificate in the file.
    let hash = identity.certificate_chain().as_slice()[0].hash();
    let (_conn, mut send, mut recv) = raw_connect(wt_port, &[hash]).await;
    write_frame(&mut send, &join_msg("certs", "cert-secret-0000", "c")).await;
    let msg = tokio::time::timeout(Duration::from_secs(5), read_stream_msg(&mut recv))
        .await
        .unwrap();
    assert!(matches!(msg, ServerMsg::Welcome { .. }), "{msg:?}");
}

/// A server that can't start says which part of its setup is wrong.
#[tokio::test]
async fn startup_failures_say_what_went_wrong() {
    async fn failure(config: &snek_server::Config) -> String {
        match snek_server::Server::bind(config).await {
            Ok(_) => panic!("started with a bad config: {config:?}"),
            Err(e) => format!("{e:#}"),
        }
    }

    let mut config = test_config("nodb");
    config.db = config.db.with_file_name("missing-dir").join("snek.db");
    assert!(
        failure(&config)
            .await
            .contains("failed to open the database")
    );

    let mut config = test_config("nocert");
    config.tls = Some(snek_server::TlsFiles {
        cert: "/nonexistent/cert.pem".into(),
        key: "/nonexistent/key.pem".into(),
    });
    assert!(
        failure(&config)
            .await
            .contains("failed to load TLS identity")
    );

    let taken = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let mut config = test_config("busy");
    config.http_port = taken.local_addr().unwrap().port();
    assert!(failure(&config).await.contains("failed to bind http"));
}

/// Stopping the server tells every connected client so, rather than
/// leaving them to time out, and stops serving HTTP.
#[tokio::test]
async fn shutting_down_tells_clients_and_stops_serving() {
    let server = snek_server::Server::bind(&test_config("shutdown"))
        .await
        .unwrap();
    let (http_port, wt_port) = (server.http_addr().port(), server.wt_port());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));

    let mut alice = Client::connect(wt_port, "bye", "alice-secret-0000", info("alice")).await;
    assert!(matches!(alice.next_msg().await, ServerMsg::Welcome { .. }));
    stop.send(()).unwrap();

    tokio::time::timeout(Duration::from_secs(10), serving)
        .await
        .expect("the server never finished stopping")
        .unwrap()
        .unwrap();
    match tokio::time::timeout(Duration::from_secs(5), alice.conn.closed()).await {
        Ok(wtransport::error::ConnectionError::ApplicationClosed(close)) => {
            assert_eq!(close.reason(), b"server shutting down");
        }
        other => panic!("the client wasn't told the server was going: {other:?}"),
    }
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", http_port))
            .await
            .is_err(),
        "still taking HTTP connections"
    );
}

/// A connection must open with a usable join (or a watch); anything else
/// is turned away before a session hears of it.
#[tokio::test]
async fn unusable_joins_are_turned_away() {
    let (http_port, wt_port) = start_server("refuse").await;

    // A profile with no name. (The server writes "set up your player
    // first" and then drops the connection at once, so unlike the version
    // refusal the message doesn't reliably arrive; only the close is
    // checked here.)
    let (conn, mut send, _recv) = raw_connect(wt_port, &[]).await;
    write_frame(&mut send, &join_msg("refuse", "good-secret-0000", " ")).await;
    assert!(closed_by_server(&conn).await);

    // A secret too short to be one, a session name with nothing usable in
    // it, and a first frame that is neither join nor watch.
    for first in [
        join_msg("refuse", "short", "x"),
        join_msg("!!!", "good-secret-0000", "x"),
        ClientMsg::Ping(1),
    ] {
        let (conn, mut send, _recv) = raw_connect(wt_port, &[]).await;
        write_frame(&mut send, &first).await;
        assert!(closed_by_server(&conn).await, "kept {first:?}");
    }

    // A frame claiming to be bigger than any frame may be.
    let (conn, mut send, _recv) = raw_connect(wt_port, &[]).await;
    send.write_all(&(2u32 << 20).to_le_bytes()).await.unwrap();
    assert!(closed_by_server(&conn).await);

    // A length with the stream ending before the frame does.
    let (conn, mut send, _recv) = raw_connect(wt_port, &[]).await;
    send.write_all(&100u32.to_le_bytes()).await.unwrap();
    send.finish().await.unwrap();
    assert!(closed_by_server(&conn).await);

    // None of them made it into a session.
    assert_eq!(http_get(http_port, "/api/sessions").await, "[]");
}

/// The home screen measures liveness with datagram pings, and leaving is
/// just closing its stream.
#[tokio::test]
async fn a_watcher_is_answered_and_can_leave() {
    let (_, wt_port) = start_server("watcher").await;
    let (conn, mut send, mut recv) = raw_connect(wt_port, &[]).await;
    write_frame(&mut send, &ClientMsg::WatchGames).await;
    let first = tokio::time::timeout(Duration::from_secs(5), read_stream_msg(&mut recv))
        .await
        .unwrap();
    assert!(matches!(first, ServerMsg::Sessions(_)));

    let mut ponged = false;
    for n in 0..20 {
        conn.send_datagram(encode(&ClientMsg::Ping(n))).unwrap();
        if let Ok(Ok(dgram)) =
            tokio::time::timeout(Duration::from_millis(250), conn.receive_datagram()).await
        {
            assert!(matches!(decode(&dgram), Some(ServerMsg::Pong(_))));
            ponged = true;
            break;
        }
    }
    assert!(ponged, "a watcher's ping was never answered");

    send.finish().await.unwrap();
    assert!(
        closed_by_server(&conn).await,
        "the watch outlived its stream"
    );
}
