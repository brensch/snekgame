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

use crate::types::{BotDifficulty, Dir, GameConfig, PlayerId, PlayerProfile, Turn, Winner};
use serde::{Deserialize, Serialize};

/// Bump when messages change incompatibly; the server rejects mismatches so
/// stale cached frontends reload instead of misdecoding frames.
pub const PROTOCOL_VERSION: u16 = 19;

/// The client-editable part of a profile.
///
/// The public player id is derived server-side (a hash of the client's
/// private secret), so a client can never claim someone else's identity —
/// and moves are bound to the connection's server-side identity, never to
/// anything in the message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileInfo {
    /// Display name.
    pub name: String,
    /// The emoji on the snek's head.
    pub emoji: String,
    /// The snek's body colour, as a CSS colour string.
    pub colour: String,
}

/// Everything a client sends: on the control stream, or (`Move`, `Ping`)
/// as datagrams.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// First message on every (re)connection. `secret` never leaves the
    /// server; everyone else only ever sees the derived public id.
    Join {
        /// The client's [`PROTOCOL_VERSION`]; a mismatch is refused.
        version: u16,
        /// The lobby to join, from the shared link.
        session: String,
        /// The client's private identity (kept in localStorage).
        secret: String,
        /// How the player wants to appear.
        profile: ProfileInfo,
    },
    /// Change name, emoji or colour.
    UpdateProfile(ProfileInfo),
    /// Say whether this player is ready to start.
    SetReady(bool),
    /// Change the lobby's settings for the next game.
    Configure(GameConfig),
    /// Start the game now.
    Start,
    /// Steer: applied at the next tick, if it arrives in time.
    Move(Dir),
    /// Add a server-hosted bot to the lobby.
    AddBot(BotDifficulty),
    /// Remove a player (or bot) from the lobby (anyone can kick, like tac).
    Kick(PlayerId),
    /// Round-trip probe; echoed back as `Pong` with the same number.
    Ping(u32),
    /// Hold the game where it is, or let it run on again.
    SetPaused(bool),
    /// Whether this device can currently compute. A backgrounded page or
    /// a locked screen suspends its workers, so it must say so — otherwise
    /// its bots quietly hold their heading into a wall.
    HostAvailable {
        /// False while the page is hidden or the device is locked.
        can_compute: bool,
    },
    /// This device could not produce a move for a compute request. Sent so
    /// the failure is visible server-side instead of looking identical to
    /// a silent client.
    ComputeFailed {
        /// The `ComputeRequest` this answers.
        request_id: u64,
        /// What went wrong, for the server's logs.
        reason: String,
    },
    /// Alternative first message: subscribe to the active-games list
    /// instead of joining a session (used by the home screen).
    WatchGames,
    /// Self-measured search speed (sims/sec), from ~600ms of real search in
    /// the lobby. Makes this connection eligible to host bot compute; the
    /// fastest phones get picked first.
    Benchmark {
        /// Simulations per second the bot search managed.
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
        /// The `ComputeRequest` this answers.
        request_id: u64,
        /// The bot's move.
        dir: Dir,
        /// [`crate::bot::bind_proof`] of the request's nonce, its id and
        /// `dir`.
        proof: u64,
        /// How the device spent the request, for the server's logs.
        tm: ComputeTelemetry,
    },
    /// Deal the lobby a new hand of foods to pick from. Everyone sees the
    /// same hand; the food currently picked stays in it.
    ShuffleFood,
    /// Sent once a dropped connection is back, saying how the reconnect
    /// went, so a slow one can be explained from the server's log rather
    /// than guessed at.
    Reconnected(ReconnectReport),
}

/// Everything about a lobby that its members see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbyState {
    /// The lobby's id, as in its link.
    pub session: String,
    /// Server phase truth: whether a game is running right now. A client
    /// holding an *unfinished* local game when this says false missed the
    /// ending while away and must fall back to the lobby.
    pub game_running: bool,
    /// The settings the next game will use.
    pub config: GameConfig,
    /// Everyone in the lobby, in join order (bots included).
    pub players: Vec<PlayerProfile>,
    /// The players who have said they're ready.
    pub ready: Vec<PlayerId>,
    /// Winners of the last finished game in this session, if any.
    pub last_winners: Vec<Winner>,
    /// History id of the last finished game, if saved.
    pub last_game_id: Option<i64>,
    /// Who last changed the settings (None until somebody does).
    pub config_changed_by: Option<PlayerId>,
    /// Bumped on every settings change, so a client can tell a fresh change
    /// from the same lobby being sent again for some other reason.
    pub config_rev: u32,
    /// The foods on offer to pick from, the picked one among them. Held by
    /// the server so every phone shows the same hand.
    pub food_hand: Vec<String>,
}

/// How a reconnect went, timed by the client. All times are milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconnectReport {
    /// Connection attempts it took, this one included.
    pub attempts: u32,
    /// From the last thing heard on the old connection to the first thing
    /// heard on this one: how long the player was cut off.
    pub silent_ms: u32,
    /// From the first attempt starting to the server answering.
    pub reconnect_ms: u32,
    /// From the page coming back into view to the server answering, if it
    /// was hidden (a locked phone, another app) when the connection went.
    pub since_visible_ms: Option<u32>,
}

