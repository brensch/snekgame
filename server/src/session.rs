//! One actor task per session (lobby). Websocket connections forward decoded
//! client messages in; the actor owns all game state, runs the tick loop, and
//! broadcasts pre-encoded frames out through per-connection channels.
//!
//! This is the whole concurrency model: no locks around game state, no shared
//! mutation — just message passing. A slow client whose outbound queue fills
//! up gets dropped and reconnects into a full state snapshot.

use crate::db::Db;
use snek_shared::protocol::*;
use snek_shared::{
    bot, BotDifficulty, Dir, Engine, GameConfig, GameRecord, PlayerId, PlayerProfile, Turn,
    Winner,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

pub const COUNTDOWN_MS: u32 = 3000;
const MAX_PLAYERS: usize = 8;
/// Outbound frames buffered per connection before we call the client dead.
const CONN_BUFFER: usize = 256;
/// How long an empty session lingers (so a network blip or refresh doesn't
/// wipe the lobby) before it is tidied away.
const EMPTY_GRACE: Duration = Duration::from_secs(60);

/// Outbound message with its transport class: control frames ride the
/// reliable stream; snapshots ride (redundant) datagrams and may be dropped.
#[derive(Clone)]
pub enum Frame {
    Control(Arc<[u8]>),
    Snapshot(Arc<[u8]>),
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

// ---------------------------------------------------------------------------
// Registry: session name -> live actor
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Registry {
    inner: Arc<Mutex<HashMap<String, mpsc::Sender<Cmd>>>>,
    /// Live snapshots for the active-games list, kept fresh by the actors.
    infos: Arc<Mutex<HashMap<String, SessionSummary>>>,
    /// Pushes the active list to /ws/games watchers on every change.
    games_watch: Arc<tokio::sync::watch::Sender<Vec<SessionSummary>>>,
    db: Arc<Db>,
    next_conn_id: Arc<AtomicU64>,
}

impl Registry {
    pub fn new(db: Arc<Db>) -> Registry {
        let (games_watch, _) = tokio::sync::watch::channel(Vec::new());
        Registry {
            inner: Arc::new(Mutex::new(HashMap::new())),
            infos: Arc::new(Mutex::new(HashMap::new())),
            games_watch: Arc::new(games_watch),
            db,
            next_conn_id: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn watch_games(&self) -> tokio::sync::watch::Receiver<Vec<SessionSummary>> {
        self.games_watch.subscribe()
    }

    pub fn new_conn_id(&self) -> u64 {
        self.next_conn_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Sessions with someone actually in them, for the games list.
    pub fn active_sessions(&self) -> Vec<SessionSummary> {
        let mut list: Vec<SessionSummary> = self
            .infos
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.players > 0)
            .cloned()
            .collect();
        list.sort_by(|a, b| a.host_name.cmp(&b.host_name).then(a.id.cmp(&b.id)));
        list
    }

    fn publish(&self, info: SessionSummary) {
        self.infos.lock().unwrap().insert(info.id.clone(), info);
        self.games_watch.send_replace(self.active_sessions());
    }

    /// Get the actor for a session, spawning it if needed. Retries once if it
    /// races with an actor shutting down.
    pub async fn connect(
        &self,
        session: &str,
        conn_id: u64,
        profile: PlayerProfile,
        tx: mpsc::Sender<Frame>,
    ) -> mpsc::Sender<Cmd> {
        loop {
            let sender = {
                let mut map = self.inner.lock().unwrap();
                map.entry(session.to_string())
                    .or_insert_with(|| {
                        SessionActor::spawn(session.to_string(), self.clone(), self.db.clone())
                    })
                    .clone()
            };
            let cmd = Cmd::Connect {
                conn_id,
                profile: profile.clone(),
                tx: tx.clone(),
            };
            match sender.send(cmd).await {
                Ok(()) => return sender,
                Err(_) => {
                    // Actor exited between lookup and send; clear and retry.
                    self.inner.lock().unwrap().remove(session);
                }
            }
        }
    }

    fn remove(&self, session: &str, sender: &mpsc::Sender<Cmd>) {
        let mut map = self.inner.lock().unwrap();
        if let Some(current) = map.get(session) {
            if current.same_channel(sender) {
                map.remove(session);
            }
        }
        self.infos.lock().unwrap().remove(session);
        self.games_watch.send_replace(self.active_sessions());
    }
}

// ---------------------------------------------------------------------------
// Session actor
// ---------------------------------------------------------------------------

pub enum Cmd {
    Connect {
        conn_id: u64,
        profile: PlayerProfile,
        tx: mpsc::Sender<Frame>,
    },
    Disconnect {
        conn_id: u64,
    },
    Msg {
        conn_id: u64,
        msg: ClientMsg,
    },
}

struct Conn {
    player: PlayerId,
    tx: mpsc::Sender<Frame>,
}

struct RunningGame {
    engine: Engine,
    players: Vec<PlayerProfile>,
    turns: Vec<Turn>,
    pending: BTreeMap<PlayerId, Dir>,
    started_at_ms: i64,
}

struct SessionActor {
    name: String,
    registry: Registry,
    db: Arc<Db>,
    rx: mpsc::Receiver<Cmd>,
    self_sender: mpsc::Sender<Cmd>,
    conns: HashMap<u64, Conn>,
    config: GameConfig,
    lobby: Vec<PlayerProfile>,
    /// Server-hosted bots in the lobby (kept across rematches until kicked).
    bots: HashMap<PlayerId, BotDifficulty>,
    bot_counter: u32,
    ready: HashSet<PlayerId>,
    game: Option<RunningGame>,
    next_tick: Option<Instant>,
    /// Set when the last connection drops; the actor tidies itself away
    /// after EMPTY_GRACE unless someone comes back.
    empty_deadline: Option<Instant>,
    last_winners: Vec<Winner>,
    last_game_id: Option<i64>,
}

impl SessionActor {
    fn spawn(name: String, registry: Registry, db: Arc<Db>) -> mpsc::Sender<Cmd> {
        let (tx, rx) = mpsc::channel(1024);
        let actor = SessionActor {
            name,
            registry,
            db,
            rx,
            self_sender: tx.clone(),
            conns: HashMap::new(),
            config: GameConfig::default(),
            lobby: Vec::new(),
            bots: HashMap::new(),
            bot_counter: 0,
            ready: HashSet::new(),
            game: None,
            next_tick: None,
            empty_deadline: None,
            last_winners: Vec::new(),
            last_game_id: None,
        };
        tokio::spawn(actor.run());
        tx
    }

    fn publish_info(&self) {
        // Humans only — presence in the lobby means a live connection, so
        // this count drops the moment someone leaves and the game disappears
        // from the list when the last human does (bots don't keep it listed).
        let humans = self.lobby.len().saturating_sub(self.bots.len());
        // Games aren't named; they're known by their host — the first human
        // currently in the lobby.
        let host = self
            .lobby
            .iter()
            .find(|p| !self.bots.contains_key(&p.id));
        self.registry.publish(SessionSummary {
            id: self.name.clone(),
            host_name: host.map(|p| p.name.clone()).unwrap_or_default(),
            host_emoji: host.map(|p| p.emoji.clone()).unwrap_or_default(),
            players: humans as u32,
            bots: self.bots.len() as u32,
            playing: self.game.is_some(),
        });
    }

    async fn run(mut self) {
        tracing::info!(session = %self.name, "session started");
        loop {
            let tick_sleep = async {
                match self.next_tick {
                    Some(t) => tokio::time::sleep_until(t).await,
                    None => std::future::pending::<()>().await,
                }
            };
            let empty_sleep = async {
                match self.empty_deadline {
                    Some(t) => tokio::time::sleep_until(t).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                cmd = self.rx.recv() => match cmd {
                    Some(cmd) => self.handle(cmd).await,
                    None => break,
                },
                _ = tick_sleep => self.tick().await,
                _ = empty_sleep => {
                    if self.conns.is_empty() {
                        break; // nobody came back — tidy up
                    }
                    self.empty_deadline = None;
                }
            }
        }
        tracing::info!(session = %self.name, "session ended");
        self.registry.remove(&self.name.clone(), &self.self_sender);
    }

    async fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Connect {
                conn_id,
                profile,
                tx,
            } => {
                self.empty_deadline = None;
                self.conns.insert(
                    conn_id,
                    Conn {
                        player: profile.id.clone(),
                        tx,
                    },
                );
                self.upsert_lobby_player(profile);
                // Late joiner while a game runs: catch them up so they can
                // spectate (or resume, if they are in it).
                if let Some(game) = &self.game {
                    let frame = control_frame(&ServerMsg::GameInProgress {
                        config: self.config,
                        players: game.players.clone(),
                        turns: game.turns.clone(),
                    });
                    self.send_to(conn_id, frame).await;
                }
                self.broadcast_lobby().await;
            }
            Cmd::Disconnect { conn_id } => {
                if let Some(conn) = self.conns.remove(&conn_id) {
                    let gone = !self.conns.values().any(|c| c.player == conn.player);
                    if gone {
                        // Their snek keeps sliding if a game is running; they
                        // just leave the next-game roster.
                        self.lobby.retain(|p| p.id != conn.player);
                        self.ready.remove(&conn.player);
                    }
                }
                if self.conns.is_empty() {
                    self.empty_deadline = Some(Instant::now() + EMPTY_GRACE);
                }
                self.broadcast_lobby().await;
            }
            Cmd::Msg { conn_id, msg } => self.client_msg(conn_id, msg).await,
        }
    }

    async fn client_msg(&mut self, conn_id: u64, msg: ClientMsg) {
        let player = match self.conns.get(&conn_id) {
            Some(c) => c.player.clone(),
            None => return,
        };
        match msg {
            // Identity is fixed to the connection: whatever a client sends,
            // profile changes only ever apply to *its own* public id.
            ClientMsg::Join { profile: info, .. } | ClientMsg::UpdateProfile(info) => {
                if info.name.trim().is_empty()
                    || info.name.len() > 32
                    || info.emoji.len() > 16
                    || info.colour.len() > 32
                {
                    return;
                }
                self.upsert_lobby_player(PlayerProfile {
                    id: player,
                    name: info.name,
                    emoji: info.emoji,
                    colour: info.colour,
                });
                self.broadcast_lobby().await;
            }
            ClientMsg::SetReady(ready) => {
                if ready {
                    self.ready.insert(player);
                } else {
                    self.ready.remove(&player);
                }
                self.broadcast_lobby().await;
            }
            ClientMsg::Configure(config) => {
                if self.game.is_none() && config.valid() {
                    self.config = config;
                    // Changing the deal un-readies everyone.
                    self.ready.clear();
                    self.broadcast_lobby().await;
                }
            }
            ClientMsg::Start => self.start_game().await,
            ClientMsg::Move(dir) => {
                if let Some(game) = &mut self.game {
                    if game
                        .turns
                        .last()
                        .map(|t| t.alive.contains(&player))
                        .unwrap_or(false)
                    {
                        game.pending.insert(player, dir);
                    }
                }
            }
            ClientMsg::AddBot(difficulty) => {
                if self.game.is_none() && self.lobby.len() < MAX_PLAYERS {
                    self.bot_counter += 1;
                    let (name, emoji, colour) = match difficulty {
                        BotDifficulty::Easy => ("wormy (easy)", "🐛", "hsl(95, 45%, 65%)"),
                        BotDifficulty::Medium => ("robo (medium)", "🤖", "hsl(210, 50%, 68%)"),
                        BotDifficulty::Hard => ("draco (hard)", "🐲", "hsl(275, 45%, 68%)"),
                    };
                    let id = format!("bot-{}", self.bot_counter);
                    self.bots.insert(id.clone(), difficulty);
                    self.lobby.push(PlayerProfile {
                        id,
                        name: name.to_string(),
                        emoji: emoji.to_string(),
                        colour: colour.to_string(),
                    });
                    self.broadcast_lobby().await;
                }
            }
            ClientMsg::Kick(target) => {
                if self.game.is_none() {
                    self.lobby.retain(|p| p.id != target);
                    self.bots.remove(&target);
                    self.ready.remove(&target);
                    self.broadcast_lobby().await;
                }
            }
            ClientMsg::Ping(n) => {
                self.send_to(conn_id, control_frame(&ServerMsg::Pong(n))).await;
            }
            // Handled at the transport layer before a session exists.
            ClientMsg::WatchGames => {}
        }
    }

    fn upsert_lobby_player(&mut self, profile: PlayerProfile) {
        match self.lobby.iter_mut().find(|p| p.id == profile.id) {
            Some(existing) => *existing = profile,
            None => {
                if self.lobby.len() < MAX_PLAYERS {
                    self.lobby.push(profile);
                }
            }
        }
    }

    async fn start_game(&mut self) {
        if self.game.is_some() || self.lobby.is_empty() {
            return;
        }
        // Bots are always ready; every human must be.
        let all_ready = self
            .lobby
            .iter()
            .filter(|p| !self.bots.contains_key(&p.id))
            .all(|p| self.ready.contains(&p.id));
        if !all_ready {
            return;
        }

        let players = self.lobby.clone();
        let ids: Vec<PlayerId> = players.iter().map(|p| p.id.clone()).collect();
        let seed = rand::random::<u64>();
        let mut engine = Engine::new(self.config, ids, seed);
        let first = engine.first_turn();

        let frame = control_frame(&ServerMsg::GameStart {
            config: self.config,
            players: players.clone(),
            turn: first.clone(),
            countdown_ms: COUNTDOWN_MS,
        });
        self.broadcast(frame).await;

        self.game = Some(RunningGame {
            engine,
            players,
            turns: vec![first],
            pending: BTreeMap::new(),
            started_at_ms: now_ms(),
        });
        self.ready.clear();
        self.publish_info();
        self.next_tick =
            Some(Instant::now() + Duration::from_millis(COUNTDOWN_MS as u64));
    }

    async fn tick(&mut self) {
        let Some(game) = &mut self.game else {
            self.next_tick = None;
            return;
        };

        // Bots think in parallel on blocking threads (PUCT is time-budgeted,
        // capped by tick length so the game loop never slips).
        let current = game.turns.last().unwrap().clone();
        let tick_cap = (self.config.tick_ms / 3).max(5);
        let handles: Vec<_> = self
            .bots
            .iter()
            .filter(|(id, _)| current.alive.contains(*id))
            .map(|(bot_id, &difficulty)| {
                let config = self.config;
                let turn = current.clone();
                let id = bot_id.clone();
                let seed = rand::random::<u64>();
                let budget = bot::budget_ms(difficulty).min(tick_cap);
                tokio::task::spawn_blocking(move || {
                    let mv = bot::choose_move(&config, &turn, &id, difficulty, seed, Some(budget));
                    (id, mv)
                })
            })
            .collect();
        for handle in handles {
            if let Ok((id, mv)) = handle.await {
                game.pending.insert(id, mv);
            }
        }
        let game = self.game.as_mut().unwrap();

        let moves = std::mem::take(&mut game.pending);
        let next = game.engine.next_turn(game.turns.last().unwrap(), &moves);
        let over = next.game_over();
        game.turns.push(next.clone());

        // Snapshots are datagram-class: unreliable, redundant, single-packet.
        self.broadcast(Frame::Snapshot(encode(&ServerMsg::Turn(next)).into())).await;

        if over {
            self.finish_game().await;
        } else {
            self.next_tick =
                Some(Instant::now() + Duration::from_millis(self.config.tick_ms as u64));
        }
    }

    async fn finish_game(&mut self) {
        let Some(game) = self.game.take() else { return };
        self.next_tick = None;

        let record = GameRecord {
            config: self.config,
            players: game.players.clone(),
            seed: game.engine.initial_seed(),
            turns: game.turns.clone(),
        };
        self.last_winners = record
            .turns
            .last()
            .map(|t| t.winners.clone())
            .unwrap_or_default();

        let db = self.db.clone();
        let started = game.started_at_ms;
        let finished = now_ms();
        let saved = tokio::task::spawn_blocking(move || {
            db.insert_game(started, finished, &record)
        })
        .await;

        match saved {
            Ok(Ok(id)) => {
                self.last_game_id = Some(id);
                self.broadcast(control_frame(&ServerMsg::GameSaved { id })).await;
            }
            Ok(Err(e)) => tracing::error!("failed to save game: {e}"),
            Err(e) => tracing::error!("save task panicked: {e}"),
        }

        self.broadcast_lobby().await;
    }

    async fn broadcast_lobby(&mut self) {
        let state = LobbyState {
            session: self.name.clone(),
            game_running: self.game.is_some(),
            config: self.config,
            players: self.lobby.clone(),
            ready: self.ready.iter().cloned().collect(),
            last_winners: self.last_winners.clone(),
            last_game_id: self.last_game_id,
        };
        self.publish_info();
        self.broadcast(control_frame(&ServerMsg::Lobby(state))).await;
    }

    /// Send a pre-encoded frame to every connection; drop connections whose
    /// buffers are full (they will reconnect into a snapshot).
    async fn broadcast(&mut self, frame: Frame) {
        let mut dead = Vec::new();
        for (&id, conn) in &self.conns {
            if conn.tx.try_send(frame.clone()).is_err() {
                dead.push(id);
            }
        }
        for id in dead {
            tracing::warn!(session = %self.name, conn = id, "dropping slow connection");
            self.conns.remove(&id);
        }
    }

    async fn send_to(&mut self, conn_id: u64, frame: Frame) {
        if let Some(conn) = self.conns.get(&conn_id) {
            if conn.tx.try_send(frame).is_err() {
                self.conns.remove(&conn_id);
            }
        }
    }
}

fn control_frame(msg: &ServerMsg) -> Frame {
    Frame::Control(encode(msg).into())
}

/// Per-connection outbound channel, sized so a healthy client never blocks
/// and a stuck one gets detected.
pub fn conn_channel() -> (mpsc::Sender<Frame>, mpsc::Receiver<Frame>) {
    mpsc::channel(CONN_BUFFER)
}
