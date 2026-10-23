//! The snek rules engine — a Rust port of tac's `SnekProcessor`, adapted from
//! turn-based simultaneous moves to fixed real-time ticks.
//!
//! Rules:
//! - Sneks start length 3, stacked on one cell, placed around the board edges.
//! - Each tick every living snek moves one cell: its submitted direction, else
//!   its previous direction, else the first non-wall direction. Reversing into
//!   your own neck is ignored (you keep going straight).
//! - Perimeter cells are walls; hitting one kills you. (The UI doesn't draw
//!   them — it crops the view to the interior.)
//! - Head-on collisions kill the shortest snek(s); running into a body kills
//!   the runner.
//! - Health starts at 100 and drops 1 per tick; eating food restores it to 100
//!   and grows you by one. 0 health starves you.
//! - Food spawns on a random free cell with 50% probability each tick.
//! - The game ends when at most one snek lives (solo games: when it dies);
//!   players are ranked by turns survived.

use crate::rng::Rng;
use crate::types::{Clash, ClashReason, Dir, GameConfig, GameRecord, PlayerId, Pos, Turn, Winner};
use std::cmp::Reverse;
use std::collections::BTreeMap;

const START_LENGTH: usize = 3;
const MAX_HEALTH: u8 = 100;
/// Official battlesnake standard-mode food settings: keep at least
/// `MIN_FOOD` on the board, otherwise spawn one with 15% chance per turn.
const FOOD_SPAWN_CHANCE: f64 = 0.15;
const MIN_FOOD: usize = 1;

/// The direction a snek starts moving: clockwise around the board, based on
/// where it spawned.
///
/// Of the two clockwise-tangent candidates, pick the one with more room
/// before the wall (so corner spawns don't drive straight into it). Also
/// used by the UI to pre-select the d-pad.
#[must_use]
pub fn clockwise_start_dir(config: &GameConfig, pos: Pos) -> Dir {
    let (w, h) = (
        i32::from(config.board_width),
        i32::from(config.board_height),
    );
    let (x, y) = (i32::from(pos) % w, i32::from(pos) / w);
    // Offset from centre, doubled to stay in integers.
    let (vx, vy) = (2 * x - (w - 1), 2 * y - (h - 1));
    // Clockwise tangent in screen coordinates (y grows downward).
    let (tx, ty) = (-vy, vx);

    let mut candidates: Vec<(i32, i32, Dir)> = Vec::new(); // (room, |component|, dir)
    if tx > 0 {
        candidates.push(((w - 2) - x, tx.abs(), Dir::Right));
    }
    if tx < 0 {
        candidates.push((x - 1, tx.abs(), Dir::Left));
    }
    if ty > 0 {
        candidates.push(((h - 2) - y, ty.abs(), Dir::Down));
    }
    if ty < 0 {
        candidates.push((y - 1, ty.abs(), Dir::Up));
    }
    candidates
        .into_iter()
        .max_by_key(|&(room, comp, _)| (room, comp))
        // Dead centre: arbitrary but deterministic.
        .map_or(Dir::Right, |(_, _, dir)| dir)
}

/// The cell one step from `from` on a `w` x `h` board, or None off the
/// edge. Shared with the bot, whose search must step exactly like this.
pub(crate) fn step(from: Pos, dir: Dir, w: u16, h: u16) -> Option<Pos> {
    let (x, y) = (from % w, from / w);
    match dir {
        Dir::Up => (y > 0).then(|| from - w),
        Dir::Down => (y + 1 < h).then(|| from + w),
        Dir::Left => (x > 0).then(|| from - 1),
        Dir::Right => (x + 1 < w).then(|| from + 1),
    }
}

/// The move that took a snek from `from` to `to`, if they are neighbours.
pub(crate) const fn dir_between(from: Pos, to: Pos, width: u16) -> Option<Dir> {
    let (fx, fy) = (from % width, from / width);
    let (tx, ty) = (to % width, to / width);
    if fx == tx {
        if ty + 1 == fy {
            Some(Dir::Up)
        } else if fy + 1 == ty {
            Some(Dir::Down)
        } else {
            None
        }
    } else if fy == ty {
        if tx + 1 == fx {
            Some(Dir::Left)
        } else if fx + 1 == tx {
            Some(Dir::Right)
        } else {
            None
        }
    } else {
        None
    }
}

