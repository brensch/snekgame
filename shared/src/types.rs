//! The game's data: configuration, players, and the resolved turns that
//! make up a game. Everything here crosses the wire or lands in the
//! database, so it is all plain serde data.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Cell index into the board: `y * width + x`. Same convention as tac.
pub type Pos = u16;

/// Client-generated stable id (uuid string kept in localStorage).
pub type PlayerId = String;

/// Who a player is, as everyone else sees them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerProfile {
    /// Public id: a hash of the client's secret, or `bot-N` for a bot.
    pub id: PlayerId,
    /// Display name, chosen by the player.
    pub name: String,
    /// The emoji drawn as the snek's head.
    pub emoji: String,
    /// The snek's body colour, as a CSS colour string.
    pub colour: String,
}

/// The lobby's settings for a game. Board sizes count the wall ring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameConfig {
    /// Columns, including the wall on each side.
    pub board_width: u8,
    /// Rows, including the wall on each side.
    pub board_height: u8,
    /// Real-time tick interval in milliseconds.
    pub tick_ms: u32,
    /// What the food looks like this game. Chosen in the lobby and stored
    /// with the record, so a replay shows what the players actually ate.
    pub food_emoji: String,
}

/// What food was before it was choosable, and what every game played up to
/// then is stamped with.
pub const DEFAULT_FOOD: &str = "🎃";

/// Things a snake could plausibly want to eat. Only edible things belong
/// here — the food is the one emoji on the board that has to read as a
/// snack at a glance.
pub const FOODS: &[&str] = &[
    "🎃", "🍎", "🍕", "🍔", "🍩", "🍒", "🍓", "🌮", "🍗", "🥑", "🍇", "🍌", "🥕", "🧀", "🍰", "🍪",
    "🥨", "🌭", "🍤", "🍣", "🥩", "🍉", "🍍", "🥞", "🍦", "🍫", "🥐", "🌽", "🍄", "🥟", "🍭", "🧁",
    "🍿", "🥓", "🍑", "🥝", "🫐", "🍆", "🥦", "🦴", "🍋", "🥯", "🍠", "🧇",
];

/// How many foods the lobby offers at once.
pub const FOOD_HAND: usize = 8;

/// A hand of [`FOOD_HAND`] foods with `keep` at the front.
///
/// The rest are drawn from [`FOODS`] by `pick`, which is given how many are
/// left to choose from and returns an index below that. Whatever is
/// currently chosen always survives a shuffle, in the same place, so the
/// choice just made can't be shuffled out from under anyone.
pub fn deal_food_hand(keep: &str, mut pick: impl FnMut(usize) -> usize) -> Vec<String> {
    let mut rest: Vec<&str> = FOODS.iter().copied().filter(|f| *f != keep).collect();
    let mut hand = Vec::with_capacity(FOOD_HAND);
    hand.push(keep.to_owned());
    while hand.len() < FOOD_HAND && !rest.is_empty() {
        let i = pick(rest.len()).min(rest.len() - 1);
        hand.push(rest.swap_remove(i).to_owned());
    }
    hand
}

impl Default for GameConfig {
    fn default() -> Self {
        // tac's "medium" board; 1s ticks for fast turns.
        Self {
            board_width: 13,
            board_height: 13,
            food_emoji: DEFAULT_FOOD.to_owned(),
            tick_ms: 1000,
        }
    }
}

impl GameConfig {
    /// Every cell on the board, walls included: the length of a per-cell
    /// array indexed by [`Pos`].
    #[must_use]
    pub fn cells(&self) -> usize {
        usize::from(self.board_width) * usize::from(self.board_height)
    }

    /// Whether these settings are allowed. The server ignores any others,
    /// so the engine never sees a board too small to place sneks on.
    #[must_use]
    pub fn valid(&self) -> bool {
        (7..=25).contains(&self.board_width)
            && (7..=25).contains(&self.board_height)
            && (250..=10_000).contains(&self.tick_ms)
            // One glyph, and a glyph can be several chars (skin tones, ZWJ).
            && !self.food_emoji.is_empty()
            && self.food_emoji.chars().count() <= 8
    }

    /// Perimeter wall cells. Walls are derived, not stored — and not drawn:
    /// the UI crops the view to the interior so the screen edge is the wall.
    #[must_use]
    pub fn walls(&self) -> Vec<Pos> {
        let (w, h) = (u16::from(self.board_width), u16::from(self.board_height));
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

    /// Whether `pos` is in the perimeter ring.
    #[must_use]
    pub fn is_wall(&self, pos: Pos) -> bool {
        let (w, h) = (u16::from(self.board_width), u16::from(self.board_height));
        let (x, y) = (pos % w, pos / w);
        x == 0 || y == 0 || x == w - 1 || y == h - 1
    }
}

/// House-bot strength: how many moves it looks ahead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BotDifficulty {
    /// 8 ms of search a move.
    Easy,
    /// 35 ms of search a move.
    Medium,
    /// 100 ms of search a move.
    Hard,
}

