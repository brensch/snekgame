//! The wire protocol: postcard-encoded messages over WebTransport (QUIC).
//! Both sides share these exact types, so there is no schema drift and no
//! JSON overhead.
//!
//! Transport mapping:
//! - **Datagrams** (unreliable, unordered, single-packet): `Turn` snapshots
//!   going down; `Move` and `Ping` going up. Every snapshot is full state
//!   and sequence-numbered by turn, so loss and reordering are harmless —
//!   the newest one always heals the picture. Senders repeat datagrams for
//!   redundancy; receivers dedup.
//! - **One bidirectional control stream** (reliable, ordered): everything
//!   else — join, lobby traffic, game start/end, full-state catch-ups.
//!   Frames on the stream are u32-le length-prefixed postcard.

use crate::types::*;
use serde::{Deserialize, Serialize};

/// Bump when messages change incompatibly; the server rejects mismatches so
/// stale cached frontends reload instead of misdecoding frames.
pub const PROTOCOL_VERSION: u16 = 17;

/// The client-editable part of a profile. The public player id is derived
/// server-side (a hash of the client's private secret), so a client can never
/// claim someone else's identity — and moves are bound to the connection's
/// server-side identity, never to anything in the message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileInfo {
    pub name: String,
    pub emoji: String,
    pub colour: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// First message on every (re)connection. `secret` never leaves the
    /// server; everyone else only ever sees the derived public id.
    Join {
        version: u16,
        session: String,
        secret: String,
        profile: ProfileInfo,
    },
    UpdateProfile(ProfileInfo),
    SetReady(bool),
    Configure(GameConfig),
    Start,
    Move(Dir),
    /// Add a server-hosted bot to the lobby.
    AddBot(BotDifficulty),
    /// Remove a player (or bot) from the lobby (anyone can kick, like tac).
    Kick(PlayerId),
    Ping(u32),
    /// Hold the game where it is, or let it run on again.
    SetPaused(bool),
    /// Whether this device can currently compute. A backgrounded page or
    /// a locked screen suspends its workers, so it must say so — otherwise
    /// its bots quietly hold their heading into a wall.
    HostAvailable {
        can_compute: bool,
    },
    /// This device could not produce a move for a compute request. Sent so
    /// the failure is visible server-side instead of looking identical to
    /// a silent client.
    ComputeFailed {
        request_id: u64,
        reason: String,
    },
    /// Alternative first message: subscribe to the active-games list
    /// instead of joining a session (used by the home screen).
    WatchGames,
    /// Self-measured search speed (sims/sec), from ~600ms of real search in
    /// the lobby. Makes this connection eligible to host bot compute; the
    /// fastest phones get picked first.
    Benchmark {
        sims_per_sec: u32,
        /// Short device description (parsed from the user agent).
        device: String,
        /// Logical cores, so the server never asks a device for more
        /// concurrent thinking than it has threads to do it with.
        cores: u32,
    },
    /// Answer to a `ComputeRequest`: the deterministic search result plus
    /// the proof binding the nonce to the work performed. The server
    /// randomly re-runs the identical search to audit; mismatches strike
    /// the host and eventually ban it from computing.
    BotMove {
        request_id: u64,
        dir: Dir,
        proof: u64,
        tm: ComputeTelemetry,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LobbyState {
    pub session: String,
    /// Server phase truth: whether a game is running right now. A client
    /// holding an *unfinished* local game when this says false missed the
    /// ending while away and must fall back to the lobby.
    pub game_running: bool,
    pub config: GameConfig,
    pub players: Vec<PlayerProfile>,
    pub ready: Vec<PlayerId>,
    /// Winners of the last finished game in this session, if any.
    pub last_winners: Vec<Winner>,
    /// History id of the last finished game, if saved.
    pub last_game_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMsg {
    /// First message after a successful join: your public player id.
    Welcome {
        your_id: PlayerId,
    },
    /// Full lobby snapshot. Sent on join and whenever the lobby changes.
    Lobby(LobbyState),
    /// A game is starting; first tick fires after `countdown_ms`.
    GameStart {
        config: GameConfig,
        players: Vec<PlayerProfile>,
        turn: Turn,
        countdown_ms: u32,
    },
    /// One resolved tick.
    Turn(Turn),
    /// Full game state so far — sent when (re)joining mid-game.
    GameInProgress {
        config: GameConfig,
        players: Vec<PlayerProfile>,
        turns: Vec<Turn>,
    },
    /// The finished game was written to history under this id, with the
    /// per-device compute contributions for the scoreboard.
    GameSaved {
        id: i64,
        compute: Vec<ComputeStat>,
    },
    /// Active-games list push (for WatchGames subscribers).
    Sessions(Vec<SessionSummary>),
    /// Run the bot search for `bot_id` on this device and answer within
    /// `think_ms`. The search is anytime: think until the deadline, then
    /// return the best move found. The device watches its own clock, so
    /// there is nothing for the server to predict about its speed.
    ComputeRequest {
        request_id: u64,
        bot_id: PlayerId,
        bot_name: String,
        config: GameConfig,
        turn: Turn,
        think_ms: u32,
        nonce: u64,
    },
    Error(String),
    Pong(u32),
    /// The game is held, or running again.
    Paused(bool),
}

pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    postcard::to_allocvec(msg).expect("postcard encode cannot fail for our types")
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Option<T> {
    postcard::from_bytes(bytes).ok()
}

/// One row of the active-games list. Games aren't named — they're known by
/// their host (the first human in the lobby). `players` counts humans only,
/// so bot-only lobbies vanish the moment everyone leaves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub host_name: String,
    pub host_emoji: String,
    pub players: u32,
    pub bots: u32,
    pub playing: bool,
}

/// One compute host's contribution to a finished game.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComputeStat {
    pub name: String,
    pub device: String,
    pub moves: u32,
    pub sims: u64,
    pub wall_ms: u64,
}