/// `a + (b - a) * i / segments` rounded like JS `Math.round` (halves toward
/// +inf), matching tac's `getMidpoints` exactly. Done in integers: with
/// `i < segments` every term is non-negative, so the division floors.
fn lerp_round(a: u16, b: u16, i: u32, segments: u32) -> u16 {
    let doubled = 2 * u32::from(a) * (segments - i) + 2 * u32::from(b) * i + segments;
    // Always between a and b, so it fits.
    u16::try_from(doubled / (2 * segments)).unwrap_or_else(|_| a.max(b))
}

/// One game: its rules, plus the state that isn't in the turns themselves
/// (the random stream and how long everyone has survived).
pub struct Engine {
    /// The settings the game is played with.
    pub config: GameConfig,
    /// Everyone playing. Start positions and tied rankings follow this
    /// order.
    pub players: Vec<PlayerId>,
    seed: u64,
    rng: Rng,
    /// Turns each player has been alive for — the ranking metric.
    survival: BTreeMap<PlayerId, u32>,
}

impl Engine {
    /// A game that hasn't started: call [`Engine::first_turn`] next.
    #[must_use]
    pub fn new(config: GameConfig, players: Vec<PlayerId>, seed: u64) -> Self {
        let survival = players.iter().map(|p| (p.clone(), 0)).collect();
        Self {
            config,
            players,
            seed,
            rng: Rng::new(seed),
            survival,
        }
    }

    /// The seed this game was created with (stored in the game record so a
    /// finished game can be re-simulated).
    #[must_use]
    pub const fn initial_seed(&self) -> u64 {
        self.seed
    }

    /// Re-run a stored game from its inputs. Returns the reconstructed
    /// turns: the starting position, then one per recorded turn after it.
    #[must_use]
    pub fn replay(record: &GameRecord) -> Vec<Turn> {
        let players: Vec<PlayerId> = record.players.iter().map(|p| p.id.clone()).collect();
        let mut engine = Self::new(record.config.clone(), players, record.seed);
        let mut turns = Vec::with_capacity(record.turns.len());
        let mut last = engine.first_turn();
        for recorded in record.turns.iter().skip(1) {
            let next = engine.next_turn(&last, &recorded.moves);
            turns.push(std::mem::replace(&mut last, next));
        }
        turns.push(last);
        turns
    }

    fn width(&self) -> u16 {
        u16::from(self.config.board_width)
    }

    fn height(&self) -> u16 {
        u16::from(self.config.board_height)
    }

    fn pos(&self, x: u16, y: u16) -> Pos {
        y * self.width() + x
    }

    fn xy(&self, pos: Pos) -> (u16, u16) {
        (pos % self.width(), pos / self.width())
    }

    // ------------------------------------------------------------------
    // Game start
    // ------------------------------------------------------------------

    /// The starting position: every snek stacked on its spawn cell, the
    /// starting food placed. Call once, before any [`Engine::next_turn`].
    pub fn first_turn(&mut self) -> Turn {
        let snakes = self.initial_snakes();
        let food = self.initial_food(&snakes);
        let alive: Vec<PlayerId> = self.players.clone();
        for p in &alive {
            if let Some(turns) = self.survival.get_mut(p) {
                *turns += 1;
            }
        }
        Turn {
            turn_number: 1,
            health: alive.iter().map(|p| (p.clone(), MAX_HEALTH)).collect(),
            snakes,
            alive,
            food,
            moves: BTreeMap::new(),
            clashes: Vec::new(),
            winners: Vec::new(),
        }
    }

    fn initial_snakes(&self) -> BTreeMap<PlayerId, Vec<Pos>> {
        let positions = self.starting_positions();
        self.players
            .iter()
            .zip(positions)
            .map(|(p, (x, y))| (p.clone(), vec![self.pos(x, y); START_LENGTH]))
            .collect()
    }