/// Server bots get ids like `bot-1`. Human public ids are hex hashes, so the
/// prefix can never collide — both ends use this to tell them apart.
#[must_use]
pub fn is_bot(player_id: &str) -> bool {
    player_id.starts_with("bot-")
}

/// A move: one cell in screen directions (y grows downward).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Dir {
    /// Toward row 0.
    Up,
    /// Toward the bottom row.
    Down,
    /// Toward column 0.
    Left,
    /// Toward the rightmost column.
    Right,
}

impl Dir {
    /// The (dx, dy) one step this way moves.
    #[must_use]
    pub const fn delta(self) -> (i16, i16) {
        match self {
            Self::Up => (0, -1),
            Self::Down => (0, 1),
            Self::Left => (-1, 0),
            Self::Right => (1, 0),
        }
    }

    /// The way back: the move a snek can't make, since it is its own neck.
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Up => Self::Down,
            Self::Down => Self::Up,
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }

    /// Every direction, in the fixed order searches iterate them in.
    pub const ALL: [Self; 4] = [Self::Up, Self::Down, Self::Left, Self::Right];
}

/// How a snek died.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClashReason {
    /// Moved into the perimeter.
    Wall,
    /// Moved into its own body.
    SelfCollision,
    /// Met another head on the same cell and was not the longest there.
    HeadOn,
    /// Moved into another snek's body.
    Body,
    /// Ran out of health.
    Starved,
}

impl ClashReason {
    /// How the death reads in a sentence: "<name> hit a wall".
    #[must_use]
    pub const fn text(&self) -> &'static str {
        match self {
            Self::Wall => "hit a wall",
            Self::SelfCollision => "ate itself",
            Self::HeadOn => "lost a head-on collision",
            Self::Body => "ran into another snek",
            Self::Starved => "starved",
        }
    }
}

/// One cell of a death. A dead snek gets one clash per body cell, so the
/// renderer can draw the whole corpse where it fell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clash {
    /// A cell the corpse covered.
    pub pos: Pos,
    /// The dead snek first, then whoever it collided with.
    pub players: Vec<PlayerId>,
    /// What killed it.
    pub reason: ClashReason,
}

/// A player's final standing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Winner {
    /// The player this standing is for.
    pub player: PlayerId,
    /// Turns survived — the ranking metric, as in tac.
    pub survival_turns: u32,
    /// Final length when the game ended (0 if fully dead).
    pub length: u16,
}

/// A game is drawn when nobody outlasted everyone else — the top survival
/// count is shared. Winners are still ranked data; presentation decides
/// whether anyone gets a medal.
#[must_use]
pub fn is_draw(winners: &[Winner]) -> bool {
    winners.len() >= 2 && winners[0].survival_turns == winners[1].survival_turns
}

/// One resolved game tick. `BTreeMap`s keep serialization deterministic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    /// 1 for the starting position, then one more per tick.
    pub turn_number: u32,
    /// Head-first cell lists, like tac's `playerPieces`. Dead sneks are
    /// removed.
    pub snakes: BTreeMap<PlayerId, Vec<Pos>>,
    /// Health of every living snek, 100 down to 1.
    pub health: BTreeMap<PlayerId, u8>,
    /// Living players, in the game's player order.
    pub alive: Vec<PlayerId>,
    /// Food cells, in the order they appeared.
    pub food: Vec<Pos>,
    /// Moves that were applied to produce this turn.
    pub moves: BTreeMap<PlayerId, Dir>,
    /// Everyone who died this tick, a clash per corpse cell.
    pub clashes: Vec<Clash>,
    /// Non-empty exactly when the game is over; sorted best-first.
    pub winners: Vec<Winner>,
}

impl Turn {
    /// Whether this is the last turn of the game.
    #[must_use]
    pub const fn game_over(&self) -> bool {
        !self.winners.is_empty()
    }
}

/// A full game — what gets written to sqlite on completion and replayed
/// in the history UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameRecord {
    /// The settings it was played with.
    pub config: GameConfig,
    /// Everyone who played, in the engine's player order.
    pub players: Vec<PlayerProfile>,
    /// The engine's seed, so the game can be re-simulated from its moves.
    pub seed: u64,
    /// Every turn, from the starting position to the last.
    pub turns: Vec<Turn>,
}