/// Everything the server sends: on the control stream, or (`Turn`,
/// `Pong`) as datagrams.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    /// First message after a successful join: your public player id.
    Welcome {
        /// The id the server derived from this client's secret.
        your_id: PlayerId,
    },
    /// Full lobby snapshot. Sent on join and whenever the lobby changes.
    Lobby(LobbyState),
    /// A game is starting; first tick fires after `countdown_ms`.
    GameStart {
        /// The settings the game is played with.
        config: GameConfig,
        /// Everyone playing, in the engine's player order.
        players: Vec<PlayerProfile>,
        /// The starting position.
        turn: Turn,
        /// Time until the first tick.
        countdown_ms: u32,
    },
    /// One resolved tick.
    Turn(Turn),
    /// Full game state so far — sent when (re)joining mid-game.
    GameInProgress {
        /// The settings the game is played with.
        config: GameConfig,
        /// Everyone playing, in the engine's player order.
        players: Vec<PlayerProfile>,
        /// Every turn so far, the starting position first.
        turns: Vec<Turn>,
    },
    /// The finished game was written to history under this id, with the
    /// per-device compute contributions for the scoreboard.
    GameSaved {
        /// The game's history id.
        id: i64,
        /// Who did the bots' thinking, and how much.
        compute: Vec<ComputeStat>,
    },
    /// Active-games list push (for `WatchGames` subscribers).
    Sessions(Vec<SessionSummary>),
    /// Run the bot search for `bot_id` on this device and answer within
    /// `think_ms`. The search is anytime: think until the deadline, then
    /// return the best move found. The device watches its own clock, so
    /// there is nothing for the server to predict about its speed.
    ComputeRequest {
        /// Echoed in the answer, so a late reply can't be taken for the
        /// current one.
        request_id: u64,
        /// The bot to move for.
        bot_id: PlayerId,
        /// The bot's display name, for the device's own UI.
        bot_name: String,
        /// The game's settings.
        config: GameConfig,
        /// The position to move from.
        turn: Turn,
        /// How long the device may think.
        think_ms: u32,
        /// Fresh per request; the answer's proof must bind it.
        nonce: u64,
    },
    /// Something the client asked for was refused, said for a person to read.
    Error(String),
    /// Answer to `Ping`, with the same number.
    Pong(u32),
    /// The game is held, or running again.
    Paused(bool),
}

/// Postcard bytes of a message, as framed on the stream or sent as a
/// datagram.
///
/// # Panics
///
/// Never for this crate's types: postcard only fails on types serde can't
/// describe up front (sequences of unknown length, custom `Serialize` impls
/// that error), and writing into a growable `Vec` never runs out of room.
#[expect(
    clippy::expect_used,
    reason = "postcard cannot fail on our derive(Serialize) types; callers would only unwrap a Result"
)]
pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    postcard::to_allocvec(msg).expect("postcard encode cannot fail for our types")
}

/// The message in `bytes`, or None if they aren't one (truncated, corrupt
/// or from an incompatible version).
#[must_use]
pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Option<T> {
    postcard::from_bytes(bytes).ok()
}

/// One row of the active-games list. Games aren't named — they're known by
/// their host (the first human in the lobby). `players` counts humans only,
/// so bot-only lobbies vanish the moment everyone leaves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// The lobby's id, as in its link.
    pub id: String,
    /// The host's display name.
    pub host_name: String,
    /// The host's emoji.
    pub host_emoji: String,
    /// The host's colour.
    pub host_colour: String,
    /// Everyone in it, host first, bots last.
    pub emojis: Vec<String>,
    /// Humans in the lobby.
    pub players: u32,
    /// Bots in the lobby.
    pub bots: u32,
    /// Whether a game is running in it right now.
    pub playing: bool,
}

/// One compute host's contribution to a finished game.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputeStat {
    /// The player whose device it was.
    pub name: String,
    /// The device, as it described itself in its `Benchmark`.
    pub device: String,
    /// Bot moves it computed.
    pub moves: u32,
    /// Simulations those moves took in all.
    pub sims: u64,
    /// Time it spent searching, in all.
    pub wall_ms: u64,
}

/// What a device actually did with a compute request.
///
/// Reported for every move so a bot's behaviour is explainable from the
/// server logs instead of inferred: a bot that plays badly either got few
/// simulations, waited too long for a thread, or fell back to the UI thread
/// — and this says which.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Simulations the search ran.
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputeOverview {
    /// Everything, ever.
    pub totals: ComputeTotals,
    /// Grouped by player + device, biggest contributors first.
    pub donors: Vec<ComputeDonor>,
}

/// The botnet's all-time totals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputeTotals {
    /// Distinct player + device pairs that ever computed a move.
    pub devices: u32,
    /// Games with at least one move computed by a device.
    pub games: u32,
    /// Bot moves computed.
    pub moves: u64,
    /// Simulations those moves took.
    pub sims: u64,
    /// Time spent searching.
    pub wall_ms: u64,
}

/// One player's device and everything it has computed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputeDonor {
    /// The player's display name.
    pub name: String,
    /// The device, as it described itself.
    pub device: String,
    /// Games it computed moves in.
    pub games: u32,
    /// Bot moves computed.
    pub moves: u64,
    /// Simulations those moves took.
    pub sims: u64,
    /// Time spent searching.
    pub wall_ms: u64,
    /// Its fastest `Benchmark`, in simulations per second.
    pub best_benchmark: u32,
}

/// One page of history. `next` is the keyset cursor for the next (older)
/// page: pass it back as ?before=.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryPage {
    /// Newest first.
    pub entries: Vec<HistoryEntry>,
    /// None on the last page.
    pub next: Option<i64>,
}

/// Summary row for the history list API.
///
/// This one travels as JSON over a plain GET since it is not
/// perf-sensitive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// History id, as in the replay's link.
    pub id: i64,
    /// When it ended, Unix milliseconds.
    pub finished_at_ms: i64,
    /// Turns played, the starting position included.
    pub num_turns: u32,
    /// Everyone who played.
    pub players: Vec<PlayerProfile>,
    /// Final standings, best first.
    pub winners: Vec<Winner>,
}
