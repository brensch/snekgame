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
        ip: String,
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
                ip: ip.clone(),
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
        ip: String,
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
    /// Remote IP (device fingerprinting for compute stats).
    ip: String,
    /// Self-described device (from the user agent).
    device: String,
    /// Self-reported search speed from the lobby benchmark; present =
    /// eligible to host bot compute.
    sims_per_sec: Option<u32>,
    /// Simulations per second the device most recently achieved, used only
    /// to rank hosts (fastest phone gets first pick of the bots). Nothing
    /// is budgeted from it — devices are given a deadline and report what
    /// they managed within it.
    recent_sims_per_sec: Option<u32>,
    /// Fraction of the nominal thinking window this host actually gets.
    /// The one thing worth controlling is the only thing we can observe
    /// cleanly: did the answer arrive before the tick? Shrink hard when it
    /// does not, creep back when it does.
    think_share: f64,
    /// Protocol abuse only (bad checksum, answering someone else's
    /// request). Three of these stop us trusting the connection.
    strikes: u32,
}

struct PendingCompute {
    bot_id: PlayerId,
    conn_id: u64,
    turn: Turn,
    nonce: u64,
}

struct ComputeAccum {
    moves: u32,
    sims: u64,
    wall_ms: u64,
    worst_total_ms: u32,
    worst_queue_ms: u32,
    min_sims: u32,
    fallbacks: u32,
}

impl Default for ComputeAccum {
    fn default() -> Self {
        ComputeAccum {
            moves: 0,
            sims: 0,
            wall_ms: 0,
            worst_total_ms: 0,
            worst_queue_ms: 0,
            min_sims: u32::MAX,
            fallbacks: 0,
        }
    }
}

