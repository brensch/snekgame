//! The wire protocol: postcard-encoded messages over websocket binary frames.
//! Both sides share these exact types, so there is no schema drift and no
//! JSON overhead.

use crate::types::*;
use serde::{Deserialize, Serialize};

/// Bump when messages change incompatibly; the server rejects mismatches so
/// stale cached frontends reload instead of misdecoding frames.
pub const PROTOCOL_VERSION: u16 = 1;

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
    /// Remove a player from the lobby (anyone can kick, like tac).
    Kick(PlayerId),
    Ping(u32),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LobbyState {
    pub session: String,
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
    /// The finished game was written to history under this id.
    GameSaved { id: i64 },
    Error(String),
    Pong(u32),
}

pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    postcard::to_allocvec(msg).expect("postcard encode cannot fail for our types")
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Option<T> {
    postcard::from_bytes(bytes).ok()
}

/// Summary row for the history list API (this one travels as JSON over a
/// plain GET since it is not perf-sensitive).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: i64,
    pub session: String,
    pub finished_at_ms: i64,
    pub num_turns: u32,
    pub players: Vec<PlayerProfile>,
    pub winners: Vec<Winner>,
}