    /// Port of tac's `generateStartingPositions`: corners of an inset ring,
    /// then edge midpoints at increasing depth, then inner rings.
    fn starting_positions(&self) -> Vec<(u16, u16)> {
        let (w, h) = (self.width(), self.height());
        let wanted = self.players.len();

        let start_x = if w.saturating_sub(1) % 4 == 0 { 2 } else { 1 };
        let start_y = if h.saturating_sub(1) % 4 == 0 { 2 } else { 1 };
        let end_x = w.saturating_sub(start_x + 1);
        let end_y = h.saturating_sub(start_y + 1);

        let mut positions: Vec<(u16, u16)> = vec![
            (start_x, start_y),
            (end_x, start_y),
            (start_x, end_y),
            (end_x, end_y),
        ];

        let edges = [
            ((start_x, start_y), (end_x, start_y)),
            ((end_x, start_y), (end_x, end_y)),
            ((end_x, end_y), (start_x, end_y)),
            ((start_x, end_y), (start_x, start_y)),
        ];

        let mut depth = 0u32;
        while positions.len() < wanted {
            let mut added = 0;
            let segments = 2u32.pow(depth + 1);
            for &((sx, sy), (ex, ey)) in &edges {
                let mut odd = 1;
                while odd < segments {
                    let x = lerp_round(sx, ex, odd, segments);
                    let y = lerp_round(sy, ey, odd, segments);
                    if !positions.contains(&(x, y)) {
                        positions.push((x, y));
                        added += 1;
                    }
                    odd += 2;
                }
            }
            depth += 1;
            if added == 0 {
                break;
            }
        }

        // Inner rings if the edges ran out of room.
        let (mut left, mut top) = (3, 3);
        let (mut right, mut bottom) = (w.saturating_sub(4), h.saturating_sub(4));
        while positions.len() < wanted && left < right && top < bottom {
            let mut ring = vec![(left, top), (right, top), (left, bottom), (right, bottom)];
            if right - left > 2 {
                ring.push((left.midpoint(right), top));
                ring.push((left.midpoint(right), bottom));
            }
            if bottom - top > 2 {
                ring.push((left, top.midpoint(bottom)));
                ring.push((right, top.midpoint(bottom)));
            }
            for p in ring {
                if !positions.contains(&p) {
                    positions.push(p);
                }
            }
            left += 2;
            top += 2;
            right -= 2;
            bottom -= 2;
        }

        positions.truncate(wanted);
        positions
    }

    /// Official battlesnake `PlaceFoodFixed`: one food exactly two moves
    /// diagonally from each snek — never toward the centre, never in a
    /// corner, picked at random among the legal diagonals — plus one in the
    /// dead centre "for dramatic purposes".
    fn initial_food(&mut self, snakes: &BTreeMap<PlayerId, Vec<Pos>>) -> Vec<Pos> {
        let (w, h) = (self.width(), self.height());
        let (cx, cy) = (w.saturating_sub(1) / 2, h.saturating_sub(1) / 2);
        let mut occupied: Vec<Pos> = snakes.values().flatten().copied().collect();
        let mut food: Vec<Pos> = Vec::new();

        let heads: Vec<(u16, u16)> = snakes
            .values()
            .filter_map(|s| s.first().map(|&head| self.xy(head)))
            .collect();
        for (hx, hy) in heads {
            let mut available: Vec<Pos> = Vec::new();
            for (dx, dy) in [(-1, -1), (-1, 1), (1, -1), (1, 1)] {
                let (Some(fx), Some(fy)) = (hx.checked_add_signed(dx), hy.checked_add_signed(dy))
                else {
                    continue;
                };
                // Playable area only — our perimeter ring is wall.
                if fx < 1 || fy < 1 || fx + 2 > w || fy + 2 > h {
                    continue;
                }
                let p = self.pos(fx, fy);
                if food.contains(&p) || occupied.contains(&p) {
                    continue;
                }
                // Must sit further from the centre than the head on an axis.
                let away = (fx < hx && hx < cx)
                    || (cx < hx && hx < fx)
                    || (fy < hy && hy < cy)
                    || (cy < hy && hy < fy);
                if !away {
                    continue;
                }
                // Never in a corner of the playable area.
                if (fx == 1 || fx == w - 2) && (fy == 1 || fy == h - 2) {
                    continue;
                }
                available.push(p);
            }
            if !available.is_empty() {
                let pick = self.rng.below(available.len());
                let p = available[pick];
                food.push(p);
                occupied.push(p);
            }
        }

        // Always one in the dead centre.
        let centre = self.pos(cx, cy);
        if !occupied.contains(&centre) {
            food.push(centre);
        }
        food
    }

    // ------------------------------------------------------------------
    // Tick resolution
    // ------------------------------------------------------------------

