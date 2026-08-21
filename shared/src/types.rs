use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Cell index into the board: `y * width + x`. Same convention as tac.
pub type Pos = u16;

/// Client-generated stable id (uuid string kept in localStorage).
pub type PlayerId = String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerProfile {
    pub id: PlayerId,
    pub name: String,
    pub emoji: String,
    pub colour: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameConfig {
    pub board_width: u8,
    pub board_height: u8,
    /// Real-time tick interval in milliseconds.
    pub tick_ms: u32,
}

impl Default for GameConfig {
    fn default() -> Self {
        // tac's "medium" board; 1s ticks for fast turns.
        GameConfig {
            board_width: 13,
            board_height: 13,
            tick_ms: 1000,
        }
    }
}

impl GameConfig {
    pub fn cells(&self) -> usize {
        self.board_width as usize * self.board_height as usize
    }

    pub fn valid(&self) -> bool {
        (7..=25).contains(&self.board_width)
            && (7..=25).contains(&self.board_height)
            && (250..=10_000).contains(&self.tick_ms)
    }

    /// Perimeter wall cells. Walls are derived, not stored — and not drawn:
    /// the UI crops the view to the interior so the screen edge is the wall.
    pub fn walls(&self) -> Vec<Pos> {
        let (w, h) = (self.board_width as u16, self.board_height as u16);
        let mut out = Vec::new();
        for x in 0..w {
            out.push(x);
            out.push((h - 1) * w + x);
        }
        for y in 1..h - 1 {
            out.push(y * w);
            out.push(y * w + (w - 1));
        }
        out
    }

    pub fn is_wall(&self, pos: Pos) -> bool {
        let (w, h) = (self.board_width as u16, self.board_height as u16);
        let (x, y) = (pos % w, pos / w);
        x == 0 || y == 0 || x == w - 1 || y == h - 1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    pub fn delta(self) -> (i16, i16) {
        match self {
            Dir::Up => (0, -1),
            Dir::Down => (0, 1),
            Dir::Left => (-1, 0),
            Dir::Right => (1, 0),
        }
    }

    pub fn opposite(self) -> Dir {
        match self {
            Dir::Up => Dir::Down,
            Dir::Down => Dir::Up,
            Dir::Left => Dir::Right,
            Dir::Right => Dir::Left,
        }
    }

    pub const ALL: [Dir; 4] = [Dir::Up, Dir::Down, Dir::Left, Dir::Right];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClashReason {
    Wall,
    SelfCollision,
    HeadOn,
    Body,
    Starved,
}

impl ClashReason {
    pub fn text(&self) -> &'static str {
        match self {
            ClashReason::Wall => "hit a wall",
            ClashReason::SelfCollision => "ate itself",
            ClashReason::HeadOn => "lost a head-on collision",
            ClashReason::Body => "ran into another snek",
            ClashReason::Starved => "starved",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clash {
    pub pos: Pos,
    pub players: Vec<PlayerId>,
    pub reason: ClashReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Winner {
    pub player: PlayerId,
    /// Turns survived — the ranking metric, as in tac.
    pub survival_turns: u32,
    /// Final length when the game ended (0 if fully dead).
    pub length: u16,
}

/// One resolved game tick. BTreeMaps keep serialization deterministic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub turn_number: u32,
    /// Head-first cell lists, like tac's playerPieces. Dead sneks are removed.
    pub snakes: BTreeMap<PlayerId, Vec<Pos>>,
    pub health: BTreeMap<PlayerId, u8>,
    pub alive: Vec<PlayerId>,
    pub food: Vec<Pos>,
    /// Moves that were applied to produce this turn.
    pub moves: BTreeMap<PlayerId, Dir>,
    pub clashes: Vec<Clash>,
    /// Non-empty exactly when the game is over; sorted best-first.
    pub winners: Vec<Winner>,
}

impl Turn {
    pub fn game_over(&self) -> bool {
        !self.winners.is_empty()
    }
}

/// A full game — what gets written to sqlite on completion and replayed
/// in the history UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameRecord {
    pub config: GameConfig,
    pub players: Vec<PlayerProfile>,
    pub seed: u64,
    pub turns: Vec<Turn>,
}