struct RunningGame {
    engine: Engine,
    players: Vec<PlayerProfile>,
    turns: Vec<Turn>,
    pending: BTreeMap<PlayerId, Dir>,
    /// Per-connection compute contributions this game.
    compute: HashMap<u64, ComputeAccum>,
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
    /// bot id -> conn currently computing for it.
    bot_hosts: HashMap<PlayerId, u64>,
    /// outstanding compute requests: id -> (bot, conn, inputs for audit).
    pending_compute: HashMap<u64, PendingCompute>,
    next_request_id: u64,
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
            bot_hosts: HashMap::new(),
            pending_compute: HashMap::new(),
            next_request_id: 1,
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
                ip,
                tx,
            } => {
                self.empty_deadline = None;
                self.conns.insert(
                    conn_id,
                    Conn {
                        player: profile.id.clone(),
                        tx,
                        ip,
                        device: String::new(),
                        sims_per_sec: None,
                        recent_sims_per_sec: None,
                        think_share: 1.0,
                        strikes: 0,
                    },
                );
                // A watcher arriving un-pauses a game frozen for lack of one.
                if self.game.is_some() && self.next_tick.is_none() {
                    self.next_tick =
                        Some(Instant::now() + Duration::from_millis(self.config.tick_ms as u64));
                }
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
                self.assign_bots();
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
                        BotDifficulty::Easy => ("cool guy", "🐛", "hsl(95, 45%, 65%)"),
                        BotDifficulty::Medium => ("dude", "🤖", "hsl(210, 50%, 68%)"),
                        BotDifficulty::Hard => ("lad", "🐲", "hsl(275, 45%, 68%)"),
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
            ClientMsg::Benchmark { sims_per_sec, device } => {
                if let Some(conn) = self.conns.get_mut(&conn_id) {
                    conn.sims_per_sec = Some(sims_per_sec.clamp(500, 2_000_000));
                    conn.device = device.chars().take(80).collect();
                }
                self.assign_bots();
            }
            ClientMsg::BotMove { request_id, dir, proof, tm } => {
                let Some(pending) = self.pending_compute.remove(&request_id) else {
                    return; // stale or bogus
                };
                if pending.conn_id != conn_id {
                    // Someone answered a request that wasn't theirs.
                    self.strike(conn_id, "answered another host's request");
                    return;
                }
                // Guard, not fortress: the checksum must bind our nonce and
                // request id to the move. Cheap to verify, impossible to
                // replay. (The deterministic search keeps full recompute
                // audits on the table if cheating ever gets real.)
                if proof != bot::bind_proof(pending.nonce, request_id, dir) {
                    self.strike(conn_id, "bad response checksum");
                    return;
                }
                if let Some(game) = &mut self.game {
                    let acc = game.compute.entry(conn_id).or_default();
                    acc.moves += 1;
                    acc.sims += tm.sims as u64;
                    acc.wall_ms += tm.search_ms.min(60_000) as u64;
                    acc.worst_total_ms = acc.worst_total_ms.max(tm.total_ms);
                    acc.worst_queue_ms = acc.worst_queue_ms.max(tm.queue_ms);
                    acc.min_sims = acc.min_sims.min(tm.sims);
                    if tm.fallback {
                        acc.fallbacks += 1;
                    }
                }
                // Anything that explains a bad-looking bot gets said out
                // loud; the rest is summarised when the game ends. A move
                // with no slices at all is a *forced* one — a single legal
                // option, decided without searching — which is correct, not
                // poor.
                let forced = tm.slices == 0 && tm.sims == 0;
                if !forced
                    && (tm.fallback || tm.sims < 800 || tm.total_ms > self.config.tick_ms)
                {
                    tracing::info!(
                        session = %self.name, conn = conn_id, bot = %pending.bot_id,
                        think_ms = tm.think_ms, queue_ms = tm.queue_ms,
                        search_ms = tm.search_ms, total_ms = tm.total_ms,
                        sims = tm.sims, slices = tm.slices, fallback = tm.fallback,
                        "compute was poor"
                    );
                }
                // Ranking signal only: how much thinking this device got
                // done inside its deadline.
                if tm.search_ms >= 4 && tm.sims > 0 {
                    if let Some(conn) = self.conns.get_mut(&conn_id) {
                        let rate = (tm.sims as u64 * 1000 / tm.search_ms as u64) as u32;
                        conn.recent_sims_per_sec = Some(match conn.recent_sims_per_sec {
                            Some(prev) => (prev * 7 + rate * 3) / 10,
                            None => rate,
                        });
                    }
                }

                if let Some(game) = &mut self.game {
                    let current = game.turns.last().map(|t| t.turn_number);
                    if current != Some(pending.turn.turn_number) {
                        tracing::info!(
                            session = %self.name,
                            bot = %pending.bot_id,
                            answered_for = pending.turn.turn_number,
                            now_at = current.unwrap_or(0),
                            total_ms = tm.total_ms,
                            queue_ms = tm.queue_ms,
                            sims = tm.sims,
                            "compute answer arrived too late to be used"
                        );
                        if let Some(conn) = self.conns.get_mut(&conn_id) {
                            // Floor it well above uselessness: a bot given
                            // a few dozen milliseconds plays at random,
                            // which looks worse than the occasional late
                            // answer it was protecting us from.
                            conn.think_share = (conn.think_share * 0.7).max(0.45);
                        }
                    } else if let Some(conn) = self.conns.get_mut(&conn_id) {
                        conn.think_share = (conn.think_share + 0.05).min(1.0);
                    }
                    if current == Some(pending.turn.turn_number)
                        && game
                            .turns
                            .last()
                            .map(|t| t.alive.contains(&pending.bot_id))
                            .unwrap_or(false)
                    {
                        game.pending.insert(pending.bot_id.clone(), dir);
                    }
                }

            }
            ClientMsg::ComputeFailed { request_id, reason } => {
                let bot = self
                    .pending_compute
                    .get(&request_id)
                    .map(|pc| pc.bot_id.clone())
                    .unwrap_or_default();
                tracing::info!(
                    session = %self.name, conn = conn_id, %bot, %reason,
                    "device could not compute a bot move"
                );
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
            compute: HashMap::new(),
            started_at_ms: now_ms(),
        });
        // Never let one bad game poison the next: trust resets, and the
        // budget restarts from a modest value it can climb from.
        for conn in self.conns.values_mut() {
            conn.strikes = 0;
            conn.think_share = conn.think_share.max(0.5);
        }
        self.ready.clear();
        self.publish_info();
        self.next_tick =
            Some(Instant::now() + Duration::from_millis(COUNTDOWN_MS as u64));
        self.assign_bots();
        self.issue_compute_requests().await;
    }

    async fn tick(&mut self) {
        // Anchor the cadence to when this tick was DUE, not to whenever we
        // finish processing it — otherwise per-tick work (and actor-queue
        // delays) add onto the period and the rhythm wobbles.
        let due = self.next_tick.take().unwrap_or_else(Instant::now);
        let Some(game) = &mut self.game else {
            self.next_tick = None;
            return;
        };

        // Nobody watching: the bots don't play. Freeze the tick loop; the
        // next human connection resumes it.
        if self.conns.is_empty() {
            self.next_tick = None;
            return;
        }

        // Distributed compute reconciliation: hosts whose requests never
        // came back get a strike and their bots fail over to the next
        // phone. (No server fallback — bots without an answer coast.)
        let current = game.turns.last().unwrap().clone();
        let mut late_hosts: Vec<u64> = Vec::new();
        let mut missing: Vec<PlayerId> = Vec::new();
        for pc in self.pending_compute.values() {
            if pc.turn.turn_number == current.turn_number
                && current.alive.contains(&pc.bot_id)
                && !game.pending.contains_key(&pc.bot_id)
            {
                late_hosts.push(pc.conn_id);
            }
        }
        for bot_id in self.bots.keys() {
            if current.alive.contains(bot_id) && !game.pending.contains_key(bot_id) {
                missing.push(bot_id.clone());
            }
        }
        late_hosts.sort_unstable();
        late_hosts.dedup();
        // Anything alive without a move this tick coasts straight ahead —
        // the visible symptom of a compute failure. Say exactly why.
        for bot_id in self.bots.keys() {
            if !current.alive.contains(bot_id) || game.pending.contains_key(bot_id) {
                continue;
            }
            let pending = self
                .pending_compute
                .values()
                .find(|pc| &pc.bot_id == bot_id && pc.turn.turn_number == current.turn_number);
            match (pending, self.bot_hosts.get(bot_id)) {
                (Some(pc), _) => tracing::info!(
                    session = %self.name, bot = %bot_id, turn = current.turn_number,
                    conn = pc.conn_id,
                    "no answer before the tick — bot holds its heading"
                ),
                (None, None) => tracing::info!(
                    session = %self.name, bot = %bot_id, turn = current.turn_number,
                    "no host assigned — bot holds its heading"
                ),
                (None, Some(conn)) => tracing::info!(
                    session = %self.name, bot = %bot_id, turn = current.turn_number,
                    conn = conn,
                    "no request outstanding — bot holds its heading"
                ),
            }
        }
        // Hard lock: the server NEVER searches. Bots whose host didn't
        // answer simply keep their heading this tick (the engine's default).
        drop(missing);
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
            // Fixed cadence from the due time. If we somehow fell more than
            // a period behind (stall, load), skip forward instead of
            // machine-gunning catch-up ticks.
            let period = Duration::from_millis(self.config.tick_ms as u64);
            let mut next = due + period;
            let now = Instant::now();
            if next <= now {
                next = now + period;
            }
            self.next_tick = Some(next);
            if self.bot_hosts.is_empty() {
                self.assign_bots();
            }
            self.issue_compute_requests().await;
        }
    }

    async fn finish_game(&mut self) {
        let Some(game) = self.game.take() else { return };
        self.next_tick = None;
        self.pending_compute.clear();
        self.bot_hosts.clear();

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

        // One line per host: how the device actually behaved this game.
        for (conn_id, acc) in &game.compute {
            tracing::info!(
                session = %self.name, conn = conn_id, moves = acc.moves,
                mean_sims = acc.sims / acc.moves.max(1) as u64,
                min_sims = if acc.min_sims == u32::MAX { 0 } else { acc.min_sims },
                worst_total_ms = acc.worst_total_ms,
                worst_queue_ms = acc.worst_queue_ms,
                fallbacks = acc.fallbacks,
                "compute summary"
            );
        }

        // Per-device compute scoreboard: who thought for the bots, on what.
        let compute: Vec<(ComputeStat, PlayerId, String, u32)> = game
            .compute
            .iter()
            .filter_map(|(conn_id, acc)| {
                let conn = self.conns.get(conn_id)?;
                let name = self
                    .lobby
                    .iter()
                    .find(|p| p.id == conn.player)
                    .map(|p| format!("{} {}", p.emoji, p.name))
                    .unwrap_or_else(|| "departed".to_string());
                Some((
                    ComputeStat {
                        name,
                        device: conn.device.clone(),
                        moves: acc.moves,
                        sims: acc.sims,
                        wall_ms: acc.wall_ms,
                    },
                    conn.player.clone(),
                    conn.ip.clone(),
                    conn.sims_per_sec.unwrap_or(0),
                ))
            })
            .collect();

        match saved {
            Ok(Ok(id)) => {
                self.last_game_id = Some(id);
                let stats: Vec<ComputeStat> =
                    compute.iter().map(|(s, _, _, _)| s.clone()).collect();
                let db = self.db.clone();
                let rows = compute;
                tokio::task::spawn_blocking(move || {
                    for (stat, player_id, ip, benchmark) in rows {
                        if let Err(e) = db.insert_compute_stat(
                            id,
                            &player_id,
                            &stat.name,
                            &ip,
                            &stat.device,
                            benchmark,
                            stat.moves,
                            stat.sims,
                            stat.wall_ms,
                        ) {
                            tracing::error!("failed to save compute stat: {e}");
                        }
                    }
                });
                self.broadcast(control_frame(&ServerMsg::GameSaved { id, compute: stats }))
                    .await;
            }
            Ok(Err(e)) => tracing::error!("failed to save game: {e}"),
            Err(e) => tracing::error!("save task panicked: {e}"),
        }

        self.broadcast_lobby().await;
    }

    /// Spread bots across eligible hosts (benchmarked, un-banned conns),
    /// fastest phones first: bot i goes to host (i mod n) in speed order.
    fn assign_bots(&mut self) {
        let mut hosts: Vec<(u64, u32)> = self
            .conns
            .iter()
            .filter(|(_, c)| c.strikes < 3)
            .filter_map(|(&id, c)| {
                c.recent_sims_per_sec
                    .or(c.sims_per_sec)
                    .map(|rate| (id, rate))
            })
            .collect();
        hosts.sort_by_key(|&(id, r)| (std::cmp::Reverse(r), id));
        self.bot_hosts.clear();
        if hosts.is_empty() {
            return;
        }
        let mut bot_ids: Vec<&PlayerId> = self.bots.keys().collect();
        bot_ids.sort();
        for (i, bot_id) in bot_ids.into_iter().enumerate() {
            self.bot_hosts
                .insert(bot_id.clone(), hosts[i % hosts.len()].0);
        }
    }

    /// A host misbehaved at the protocol level (bad checksum, answering a
    /// request that was not its own). Three of these stop us trusting it.
    fn strike(&mut self, conn_id: u64, why: &'static str) {
        if let Some(conn) = self.conns.get_mut(&conn_id) {
            conn.strikes += 1;
            tracing::info!(session = %self.name, conn = conn_id, why, strikes = conn.strikes,
                "compute host strike");
            // Eligibility is checked against `strikes`, which a new game
            // resets — never null the benchmark, or the device could never
            // host again for the life of the connection.
        }
        self.assign_bots();
    }

    /// Kick off next-turn compute on the assigned phones: each request is
    /// (turn, sims, nonce), due back before the next tick.
    async fn issue_compute_requests(&mut self) {
        let Some(game) = &self.game else { return };
        let turn = game.turns.last().unwrap().clone();
        if turn.game_over() {
            return;
        }
        self.pending_compute.clear();
        // How many bots each host carries this turn (splits their budget).
        let mut load: HashMap<u64, u32> = HashMap::new();
        for (bot_id, conn_id) in &self.bot_hosts {
            if turn.alive.contains(bot_id) {
                *load.entry(*conn_id).or_default() += 1;
            }
        }
        let mut requests: Vec<(u64, Frame)> = Vec::new();
        for (bot_id, &difficulty) in &self.bots {
            if !turn.alive.contains(bot_id) {
                continue;
            }
            let Some(&conn_id) = self.bot_hosts.get(bot_id) else {
                continue; // no hosts: tick() falls back server-side
            };
            if !self.conns.contains_key(&conn_id) {
                continue;
            }
            // Everything a host runs must finish inside the tick, with room
            // for the round trip and for a phone whose main thread is also
            // drawing the game. Bots on a device think in parallel (a worker
            // each), so the window is not divided between them — but it is
            // scaled by how reliably this host has been answering.
            let share = self.conns.get(&conn_id).map_or(1.0, |c| c.think_share);
            let window = (self.config.tick_ms as f64 * 0.7 - 60.0).max(25.0);
            let difficulty_factor = match difficulty {
                BotDifficulty::Easy => 0.2,
                BotDifficulty::Medium => 0.5,
                BotDifficulty::Hard => 1.0,
            };
            let think_ms =
                (window * difficulty_factor * share).clamp(20.0, 2_000.0) as u32;
            let request_id = self.next_request_id;
            self.next_request_id += 1;
            let nonce = rand::random::<u64>();
            self.pending_compute.insert(
                request_id,
                PendingCompute {
                    bot_id: bot_id.clone(),
                    conn_id,
                    turn: turn.clone(),
                    nonce,
                },
            );
            let bot_name = self
                .lobby
                .iter()
                .find(|p| p.id == *bot_id)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            requests.push((
                conn_id,
                control_frame(&ServerMsg::ComputeRequest {
                    request_id,
                    bot_id: bot_id.clone(),
                    bot_name,
                    config: self.config,
                    turn: turn.clone(),
                    think_ms,
                    nonce,
                }),
            ));
        }
        for (conn_id, frame) in requests {
            self.send_to(conn_id, frame).await;
        }
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
