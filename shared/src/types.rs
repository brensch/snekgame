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
    /// The tick interval the game starts at, in milliseconds. It shortens
    /// as the game goes on: see [`GameConfig::tick_ms_at`].
    pub tick_ms: u32,
    /// What the food looks like this game. Chosen in the lobby and stored
    /// with the record, so a replay shows what the players actually ate.
    pub food_emoji: String,
    /// Cerebral: no clock pushing anyone along. Every turn waits for each
    /// player to choose a move, up to [`CEREBRAL_TURN_MS`], and goes the
    /// moment they all have; `tick_ms` is ignored.
    pub cerebral: bool,
}

/// The longest a cerebral turn waits for its players.
pub const CEREBRAL_TURN_MS: u32 = 5_000;
/// The longest a cerebral turn waits for its bots.
pub const CEREBRAL_BOT_MS: u32 = 1_000;

/// What food was before it was choosable, and what every game played up to
/// then is stamped with.
pub const DEFAULT_FOOD: &str = "🎃";

impl Default for GameConfig {
    fn default() -> Self {
        // tac's "medium" board; 1s ticks for fast turns.
        Self {
            board_width: 13,
            board_height: 13,
            food_emoji: DEFAULT_FOOD.to_owned(),
            tick_ms: 1000,
            cerebral: false,
        }
    }
}

/// How many turns a game takes to reach its fastest.
pub const RAMP_TURNS: u32 = 1000;
/// The tick every game reaches by [`RAMP_TURNS`], whatever speed it started
/// at, and never goes below.
///
/// Any shorter and a phone's own latency starts deciding games, and the
/// bots get no time to think.
pub const FASTEST_TICK_MS: u32 = 200;

impl GameConfig {
    /// The wait for the next turn once `turns_played` turns have been
    /// played (the first wait is the starting interval). Games speed up
    /// gently and steadily: every turn takes the same slice off the wait,
    /// from the starting interval down to the floor at [`RAMP_TURNS`], and
    /// it stays there. The floor is [`FASTEST_TICK_MS`] for every speed (or
    /// the start, should a game start quicker). Integer arithmetic, so every
    /// machine agrees to the millisecond. A cerebral game's turns wait
    /// the same [`CEREBRAL_TURN_MS`] at most, however long it runs.
    #[must_use]
    pub fn tick_ms_at(&self, turns_played: u32) -> u32 {
        if self.cerebral {
            return CEREBRAL_TURN_MS;
        }
        let floor = FASTEST_TICK_MS.min(self.tick_ms);
        let ramp = u64::from(self.tick_ms - floor);
        let done = u64::from(turns_played.min(RAMP_TURNS));
        let taken = ramp * done / u64::from(RAMP_TURNS);
        // `taken` is at most `ramp`, which came from a u32.
        self.tick_ms - u32::try_from(taken).unwrap_or(self.tick_ms - floor)
    }

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
pub const fn is_draw(winners: &[Winner]) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Games speed up turn by turn, reach their floor at turn 1000, and
    /// stay there.
    #[test]
    fn ticks_get_faster_then_level_off() {
        let normal = GameConfig::default();
        assert_eq!(normal.tick_ms_at(0), 1000);
        assert_eq!(normal.tick_ms_at(4), 997, "barely faster at first");
        let mut last = normal.tick_ms_at(0);
        for turn in 1..=RAMP_TURNS {
            let tick = normal.tick_ms_at(turn);
            assert!(tick <= last, "turn {turn} got slower");
            last = tick;
        }
        assert_eq!(normal.tick_ms_at(500), 600, "halfway there at turn 500");
        assert_eq!(normal.tick_ms_at(RAMP_TURNS - 1), 201, "not quite there");
        assert_eq!(normal.tick_ms_at(RAMP_TURNS), FASTEST_TICK_MS, "flat out");
        assert_eq!(
            normal.tick_ms_at(10_000),
            FASTEST_TICK_MS,
            "and it stays there"
        );
        // Every speed ends up in the same place.
        for start in [500, 2000, 10_000] {
            let config = GameConfig {
                tick_ms: start,
                ..GameConfig::default()
            };
            assert_eq!(
                config.tick_ms_at(RAMP_TURNS),
                FASTEST_TICK_MS,
                "from {start}"
            );
        }
        let quickest = GameConfig {
            tick_ms: 250,
            ..GameConfig::default()
        };
        assert_eq!(
            quickest.tick_ms_at(RAMP_TURNS),
            FASTEST_TICK_MS,
            "the quickest start still speeds up"
        );
    }