/// What a device actually did with a compute request. Reported for every
/// move so a bot's behaviour is explainable from the server logs instead
/// of inferred: a bot that plays badly either got few simulations, waited
/// too long for a thread, or fell back to the UI thread — and this says
/// which.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ComputeTelemetry {
    /// Request arrival -> the worker actually starting on it.
    pub queue_ms: u32,
    /// Time the worker spent searching.
    pub search_ms: u32,
    /// Arrival -> reply sent, i.e. everything the server's tick budget is
    /// really spent on.
    pub total_ms: u32,
    /// Deadline the device was given.
    pub think_ms: u32,
    pub sims: u32,
    /// Search slices completed — deadline granularity. One slice means the
    /// device barely searched at all.
    pub slices: u32,
    /// Computed on the UI thread because no worker was available.
    pub fallback: bool,
    /// Worst event-loop stall seen on the device since its last answer. The
    /// page is single threaded: while it is drawing a board of long snakes,
    /// an arriving request simply waits, and that wait is invisible to every
    /// other number here because they only start once the message is picked
    /// up.
    pub loop_lag_ms: u32,
}

/// All-time client-compute statistics (JSON, /api/compute).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComputeOverview {
    pub totals: ComputeTotals,
    /// Grouped by player + device, biggest contributors first.
    pub donors: Vec<ComputeDonor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComputeTotals {
    pub devices: u32,
    pub games: u32,
    pub moves: u64,
    pub sims: u64,
    pub wall_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComputeDonor {
    pub name: String,
    pub device: String,
    pub games: u32,
    pub moves: u64,
    pub sims: u64,
    pub wall_ms: u64,
    pub best_benchmark: u32,
}

/// One page of history. `next` is the keyset cursor for the next (older)
/// page: pass it back as ?before=.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    pub next: Option<i64>,
}

/// Summary row for the history list API (this one travels as JSON over a
/// plain GET since it is not perf-sensitive).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: i64,
    pub finished_at_ms: i64,
    pub num_turns: u32,
    pub players: Vec<PlayerProfile>,
    pub winners: Vec<Winner>,
}