    /// Resolve one tick from `prev` with the moves players submitted for it
    /// (anyone missing keeps going the way they were heading).
    pub fn next_turn(&mut self, prev: &Turn, submitted: &BTreeMap<PlayerId, Dir>) -> Turn {
        let mut snakes = prev.snakes.clone();
        let mut health = prev.health.clone();
        let mut food = prev.food.clone();
        let mut alive = prev.alive.clone();
        let mut clashes: Vec<Clash> = Vec::new();

        let (applied, dead) = self.move_snakes(&prev.alive, submitted, &mut snakes, &mut clashes);
        // Phase 1 deaths (walls, self-collisions) resolve first and their
        // corpses leave the board: a snake that died this tick cannot kill
        // anyone else.
        remove_dead(&mut snakes, &mut health, &mut alive, &dead);

        let dead = collide(&snakes, &mut clashes);
        remove_dead(&mut snakes, &mut health, &mut alive, &dead);

        let starved = feed(&alive, &mut snakes, &mut health, &mut food, &mut clashes);
        remove_dead(&mut snakes, &mut health, &mut alive, &starved);

        // --- Spawn food (battlesnake standard rules) -----------------------
        if food.len() < MIN_FOOD || self.rng.chance(FOOD_SPAWN_CHANCE) {
            let free = self.free_cells(&snakes, &food);
            if !free.is_empty() {
                let pick = self.rng.below(free.len());
                food.push(free[pick]);
            }
        }

        // --- Survival + winners -------------------------------------------
        for p in &alive {
            if let Some(turns) = self.survival.get_mut(p) {
                *turns += 1;
            }
        }

        let over_threshold = usize::from(self.players.len() > 1);
        let winners = if alive.len() <= over_threshold {
            self.ranking(&snakes)
        } else {
            Vec::new()
        };

        Turn {
            turn_number: prev.turn_number + 1,
            snakes,
            health,
            alive,
            food,
            moves: applied,
            clashes,
            winners,
        }
    }

    /// Move every living snek one cell: its submitted direction, else the
    /// way it was going, else clockwise. Returns the direction each one
    /// actually took, and who died doing it (into a wall or itself).
    fn move_snakes(
        &self,
        players: &[PlayerId],
        submitted: &BTreeMap<PlayerId, Dir>,
        snakes: &mut BTreeMap<PlayerId, Vec<Pos>>,
        clashes: &mut Vec<Clash>,
    ) -> (BTreeMap<PlayerId, Dir>, Vec<PlayerId>) {
        let mut applied: BTreeMap<PlayerId, Dir> = BTreeMap::new();
        let mut dead: Vec<PlayerId> = Vec::new();
        for player in players {
            let Some(snake) = snakes.get_mut(player) else {
                continue;
            };
            let Some(&head) = snake.first() else {
                continue;
            };
            let last_dir = last_direction(snake, self.width());

            let dir = submitted
                .get(player)
                .copied()
                // Ignore reversing straight into your own neck.
                .filter(|&d| last_dir != Some(d.opposite()))
                .or(last_dir)
                // Stacked snek with no history: head off clockwise.
                .unwrap_or_else(|| clockwise_start_dir(&self.config, head));
            applied.insert(player.clone(), dir);

            let target = self.target(head, dir);
            let target = match target {
                Some(t) if !self.config.is_wall(t) => t,
                _ => {
                    // Off the board or into the wall: die where you stood.
                    // The corpse is removed before cross-snake collisions —
                    // running into a snake that died this tick is safe.
                    dead.push(player.clone());
                    clashes.extend(snake.iter().map(|&pos| Clash {
                        pos,
                        players: vec![player.clone()],
                        reason: ClashReason::Wall,
                    }));
                    continue;
                }
            };

            snake.pop();
            if snake.contains(&target) {
                dead.push(player.clone());
                clashes.extend(snake.iter().map(|&pos| Clash {
                    pos,
                    players: vec![player.clone()],
                    reason: ClashReason::SelfCollision,
                }));
                continue;
            }
            snake.insert(0, target);
        }
        (applied, dead)
    }

