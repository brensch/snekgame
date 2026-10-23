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
pub const PROTOCOL_VERSION: u16 = 20;

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
    /// Rename the game (between games; see [`valid_game_name`]).
    Rename(String),
    /// I'm leaving: take me out of the lobby now (no held seat). Answered
    /// with [`ServerMsg::Left`].
    Leave,
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
    /// How getting this connection went (see [`ConnectReport`]).
    Connected(ConnectReport),
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
    /// Everyone who played the last finished game, so it can still be told
    /// who they were after they've left.
    pub last_players: Vec<PlayerProfile>,
    /// History id of the last finished game, if saved.
    pub last_game_id: Option<i64>,
    /// Who last changed the settings (None until somebody does).
    pub config_changed_by: Option<PlayerId>,
    /// Bumped on every settings change (the name included), so a client can
    /// tell a fresh change from the same lobby being sent again for some
    /// other reason.
    pub config_rev: u32,
    /// What the game is called. It starts as its link's three words and
    /// anyone can change it; the link itself never changes.
    pub name: String,
}

/// How getting connected went, as the client saw it.
///
/// Sent once the server has answered a new connection, so a slow or stuck
/// one can be explained from the server's log rather than guessed at. Times
/// are milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectReport {
    /// Whether this replaces a connection that was lost (as opposed to the
    /// page's first).
    pub reconnect: bool,
    /// Attempts it took, this one included.
    pub attempts: u32,
    /// For a reconnect: from the last thing heard on the old connection to
    /// the first thing heard on this one, i.e. how long the player was cut
    /// off.
    pub silent_ms: Option<u32>,
    /// From the page coming back into view to the server answering, if it
    /// was hidden (a locked phone, another app) when the connection went.
    pub since_visible_ms: Option<u32>,
    /// From the page starting to load to now.
    pub page_ms: u32,
    /// This attempt: opening the connection until it was ready.
    pub handshake_ms: u32,
    /// This attempt: from ready to the server's first word (the join).
    pub join_ms: u32,
    /// The page's recent connection events, oldest first, including those
    /// of the page before a reload: what it was doing when it got stuck.
    pub trail: Vec<String>,
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
    /// You were taken out of the lobby, by this player (None if they have
    /// since left). Nothing more comes on this connection.
    Kicked(Option<PlayerProfile>),
    /// You've left (see [`ClientMsg::Leave`]). Nothing more comes on this
    /// connection.
    Left,
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

/// One row of the active-games list. A game is known by its name; nobody
/// in it owns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// The game's id, as in its link (see [`crate::names`]).
    pub id: String,
    /// What it's called now (see [`LobbyState::name`]).
    pub name: String,
    /// Everyone in it, in the order they joined.
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

/// The name a game starts with: its link's words, spaced out.
#[must_use]
pub fn default_game_name(id: &str) -> String {
    id.replace('-', " ")
}

