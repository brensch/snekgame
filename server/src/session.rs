//! One actor task per session (lobby).
//!
//! WebTransport connections forward decoded client messages in; the actor
//! owns all game state, runs the tick loop, and broadcasts pre-encoded
//! frames out through per-connection channels.
//!
//! This is the whole concurrency model: no locks around game state, no shared
//! mutation — just message passing. A slow client whose outbound queue fills
//! up gets dropped and reconnects into a full state snapshot.

use crate::db::Db;
use crate::lock;
use snek_shared::protocol::{
    ClientMsg, ComputeStat, ComputeTelemetry, ConnectReport, Hold, LobbyState, PAUSES_PER_GAME,
    ServerMsg, SessionSummary, encode, valid_game_name,
};
use snek_shared::{
    BotDifficulty, CEREBRAL_BOT_MS, Dir, Engine, GameConfig, GameRecord, PlayerId, PlayerProfile,
    Turn, Winner, bot,
};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

/// Three seconds of "this one's you", then ready, steady, go (a second
/// each): the same at every speed, and again after a pause.
pub const COUNTDOWN_MS: u32 = 6000;
/// After a pause: a second of "resuming", then the same as a start.
pub const RESUME_MS: u32 = 1000 + COUNTDOWN_MS;
const MAX_PLAYERS: usize = 8;
/// Outbound frames buffered per connection before we call the client dead.
const CONN_BUFFER: usize = 256;
/// How long an empty session lingers (so a network blip or refresh doesn't
/// wipe the lobby) before it is tidied away.
const EMPTY_GRACE: Duration = Duration::from_mins(1);
/// How long a player keeps their seat after their connection drops. A
/// backgrounded tab stops reading its socket and the server times it out
/// within ten seconds, so without this, glancing at another app costs you
/// your place in the lobby and whatever you had already agreed to.
const AWAY_GRACE: Duration = Duration::from_secs(45);

/// Outbound message with its transport class: control frames ride the
/// reliable stream; snapshots ride (redundant) datagrams and may be dropped.
#[derive(Clone)]
pub enum Frame {
    /// Must arrive, in order: lobby state, game start/end, catch-ups.
    Control(Arc<[u8]>),
    /// Superseded by the next one anyway, so losing it is fine.
    Snapshot(Arc<[u8]>),
}

/// Wall-clock time for the history record. A clock set before 1970 (or
/// absurdly far ahead) records 0 rather than taking the game down with it.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// A duration in whole milliseconds, for timings that are logged and
/// compared. Saturates: no measured interval comes near `u64::MAX` ms.
fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// A count of players or bots as the protocol carries it. Lobbies hold at
/// most `MAX_PLAYERS`, so this never actually saturates.
fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

// ---------------------------------------------------------------------------
// Registry: session name -> live actor
// ---------------------------------------------------------------------------

/// Every live session, shared by all connections: hands each one the actor
/// it belongs to and keeps the active-games list current.
#[derive(Clone)]
pub struct Registry {
    inner: Arc<Mutex<HashMap<String, mpsc::Sender<Cmd>>>>,
    /// Live snapshots for the active-games list, kept fresh by the actors.
    infos: Arc<Mutex<HashMap<String, SessionSummary>>>,
    /// Pushes the active list to `WatchGames` subscribers on every change.
    games_watch: Arc<tokio::sync::watch::Sender<Vec<SessionSummary>>>,
    db: Arc<Db>,
    next_conn_id: Arc<AtomicU64>,
}