    /// Everyone, best first: by turns survived, ties in player order.
    fn ranking(&self, snakes: &BTreeMap<PlayerId, Vec<Pos>>) -> Vec<Winner> {
        let mut winners: Vec<Winner> = self
            .players
            .iter()
            .map(|p| Winner {
                player: p.clone(),
                survival_turns: self.survival[p],
                // A snek can't outgrow the board, which has far fewer cells.
                length: snakes
                    .get(p)
                    .map_or(0, |s| u16::try_from(s.len()).unwrap_or(u16::MAX)),
            })
            .collect();
        winners.sort_by_key(|w| Reverse(w.survival_turns));
        winners
    }

    fn target(&self, from: Pos, dir: Dir) -> Option<Pos> {
        step(from, dir, self.width(), self.height())
    }

    fn free_cells(&self, snakes: &BTreeMap<PlayerId, Vec<Pos>>, food: &[Pos]) -> Vec<Pos> {
        let mut occupied = vec![false; self.config.cells()];
        for &pos in snakes.values().flatten().chain(food.iter()) {
            occupied[usize::from(pos)] = true;
        }
        (0..self.width() * self.height())
            .filter(|&p| !occupied[usize::from(p)] && !self.config.is_wall(p))
            .collect()
    }
}

/// Head-on and body collisions among the survivors of the move, all at
/// once. Returns who died.
fn collide(snakes: &BTreeMap<PlayerId, Vec<Pos>>, clashes: &mut Vec<Clash>) -> Vec<PlayerId> {
    let mut dead: Vec<PlayerId> = Vec::new();
    let mut heads_at: BTreeMap<Pos, Vec<PlayerId>> = BTreeMap::new();
    let mut occupants_at: BTreeMap<Pos, Vec<PlayerId>> = BTreeMap::new();
    for (player, snake) in snakes {
        for (i, &pos) in snake.iter().enumerate() {
            occupants_at.entry(pos).or_default().push(player.clone());
            if i == 0 {
                heads_at.entry(pos).or_default().push(player.clone());
            }
        }
    }

    for (&pos, players_here) in &heads_at {
        match players_here.as_slice() {
            [p] => {
                let others: Vec<PlayerId> = occupants_at[&pos]
                    .iter()
                    .filter(|o| *o != p)
                    .cloned()
                    .collect();
                if !others.is_empty() && !dead.contains(p) {
                    dead.push(p.clone());
                    let mut involved = vec![p.clone()];
                    involved.extend(others);
                    clashes.extend(snakes[p].iter().map(|&pos| Clash {
                        pos,
                        players: involved.clone(),
                        reason: ClashReason::Body,
                    }));
                }
            }
            meeting => {
                // Head-on: the shortest snek(s) die.
                let Some(min_len) = meeting.iter().map(|p| snakes[p].len()).min() else {
                    continue;
                };
                for p in meeting {
                    if snakes[p].len() == min_len && !dead.contains(p) {
                        dead.push(p.clone());
                        // Dead player first, so renderers know whose corpse
                        // (and head cell) this clash trail belongs to.
                        let mut involved = vec![p.clone()];
                        involved.extend(meeting.iter().filter(|o| *o != p).cloned());
                        clashes.extend(snakes[p].iter().map(|&pos| Clash {
                            pos,
                            players: involved.clone(),
                            reason: ClashReason::HeadOn,
                        }));
                    }
                }
            }
        }
    }
    dead
}

/// Eating and starvation: a head on food eats it (growing by one and
/// healing fully), every other snek loses one health. Returns who starved.
fn feed(
    alive: &[PlayerId],
    snakes: &mut BTreeMap<PlayerId, Vec<Pos>>,
    health: &mut BTreeMap<PlayerId, u8>,
    food: &mut Vec<Pos>,
    clashes: &mut Vec<Clash>,
) -> Vec<PlayerId> {
    let mut starved: Vec<PlayerId> = Vec::new();
    for player in alive {
        let Some(snake) = snakes.get_mut(player) else {
            continue;
        };
        let (Some(&head), Some(&tail)) = (snake.first(), snake.last()) else {
            continue;
        };
        if let Some(i) = food.iter().position(|&f| f == head) {
            food.remove(i);
            snake.push(tail);
            health.insert(player.clone(), MAX_HEALTH);
        } else if let Some(hp) = health.get_mut(player) {
            *hp = hp.saturating_sub(1);
            if *hp == 0 {
                starved.push(player.clone());
                clashes.extend(snake.iter().map(|&pos| Clash {
                    pos,
                    players: vec![player.clone()],
                    reason: ClashReason::Starved,
                }));
            }
        }
    }
    starved
}