    /// The lobby's limits: boards 7..=25 a side, ticks 250..=10000 ms, and
    /// one glyph of food (which may be several chars).
    #[test]
    fn config_validation_enforces_the_lobby_limits() {
        let ok = GameConfig::default();
        assert!(ok.valid());
        let with = |f: fn(&mut GameConfig)| {
            let mut c = ok.clone();
            f(&mut c);
            c.valid()
        };
        assert!(with(|c| c.board_width = 7) && with(|c| c.board_height = 25));
        assert!(!with(|c| c.board_width = 6));
        assert!(!with(|c| c.board_height = 26));
        assert!(with(|c| c.tick_ms = 250) && with(|c| c.tick_ms = 10_000));
        assert!(!with(|c| c.tick_ms = 249));
        assert!(!with(|c| c.tick_ms = 10_001));
        assert!(!with(|c| c.food_emoji = String::new()));
        assert!(
            with(|c| c.food_emoji = "👩🏽‍🍳".to_owned()),
            "a ZWJ glyph is one food"
        );
        assert!(!with(|c| c.food_emoji = "pumpkins!".to_owned()));
    }

    /// The wall ring is exactly the perimeter: every listed cell is a wall,
    /// none twice, and nothing inside is.
    #[test]
    fn walls_are_the_perimeter_ring() {
        let c = GameConfig {
            board_width: 9,
            board_height: 7,
            ..GameConfig::default()
        };
        let mut walls = c.walls();
        walls.sort_unstable();
        walls.dedup();
        assert_eq!(walls.len(), 2 * 9 + 2 * 7 - 4);
        assert_eq!(c.cells(), 63);
        for pos in 0..63 {
            assert_eq!(c.is_wall(pos), walls.contains(&pos), "cell {pos}");
        }
    }

    #[test]
    fn directions_and_their_opposites() {
        for d in Dir::ALL {
            let (dx, dy) = d.delta();
            assert_eq!(dx.abs() + dy.abs(), 1, "{d:?} is one step");
            assert_eq!(d.opposite().delta(), (-dx, -dy));
            assert_eq!(d.opposite().opposite(), d);
        }
    }

    #[test]
    fn every_death_reads_differently() {
        let all = [
            ClashReason::Wall,
            ClashReason::SelfCollision,
            ClashReason::HeadOn,
            ClashReason::Body,
            ClashReason::Starved,
        ];
        let mut texts: Vec<&str> = all.iter().map(ClashReason::text).collect();
        texts.sort_unstable();
        texts.dedup();
        assert_eq!(texts.len(), all.len());
    }

    #[test]
    fn bots_are_told_apart_by_id() {
        assert!(is_bot("bot-1"));
        assert!(!is_bot("3fa9c1"));
        assert!(!is_bot("robot-1"));
    }

    /// A draw is a shared top survival count, not any tie further down.
    #[test]
    fn a_draw_is_a_shared_top() {
        let w = |player: &str, survival_turns| Winner {
            player: player.to_owned(),
            survival_turns,
            length: 0,
        };
        assert!(is_draw(&[w("a", 9), w("b", 9)]));
        assert!(!is_draw(&[w("a", 10), w("b", 9), w("c", 9)]));
        assert!(!is_draw(&[w("solo", 40)]));
        assert!(!is_draw(&[]));
    }
}