impl Registry {
    /// An empty registry whose sessions save finished games to `db`.
    #[must_use]
    pub fn new(db: Arc<Db>) -> Self {
        let (games_watch, _) = tokio::sync::watch::channel(Vec::new());
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            infos: Arc::new(Mutex::new(HashMap::new())),
            games_watch: Arc::new(games_watch),
            db,
            next_conn_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// The active-games list, now and on every change after.
    #[must_use]
    pub fn watch_games(&self) -> tokio::sync::watch::Receiver<Vec<SessionSummary>> {
        self.games_watch.subscribe()
    }

    /// A fresh id for a connection, unique for the life of the process.
    #[must_use]
    pub fn new_conn_id(&self) -> u64 {
        self.next_conn_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Every game there is, by name, for the games list. A game exists until
    /// it has had nobody connected for a minute (see `EMPTY_GRACE`).
    #[must_use]
    pub fn active_sessions(&self) -> Vec<SessionSummary> {
        let mut list: Vec<SessionSummary> = lock(&self.infos).values().cloned().collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    fn publish(&self, info: SessionSummary) {
        lock(&self.infos).insert(info.id.clone(), info);
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
                lock(&self.inner)
                    .entry(session.to_owned())
                    .or_insert_with(|| {
                        SessionActor::spawn(session.to_owned(), self.clone(), Arc::clone(&self.db))
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
                    lock(&self.inner).remove(session);
                }
            }
        }
    }

    fn remove(&self, session: &str, sender: &mpsc::Sender<Cmd>) {
        {
            let mut map = lock(&self.inner);
            if map
                .get(session)
                .is_some_and(|current| current.same_channel(sender))
            {
                map.remove(session);
            }
        }
        lock(&self.infos).remove(session);
        self.games_watch.send_replace(self.active_sessions());
    }
}

// ---------------------------------------------------------------------------
// Session actor
// ---------------------------------------------------------------------------

/// What the transport tells a session's actor.
pub enum Cmd {
    /// A player's connection joined the session.
    Connect {
        /// The connection, from `Registry::new_conn_id`.
        conn_id: u64,
        /// Who it is, already validated, under their public id.
        profile: PlayerProfile,
        /// Remote address, for telling devices apart in the compute stats.
        ip: String,
        /// Where this connection's outbound frames go.
        tx: mpsc::Sender<Frame>,
    },
    /// The connection went away. The player keeps their seat for a while.
    Disconnect {
        /// The connection that went.
        conn_id: u64,
    },
    /// A client message, from the stream or a datagram.
    Msg {
        /// The connection it came in on.
        conn_id: u64,
        /// The message itself.
        msg: ClientMsg,
    },
    /// A keepalive reached us. Cheaper than a full message and, for a
    /// device that is only watching, the sole evidence it is still there.
    Alive {
        /// The connection it came in on.
        conn_id: u64,
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
    /// Logical cores this device reported.
    cores: u32,
    /// What the device last said about itself. A hidden page suspends its
    /// workers, and it keeps saying so by *not* saying otherwise: only an
    /// explicit "I'm back" clears this. A hidden page can still get the odd
    /// message out, and treating those as proof of life un-paused the game
    /// immediately after it paused.
    declared_available: bool,
    /// What we've inferred from silence, when the device never got a word
    /// out — a screen can cut power to the page mid-message. Any message at
    /// all clears this, so an inference can never become a permanent ban.
    presumed_asleep: bool,
    /// Consecutive compute requests this device didn't answer at all. A
    /// screen going off can suspend the page before its "I'm hidden"
    /// message escapes, so silence has to be readable as absence too.
    misses: u32,
    /// Protocol abuse only (bad checksum, answering someone else's
    /// request). Three of these stop us trusting the connection.
    strikes: u32,
}

struct PendingCompute {
    bot_id: PlayerId,
    conn_id: u64,
    turn: Turn,
    nonce: u64,
    /// When we put it on the wire. The device reports how long it spent
    /// thinking; the difference is everything else — both network legs and
    /// whatever the browser did before handing the message to the page.
    /// Without it a slow link and a slow phone are the same event.
    issued_at: Instant,
}

/// The little we need to keep about a request whose tick has passed: just
/// enough to recognise a late answer and say whose it was.
struct RetiredCompute {
    conn_id: u64,
    bot_id: PlayerId,
    turn_number: u32,
    issued_at: Instant,
    /// Whether a late answer for this request has already cost the host
    /// some window — answers arrive twice on purpose.
    penalised: bool,
}

struct ComputeAccum {
    moves: u32,
    sims: u64,
    wall_ms: u64,
    worst_total_ms: u32,
    worst_queue_ms: u32,
    /// Issue to answer, measured here. Everything the device didn't spend
    /// thinking is the wire.
    worst_round_trip_ms: u64,
    worst_wire_ms: u64,
    wire_ms_total: u64,
    worst_loop_lag_ms: u32,
    min_sims: u32,
    fallbacks: u32,
}

impl Default for ComputeAccum {
    fn default() -> Self {
        Self {
            moves: 0,
            sims: 0,
            wall_ms: 0,
            worst_total_ms: 0,
            worst_queue_ms: 0,
            worst_round_trip_ms: 0,
            worst_wire_ms: 0,
            wire_ms_total: 0,
            worst_loop_lag_ms: 0,
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
    /// When the turn on the board started taking moves: the end of the
    /// countdown, the end of a resume, or the last tick. A cerebral turn
    /// never goes before it, and gives its bots a second from it.
    opened: Instant,
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
    /// Who has the game paused, if anyone: held by a player rather than by
    /// circumstance. Distinct from the freezes the session does on its own
    /// (nobody watching, nobody able to think) — those resume themselves,
    /// and this one must not.
    paused_by: Option<PlayerId>,
    /// Pauses each player has left this game.
    pauses_left: BTreeMap<PlayerId, u8>,
    pending_compute: HashMap<u64, PendingCompute>,
    /// Requests whose tick has already resolved, kept briefly so a late
    /// answer is still recognisable. Dropping them on sight made lateness
    /// invisible — and lateness is the one signal the thinking window is
    /// adjusted from, so a struggling device was never asked for less.
    retired_compute: VecDeque<(u64, RetiredCompute)>,
    next_request_id: u64,
    next_tick: Option<Instant>,
    /// Set when the last connection drops; the actor tidies itself away
    /// after `EMPTY_GRACE` unless someone comes back.
    empty_deadline: Option<Instant>,
    /// Players holding a seat with no connection behind it, and when that
    /// stops being true.
    away: HashMap<PlayerId, Instant>,
    last_winners: Vec<Winner>,
    last_players: Vec<PlayerProfile>,
    last_game_id: Option<i64>,
    config_changed_by: Option<PlayerId>,
    config_rev: u32,
    /// What the game is called (see `LobbyState::name`).
    title: String,
}

impl SessionActor {
    fn spawn(name: String, registry: Registry, db: Arc<Db>) -> mpsc::Sender<Cmd> {
        let actor = Self::new(name, registry, db);
        let tx = actor.self_sender.clone();
        tokio::spawn(actor.run());
        tx
    }

    /// A session with nobody in it yet, not yet running.
    fn new(name: String, registry: Registry, db: Arc<Db>) -> Self {
        let (tx, rx) = mpsc::channel(1024);
        Self {
            title: snek_shared::names::game_name(|n| rand::random_range(0..n)),
            name,
            registry,
            db,
            rx,
            self_sender: tx,
            conns: HashMap::new(),
            config: GameConfig::default(),
            lobby: Vec::new(),
            bots: HashMap::new(),
            bot_counter: 0,
            ready: HashSet::new(),
            game: None,
            bot_hosts: HashMap::new(),
            paused_by: None,
            pauses_left: BTreeMap::new(),
            pending_compute: HashMap::new(),
            retired_compute: VecDeque::new(),
            next_request_id: 1,
            next_tick: None,
            empty_deadline: None,
            away: HashMap::new(),
            last_winners: Vec::new(),
            last_players: Vec::new(),
            last_game_id: None,
            config_changed_by: None,
            config_rev: 0,
        }
    }

    fn publish_info(&self) {
        let humans = self.lobby.len().saturating_sub(self.bots.len());
        self.registry.publish(SessionSummary {
            id: self.name.clone(),
            name: self.title.clone(),
            emojis: self.lobby.iter().map(|p| p.emoji.clone()).collect(),
            players: count(humans),
            bots: count(self.bots.len()),
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
            let away_sleep = async {
                match self.away.values().min().copied() {
                    Some(t) => tokio::time::sleep_until(t).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                cmd = self.rx.recv() => match cmd {
                    Some(cmd) => self.handle(cmd),
                    None => break,
                },
                () = tick_sleep => self.tick().await,
                () = empty_sleep => {
                    if self.conns.is_empty() || !self.has_people() {
                        break; // nobody came back, or nobody's left: tidy up
                    }
                    self.empty_deadline = None;
                }
                () = away_sleep => self.release_expired_seats(),
            }
        }
        tracing::info!(session = %self.name, "session ended");
        self.registry.remove(&self.name.clone(), &self.self_sender);
    }

    /// Give up the seats of players who never came back.
    fn release_expired_seats(&mut self) {
        let now = Instant::now();
        let expired: Vec<PlayerId> = self
            .away
            .iter()
            .filter(|&(_, &deadline)| deadline <= now)
            .map(|(id, _)| id.clone())
            .collect();
        if expired.is_empty() {
            return;
        }
        for id in &expired {
            tracing::info!(session = %self.name, player = %id, "seat released");
            self.away.remove(id);
            self.lobby.retain(|p| p.id != *id);
            self.ready.remove(id);
        }
        self.assign_bots();
        self.broadcast_lobby();
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Connect {
                conn_id,
                profile,
                ip,
                tx,
            } => {
                // Logged so a drop and the return that follows it can be
                // read off the log as a pair, with the time between them.
                tracing::info!(session = %self.name, player = %profile.id, conn = conn_id, "connected");
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
                        cores: 4,
                        declared_available: true,
                        presumed_asleep: false,
                        misses: 0,
                        strikes: 0,
                    },
                );
                // A watcher arriving restarts a game frozen for lack of one.
                self.resume_tick();
                self.away.remove(&self.conns[&conn_id].player);
                self.upsert_lobby_player(profile);
                // Late joiner while a game runs: catch them up so they can
                // spectate (or resume, if they are in it).
                if let Some(game) = &self.game {
                    let frame = control_frame(&ServerMsg::GameInProgress {
                        config: self.config.clone(),
                        players: game.players.clone(),
                        turns: game.turns.clone(),
                    });
                    self.send_to(conn_id, frame);
                    self.send_to(conn_id, control_frame(&self.hold(None)));
                }
                self.broadcast_lobby();
            }
            Cmd::Disconnect { conn_id } => {
                if let Some(conn) = self.conns.remove(&conn_id) {
                    let gone = !self.conns.values().any(|c| c.player == conn.player);
                    if gone {
                        // Their snek keeps sliding if a game is running, and
                        // their seat waits for them. Dropping it here is what
                        // made coming back to a tab feel broken: the roster
                        // entry vanished, the ready they had given went with
                        // it, and they rejoined as somebody new.
                        self.away
                            .insert(conn.player.clone(), Instant::now() + AWAY_GRACE);
                        tracing::info!(
                            session = %self.name, player = %conn.player,
                            "connection dropped — holding their seat"
                        );
                    }
                }
                if self.conns.is_empty() {
                    self.empty_deadline = Some(Instant::now() + EMPTY_GRACE);
                }
                self.assign_bots();
                self.broadcast_lobby();
            }
            Cmd::Msg { conn_id, msg } => self.client_msg(conn_id, msg),
            Cmd::Alive { conn_id } => self.mark_alive(conn_id),
        }
    }

    /// Anything at all from a device disproves an *inference* that it went
    /// quiet — without this, a device parked by a couple of missed ticks
    /// would never be sent another request and so could never prove
    /// otherwise, a permanent ban by silence. It deliberately does not
    /// override what the device said about itself.
    fn mark_alive(&mut self, conn_id: u64) {
        let was_asleep = match self.conns.get_mut(&conn_id) {
            Some(conn) => {
                conn.misses = 0;
                let was = conn.presumed_asleep;
                conn.presumed_asleep = false;
                was && conn.declared_available
            }
            None => return,
        };
        if was_asleep {
            tracing::info!(session = %self.name, conn = conn_id, "device is awake again");
            self.assign_bots();
        }
    }

    fn client_msg(&mut self, conn_id: u64, msg: ClientMsg) {
        let player = match self.conns.get(&conn_id) {
            Some(c) => c.player.clone(),
            None => return,
        };
        self.mark_alive(conn_id);
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
                self.broadcast_lobby();
            }
            ClientMsg::SetReady(ready) => {
                if ready {
                    self.ready.insert(player);
                } else {
                    self.ready.remove(&player);
                }
                self.broadcast_lobby();
            }
            ClientMsg::Configure(config) => self.configure(player, config),
            ClientMsg::Rename(name) => self.rename(player, &name),
            ClientMsg::Leave => self.leave(&player),
            ClientMsg::SetPaused(true) => self.pause(player),
            ClientMsg::SetPaused(false) => self.unpause(&player),
            ClientMsg::Start => self.start_game(),
            ClientMsg::Move { dir, after } => {
                let cerebral = self.config.cerebral;
                if let Some(game) = &mut self.game
                    && game.turns.last().is_some_and(|t| {
                        t.alive.contains(&player) && (!cerebral || t.turn_number == after)
                    })
                {
                    game.pending.insert(player, dir);
                    self.go_when_everyone_has_moved();
                }
            }
            ClientMsg::AddBot(difficulty) => self.add_bot(difficulty),
            ClientMsg::Kick(target) => self.kick(&player, &target),
            ClientMsg::Benchmark {
                sims_per_sec,
                device,
                cores,
            } => {
                if let Some(conn) = self.conns.get_mut(&conn_id) {
                    conn.sims_per_sec = Some(sims_per_sec.clamp(500, 2_000_000));
                    conn.device = device.chars().take(80).collect();
                    conn.cores = cores.clamp(1, 64);
                }
                self.assign_bots();
            }
            ClientMsg::HostAvailable { can_compute } => {
                self.set_host_available(conn_id, can_compute);
            }
            ClientMsg::BotMove {
                request_id,
                dir,
                proof,
                tm,
            } => self.bot_move(conn_id, request_id, dir, proof, &tm),
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
                self.send_to(conn_id, control_frame(&ServerMsg::Pong(n)));
            }
            ClientMsg::Connected(report) => self.log_connect(&player, conn_id, &report),
            // Handled at the transport layer before a session exists.
            ClientMsg::WatchGames => {}
        }
    }

    /// Change the settings for the next game, between games only.
    fn configure(&mut self, player: PlayerId, config: GameConfig) {
        // Picking what is already picked changes nothing, so it shouldn't
        // claim the settings either.
        if self.game.is_some() || !config.valid() || config == self.config {
            return;
        }
        self.config = config;
        self.config_changed_by = Some(player);
        self.config_rev = self.config_rev.wrapping_add(1);
        // Readiness stands: agreeing to play isn't agreeing to one exact set
        // of rules, and everyone is told who changed what as it happens.
        self.broadcast_lobby();
    }

    /// Someone leaving on purpose: out of the lobby at once, rather than
    /// holding a seat the way a dropped connection does. Their connections
    /// are told and let go, so when the transport closes there is nothing
    /// left to hold a seat for. A running game carries on without them.
    fn leave(&mut self, player: &PlayerId) {
        self.lobby.retain(|p| p.id != *player);
        self.ready.remove(player);
        self.away.remove(player);
        let theirs: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, c)| c.player == *player)
            .map(|(&id, _)| id)
            .collect();
        tracing::info!(session = %self.name, %player, "left");
        for conn_id in theirs {
            self.send_to(conn_id, control_frame(&ServerMsg::Left));
            self.conns.remove(&conn_id);
        }
        if !self.has_people() {
            // The last person out: the game goes now, not in a minute.
            self.empty_deadline = Some(Instant::now());
        } else if self.conns.is_empty() {
            self.empty_deadline = Some(Instant::now() + EMPTY_GRACE);
        }
        self.assign_bots();
        self.broadcast_lobby();
    }

    /// Whether any human is still in the lobby (holding a seat counts; bots
    /// don't).
    fn has_people(&self) -> bool {
        self.lobby.iter().any(|p| !self.bots.contains_key(&p.id))
    }

    /// Call the game something else, between games. A rename is a settings
    /// change like any other: it's credited to whoever made it and everyone
    /// else is told.
    fn rename(&mut self, player: PlayerId, name: &str) {
        let name = name.trim();
        if self.game.is_some() || !valid_game_name(name) || name == self.title {
            return;
        }
        name.clone_into(&mut self.title);
        self.config_changed_by = Some(player);
        self.config_rev = self.config_rev.wrapping_add(1);
        self.broadcast_lobby();
    }

    /// A client's account of how its reconnect went, for the log: the only
    /// place a slow one on somebody's phone can be seen afterwards.
    fn log_connect(&self, player: &PlayerId, conn_id: u64, report: &ConnectReport) {
        /// A misbehaving client can't fill the log: this much trail at most.
        const TRAIL_EVENTS: usize = 40;
        const EVENT_CHARS: usize = 120;
        let skip = report.trail.len().saturating_sub(TRAIL_EVENTS);
        let trail = report
            .trail
            .iter()
            .skip(skip)
            .map(|e| e.chars().take(EVENT_CHARS).collect::<String>())
            .collect::<Vec<_>>()
            .join(" | ");
        tracing::info!(
            session = %self.name, %player, conn = conn_id,
            reconnect = report.reconnect,
            attempts = report.attempts,
            silent_ms = ?report.silent_ms,
            since_visible_ms = ?report.since_visible_ms,
            page_ms = report.page_ms,
            handshake_ms = report.handshake_ms,
            join_ms = report.join_ms,
            %trail,
            "connect report"
        );
    }

    /// Take a player or bot out of the lobby, between games only.
    ///
    /// Nobody can remove themselves (that's what leaving is for). The player
    /// removed is told so, and by whom, and their connections are let go:
    /// their phone goes back to the home screen rather than sitting in a
    /// lobby it is no longer in.
    fn kick(&mut self, kicker: &PlayerId, target: &PlayerId) {
        if self.game.is_some() || kicker == target {
            return;
        }
        let by = self.lobby.iter().find(|p| p.id == *kicker).cloned();
        self.lobby.retain(|p| p.id != *target);
        self.bots.remove(target);
        self.ready.remove(target);
        // Removing someone settles it — no held seat should outlive the
        // decision and put them back.
        self.away.remove(target);
        let theirs: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, c)| c.player == *target)
            .map(|(&id, _)| id)
            .collect();
        if !theirs.is_empty() {
            tracing::info!(session = %self.name, player = %target, by = %kicker, "removed from the lobby");
        }
        for conn_id in theirs {
            self.send_to(conn_id, control_frame(&ServerMsg::Kicked(by.clone())));
            self.conns.remove(&conn_id);
        }
        self.assign_bots();
        self.broadcast_lobby();
    }

    /// What a device says about whether it can think for bots right now —
    /// the page hiding or coming back.
    fn set_host_available(&mut self, conn_id: u64, can_compute: bool) {
        if let Some(conn) = self.conns.get_mut(&conn_id) {
            if conn.declared_available != can_compute {
                tracing::info!(
                    session = %self.name, conn = conn_id, can_compute,
                    "compute host availability changed"
                );
            }
            conn.declared_available = can_compute;
        }
        self.assign_bots();
        if can_compute {
            self.resume_tick();
        }
    }

    /// A server-hosted bot joins the lobby, if there is room and no game.
    fn add_bot(&mut self, difficulty: BotDifficulty) {
        if self.game.is_none() && self.lobby.len() < MAX_PLAYERS {
            self.bot_counter += 1;
            let (name, emoji, colour) = match difficulty {
                BotDifficulty::Easy => ("cool guy", "🐛", "hsl(95, 45%, 65%)"),
                BotDifficulty::Medium => ("dude", "🐍", "hsl(210, 50%, 68%)"),
                BotDifficulty::Hard => ("lad", "🐲", "hsl(275, 45%, 68%)"),
            };
            let id = format!("bot-{}", self.bot_counter);
            self.bots.insert(id.clone(), difficulty);
            self.lobby.push(PlayerProfile {
                id,
                name: name.to_owned(),
                emoji: emoji.to_owned(),
                colour: colour.to_owned(),
            });
            self.broadcast_lobby();
        }
    }

    /// A host's answer for one of its bots: checked against the request it
    /// claims to answer, counted towards the host's stats and thinking
    /// window, and — if it is still for the current turn — the bot's move.
    fn bot_move(
        &mut self,
        conn_id: u64,
        request_id: u64,
        dir: Dir,
        proof: u64,
        tm: &ComputeTelemetry,
    ) {
        let Some(pending) = self.pending_compute.remove(&request_id) else {
            // Not outstanding: either an answer we have already given up
            // waiting for, or noise. A late answer is worth hearing — it
            // says this device is trying to do more than it can in the time
            // it has.
            self.late_answer(conn_id, request_id, tm);
            return;
        };
        if pending.conn_id != conn_id {
            // Someone answered a request that wasn't theirs.
            self.strike(conn_id, "answered another host's request");
            return;
        }
        // Guard, not fortress: the checksum must bind our nonce and request
        // id to the move. Cheap to verify, impossible to replay. (The
        // deterministic search keeps full recompute audits on the table if
        // cheating ever gets real.)
        if proof != bot::bind_proof(pending.nonce, request_id, dir) {
            self.strike(conn_id, "bad response checksum");
            return;
        }
        self.record_compute(conn_id, &pending, tm);

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
                    conn.misses = 0;
                    // Floor it well above uselessness: a bot given a few
                    // dozen milliseconds plays at random, which looks worse
                    // than the occasional late answer it was protecting us
                    // from.
                    conn.think_share = (conn.think_share * 0.7).max(0.45);
                }
            } else if let Some(conn) = self.conns.get_mut(&conn_id) {
                conn.misses = 0;
                conn.think_share = (conn.think_share + 0.05).min(1.0);
            }
            if current == Some(pending.turn.turn_number)
                && game
                    .turns
                    .last()
                    .is_some_and(|t| t.alive.contains(&pending.bot_id))
            {
                game.pending.insert(pending.bot_id, dir);
                self.go_when_everyone_has_moved();
            }
        }
    }

    /// A cerebral turn goes as soon as everyone has chosen: every living
    /// player with a connection (nobody waits on a phone that's gone), and
    /// every living bot, or once the bots' second is up. Never before the
    /// turn opened, and never later than its clock already says.
    fn go_when_everyone_has_moved(&mut self) {
        let Some(game) = &self.game else { return };
        let Some(turn) = game.turns.last() else {
            return;
        };
        let Some(due) = self.next_tick else {
            // Held (paused, or nobody to play): not ours to start.
            return;
        };
        if !self.config.cerebral {
            return;
        }
        let here: HashSet<&PlayerId> = self.conns.values().map(|c| &c.player).collect();
        let (bots, people): (Vec<&PlayerId>, Vec<&PlayerId>) =
            turn.alive.iter().partition(|p| self.bots.contains_key(*p));
        let moved = |p: &&PlayerId| game.pending.contains_key(*p);
        if !people.iter().filter(|p| here.contains(**p)).all(moved) {
            return;
        }
        let at = if bots.iter().all(moved) {
            game.opened
        } else {
            game.opened + Duration::from_millis(u64::from(CEREBRAL_BOT_MS))
        }
        .max(Instant::now());
        if at < due {
            self.next_tick = Some(at);
        }
    }

    /// An answer to a request whose tick has already gone. Logged, and it
    /// costs the host some of its thinking window — once per request.
    fn late_answer(&mut self, conn_id: u64, request_id: u64, tm: &ComputeTelemetry) {
        let Some((_, r)) = self
            .retired_compute
            .iter_mut()
            .find(|(id, _)| *id == request_id)
        else {
            return;
        };
        if r.conn_id != conn_id {
            return;
        }
        let behind = self
            .game
            .as_ref()
            .and_then(|g| g.turns.last())
            .map_or(0, |t| t.turn_number.saturating_sub(r.turn_number));
        let round_trip_ms = millis(r.issued_at.elapsed());
        tracing::info!(
            session = %self.name, bot = %r.bot_id, conn = conn_id,
            answered_for = r.turn_number, ticks_behind = behind,
            total_ms = tm.total_ms, queue_ms = tm.queue_ms,
            sims = tm.sims,
            loop_lag_ms = tm.loop_lag_ms,
            round_trip_ms,
            wire_ms = round_trip_ms.saturating_sub(u64::from(tm.total_ms)),
            "compute answer arrived after its tick had gone"
        );
        if let Some(conn) = self.conns.get_mut(&conn_id) {
            conn.misses = 0;
            if !r.penalised {
                conn.think_share = (conn.think_share * 0.7).max(0.45);
            }
        }
        // Answers are sent twice for redundancy, so the second copy must not
        // be read as a second failure and shrink the window all over again.
        r.penalised = true;
    }

    /// Fold a checked answer's telemetry into this game's per-host stats
    /// and the host's speed ranking.
    fn record_compute(&mut self, conn_id: u64, pending: &PendingCompute, tm: &ComputeTelemetry) {
        if let Some(game) = &mut self.game {
            let acc = game.compute.entry(conn_id).or_default();
            acc.moves += 1;
            acc.sims += u64::from(tm.sims);
            acc.wall_ms += u64::from(tm.search_ms.min(60_000));
            acc.worst_total_ms = acc.worst_total_ms.max(tm.total_ms);
            acc.worst_queue_ms = acc.worst_queue_ms.max(tm.queue_ms);
            let round_trip = millis(pending.issued_at.elapsed());
            let wire = round_trip.saturating_sub(u64::from(tm.total_ms));
            acc.worst_round_trip_ms = acc.worst_round_trip_ms.max(round_trip);
            acc.worst_wire_ms = acc.worst_wire_ms.max(wire);
            acc.wire_ms_total += wire;
            acc.worst_loop_lag_ms = acc.worst_loop_lag_ms.max(tm.loop_lag_ms);
            acc.min_sims = acc.min_sims.min(tm.sims);
            if tm.fallback {
                acc.fallbacks += 1;
            }
        }
        // Anything that explains a bad-looking bot gets said out loud; the
        // rest is summarised when the game ends. A move with no slices at
        // all is a *forced* one — a single legal option, decided without
        // searching — which is correct, not poor.
        let forced = tm.slices == 0 && tm.sims == 0;
        if !forced && (tm.fallback || tm.sims < 800 || tm.total_ms > self.tick_ms_now()) {
            tracing::info!(
                session = %self.name, conn = conn_id, bot = %pending.bot_id,
                think_ms = tm.think_ms, queue_ms = tm.queue_ms,
                search_ms = tm.search_ms, total_ms = tm.total_ms,
                sims = tm.sims, slices = tm.slices, fallback = tm.fallback,
                "compute was poor"
            );
        }
        // Ranking signal only: how much thinking this device got done
        // inside its deadline.
        if tm.search_ms >= 4
            && tm.sims > 0
            && let Some(conn) = self.conns.get_mut(&conn_id)
        {
            let rate = u64::from(tm.sims) * 1000 / u64::from(tm.search_ms);
            let blended = conn
                .recent_sims_per_sec
                .map_or(rate, |prev| (u64::from(prev) * 7 + rate * 3) / 10);
            conn.recent_sims_per_sec = Some(u32::try_from(blended).unwrap_or(u32::MAX));
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

    /// `player` pauses the game: if one's running, nobody has it paused,
    /// and they have a pause left.
    fn pause(&mut self, player: PlayerId) {
        if self.game.is_none() || self.paused_by.is_some() {
            return;
        }
        let Some(left) = self.pauses_left.get_mut(&player).filter(|l| **l > 0) else {
            return;
        };
        *left -= 1;
        tracing::info!(session = %self.name, %player, left = *left, "paused");
        self.paused_by = Some(player);
        self.next_tick = None;
        self.broadcast_hold(None);
    }

    /// `player` resumes the game: theirs to resume if they paused it, or
    /// anyone's if whoever did has gone (a game can't be left stuck). It
    /// plays again after a second of "resuming" and the countdown a game
    /// starts with.
    fn unpause(&mut self, player: &PlayerId) {
        let Some(by) = &self.paused_by else {
            return;
        };
        let pauser_here = self.conns.values().any(|c| c.player == *by);
        if by != player && pauser_here {
            return;
        }
        tracing::info!(session = %self.name, %player, "resumed");
        self.paused_by = None;
        let opens = Instant::now() + Duration::from_millis(u64::from(RESUME_MS));
        self.next_tick = Some(opens);
        if let Some(game) = &mut self.game {
            game.opened = opens;
        }
        self.broadcast_hold(Some(RESUME_MS));
    }

    /// The game's pause state, as told to everyone.
    fn hold(&self, resume_in_ms: Option<u32>) -> ServerMsg {
        ServerMsg::Hold(Hold {
            by: self.paused_by.clone(),
            pauses_left: self.pauses_left.clone(),
            resume_in_ms,
        })
    }

    fn broadcast_hold(&mut self, resume_in_ms: Option<u32>) {
        let frame = control_frame(&self.hold(resume_in_ms));
        self.broadcast(&frame);
    }

    fn start_game(&mut self) {
        if self.game.is_some() || self.lobby.is_empty() {
            return;
        }
        self.paused_by = None;
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
        let mut engine = Engine::new(self.config.clone(), ids, seed);
        let first = engine.first_turn();

        let frame = control_frame(&ServerMsg::GameStart {
            config: self.config.clone(),
            players: players.clone(),
            turn: first.clone(),
            countdown_ms: COUNTDOWN_MS,
        });
        self.broadcast(&frame);

        let opens = Instant::now() + Duration::from_millis(u64::from(COUNTDOWN_MS));
        self.game = Some(RunningGame {
            engine,
            players,
            turns: vec![first],
            pending: BTreeMap::new(),
            compute: HashMap::new(),
            started_at_ms: now_ms(),
            opened: opens,
        });
        // Never let one bad game poison the next: trust resets, and the
        // budget restarts from a modest value it can climb from.
        for conn in self.conns.values_mut() {
            conn.strikes = 0;
            conn.think_share = conn.think_share.max(0.5);
        }
        self.ready.clear();
        // Everyone in it starts with their full allowance of pauses.
        self.pauses_left = self
            .lobby
            .iter()
            .filter(|p| !self.bots.contains_key(&p.id))
            .map(|p| (p.id.clone(), PAUSES_PER_GAME))
            .collect();
        self.broadcast_hold(None);
        self.publish_info();
        self.next_tick = Some(opens);
        self.assign_bots();
        self.issue_compute_requests();
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

        // Held on purpose.
        if self.paused_by.is_some() {
            self.next_tick = None;
            return;
        }

        // Nobody watching: the bots don't play. Freeze the tick loop; the
        // next human connection resumes it.
        if self.conns.is_empty() {
            self.next_tick = None;
            return;
        }

        // Nobody able to think for the bots either — every host's page is
        // hidden or its screen is off, so their workers are suspended.
        // Freezing is honest; ticking on would just march every bot into a
        // wall while the phone sleeps.
        if !self.bots.is_empty() && self.bot_hosts.is_empty() {
            tracing::info!(
                session = %self.name,
                "no device can compute right now — pausing until one returns"
            );
            self.next_tick = None;
            return;
        }

        let Some(current) = game.turns.last().cloned() else {
            // A game always starts with its first turn, so this can't be;
            // if it were, there would be nothing to advance from.
            tracing::error!(session = %self.name, "running game has no turns — stopping its clock");
            self.next_tick = None;
            return;
        };
        self.note_coasting_bots(&current);
        self.assign_bots();

        // Hard lock: the server NEVER searches. Bots whose host didn't
        // answer simply keep their heading this tick (the engine's default).
        let Some(game) = self.game.as_mut() else {
            return;
        };

        let moves = std::mem::take(&mut game.pending);
        let next = game.engine.next_turn(&current, &moves);
        let over = next.game_over();
        game.turns.push(next.clone());
        game.opened = Instant::now();

        // Snapshots are datagram-class: unreliable, redundant, single-packet.
        self.broadcast(&Frame::Snapshot(encode(&ServerMsg::Turn(next)).into()));

        if over {
            self.finish_game().await;
        } else {
            // Fixed cadence from the due time. If we somehow fell more than
            // a period behind (stall, load), skip forward instead of
            // machine-gunning catch-up ticks. (A cerebral turn that went
            // early was due when it went, so its next waits the full time.)
            let period = Duration::from_millis(u64::from(self.tick_ms_now()));
            let mut next = due + period;
            let now = Instant::now();
            if next <= now {
                next = now + period;
            }
            self.next_tick = Some(next);
            if self.bot_hosts.is_empty() {
                self.assign_bots();
            }
            self.issue_compute_requests();
        }
    }

    /// Distributed compute reconciliation: every bot still alive without a
    /// move this tick coasts straight ahead — the visible symptom of a
    /// compute failure — so say exactly why, and count the tick against
    /// hosts that never answered. (No server fallback — bots without an
    /// answer coast.)
    fn note_coasting_bots(&mut self, current: &Turn) {
        let Some(game) = &self.game else { return };
        let mut silent: Vec<u64> = Vec::new();
        for bot_id in self.bots.keys() {
            if !current.alive.contains(bot_id) || game.pending.contains_key(bot_id) {
                continue;
            }
            let pending = self
                .pending_compute
                .values()
                .find(|pc| &pc.bot_id == bot_id && pc.turn.turn_number == current.turn_number);
            match (pending, self.bot_hosts.get(bot_id)) {
                (Some(pc), _) => {
                    tracing::info!(
                        session = %self.name, bot = %bot_id, turn = current.turn_number,
                        conn = pc.conn_id,
                        "no answer before the tick — bot holds its heading"
                    );
                    silent.push(pc.conn_id);
                }
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
        // Two ticks of total silence and we stop believing the device is
        // there. It says so itself when it comes back — any message at all
        // clears this.
        silent.sort_unstable();
        silent.dedup();
        for conn_id in silent {
            if let Some(conn) = self.conns.get_mut(&conn_id) {
                conn.misses += 1;
                if conn.misses >= 2 && !conn.presumed_asleep {
                    conn.presumed_asleep = true;
                    tracing::info!(
                        session = %self.name, conn = conn_id,
                        "device silent for two ticks — treating it as asleep"
                    );
                }
            }
        }
    }

    async fn finish_game(&mut self) {
        let Some(game) = self.game.take() else { return };
        self.next_tick = None;
        self.pending_compute.clear();
        self.bot_hosts.clear();

        let record = GameRecord {
            config: self.config.clone(),
            players: game.players.clone(),
            seed: game.engine.initial_seed(),
            turns: game.turns.clone(),
        };
        self.last_players.clone_from(&record.players);
        self.last_winners = record
            .turns
            .last()
            .map(|t| t.winners.clone())
            .unwrap_or_default();

        let db = Arc::clone(&self.db);
        let started = game.started_at_ms;
        let finished = now_ms();
        let saved =
            tokio::task::spawn_blocking(move || db.insert_game(started, finished, &record)).await;

        // One line per host: how the device actually behaved this game.
        for (conn_id, acc) in &game.compute {
            tracing::info!(
                session = %self.name, conn = conn_id, moves = acc.moves,
                mean_sims = acc.sims / u64::from(acc.moves.max(1)),
                min_sims = if acc.min_sims == u32::MAX { 0 } else { acc.min_sims },
                worst_total_ms = acc.worst_total_ms,
                worst_queue_ms = acc.worst_queue_ms,
                worst_round_trip_ms = acc.worst_round_trip_ms,
                worst_wire_ms = acc.worst_wire_ms,
                mean_wire_ms = acc.wire_ms_total / u64::from(acc.moves.max(1)),
                worst_loop_lag_ms = acc.worst_loop_lag_ms,
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
                let name = self.lobby.iter().find(|p| p.id == conn.player).map_or_else(
                    || "departed".to_owned(),
                    |p| format!("{} {}", p.emoji, p.name),
                );
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
                let db = Arc::clone(&self.db);
                let rows = compute;
                tokio::task::spawn_blocking(move || {
                    for (stat, player_id, ip, benchmark) in rows {
                        if let Err(e) =
                            db.insert_compute_stat(id, &stat, &player_id, &ip, benchmark)
                        {
                            tracing::error!("failed to save compute stat: {e}");
                        }
                    }
                });
                self.broadcast(&control_frame(&ServerMsg::GameSaved { id, compute: stats }));
            }
            Ok(Err(e)) => tracing::error!("failed to save game: {e}"),
            Err(e) => tracing::error!("save task panicked: {e}"),
        }

        self.broadcast_lobby();
    }

    /// Spread bots across eligible hosts (benchmarked, un-banned conns),
    /// fastest phones first: bot i goes to host (i mod n) in speed order.
    fn assign_bots(&mut self) {
        let mut hosts: Vec<(u64, u32)> = self
            .conns
            .iter()
            .filter(|(_, c)| c.strikes < 3 && c.declared_available && !c.presumed_asleep)
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
        // A host appearing (or waking) restarts a game frozen for want of
        // one.
        if !self.bot_hosts.is_empty() {
            self.resume_tick();
        }
    }

    /// The wait until the next turn, at the speed the game has reached (see
    /// `GameConfig::tick_ms_at`; turns are numbered from 1). Outside a
    /// game, the starting speed.
    fn tick_ms_now(&self) -> u32 {
        let turn = self
            .game
            .as_ref()
            .and_then(|g| g.turns.last())
            .map_or(0, |t| t.turn_number.saturating_sub(1));
        self.config.tick_ms_at(turn)
    }

    /// Start the clock again, unless someone has deliberately stopped it.
    fn resume_tick(&mut self) {
        if self.game.is_some() && self.next_tick.is_none() && self.paused_by.is_none() {
            self.next_tick =
                Some(Instant::now() + Duration::from_millis(u64::from(self.tick_ms_now())));
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
    fn issue_compute_requests(&mut self) {
        let Some(game) = &self.game else { return };
        let Some(turn) = game.turns.last().cloned() else {
            return;
        };
        if turn.game_over() {
            return;
        }
        // Retire rather than forget: an answer that misses its tick still
        // has to be recognisable when it turns up.
        for (id, pc) in self.pending_compute.drain() {
            self.retired_compute.push_back((
                id,
                RetiredCompute {
                    conn_id: pc.conn_id,
                    bot_id: pc.bot_id.clone(),
                    turn_number: pc.turn.turn_number,
                    issued_at: pc.issued_at,
                    penalised: false,
                },
            ));
        }
        while self.retired_compute.len() > 32 {
            self.retired_compute.pop_front();
        }
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
            // each), so the window is shared between them only when they
            // outnumber the device's threads — and it is always scaled by
            // how reliably this host has been answering.
            let (share, cores) = self
                .conns
                .get(&conn_id)
                .map_or((1.0, 4), |c| (c.think_share, c.cores));
            // Half the tick, and no more. The rest of it belongs to the two
            // network legs — out to the device and back — plus the slice the
            // search overshoots its deadline by. At 70% a phone answering in
            // 302ms of a 500ms tick had under 200ms for both legs, and when
            // that wasn't enough the answer arrived a tick late, by which
            // point the bot had already been defaulted onto its old heading.
            // Three of those in one game, and one of them was a bot holding
            // Left into a wall with a good move sitting in flight.
            //
            // A cerebral turn waits a second for its bots, however long the
            // people take: most of that second is thinking.
            let mut window = if self.config.cerebral {
                f64::from(CEREBRAL_BOT_MS) * 0.8
            } else {
                (f64::from(self.tick_ms_now()) * 0.5).max(25.0)
            };
            // Bots on a device think concurrently, one worker each. If they
            // outnumber the threads available, asking each for the full
            // window pins the phone at 100% and makes the whole device feel
            // slow — so share the available thread-time instead.
            let mine = f64::from(load.get(&conn_id).copied().unwrap_or(1).max(1));
            let threads = f64::from(cores.saturating_sub(1).max(1));
            if mine > threads {
                window *= threads / mine;
            }
            let difficulty_factor = match difficulty {
                BotDifficulty::Easy => 0.2,
                BotDifficulty::Medium => 0.5,
                BotDifficulty::Hard => 1.0,
            };
            let think_ms = whole_ms(window * difficulty_factor * share);
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
                    issued_at: Instant::now(),
                },
            );
            let bot_name = self
                .lobby
                .iter()
                .find(|p| p.id == *bot_id)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            // Both ways, datagram first. A request is the last
            // latency-critical message that rode the reliable stream alone,
            // and one lost packet there head-of-line blocks every request
            // behind it: three bots stalled by an identical 570ms, answered
            // in 115ms of thinking, and the tick was long gone. A datagram
            // cannot block another datagram. The stream copy still follows
            // as the guarantee, and the client ignores whichever arrives
            // second.
            let bytes: Arc<[u8]> = encode(&ServerMsg::ComputeRequest {
                request_id,
                bot_id: bot_id.clone(),
                bot_name,
                config: self.config.clone(),
                turn: turn.clone(),
                think_ms,
                nonce,
            })
            .into();
            requests.push((conn_id, Frame::Snapshot(Arc::clone(&bytes))));
            requests.push((conn_id, Frame::Control(bytes)));
        }
        for (conn_id, frame) in requests {
            self.send_to(conn_id, frame);
        }
    }

    fn broadcast_lobby(&mut self) {
        let state = LobbyState {
            session: self.name.clone(),
            game_running: self.game.is_some(),
            config: self.config.clone(),
            players: self.lobby.clone(),
            ready: self.ready.iter().cloned().collect(),
            last_winners: self.last_winners.clone(),
            last_players: self.last_players.clone(),
            last_game_id: self.last_game_id,
            config_changed_by: self.config_changed_by.clone(),
            config_rev: self.config_rev,
            name: self.title.clone(),
        };
        self.publish_info();
        self.broadcast(&control_frame(&ServerMsg::Lobby(state)));
    }

    /// Send a pre-encoded frame to every connection; drop connections whose
    /// buffers are full (they will reconnect into a snapshot).
    fn broadcast(&mut self, frame: &Frame) {
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

    fn send_to(&mut self, conn_id: u64, frame: Frame) {
        if let Some(conn) = self.conns.get(&conn_id)
            && conn.tx.try_send(frame).is_err()
        {
            self.conns.remove(&conn_id);
        }
    }
}

/// A bot's thinking deadline in whole milliseconds, kept to 20..=2000 so
/// even a starved host gets long enough to find a move and a slow tick
/// can't hand out one longer than any device should spend.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to 20..=2000 first, so it is positive and fits; Rust has no checked float-to-int conversion"
)]
const fn whole_ms(ms: f64) -> u32 {
    ms.clamp(20.0, 2_000.0) as u32
}

fn control_frame(msg: &ServerMsg) -> Frame {
    Frame::Control(encode(msg).into())
}

/// Per-connection outbound channel, sized so a healthy client never blocks
/// and a stuck one gets detected.
#[must_use]
pub fn conn_channel() -> (mpsc::Sender<Frame>, mpsc::Receiver<Frame>) {
    mpsc::channel(CONN_BUFFER)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Logs, profile, temp_path};
    use snek_shared::protocol::{ProfileInfo, decode};

    /// One end of a connection as the session sees it: its frames, decoded.
    struct Peer {
        conn_id: u64,
        id: PlayerId,
        rx: mpsc::Receiver<Frame>,
    }

    impl Peer {
        /// Everything sent so far, oldest first, with whether it rode the
        /// stream (`true`) or a datagram.
        fn frames(&mut self) -> Vec<(bool, ServerMsg)> {
            let mut out = Vec::new();
            while let Ok(frame) = self.rx.try_recv() {
                let (control, bytes) = match frame {
                    Frame::Control(b) => (true, b),
                    Frame::Snapshot(b) => (false, b),
                };
                out.push((control, decode(&bytes).expect("undecodable frame")));
            }
            out
        }

        fn msgs(&mut self) -> Vec<ServerMsg> {
            self.frames().into_iter().map(|(_, m)| m).collect()
        }

        /// The newest lobby sent, if any was.
        fn lobby(&mut self) -> Option<LobbyState> {
            self.msgs().into_iter().rev().find_map(|m| match m {
                ServerMsg::Lobby(l) => Some(l),
                _ => None,
            })
        }

        /// Compute requests sent, each once (they go out both ways).
        fn requests(&mut self) -> Vec<Request> {
            let mut out: Vec<Request> = Vec::new();
            for msg in self.msgs() {
                if let ServerMsg::ComputeRequest {
                    request_id,
                    bot_id,
                    config,
                    turn,
                    think_ms,
                    nonce,
                    ..
                } = msg
                    && !out.iter().any(|r| r.id == request_id)
                {
                    out.push(Request {
                        id: request_id,
                        bot: bot_id,
                        config,
                        turn,
                        think_ms,
                        nonce,
                    });
                }
            }
            out
        }
    }

    struct Request {
        id: u64,
        bot: PlayerId,
        config: GameConfig,
        turn: Turn,
        think_ms: u32,
        nonce: u64,
    }

    impl Request {
        /// A real answer: a short search, properly bound to the request.
        fn answer(&self) -> ClientMsg {
            let mut search = bot::begin(&self.config, &self.turn, &self.bot, 200, self.nonce);
            while !search.step(64) {}
            let (dir, _) = search.result();
            self.answer_with(dir, bot::bind_proof(self.nonce, self.id, dir))
        }

        fn answer_with(&self, dir: Dir, proof: u64) -> ClientMsg {
            ClientMsg::BotMove {
                request_id: self.id,
                dir,
                proof,
                tm: telemetry(),
            }
        }
    }

    fn telemetry() -> ComputeTelemetry {
        ComputeTelemetry {
            queue_ms: 1,
            search_ms: 40,
            total_ms: 45,
            think_ms: 50,
            sims: 2_000,
            slices: 4,
            fallback: false,
            loop_lag_ms: 0,
        }
    }

    fn session() -> SessionActor {
        let db = Arc::new(Db::open(&temp_path("session.db")).unwrap());
        SessionActor::new("test".to_owned(), Registry::new(Arc::clone(&db)), db)
    }

    fn join(actor: &mut SessionActor, name: &str) -> Peer {
        join_as(actor, &format!("{name}-id"), name)
    }

    fn join_as(actor: &mut SessionActor, id: &str, name: &str) -> Peer {
        let conn_id = actor.registry.new_conn_id();
        let (tx, rx) = conn_channel();
        actor.handle(Cmd::Connect {
            conn_id,
            profile: profile(id, name),
            ip: "10.0.0.1".to_owned(),
            tx,
        });
        Peer {
            conn_id,
            id: id.to_owned(),
            rx,
        }
    }

    fn say(actor: &mut SessionActor, peer: &Peer, msg: ClientMsg) {
        actor.handle(Cmd::Msg {
            conn_id: peer.conn_id,
            msg,
        });
    }

    fn info(name: &str) -> ProfileInfo {
        ProfileInfo {
            name: name.to_owned(),
            emoji: "🦊".to_owned(),
            colour: "red".to_owned(),
        }
    }

    fn benchmark(actor: &mut SessionActor, peer: &Peer, cores: u32) {
        say(
            actor,
            peer,
            ClientMsg::Benchmark {
                sims_per_sec: 50_000,
                device: "test phone".to_owned(),
                cores,
            },
        );
    }

    /// Everyone ready, then go.
    fn start(actor: &mut SessionActor, peers: &[&Peer]) {
        for p in peers {
            say(actor, p, ClientMsg::SetReady(true));
        }
        say(actor, peers[0], ClientMsg::Start);
        assert!(actor.game.is_some(), "the game didn't start");
    }

    /// Tick until the game is over, answering nothing.
    async fn play_out(actor: &mut SessionActor) {
        for _ in 0..200 {
            if actor.game.is_none() {
                return;
            }
            actor.tick().await;
        }
        panic!("the game never ended");
    }

    #[tokio::test]
    async fn profile_changes_apply_to_your_own_seat_only() {
        let mut s = session();
        let mut alice = join(&mut s, "alice");
        let bob = join(&mut s, "bob");

        say(&mut s, &bob, ClientMsg::UpdateProfile(info("robert")));
        let lobby = alice.lobby().unwrap();
        let names: Vec<(&str, &str)> = lobby
            .players
            .iter()
            .map(|p| (p.id.as_str(), p.name.as_str()))
            .collect();
        assert_eq!(names, [("alice-id", "alice"), ("bob-id", "robert")]);

        // A blank or oversized profile changes nothing and tells nobody.
        for bad in ["  ", &"x".repeat(33)] {
            say(&mut s, &bob, ClientMsg::UpdateProfile(info(bad)));
        }
        assert!(alice.lobby().is_none());

        // A Join sent mid-session is only a profile change, whatever it says.
        say(
            &mut s,
            &bob,
            ClientMsg::Join {
                version: 0,
                session: "elsewhere".to_owned(),
                secret: "alice-secret".to_owned(),
                profile: info("bobby"),
            },
        );
        let lobby = alice.lobby().unwrap();
        assert_eq!(lobby.session, "test");
        assert_eq!(lobby.players[1].name, "bobby");
        assert_eq!(lobby.players[0].name, "alice");
    }

    #[tokio::test]
    async fn readying_up_can_be_taken_back() {
        let mut s = session();
        let mut alice = join(&mut s, "alice");
        say(&mut s, &alice, ClientMsg::SetReady(true));
        assert_eq!(alice.lobby().unwrap().ready, ["alice-id"]);
        say(&mut s, &alice, ClientMsg::SetReady(false));
        assert_eq!(alice.lobby().unwrap().ready, Vec::<String>::new());
    }

    #[tokio::test]
    async fn bots_fill_the_lobby_to_eight_and_no_further() {
        let mut s = session();
        let mut alice = join(&mut s, "alice");
        say(&mut s, &alice, ClientMsg::AddBot(BotDifficulty::Easy));
        say(&mut s, &alice, ClientMsg::AddBot(BotDifficulty::Medium));
        say(&mut s, &alice, ClientMsg::AddBot(BotDifficulty::Hard));
        let lobby = alice.lobby().unwrap();
        let names: Vec<&str> = lobby.players.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["alice", "cool guy", "dude", "lad"]);

        for _ in 0..10 {
            say(&mut s, &alice, ClientMsg::AddBot(BotDifficulty::Easy));
        }
        assert_eq!(alice.lobby().unwrap().players.len(), MAX_PLAYERS);

        // A ninth person can still come in, to watch.
        let mut carol = join(&mut s, "carol");
        let lobby = carol.lobby().unwrap();
        assert_eq!(lobby.players.len(), MAX_PLAYERS);
        assert!(!lobby.players.iter().any(|p| p.id == carol.id));
    }

    #[tokio::test]
    async fn kicking_removes_players_and_bots_between_games() {
        let mut s = session();
        let mut alice = join(&mut s, "alice");
        let bob = join(&mut s, "bob");
        say(&mut s, &alice, ClientMsg::AddBot(BotDifficulty::Easy));
        say(&mut s, &bob, ClientMsg::SetReady(true));
        // Bob's tab goes away, so he's holding a seat; kicking settles it.
        s.handle(Cmd::Disconnect {
            conn_id: bob.conn_id,
        });
        say(&mut s, &alice, ClientMsg::Kick("bot-1".to_owned()));
        say(&mut s, &alice, ClientMsg::Kick(bob.id.clone()));
        let lobby = alice.lobby().unwrap();
        assert_eq!(lobby.players.len(), 1);
        assert_eq!(lobby.ready, Vec::<String>::new());
        assert!(s.bots.is_empty());
        assert!(s.away.is_empty(), "the held seat outlived the kick");

        // Not during a game: nobody can be taken out of one, or added.
        say(&mut s, &alice, ClientMsg::AddBot(BotDifficulty::Easy));
        let carol = join(&mut s, "carol");
        start(&mut s, &[&alice, &carol]);
        alice.msgs();
        say(&mut s, &alice, ClientMsg::Kick(carol.id.clone()));
        say(&mut s, &alice, ClientMsg::AddBot(BotDifficulty::Hard));
        assert_eq!(s.lobby.len(), 3);
        assert!(alice.lobby().is_none());
    }

    #[tokio::test]
    async fn a_game_starts_only_when_every_human_is_ready() {
        let mut s = session();
        let mut alice = join(&mut s, "alice");
        let bob = join(&mut s, "bob");
        say(&mut s, &alice, ClientMsg::AddBot(BotDifficulty::Easy));
        say(&mut s, &alice, ClientMsg::SetReady(true));
        say(&mut s, &alice, ClientMsg::Start);
        assert!(s.game.is_none(), "started with bob not ready");

        say(&mut s, &bob, ClientMsg::SetReady(true));
        say(&mut s, &alice, ClientMsg::Start);
        let started = alice.msgs().into_iter().find_map(|m| match m {
            ServerMsg::GameStart {
                players,
                turn,
                countdown_ms,
                ..
            } => Some((players, turn, countdown_ms)),
            _ => None,
        });
        let (players, turn, countdown_ms) = started.expect("no game start");
        assert_eq!(players.len(), 3, "bots play too");
        assert_eq!(turn.alive.len(), 3);
        assert_eq!(countdown_ms, COUNTDOWN_MS);

        // Starting again does nothing; a late joiner is caught up.
        say(&mut s, &bob, ClientMsg::Start);
        assert!(
            !alice
                .msgs()
                .iter()
                .any(|m| matches!(m, ServerMsg::GameStart { .. }))
        );
        let mut carol = join(&mut s, "carol");
        assert!(
            carol.msgs().iter().any(
                |m| matches!(m, ServerMsg::GameInProgress { players, .. } if players.len() == 3)
            ),
            "a late joiner wasn't caught up"
        );
    }

    #[tokio::test]
    async fn nobody_can_remove_themselves() {
        let mut s = session();
        let alice = join(&mut s, "alice");
        say(&mut s, &alice, ClientMsg::Kick(alice.id.clone()));
        assert_eq!(s.lobby.len(), 1);
        assert!(s.conns.contains_key(&alice.conn_id));
    }

    #[tokio::test]
    async fn whoever_is_removed_is_told_by_whom_and_let_go() {
        let mut s = session();
        let alice = join(&mut s, "alice");
        let mut bob = join(&mut s, "bob");
        bob.msgs();
        say(&mut s, &alice, ClientMsg::Kick(bob.id.clone()));
        let told = bob.msgs().into_iter().find_map(|m| match m {
            ServerMsg::Kicked(by) => Some(by),
            _ => None,
        });
        assert_eq!(told.flatten().map(|p| p.name), Some("alice".to_owned()));
        assert!(
            !s.conns.contains_key(&bob.conn_id),
            "bob's connection was kept"
        );
        assert_eq!(s.lobby.len(), 1);
    }

    #[tokio::test]
    async fn settings_change_only_between_games_and_only_when_valid() {
        let mut s = session();
        let mut alice = join(&mut s, "alice");

        let config = GameConfig {
            food_emoji: "🦀".to_owned(),
            ..GameConfig::default()
        };
        say(&mut s, &alice, ClientMsg::Configure(config.clone()));
        assert_eq!(alice.lobby().unwrap().config, config);

        // Settings that aren't valid are ignored.
        let silly = GameConfig {
            board_width: 200,
            ..GameConfig::default()
        };
        say(&mut s, &alice, ClientMsg::Configure(silly));
        assert!(alice.lobby().is_none());

        // Nor do they change mid-game.
        start(&mut s, &[&alice]);
        alice.msgs();
        say(&mut s, &alice, ClientMsg::Configure(GameConfig::default()));
        assert!(alice.lobby().is_none());
        assert_eq!(s.config, config);
    }

    #[tokio::test]
    async fn leaving_takes_you_out_at_once() {
        let mut s = session();
        let mut alice = join(&mut s, "alice");
        let mut bob = join(&mut s, "bob");
        say(&mut s, &bob, ClientMsg::SetReady(true));
        alice.msgs();
        bob.msgs();
        say(&mut s, &bob, ClientMsg::Leave);
        assert!(bob.msgs().contains(&ServerMsg::Left), "bob wasn't told");
        let lobby = alice.lobby().unwrap();
        assert_eq!(lobby.players.len(), 1);
        assert_eq!(lobby.ready, Vec::<String>::new());
        // The transport closing afterwards holds no seat.
        s.handle(Cmd::Disconnect {
            conn_id: bob.conn_id,
        });
        assert!(s.away.is_empty(), "a seat was held for someone who left");

        // The last person out ends the game there and then, bots or no.
        say(&mut s, &alice, ClientMsg::AddBot(BotDifficulty::Easy));
        say(&mut s, &alice, ClientMsg::Leave);
        assert!(!s.has_people());
        assert!(s.empty_deadline.is_some_and(|t| t <= Instant::now()));
    }

    /// Run for real, the last person leaving takes the game off the list
    /// at once.
    #[tokio::test(start_paused = true)]
    async fn the_last_person_leaving_deletes_the_game() {
        let registry = Registry::new(Arc::new(Db::open(&temp_path("solo.db")).unwrap()));
        let mut watch = registry.watch_games();
        let (tx, mut rx) = conn_channel();
        let actor = registry
            .connect("solo", 1, profile("alice-id", "alice"), String::new(), tx)
            .await;
        watch.changed().await.unwrap();
        assert_eq!(watch.borrow_and_update().len(), 1);
        actor
            .send(Cmd::Msg {
                conn_id: 1,
                msg: ClientMsg::Leave,
            })
            .await
            .unwrap();
        while let Some(frame) = rx.recv().await {
            if let Frame::Control(bytes) = frame
                && decode::<ServerMsg>(&bytes) == Some(ServerMsg::Left)
            {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert!(
            registry.active_sessions().is_empty(),
            "the game outlived its last player"
        );
    }

    #[tokio::test]
    async fn anyone_can_rename_the_game_between_games() {
        let mut s = session();
        let mut alice = join(&mut s, "alice");
        let bob = join(&mut s, "bob");
        alice.msgs();
        assert_eq!(
            s.title.split(' ').count(),
            3,
            "a new game is named three words: {}",
            s.title
        );

        say(
            &mut s,
            &bob,
            ClientMsg::Rename("  the big one  ".to_owned()),
        );
        let lobby = alice.lobby().unwrap();
        assert_eq!(lobby.name, "the big one", "the name wasn't trimmed");
        assert_eq!(lobby.config_changed_by, Some(bob.id.clone()));

        // Nothing to read, too long, or the same again: ignored.
        say(&mut s, &bob, ClientMsg::Rename("   ".to_owned()));
        say(&mut s, &bob, ClientMsg::Rename("x".repeat(41)));
        say(&mut s, &bob, ClientMsg::Rename("the big one".to_owned()));
        assert!(alice.lobby().is_none());

        // Nor mid-game.
        start(&mut s, &[&alice, &bob]);
        say(&mut s, &bob, ClientMsg::Rename("sneaky".to_owned()));
        assert_eq!(s.title, "the big one");
    }

    #[tokio::test]
    async fn a_stream_ping_is_answered_on_the_stream() {
        let mut s = session();
        let mut alice = join(&mut s, "alice");
        alice.msgs();
        say(&mut s, &alice, ClientMsg::Ping(9));
        assert!(matches!(alice.frames()[..], [(true, ServerMsg::Pong(9))]));
        // The home screen's subscription means nothing inside a session.
        say(&mut s, &alice, ClientMsg::WatchGames);
        assert_eq!(alice.frames(), Vec::<(bool, ServerMsg)>::new());
    }

    /// Messages from a connection the session no longer has (dropped as
    /// slow, say) are ignored, not taken as some other player's.
    #[tokio::test]
    async fn an_unknown_connection_is_ignored() {
        let mut s = session();
        let mut alice = join(&mut s, "alice");
        alice.msgs();
        s.handle(Cmd::Msg {
            conn_id: 999,
            msg: ClientMsg::SetReady(true),
        });
        s.handle(Cmd::Alive { conn_id: 999 });
        s.handle(Cmd::Disconnect { conn_id: 999 });
        let lobby = alice.lobby().unwrap();
        assert_eq!(lobby.ready, Vec::<String>::new());
        assert_eq!(lobby.players.len(), 1);
    }

    #[tokio::test]
    async fn a_client_that_stops_reading_is_dropped() {
        let mut s = session();
        let mut alice = join(&mut s, "alice");
        // Bob's buffer holds one frame, which his welcome lobby fills.
        let conn_id = s.registry.new_conn_id();
        let (tx, _bob_rx) = mpsc::channel(1);
        s.handle(Cmd::Connect {
            conn_id,
            profile: profile("bob-id", "bob"),
            ip: "10.0.0.2".to_owned(),
            tx,
        });
        assert!(s.conns.contains_key(&conn_id));
        say(&mut s, &alice, ClientMsg::SetReady(true));
        assert!(!s.conns.contains_key(&conn_id), "a stuck client was kept");
        // His seat is untouched: he reconnects into it.
        assert_eq!(alice.lobby().unwrap().players.len(), 2);

        // Same for a reply meant for him alone.
        let (tx, _carol_rx) = mpsc::channel(1);
        let carol = s.registry.new_conn_id();
        s.handle(Cmd::Connect {
            conn_id: carol,
            profile: profile("carol-id", "carol"),
            ip: "10.0.0.3".to_owned(),
            tx,
        });
        s.handle(Cmd::Msg {
            conn_id: carol,
            msg: ClientMsg::Ping(1),
        });
        assert!(!s.conns.contains_key(&carol));
    }

    #[tokio::test]
    async fn reconnects_and_compute_failures_are_logged() {
        let logs = Logs::capture();
        let mut s = session();
        let alice = join(&mut s, "alice");
        say(
            &mut s,
            &alice,
            ClientMsg::Connected(ConnectReport {
                reconnect: true,
                attempts: 3,
                silent_ms: Some(4_000),
                since_visible_ms: Some(50),
                page_ms: 90_000,
                handshake_ms: 1_234,
                join_ms: 20,
                trail: vec!["open".to_owned(), "x".repeat(500)],
            }),
        );
        assert!(logs.has(&[
            "connect report",
            "attempts=3",
            "handshake_ms=1234",
            "open | xxx"
        ]));
        assert!(
            !logs.has(&[&"x".repeat(121)]),
            "a trail event wasn't cut short"
        );

        let mut alice = alice;
        benchmark(&mut s, &alice, 8);
        say(&mut s, &alice, ClientMsg::AddBot(BotDifficulty::Easy));
        start(&mut s, &[&alice]);
        let request = alice.requests().pop().expect("no compute request");
        say(
            &mut s,
            &alice,
            ClientMsg::ComputeFailed {
                request_id: request.id,
                reason: "worker crashed".to_owned(),
            },
        );
        assert!(logs.has(&["could not compute", "bot=bot-1", "worker crashed"]));
    }

    #[tokio::test]
    async fn a_hold_stops_the_clock_until_it_is_lifted() {
        let mut s = session();
        let alice = join(&mut s, "alice");
        let mut bob = join(&mut s, "bob");
        // No game, nothing to hold.
        say(&mut s, &alice, ClientMsg::SetPaused(true));
        assert!(!bob.msgs().iter().any(|m| matches!(m, ServerMsg::Hold(_))));

        start(&mut s, &[&alice, &bob]);
        bob.msgs();
        say(&mut s, &alice, ClientMsg::SetPaused(true));
        // Already held: a second pause is nothing.
        say(&mut s, &bob, ClientMsg::SetPaused(true));
        let when_paused = bob.msgs();
        let [ServerMsg::Hold(paused)] = &when_paused[..] else {
            panic!("{when_paused:?}");
        };
        assert_eq!(paused.by.as_deref(), Some(alice.id.as_str()));
        assert_eq!(paused.pauses_left[&alice.id], PAUSES_PER_GAME - 1);
        assert_eq!(
            paused.pauses_left[&bob.id], PAUSES_PER_GAME,
            "bob's was spent"
        );
        assert_eq!(s.next_tick, None);
        // Nothing a player does in the meantime restarts it, bob's resume
        // included: it's alice's to lift.
        say(&mut s, &bob, ClientMsg::HostAvailable { can_compute: true });
        s.handle(Cmd::Alive {
            conn_id: bob.conn_id,
        });
        say(&mut s, &bob, ClientMsg::SetPaused(false));
        assert_eq!(s.next_tick, None);
        assert!(s.paused_by.is_some());

        // Alice lifts it: everyone gets the countdown, then play.
        say(&mut s, &alice, ClientMsg::SetPaused(false));
        let resumed = bob.msgs();
        let [ServerMsg::Hold(lifted)] = &resumed[..] else {
            panic!("{resumed:?}");
        };
        assert_eq!(lifted.by, None);
        assert_eq!(lifted.resume_in_ms, Some(RESUME_MS));
        let due = s.next_tick.expect("not resumed");
        assert!(due > Instant::now() + Duration::from_millis(u64::from(RESUME_MS) - 100));
    }

    #[tokio::test]
    async fn three_pauses_each_and_nobody_is_held_hostage() {
        let mut s = session();
        let alice = join(&mut s, "alice");
        let bob = join(&mut s, "bob");
        start(&mut s, &[&alice, &bob]);
        for _ in 0..PAUSES_PER_GAME {
            say(&mut s, &alice, ClientMsg::SetPaused(true));
            assert!(s.paused_by.is_some());
            say(&mut s, &alice, ClientMsg::SetPaused(false));
        }
        say(&mut s, &alice, ClientMsg::SetPaused(true));
        assert_eq!(s.paused_by, None, "a fourth pause");

        // Bob pauses, then goes: alice can lift it.
        say(&mut s, &bob, ClientMsg::SetPaused(true));
        s.handle(Cmd::Disconnect {
            conn_id: bob.conn_id,
        });
        say(&mut s, &alice, ClientMsg::SetPaused(false));
        assert_eq!(s.paused_by, None, "stuck held by someone who left");
    }

    #[tokio::test]
    async fn moves_steer_the_mover_only_while_alive() {
        let mut s = session();
        let alice = join(&mut s, "alice");
        let bob = join(&mut s, "bob");
        start(&mut s, &[&alice, &bob]);
        // Carol arrives mid-game: she watches, steering nothing.
        let mut carol = join(&mut s, "carol");
        assert!(
            !s.game
                .as_ref()
                .unwrap()
                .players
                .iter()
                .any(|p| p.id == carol.id)
        );
        let heading = |s: &SessionActor, id: &str| {
            let turn = s.game.as_ref().unwrap().turns.last().unwrap().clone();
            snek_shared::clockwise_start_dir(&s.config, turn.snakes[id][0])
        };
        let alice_dir = heading(&s, &alice.id);
        say(
            &mut s,
            &alice,
            ClientMsg::Move {
                dir: alice_dir,
                after: 0,
            },
        );
        say(
            &mut s,
            &carol,
            ClientMsg::Move {
                dir: Dir::Up,
                after: 0,
            },
        );
        s.tick().await;
        let turn = carol
            .msgs()
            .into_iter()
            .find_map(|m| match m {
                ServerMsg::Turn(t) => Some(t),
                _ => None,
            })
            .expect("no turn snapshot");
        assert_eq!(turn.turn_number, 2);
        assert_eq!(turn.moves.get(&alice.id), Some(&alice_dir));
        assert!(!turn.moves.contains_key(&carol.id));
    }

    #[tokio::test]
    async fn a_finished_game_is_saved_and_the_lobby_comes_back() {
        let mut s = session();
        let mut alice = join(&mut s, "alice");
        let bob = join(&mut s, "bob");
        start(&mut s, &[&alice, &bob]);
        play_out(&mut s).await;

        let msgs = alice.msgs();
        let id = msgs
            .iter()
            .find_map(|m| match m {
                ServerMsg::GameSaved { id, compute } => {
                    assert!(compute.is_empty(), "no bots, nothing computed");
                    Some(*id)
                }
                _ => None,
            })
            .expect("game not saved");
        let ServerMsg::Lobby(lobby) = msgs.last().unwrap() else {
            panic!("the lobby didn't come back last: {msgs:?}");
        };
        assert!(!lobby.game_running);
        assert_eq!(lobby.last_game_id, Some(id));
        assert_eq!(lobby.last_winners.len(), 2);
        assert_eq!(
            lobby.last_players.len(),
            2,
            "the lobby forgot who played the last game"
        );
        assert!(lobby.ready.is_empty(), "a game clears everyone's ready");

        let meta = s.db.get_game_meta(id).unwrap().unwrap();
        assert_eq!(meta.players.len(), 2);
        assert_eq!(meta.winners, lobby.last_winners);
    }

    #[tokio::test]
    async fn a_game_that_cannot_be_saved_still_ends() {
        let logs = Logs::capture();
        let path = temp_path("unsaveable.db");
        let db = Arc::new(Db::open(&path).unwrap());
        let mut s = SessionActor::new("test".to_owned(), Registry::new(Arc::clone(&db)), db);
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch("DROP TABLE games;")
            .unwrap();
        let mut alice = join(&mut s, "alice");
        start(&mut s, &[&alice]);
        play_out(&mut s).await;

        let msgs = alice.msgs();
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, ServerMsg::GameSaved { .. }))
        );
        let lobby = lobby_in(&msgs);
        assert!(!lobby.game_running);
        assert_eq!(lobby.last_game_id, None);
        assert!(logs.has(&["failed to save game", "no such table"]));
    }

    fn lobby_in(msgs: &[ServerMsg]) -> LobbyState {
        msgs.iter()
            .rev()
            .find_map(|m| match m {
                ServerMsg::Lobby(l) => Some(l.clone()),
                _ => None,
            })
            .expect("no lobby")
    }

    /// The bots' moves come from players' devices: a request out to the
    /// host, both ways, and its answer steers the bot; the device's work
    /// is credited to it when the game is saved.
    #[tokio::test]
    async fn a_hosted_bot_is_steered_by_its_hosts_answers() {
        let mut s = session();
        let mut alice = join(&mut s, "alice");
        let mut bob = join(&mut s, "bob");
        benchmark(&mut s, &alice, 8);
        say(&mut s, &alice, ClientMsg::AddBot(BotDifficulty::Medium));
        start(&mut s, &[&alice, &bob]);

        let frames = alice.frames();
        let copies: Vec<bool> = frames
            .iter()
            .filter(|(_, m)| matches!(m, ServerMsg::ComputeRequest { .. }))
            .map(|(control, _)| *control)
            .collect();
        assert_eq!(copies, [false, true], "datagram first, then the stream");
        assert!(bob.requests().is_empty(), "bob never benchmarked");

        let mut answered = 0;
        while s.game.is_some() {
            let requests = alice.requests();
            for r in &requests {
                assert_eq!(r.bot, "bot-1");
                say(&mut s, &alice, r.answer());
                let game = s.game.as_ref().unwrap();
                assert!(game.pending.contains_key("bot-1"), "the answer was dropped");
            }
            answered += requests.len();
            s.tick().await;
        }
        assert!(answered > 0);
        let saved = bob
            .msgs()
            .into_iter()
            .find_map(|m| match m {
                ServerMsg::GameSaved { compute, .. } => Some(compute),
                _ => None,
            })
            .expect("not saved");
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].name, "🐍 alice");
        assert_eq!(saved[0].device, "test phone");
        assert_eq!(saved[0].moves, u32::try_from(answered).unwrap());
    }

    /// Bots on one device think in parallel, a worker each, until they
    /// outnumber its threads; then they share the time.
    #[tokio::test]
    async fn bots_share_the_window_when_they_outnumber_threads() {
        fn windows(cores: u32) -> Vec<u32> {
            let mut s = session();
            let mut alice = join(&mut s, "alice");
            benchmark(&mut s, &alice, cores);
            say(&mut s, &alice, ClientMsg::AddBot(BotDifficulty::Hard));
            say(&mut s, &alice, ClientMsg::AddBot(BotDifficulty::Hard));
            start(&mut s, &[&alice]);
            alice.requests().iter().map(|r| r.think_ms).collect()
        }
        // Half of the default 1s tick each, with threads to spare.
        assert_eq!(windows(8), [500, 500]);
        // Two cores leave one thread for the two of them.
        assert_eq!(windows(2), [250, 250]);
    }

    /// A bad checksum or an answer to someone else's request is a strike;
    /// three and the device stops hosting, until the next game.
    #[tokio::test]
    async fn misbehaving_hosts_are_struck_off() {
        let logs = Logs::capture();
        let mut s = session();
        let mut alice = join(&mut s, "alice");
        let bob = join(&mut s, "bob");
        benchmark(&mut s, &alice, 8);
        for _ in 0..4 {
            say(&mut s, &alice, ClientMsg::AddBot(BotDifficulty::Easy));
        }
        start(&mut s, &[&alice, &bob]);

        let mut requests = alice.requests();
        let request = requests.remove(0);
        let bot = request.bot.clone();
        // Bob answers alice's request.
        say(&mut s, &bob, request.answer());
        assert!(logs.has(&["strike", "answered another host's request"]));
        assert!(!s.game.as_ref().unwrap().pending.contains_key(&bot));

        // Alice gets the checksum wrong on the other three.
        for request in requests {
            say(&mut s, &alice, request.answer_with(Dir::Up, 12345));
        }
        assert!(logs.has(&["bad response checksum", "strikes=3"]));
        assert!(s.bot_hosts.is_empty(), "a struck-off device still hosts");
        assert!(s.game.as_ref().unwrap().pending.is_empty());
        // A bot without a host freezes the game rather than playing blind.
        let before = s.game.as_ref().unwrap().turns.len();
        s.tick().await;
        assert_eq!(s.game.as_ref().unwrap().turns.len(), before);
        assert_eq!(s.next_tick, None);
    }

    /// An answer that misses its tick is still recognised: it shrinks the
    /// host's thinking window, once per request (answers come twice), and
    /// answering in time earns it back.
    #[tokio::test]
    async fn lateness_shrinks_the_window_and_punctuality_restores_it() {
        let mut s = session();
        let mut alice = join(&mut s, "alice");
        let bob = join(&mut s, "bob");
        benchmark(&mut s, &alice, 8);
        say(&mut s, &alice, ClientMsg::AddBot(BotDifficulty::Hard));
        start(&mut s, &[&alice, &bob]);

        let first = alice.requests().pop().unwrap();
        assert_eq!(first.think_ms, 500);
        s.tick().await;
        let second = alice.requests().pop().unwrap();
        // The first answer turns up a tick late, twice; and bob replays it.
        say(&mut s, &alice, first.answer());
        say(&mut s, &alice, first.answer());
        say(&mut s, &bob, first.answer());
        say(&mut s, &alice, second.answer());
        s.tick().await;
        // Shares of half the tick, which shortens a little every turn.
        let share = |s: &SessionActor, percent: u32| s.tick_ms_now() * percent / 200;
        let third = alice.requests().pop().unwrap();
        assert!(
            third.think_ms.abs_diff(share(&s, 75)) <= 1,
            "0.7 for the late one, +0.05 for the next: {}",
            third.think_ms
        );
        say(&mut s, &alice, third.answer());
        s.tick().await;
        let fourth = alice.requests().pop().unwrap();
        assert!(
            fourth.think_ms.abs_diff(share(&s, 80)) <= 1,
            "{}",
            fourth.think_ms
        );
    }

    /// A device that goes quiet without saying so is presumed asleep after
    /// two missed ticks, and the first sign of life brings it back.
    #[tokio::test]
    async fn a_silent_host_is_presumed_asleep_until_it_speaks() {
        let logs = Logs::capture();
        let mut s = session();
        let mut alice = join(&mut s, "alice");
        let bob = join(&mut s, "bob");
        benchmark(&mut s, &alice, 8);
        say(&mut s, &alice, ClientMsg::AddBot(BotDifficulty::Easy));
        start(&mut s, &[&alice, &bob]);
        s.tick().await;
        s.tick().await;
        assert!(logs.has(&["no answer before the tick", "bot=bot-1"]));
        assert!(logs.has(&["silent for two ticks"]));
        assert!(s.bot_hosts.is_empty());
        // The tick that wrote it off still went ahead; the next one doesn't.
        let turns = s.game.as_ref().unwrap().turns.len();
        s.tick().await;
        assert_eq!(s.game.as_ref().unwrap().turns.len(), turns);
        assert_eq!(s.next_tick, None, "ticking on with nobody thinking");

        alice.msgs();
        s.handle(Cmd::Alive {
            conn_id: alice.conn_id,
        });
        assert!(logs.has(&["awake again"]));
        assert!(s.next_tick.is_some(), "the game didn't resume");
        s.tick().await;
        assert_eq!(alice.requests().len(), 1);
    }

    /// A device that says its page is hidden stops being asked; saying it's
    /// back restarts the game it was holding up.
    #[tokio::test]
    async fn a_hidden_page_stops_hosting_until_it_returns() {
        let mut s = session();
        let mut alice = join(&mut s, "alice");
        benchmark(&mut s, &alice, 8);
        say(&mut s, &alice, ClientMsg::AddBot(BotDifficulty::Easy));
        start(&mut s, &[&alice]);
        say(
            &mut s,
            &alice,
            ClientMsg::HostAvailable { can_compute: false },
        );
        alice.msgs();
        s.tick().await;
        assert!(alice.msgs().is_empty(), "ticked with the only host hidden");
        // Keepalives from a hidden page don't count as it being back.
        s.handle(Cmd::Alive {
            conn_id: alice.conn_id,
        });
        assert_eq!(s.next_tick, None);
        say(
            &mut s,
            &alice,
            ClientMsg::HostAvailable { can_compute: true },
        );
        assert!(s.next_tick.is_some());
    }

    /// A cerebral turn waits for everyone's move and goes the moment the
    /// last is in; the bots get a second, and a move for a turn already
    /// gone doesn't count towards the next.
    #[tokio::test]
    async fn a_cerebral_turn_goes_when_everyone_has_moved() {
        let mut s = session();
        let mut alice = join(&mut s, "alice");
        let bob = join(&mut s, "bob");
        benchmark(&mut s, &alice, 8);
        say(&mut s, &alice, ClientMsg::AddBot(BotDifficulty::Hard));
        let cerebral = GameConfig {
            cerebral: true,
            ..GameConfig::default()
        };
        say(&mut s, &alice, ClientMsg::Configure(cerebral));
        start(&mut s, &[&alice, &bob]);
        let shown = |s: &SessionActor| s.game.as_ref().unwrap().turns.last().unwrap().turn_number;
        let mv = |after| ClientMsg::Move {
            dir: Dir::Up,
            after,
        };
        let wait = |s: &SessionActor| {
            s.next_tick
                .unwrap()
                .saturating_duration_since(Instant::now())
        };

        // Moves during the countdown don't cut it short.
        let first = shown(&s);
        for p in [&alice, &bob] {
            say(&mut s, p, mv(first));
        }
        assert_eq!(s.next_tick, Some(s.game.as_ref().unwrap().opened));
        s.tick().await;

        // The next turn: five seconds on the clock, and the bot asked to
        // think for most of its second. (The last request is this turn's;
        // the first was the countdown's.)
        let now = shown(&s);
        assert!(wait(&s) > Duration::from_millis(4_500));
        let request = alice.requests().pop().unwrap();
        assert_eq!(request.turn.turn_number, now);
        assert!(request.think_ms <= CEREBRAL_BOT_MS, "{}", request.think_ms);

        // A repeat of last turn's move, arriving late, isn't this turn's.
        say(&mut s, &alice, mv(first));
        assert!(s.game.as_ref().unwrap().pending.is_empty());
        say(&mut s, &alice, mv(now));
        assert!(wait(&s) > Duration::from_millis(4_500), "bob hasn't moved");
        say(&mut s, &bob, mv(now));
        // People done, the bot not: it gets the rest of its second.
        let w = wait(&s);
        assert!(
            w <= Duration::from_millis(u64::from(CEREBRAL_BOT_MS)),
            "{w:?}"
        );
        assert!(w > Duration::from_millis(500), "{w:?}");
        say(&mut s, &alice, request.answer());
        assert_eq!(wait(&s), Duration::ZERO, "everyone's in: go now");
    }

    /// A timed game keeps its beat however quickly everyone moves.
    #[tokio::test]
    async fn a_timed_turn_keeps_its_beat() {
        let mut s = session();
        let alice = join(&mut s, "alice");
        start(&mut s, &[&alice]);
        s.tick().await;
        let due = s.next_tick;
        say(
            &mut s,
            &alice,
            ClientMsg::Move {
                dir: Dir::Up,
                after: 1,
            },
        );
        assert_eq!(s.next_tick, due);
    }

    /// With nobody watching, the game waits; the first one back restarts it.
    #[tokio::test]
    async fn a_game_nobody_is_watching_waits_for_someone() {
        let mut s = session();
        let alice = join(&mut s, "alice");
        let bob = join(&mut s, "bob");
        start(&mut s, &[&alice, &bob]);
        for p in [&alice, &bob] {
            s.handle(Cmd::Disconnect { conn_id: p.conn_id });
        }
        s.tick().await;
        assert_eq!(s.game.as_ref().unwrap().turns.len(), 1);
        assert_eq!(s.next_tick, None);
        assert!(s.empty_deadline.is_some());

        let mut alice = join_as(&mut s, "alice-id", "alice");
        assert!(s.next_tick.is_some());
        assert_eq!(s.empty_deadline, None);
        assert!(s.away.contains_key("bob-id") && !s.away.contains_key("alice-id"));
        assert!(
            alice
                .msgs()
                .iter()
                .any(|m| matches!(m, ServerMsg::GameInProgress { .. }))
        );
    }

    /// The beat is kept from when each tick was due. One that falls more
    /// than a period behind skips ahead instead of firing catch-up ticks.
    #[tokio::test]
    async fn a_stalled_clock_skips_ahead_rather_than_catching_up() {
        let mut s = session();
        let alice = join(&mut s, "alice");
        let bob = join(&mut s, "bob");
        start(&mut s, &[&alice, &bob]);
        let due = Instant::now();
        s.next_tick = Some(due);
        s.tick().await;
        // At the speed the game has reached (see `GameConfig::tick_ms_at`).
        let period = Duration::from_millis(u64::from(s.tick_ms_now()));
        assert_eq!(s.next_tick, Some(due + period), "on the beat");

        let long_ago = Instant::now().checked_sub(period * 5).unwrap();
        s.next_tick = Some(long_ago);
        s.tick().await;
        let next = s.next_tick.unwrap();
        assert!(next > Instant::now(), "a catch-up tick was scheduled");
        assert!(next <= Instant::now() + period);
    }

    /// Someone hosting bots without a seat (the lobby was full) is
    /// credited for their work all the same.
    #[tokio::test]
    async fn a_spectating_host_is_credited_too() {
        let mut s = session();
        let alice = join(&mut s, "alice");
        for _ in 0..7 {
            say(&mut s, &alice, ClientMsg::AddBot(BotDifficulty::Easy));
        }
        let mut carol = join(&mut s, "carol");
        benchmark(&mut s, &carol, 64);
        start(&mut s, &[&alice]);
        while s.game.is_some() {
            for r in carol.requests() {
                say(&mut s, &carol, r.answer());
            }
            s.tick().await;
        }
        let compute = carol
            .msgs()
            .into_iter()
            .find_map(|m| match m {
                ServerMsg::GameSaved { compute, .. } => Some(compute),
                _ => None,
            })
            .expect("not saved");
        assert_eq!(compute[0].name, "departed");
        assert!(compute[0].moves > 0);
    }

    /// The registry's view: one actor per session, listed while someone is
    /// in it, a fresh one for a session whose actor has just gone.
    #[tokio::test]
    async fn the_registry_lists_sessions_with_someone_in_them() {
        let db = Arc::new(Db::open(&temp_path("registry.db")).unwrap());
        let registry = Registry::new(db);
        let mut watch = registry.watch_games();
        assert_eq!(registry.active_sessions(), Vec::<SessionSummary>::new());

        // An actor that has just exited, still in the map.
        let (dead, _) = mpsc::channel(1);
        lock(&registry.inner).insert("zed".to_owned(), dead);

        let (tx, mut rx) = conn_channel();
        let actor = registry
            .connect("zed", 1, profile("z-id", "zoe"), "ip".to_owned(), tx)
            .await;
        actor
            .send(Cmd::Msg {
                conn_id: 1,
                msg: ClientMsg::Ping(5),
            })
            .await
            .unwrap();
        let mut ponged = false;
        while let Some(Frame::Control(bytes)) = rx.recv().await {
            if matches!(decode(&bytes), Some(ServerMsg::Pong(5))) {
                ponged = true;
                break;
            }
        }
        assert!(ponged, "the replacement session isn't answering");

        watch.changed().await.unwrap();
        let listed = watch.borrow_and_update().clone();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "zed");
        assert_eq!(listed[0].emojis.len(), 1, "zoe isn't listed in it");
        assert_eq!(registry.active_sessions(), listed);
    }

    /// A dropped connection keeps its seat for a while, then gives it up;
    /// a session left empty is tidied away a minute later.
    #[tokio::test(start_paused = true)]
    async fn seats_and_sessions_outlast_a_dropped_connection_for_a_while() {
        let db = Arc::new(Db::open(&temp_path("grace.db")).unwrap());
        let registry = Registry::new(db);
        let connect = |id: &str, conn_id| {
            let (tx, rx) = conn_channel();
            let registry = registry.clone();
            let profile = profile(&format!("{id}-id"), id);
            async move {
                let actor = registry
                    .connect("grace", conn_id, profile, "ip".to_owned(), tx)
                    .await;
                (actor, rx)
            }
        };
        let (actor, _alice_rx) = connect("alice", 1).await;
        let (_, mut bob_rx) = connect("bob", 2).await;
        actor
            .send(Cmd::Msg {
                conn_id: 1,
                msg: ClientMsg::SetReady(true),
            })
            .await
            .unwrap();
        actor.send(Cmd::Disconnect { conn_id: 1 }).await.unwrap();

        let latest_lobby = |rx: &mut mpsc::Receiver<Frame>| {
            let mut last = None;
            while let Ok(Frame::Control(bytes)) = rx.try_recv() {
                if let Some(ServerMsg::Lobby(l)) = decode(&bytes) {
                    last = Some(l);
                }
            }
            last
        };
        tokio::time::sleep(Duration::from_secs(44)).await;
        let lobby = latest_lobby(&mut bob_rx).unwrap();
        assert_eq!(lobby.players.len(), 2, "alice's seat went early");
        assert_eq!(lobby.ready, ["alice-id"]);

        tokio::time::sleep(Duration::from_secs(2)).await;
        let lobby = latest_lobby(&mut bob_rx).expect("nobody was told");
        assert_eq!(lobby.players.len(), 1, "alice's seat was never released");
        assert_eq!(lobby.ready, Vec::<String>::new());

        // Bob goes too: the session lingers a minute, then it's gone.
        actor.send(Cmd::Disconnect { conn_id: 2 }).await.unwrap();
        tokio::time::sleep(Duration::from_secs(59)).await;
        assert!(lock(&registry.inner).contains_key("grace"));
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!lock(&registry.inner).contains_key("grace"));
        assert_eq!(registry.active_sessions(), Vec::<SessionSummary>::new());

        // Coming back now is a fresh lobby.
        let (_, mut rx) = connect("alice", 3).await;
        tokio::task::yield_now().await;
        let lobby = latest_lobby(&mut rx).unwrap();
        assert_eq!(lobby.players.len(), 1);
        assert_eq!(lobby.ready, Vec::<String>::new());
    }
}
