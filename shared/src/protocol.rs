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
pub const PROTOCOL_VERSION: u16 = 9;

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
    },
    /// Answer to a `ComputeRequest`: the deterministic search result plus
    /// the proof binding the nonce to the work performed. The server
    /// randomly re-runs the identical search to audit; mismatches strike
    /// the host and eventually ban it from computing.
    BotMove {
        request_id: u64,
        dir: Dir,
        proof: u64,
        /// Wall time this device spent on the search.
        elapsed_ms: u32,
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
    Welcome { your_id: PlayerId },
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
    /// Run the bot search for `bot_id` on this device: exactly `sims`
    /// simulations from `turn`, echoing `nonce` inside the proof. Due back
    /// before the next tick.
    ComputeRequest {
        request_id: u64,
        bot_id: PlayerId,
        bot_name: String,
        config: GameConfig,
        turn: Turn,
        sims: u32,
        nonce: u64,
    },
    Error(String),
    Pong(u32),
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