fn last_direction(snake: &[Pos], width: u16) -> Option<Dir> {
    let (&head, &neck) = (snake.first()?, snake.get(1)?);
    if head == neck {
        return None; // still stacked at start
    }
    dir_between(neck, head, width)
}

fn remove_dead(
    snakes: &mut BTreeMap<PlayerId, Vec<Pos>>,
    health: &mut BTreeMap<PlayerId, u8>,
    alive: &mut Vec<PlayerId>,
    dead: &[PlayerId],
) {
    for p in dead {
        snakes.remove(p);
        health.remove(p);
        alive.retain(|a| a != p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DEFAULT_FOOD, PlayerProfile};

    fn config() -> GameConfig {
        GameConfig {
            board_width: 13,
            board_height: 13,
            tick_ms: 1000,
            food_emoji: DEFAULT_FOOD.to_owned(),
        }
    }

    fn two_player_engine() -> Engine {
        Engine::new(config(), vec!["a".into(), "b".into()], 42)
    }

    fn moves(pairs: &[(&str, Dir)]) -> BTreeMap<PlayerId, Dir> {
        pairs.iter().map(|&(p, d)| (p.to_owned(), d)).collect()
    }

    /// Official battlesnake start food: one per snek exactly two moves
    /// diagonally away, never toward the centre, never in a corner, plus
    /// one dead centre.
    #[test]
    fn start_food_follows_battlesnake_rules() {
        let cfg = config();
        let players: Vec<PlayerId> = (0..4).map(|i| format!("p{i}")).collect();
        let mut e = Engine::new(cfg.clone(), players, 3);
        let t = e.first_turn();
        let (w, h) = (i32::from(cfg.board_width), i32::from(cfg.board_height));
        let (cx, cy) = ((w - 1) / 2, (h - 1) / 2);
        let xy = |p: Pos| (i32::from(p) % w, i32::from(p) / w);

        assert!(
            t.food.iter().any(|&f| xy(f) == (cx, cy)),
            "centre food is mandatory"
        );
        assert_eq!(
            t.food.len(),
            5,
            "one per snek plus the centre: {:?}",
            t.food
        );

        let heads: Vec<(i32, i32)> = t.snakes.values().map(|s| xy(s[0])).collect();
        for &f in &t.food {
            let (fx, fy) = xy(f);
            if (fx, fy) == (cx, cy) {
                continue;
            }
            // Never a corner of the playable area.
            assert!(
                !((fx == 1 || fx == w - 2) && (fy == 1 || fy == h - 2)),
                "food {:?} sits in a corner",
                (fx, fy)
            );
            // Exactly two moves diagonally from some head, and away from centre.
            let owner = heads
                .iter()
                .find(|&&(hx, hy)| (fx - hx).abs() == 1 && (fy - hy).abs() == 1)
                .copied();
            let (hx, hy) =
                owner.unwrap_or_else(|| panic!("food {:?} is not diagonal to any head", (fx, fy)));
            let away = (fx < hx && hx < cx)
                || (cx < hx && hx < fx)
                || (fy < hy && hy < cy)
                || (cy < hy && hy < fy);
            assert!(away, "food {:?} sits toward the centre", (fx, fy));
        }
    }

    #[test]
    fn first_turn_setup() {
        let mut e = two_player_engine();
        let t = e.first_turn();
        assert_eq!(t.turn_number, 1);
        assert_eq!(t.alive.len(), 2);
        for snake in t.snakes.values() {
            assert_eq!(snake.len(), 3);
            assert_eq!(snake[0], snake[1]); // stacked
            assert!(!config().is_wall(snake[0]));
        }
        // centre food + up to one per snek
        assert!(!t.food.is_empty());
        assert!(t.health.values().all(|&h| h == 100));
    }

    #[test]
    fn starting_positions_distinct_for_many_players() {
        let players: Vec<PlayerId> = (0..8).map(|i| format!("p{i}")).collect();
        let mut e = Engine::new(config(), players, 1);
        let t = e.first_turn();
        let heads: Vec<Pos> = t.snakes.values().map(|s| s[0]).collect();
        let mut deduped = heads.clone();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(heads.len(), deduped.len(), "heads must not overlap");
    }

    #[test]
    fn snek_moves_and_health_drops() {
        let mut e = two_player_engine();
        let t1 = e.first_turn();
        let head_a = t1.snakes["a"][0];
        let t2 = e.next_turn(&t1, &moves(&[("a", Dir::Right), ("b", Dir::Left)]));
        if t2.alive.contains(&"a".to_owned()) {
            assert_eq!(t2.snakes["a"][0], head_a + 1);
            assert_eq!(t2.snakes["a"].len(), 3);
            assert!(t2.health["a"] == 99 || t2.health["a"] == 100);
        }
    }

    #[test]
    fn missing_move_continues_last_direction() {
        let mut e = two_player_engine();
        let t1 = e.first_turn();
        let t2 = e.next_turn(&t1, &moves(&[("a", Dir::Right), ("b", Dir::Left)]));
        let head = t2.snakes["a"][0];
        let t3 = e.next_turn(&t2, &moves(&[("b", Dir::Left)]));
        assert_eq!(t3.moves["a"], Dir::Right);
        assert_eq!(t3.snakes["a"][0], head + 1);
    }

    #[test]
    fn reverse_input_is_ignored() {
        let mut e = two_player_engine();
        let t1 = e.first_turn();
        let t2 = e.next_turn(&t1, &moves(&[("a", Dir::Right), ("b", Dir::Left)]));
        let t3 = e.next_turn(&t2, &moves(&[("a", Dir::Left), ("b", Dir::Left)]));
        assert_eq!(t3.moves["a"], Dir::Right, "reverse should be ignored");
    }

    #[test]
    fn wall_collision_kills() {
        let mut e = two_player_engine();
        let mut t = e.first_turn();
        // March a into the right wall.
        for _ in 0..15 {
            if t.game_over() {
                break;
            }
            t = e.next_turn(&t, &moves(&[("a", Dir::Right), ("b", Dir::Down)]));
        }
        assert!(!t.alive.contains(&"a".to_owned()));
    }

    #[test]
    fn eating_food_grows_and_heals() {
        let cfg = config();
        let mut e = Engine::new(cfg.clone(), vec!["solo".into()], 7);
        let t1 = e.first_turn();
        // Walk the snek onto a known food cell.
        let food = t1.food[0];
        let head = t1.snakes["solo"][0];
        let width = u16::from(cfg.board_width);
        let (food_x, food_y) = (food % width, food / width);

        let mut t = t1;
        let (mut x, mut y) = (head % width, head / width);
        // Move horizontally then vertically toward the food.
        for _ in 0..40 {
            if t.game_over() {
                break;
            }
            let dir = if x != food_x {
                if food_x > x { Dir::Right } else { Dir::Left }
            } else if y != food_y {
                if food_y > y { Dir::Down } else { Dir::Up }
            } else {
                break;
            };
            t = e.next_turn(&t, &moves(&[("solo", dir)]));
            if let Some(s) = t.snakes.get("solo") {
                (x, y) = (s[0] % width, s[0] / width);
            }
            if x == food_x && y == food_y {
                break;
            }
        }
        assert!(!t.game_over(), "snek died on the way to food");
        assert_eq!(t.snakes["solo"].len(), 4, "should have grown");
        assert_eq!(t.health["solo"], 100, "should be healed");
    }

    #[test]
    fn everyone_starts_clockwise() {
        // 8 players on a 13x13 board: 4 corners + 4 edge midpoints.
        let players: Vec<PlayerId> = (0..8).map(|i| format!("p{i}")).collect();
        let mut e = Engine::new(config(), players.clone(), 5);
        let t1 = e.first_turn();

        // Expected clockwise headings by spawn position.
        let expected = |pos: Pos| {
            let (x, y) = (pos % 13, pos / 13);
            match (x, y) {
                // Top-left corner, top mid.
                (2 | 6, 2) => Dir::Right,
                // Top-right corner, right mid.
                (10, 2 | 6) => Dir::Down,
                // Bottom-left corner, left mid.
                (2, 10 | 6) => Dir::Up,
                // Bottom-right corner, bottom mid.
                (10 | 6, 10) => Dir::Left,
                other => panic!("unexpected spawn position {other:?}"),
            }
        };
        let heads: BTreeMap<PlayerId, Pos> =
            t1.snakes.iter().map(|(p, s)| (p.clone(), s[0])).collect();

        // Nobody submits a move; everyone should head off clockwise, safely.
        let t2 = e.next_turn(&t1, &BTreeMap::new());
        assert_eq!(t2.alive.len(), 8, "clockwise starts must be safe");
        for p in &players {
            assert_eq!(
                t2.moves[p],
                expected(heads[p]),
                "player at {:?} went the wrong way",
                (heads[p] % 13, heads[p] / 13)
            );
            assert_eq!(t2.moves[p], clockwise_start_dir(&config(), heads[p]));
        }
    }

    #[test]
    fn food_replenishes_to_minimum() {
        let mut e = two_player_engine();
        let mut t = e.first_turn();
        // Force the board to zero food and step: min-food rule must restock.
        t.food.clear();
        let t2 = e.next_turn(&t, &moves(&[("a", Dir::Right), ("b", Dir::Down)]));
        assert!(!t2.food.is_empty(), "min food must be maintained");
    }

    /// Regression of game #99's ending: A drives into the wall on the same
    /// tick B steps onto A's tail cell. A's corpse must not kill B.
    #[test]
    fn same_tick_corpse_is_not_lethal() {
        let a: PlayerId = "a".into();
        let b: PlayerId = "b".into();
        let mut engine = Engine::new(config(), vec![a.clone(), b.clone()], 1);
        let mut snakes = BTreeMap::new();
        // A: head 50 = (11,3), the last playable column; Right = wall.
        snakes.insert(a.clone(), vec![50, 49, 48, 47, 60, 59, 58, 57]);
        // B: head 44 = (5,3)... Down would be 57 only from 44? 44+13=57 ✓.
        snakes.insert(b.clone(), vec![44, 31, 18, 19, 20, 33]);
        let turn = Turn {
            turn_number: 57,
            health: [(a.clone(), 90u8), (b.clone(), 90u8)].into_iter().collect(),
            snakes,
            alive: vec![a.clone(), b.clone()],
            food: vec![],
            moves: BTreeMap::new(),
            clashes: Vec::new(),
            winners: Vec::new(),
        };
        let next = engine.next_turn(&turn, &moves(&[("a", Dir::Right), ("b", Dir::Down)]));
        assert!(!next.alive.contains(&a), "A hit the wall and must die");
        assert!(
            next.alive.contains(&b),
            "B stepped onto a same-tick corpse and must survive: {:?}",
            next.clashes
        );
        assert_eq!(next.snakes[&b][0], 57, "B lands on the vacated cell");
        assert!(next.game_over(), "sole survivor ends the game");
        assert_eq!(next.winners.first().map(|w| w.player.clone()), Some(b));
    }

    #[test]
    fn game_ends_with_ranked_winners() {
        let mut e = two_player_engine();
        let mut t = e.first_turn();
        // March both into walls until the game ends.
        for _ in 0..40 {
            if t.game_over() {
                break;
            }
            t = e.next_turn(&t, &moves(&[("a", Dir::Right), ("b", Dir::Right)]));
        }
        assert!(t.game_over());
        assert_eq!(t.winners.len(), 2);
        assert!(t.winners[0].survival_turns >= t.winners[1].survival_turns);
    }

    #[test]
    fn replay_reconstructs_identical_turns() {
        let mut e = two_player_engine();
        let mut turns = vec![e.first_turn()];
        for i in 0..10 {
            if turns.last().unwrap().game_over() {
                break;
            }
            let dir = if i % 3 == 0 { Dir::Down } else { Dir::Right };
            let next = e.next_turn(
                turns.last().unwrap(),
                &moves(&[("a", dir), ("b", Dir::Down)]),
            );
            turns.push(next);
        }
        let record = GameRecord {
            config: config(),
            players: vec![
                PlayerProfile {
                    id: "a".into(),
                    name: "A".into(),
                    emoji: "🦍".into(),
                    colour: "hsl(0, 85%, 60%)".into(),
                },
                PlayerProfile {
                    id: "b".into(),
                    name: "B".into(),
                    emoji: "🦄".into(),
                    colour: "hsl(120, 85%, 60%)".into(),
                },
            ],
            seed: 42,
            turns: turns.clone(),
        };
        assert_eq!(Engine::replay(&record), turns);
    }

    #[test]
    fn postcard_roundtrip() {
        let mut e = two_player_engine();
        let t = e.first_turn();
        let bytes = postcard::to_allocvec(&t).unwrap();
        let back: Turn = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(t, back);
    }
}