/// Whether `name` will do as a game's name: something to read (not just
/// spaces), and at most 40 characters.
#[must_use]
pub fn valid_game_name(name: &str) -> bool {
    !name.trim().is_empty() && name.chars().count() <= 40
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Clash, ClashReason};
    use std::collections::BTreeMap;

    /// Which variant a message is. Exhaustive, no wildcard: a new message
    /// doesn't compile until it's listed here, and then the test below
    /// fails until it has a sample.
    const fn client_variant(msg: &ClientMsg) -> usize {
        match msg {
            ClientMsg::Join { .. } => 0,
            ClientMsg::UpdateProfile(_) => 1,
            ClientMsg::SetReady(_) => 2,
            ClientMsg::Configure(_) => 3,
            ClientMsg::Rename(_) => 4,
            ClientMsg::Leave => 5,
            ClientMsg::Start => 6,
            ClientMsg::Move(_) => 7,
            ClientMsg::AddBot(_) => 8,
            ClientMsg::Kick(_) => 9,
            ClientMsg::Ping(_) => 10,
            ClientMsg::SetPaused(_) => 11,
            ClientMsg::HostAvailable { .. } => 12,
            ClientMsg::ComputeFailed { .. } => 13,
            ClientMsg::WatchGames => 14,
            ClientMsg::Benchmark { .. } => 15,
            ClientMsg::BotMove { .. } => 16,
            ClientMsg::Connected(_) => 17,
        }
    }
    const CLIENT_VARIANTS: usize = 18;

    /// See [`client_variant`].
    const fn server_variant(msg: &ServerMsg) -> usize {
        match msg {
            ServerMsg::Welcome { .. } => 0,
            ServerMsg::Lobby(_) => 1,
            ServerMsg::GameStart { .. } => 2,
            ServerMsg::Turn(_) => 3,
            ServerMsg::GameInProgress { .. } => 4,
            ServerMsg::GameSaved { .. } => 5,
            ServerMsg::Sessions(_) => 6,
            ServerMsg::ComputeRequest { .. } => 7,
            ServerMsg::Error(_) => 8,
            ServerMsg::Pong(_) => 9,
            ServerMsg::Paused(_) => 10,
            ServerMsg::Kicked(_) => 11,
            ServerMsg::Left => 12,
        }
    }
    const SERVER_VARIANTS: usize = 13;

    fn player() -> PlayerProfile {
        PlayerProfile {
            id: "abc123def456".to_owned(),
            name: "brendy".to_owned(),
            emoji: "🐉".to_owned(),
            colour: "hsl(320, 85%, 70%)".to_owned(),
        }
    }

    fn turn() -> Turn {
        Turn {
            turn_number: 7,
            snakes: BTreeMap::from([("abc123def456".to_owned(), vec![20, 21, 22])]),
            health: BTreeMap::from([("abc123def456".to_owned(), 93)]),
            alive: vec!["abc123def456".to_owned()],
            food: vec![40, 41],
            moves: BTreeMap::from([("abc123def456".to_owned(), Dir::Left)]),
            clashes: vec![Clash {
                pos: 5,
                players: vec!["bot-1".to_owned()],
                reason: ClashReason::Body,
            }],
            winners: vec![Winner {
                player: "bot-1".to_owned(),
                survival_turns: 6,
                length: 4,
            }],
        }
    }

    /// One of every message, every field filled in (no empty lists, no
    /// Nones), so a change to any message's shape changes these bytes.
    fn samples() -> (Vec<ClientMsg>, Vec<ServerMsg>) {
        (client_samples(), server_samples())
    }

    fn client_samples() -> Vec<ClientMsg> {
        let info = ProfileInfo {
            name: "brendy".to_owned(),
            emoji: "🐉".to_owned(),
            colour: "red".to_owned(),
        };
        let config = GameConfig::default();
        vec![
            ClientMsg::Join {
                version: PROTOCOL_VERSION,
                session: "lukewarm-mandatory-potluck".to_owned(),
                secret: "0123456789abcdef".to_owned(),
                profile: info.clone(),
            },
            ClientMsg::UpdateProfile(info),
            ClientMsg::SetReady(true),
            ClientMsg::Configure(config),
            ClientMsg::Rename("the friday grudge match".to_owned()),
            ClientMsg::Leave,
            ClientMsg::Start,
            ClientMsg::Move(Dir::Up),
            ClientMsg::AddBot(BotDifficulty::Hard),
            ClientMsg::Kick("bot-1".to_owned()),
            ClientMsg::Ping(9),
            ClientMsg::SetPaused(true),
            ClientMsg::HostAvailable { can_compute: true },
            ClientMsg::ComputeFailed {
                request_id: 3,
                reason: "no worker".to_owned(),
            },
            ClientMsg::WatchGames,
            ClientMsg::Benchmark {
                sims_per_sec: 50_000,
                device: "Pixel 9".to_owned(),
                cores: 8,
            },
            ClientMsg::BotMove {
                request_id: 3,
                dir: Dir::Right,
                proof: 77,
                tm: ComputeTelemetry {
                    queue_ms: 1,
                    search_ms: 2,
                    total_ms: 3,
                    think_ms: 4,
                    sims: 5,
                    slices: 6,
                    fallback: true,
                    loop_lag_ms: 7,
                },
            },
            ClientMsg::Connected(ConnectReport {
                reconnect: true,
                attempts: 2,
                silent_ms: Some(9_000),
                since_visible_ms: Some(40),
                page_ms: 120_000,
                handshake_ms: 60,
                join_ms: 8,
                trail: vec!["open #2".to_owned()],
            }),
        ]
    }

    fn server_samples() -> Vec<ServerMsg> {
        let config = GameConfig::default();
        vec![
            ServerMsg::Welcome {
                your_id: "abc123def456".to_owned(),
            },
            ServerMsg::Lobby(LobbyState {
                session: "lukewarm-mandatory-potluck".to_owned(),
                game_running: true,
                config: config.clone(),
                players: vec![player()],
                ready: vec!["abc123def456".to_owned()],
                last_winners: turn().winners,
                last_players: vec![player()],
                last_game_id: Some(12),
                config_changed_by: Some("abc123def456".to_owned()),
                config_rev: 3,
                name: "the friday grudge match".to_owned(),
            }),
            ServerMsg::GameStart {
                config: config.clone(),
                players: vec![player()],
                turn: turn(),
                countdown_ms: 4000,
            },
            ServerMsg::Turn(turn()),
            ServerMsg::GameInProgress {
                config: config.clone(),
                players: vec![player()],
                turns: vec![turn()],
            },
            ServerMsg::GameSaved {
                id: 12,
                compute: vec![ComputeStat {
                    name: "🐉 brendy".to_owned(),
                    device: "Pixel 9".to_owned(),
                    moves: 40,
                    sims: 2_000_000,
                    wall_ms: 9_000,
                }],
            },
            ServerMsg::Sessions(vec![SessionSummary {
                id: "lukewarm-mandatory-potluck".to_owned(),
                name: "the friday grudge match".to_owned(),
                emojis: vec!["🐉".to_owned()],
                players: 1,
                bots: 1,
                playing: true,
            }]),
            ServerMsg::ComputeRequest {
                request_id: 3,
                bot_id: "bot-1".to_owned(),
                bot_name: "lad".to_owned(),
                config,
                turn: turn(),
                think_ms: 250,
                nonce: 99,
            },
            ServerMsg::Error("refresh".to_owned()),
            ServerMsg::Pong(9),
            ServerMsg::Paused(true),
            ServerMsg::Kicked(Some(player())),
            ServerMsg::Left,
        ]
    }

    /// FNV-1a: small, stable, and enough to notice any change.
    fn fingerprint(bytes: impl IntoIterator<Item = u8>) -> u64 {
        bytes.into_iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
    }

    /// The wire format is pinned to the protocol version. Changing any
    /// message changes the fingerprint, and this fails until
    /// `PROTOCOL_VERSION` goes up and the new fingerprint is recorded with
    /// it. Without that, a page loaded before a deploy keeps talking to the
    /// new server, each misreading the other, and nothing tells it to
    /// reload.
    #[test]
    fn the_wire_format_matches_the_protocol_version() {
        let (client, server) = samples();
        let mut seen: Vec<usize> = client.iter().map(client_variant).collect();
        seen.dedup();
        assert_eq!(
            seen,
            (0..CLIENT_VARIANTS).collect::<Vec<_>>(),
            "a ClientMsg has no sample"
        );
        let mut seen: Vec<usize> = server.iter().map(server_variant).collect();
        seen.dedup();
        assert_eq!(
            seen,
            (0..SERVER_VARIANTS).collect::<Vec<_>>(),
            "a ServerMsg has no sample"
        );

        let bytes = client
            .iter()
            .flat_map(encode)
            .chain(server.iter().flat_map(encode));
        let print = fingerprint(bytes);
        assert_eq!(
            (PROTOCOL_VERSION, print),
            (20, 0x470c_a4da_b20d_1e25),
            "the wire format changed: bump PROTOCOL_VERSION and record its new fingerprint"
        );
    }
}
