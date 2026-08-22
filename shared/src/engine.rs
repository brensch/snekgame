//! The snek rules engine — a Rust port of tac's SnekProcessor, adapted from
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
use crate::types::*;
use std::collections::BTreeMap;

const START_LENGTH: usize = 3;
const MAX_HEALTH: u8 = 100;
/// Official battlesnake standard-mode food settings: keep at least
/// `MIN_FOOD` on the board, otherwise spawn one with 15% chance per turn.
const FOOD_SPAWN_CHANCE: f64 = 0.15;
const MIN_FOOD: usize = 1;

/// The direction a snek starts moving: clockwise around the board, based on
/// where it spawned. Of the two clockwise-tangent candidates, pick the one
/// with more room before the wall (so corner spawns don't drive straight
/// into it). Also used by the UI to pre-select the d-pad.
pub fn clockwise_start_dir(config: &GameConfig, pos: Pos) -> Dir {
    let (w, h) = (config.board_width as i16, config.board_height as i16);
    let (x, y) = (pos as i16 % w, pos as i16 / w);
    // Offset from centre, doubled to stay in integers.
    let (vx, vy) = (2 * x - (w - 1), 2 * y - (h - 1));
    // Clockwise tangent in screen coordinates (y grows downward).
    let (tx, ty) = (-vy, vx);

    let mut candidates: Vec<(i16, i16, Dir)> = Vec::new(); // (room, |component|, dir)
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
        .map(|(_, _, dir)| dir)
        .unwrap_or(Dir::Right) // dead centre: arbitrary but deterministic
}

pub struct Engine {
    pub config: GameConfig,
    pub players: Vec<PlayerId>,
    seed: u64,
    rng: Rng,
    /// Turns each player has been alive for — the ranking metric.
    survival: BTreeMap<PlayerId, u32>,
}

impl Engine {
    pub fn new(config: GameConfig, players: Vec<PlayerId>, seed: u64) -> Self {
        let survival = players.iter().map(|p| (p.clone(), 0)).collect();
        Engine {
            config,
            players,
            seed,
            rng: Rng::new(seed),
            survival,
        }
    }

    /// The seed this game was created with (stored in the game record so a
    /// finished game can be re-simulated).
    pub fn initial_seed(&self) -> u64 {
        self.seed
    }

    /// Re-run a stored game from its inputs. Returns the reconstructed turns.
    pub fn replay(record: &GameRecord) -> Vec<Turn> {
        let players: Vec<PlayerId> = record.players.iter().map(|p| p.id.clone()).collect();
        let mut engine = Engine::new(record.config, players, record.seed);
        let mut turns = vec![engine.first_turn()];
        for recorded in &record.turns[1..] {
            let next = engine.next_turn(turns.last().unwrap(), &recorded.moves);
            turns.push(next);
        }
        turns
    }

    fn width(&self) -> i16 {
        self.config.board_width as i16
    }

    fn height(&self) -> i16 {
        self.config.board_height as i16
    }

    fn pos(&self, x: i16, y: i16) -> Pos {
        (y * self.width() + x) as Pos
    }

    fn xy(&self, pos: Pos) -> (i16, i16) {
        (pos as i16 % self.width(), pos as i16 / self.width())
    }

    // ------------------------------------------------------------------
    // Game start
    // ------------------------------------------------------------------

    pub fn first_turn(&mut self) -> Turn {
        let snakes = self.initial_snakes();
        let food = self.initial_food(&snakes);
        let alive: Vec<PlayerId> = self.players.clone();
        for p in &alive {
            *self.survival.get_mut(p).unwrap() += 1;
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

    /// Port of tac's generateStartingPositions: corners of an inset ring,
    /// then edge midpoints at increasing depth, then inner rings.
    fn starting_positions(&self) -> Vec<(i16, i16)> {
        let (w, h) = (self.width(), self.height());
        let n = self.players.len();

        let start_x = if (w - 1) % 4 == 0 { 2 } else { 1 };
        let start_y = if (h - 1) % 4 == 0 { 2 } else { 1 };
        let end_x = w - start_x - 1;
        let end_y = h - start_y - 1;

        let mut positions: Vec<(i16, i16)> = vec![
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
        while positions.len() < n {
            let mut added = 0;
            let segments = 2i32.pow(depth + 1);
            for &((sx, sy), (ex, ey)) in &edges {
                let mut i = 1;
                while i < segments {
                    // JS Math.round semantics (half rounds toward +inf),
                    // matching tac's getMidpoints exactly.
                    let js_round = |v: f64| (v + 0.5).floor() as i16;
                    let x = js_round(sx as f64 + (ex - sx) as f64 * i as f64 / segments as f64);
                    let y = js_round(sy as f64 + (ey - sy) as f64 * i as f64 / segments as f64);
                    if !positions.contains(&(x, y)) {
                        positions.push((x, y));
                        added += 1;
                    }
                    i += 2;
                }
            }
            depth += 1;
            if added == 0 {
                break;
            }
        }

        // Inner rings if the edges ran out of room.
        let (mut isx, mut isy, mut iex, mut iey) = (3, 3, w - 4, h - 4);
        while positions.len() < n && isx < iex && isy < iey {
            let mut ring = vec![(isx, isy), (iex, isy), (isx, iey), (iex, iey)];
            if iex - isx > 2 {
                ring.push(((isx + iex) / 2, isy));
                ring.push(((isx + iex) / 2, iey));
            }
            if iey - isy > 2 {
                ring.push((isx, (isy + iey) / 2));
                ring.push((iex, (isy + iey) / 2));
            }
            for p in ring {
                if !positions.contains(&p) {
                    positions.push(p);
                }
            }
            isx += 2;
            isy += 2;
            iex -= 2;
            iey -= 2;
        }

        positions.truncate(n);
        positions
    }

    /// Port of initializeFood: one in the centre plus one diagonal to each
    /// snek's head.
    fn initial_food(&self, snakes: &BTreeMap<PlayerId, Vec<Pos>>) -> Vec<Pos> {
        let (w, h) = (self.width(), self.height());
        let mut occupied: Vec<Pos> = snakes.values().flatten().copied().collect();
        let mut food = Vec::new();

        let centre = self.pos(w / 2, h / 2);
        if !occupied.contains(&centre) {
            food.push(centre);
            occupied.push(centre);
        }

        for snake in snakes.values() {
            let (hx, hy) = self.xy(snake[0]);
            for (dx, dy) in [(1, 1), (1, -1), (-1, 1), (-1, -1)] {
                let (fx, fy) = (hx + dx, hy + dy);
                if fx >= 1 && fx < w - 1 && fy >= 1 && fy < h - 1 {
                    let p = self.pos(fx, fy);
                    if !occupied.contains(&p) {
                        food.push(p);
                        occupied.push(p);
                        break;
                    }
                }
            }
        }
        food
    }

    // ------------------------------------------------------------------
    // Tick resolution
    // ------------------------------------------------------------------

    pub fn next_turn(&mut self, prev: &Turn, submitted: &BTreeMap<PlayerId, Dir>) -> Turn {
        let mut snakes = prev.snakes.clone();
        let mut health = prev.health.clone();
        let mut food = prev.food.clone();
        let mut alive = prev.alive.clone();
        let mut clashes: Vec<Clash> = Vec::new();
        let mut dead: Vec<PlayerId> = Vec::new();
        let mut applied: BTreeMap<PlayerId, Dir> = BTreeMap::new();

        // --- Move every living snek ---------------------------------------
        for player in prev.alive.clone() {
            let snake = snakes.get_mut(&player).unwrap();
            let head = snake[0];
            let last_dir = last_direction(snake, self.config.board_width as i16);

            let mut dir = submitted.get(&player).copied();
            // Ignore reversing straight into your own neck.
            if let (Some(d), Some(last)) = (dir, last_dir) {
                if d == last.opposite() {
                    dir = None;
                }
            }
            let dir = dir
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
                    let body = snake.clone();
                    clashes.extend(body.into_iter().map(|pos| Clash {
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
                let body = snake.clone();
                clashes.extend(body.into_iter().map(|pos| Clash {
                    pos,
                    players: vec![player.clone()],
                    reason: ClashReason::SelfCollision,
                }));
                continue;
            }
            snake.insert(0, target);
        }

        // Phase 1 deaths (walls, self-collisions) resolve first and their
        // corpses leave the board: a snake that died this tick cannot kill
        // anyone else.
        remove_dead(&mut snakes, &mut health, &mut alive, &dead);
        dead.clear();

        // --- Cross-snek collisions (among survivors, simultaneously) ------
        let mut heads_at: BTreeMap<Pos, Vec<PlayerId>> = BTreeMap::new();
        let mut occupants_at: BTreeMap<Pos, Vec<PlayerId>> = BTreeMap::new();
        for (player, snake) in &snakes {
            for (i, &pos) in snake.iter().enumerate() {
                occupants_at.entry(pos).or_default().push(player.clone());
                if i == 0 {
                    heads_at.entry(pos).or_default().push(player.clone());
                }
            }
        }

        for (&pos, players_here) in &heads_at {
            if players_here.len() > 1 {
                // Head-on: the shortest snek(s) die.
                let min_len = players_here
                    .iter()
                    .map(|p| snakes[p].len())
                    .min()
                    .unwrap();
                for p in players_here {
                    if snakes[p].len() == min_len && !dead.contains(p) {
                        dead.push(p.clone());
                        // Dead player first, so renderers know whose corpse
                        // (and head cell) this clash trail belongs to.
                        let mut involved = vec![p.clone()];
                        involved.extend(players_here.iter().filter(|o| *o != p).cloned());
                        clashes.extend(snakes[p].iter().map(|&pos| Clash {
                            pos,
                            players: involved.clone(),
                            reason: ClashReason::HeadOn,
                        }));
                    }
                }
            } else {
                let p = &players_here[0];
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
        }

        remove_dead(&mut snakes, &mut health, &mut alive, &dead);

        // --- Food + health -------------------------------------------------
        let mut starved: Vec<PlayerId> = Vec::new();
        for player in alive.clone() {
            let snake = snakes.get_mut(&player).unwrap();
            let head = snake[0];
            if let Some(i) = food.iter().position(|&f| f == head) {
                food.remove(i);
                let tail = *snake.last().unwrap();
                snake.push(tail);
                health.insert(player.clone(), MAX_HEALTH);
            } else {
                let hp = health.get_mut(&player).unwrap();
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
            *self.survival.get_mut(p).unwrap() += 1;
        }

        let over_threshold = if self.players.len() > 1 { 1 } else { 0 };
        let winners = if alive.len() <= over_threshold {
            let mut winners: Vec<Winner> = self
                .players
                .iter()
                .map(|p| Winner {
                    player: p.clone(),
                    survival_turns: self.survival[p],
                    length: snakes.get(p).map(|s| s.len() as u16).unwrap_or(0),
                })
                .collect();
            winners.sort_by(|a, b| b.survival_turns.cmp(&a.survival_turns));
            winners
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

    fn target(&self, from: Pos, dir: Dir) -> Option<Pos> {
        let (x, y) = self.xy(from);
        let (dx, dy) = dir.delta();
        let (nx, ny) = (x + dx, y + dy);
        if nx < 0 || nx >= self.width() || ny < 0 || ny >= self.height() {
            None
        } else {
            Some(self.pos(nx, ny))
        }
    }

    fn free_cells(&self, snakes: &BTreeMap<PlayerId, Vec<Pos>>, food: &[Pos]) -> Vec<Pos> {
        let mut occupied = vec![false; self.config.cells()];
        for &pos in snakes.values().flatten().chain(food.iter()) {
            occupied[pos as usize] = true;
        }
        (0..self.config.cells() as u16)
            .filter(|&p| !occupied[p as usize] && !self.config.is_wall(p))
            .collect()
    }
}

fn last_direction(snake: &[Pos], width: i16) -> Option<Dir> {
    let (head, neck) = (snake[0] as i16, *snake.get(1)? as i16);
    if head == neck {
        return None; // still stacked at start
    }
    let (hx, hy) = (head % width, head / width);
    let (nx, ny) = (neck % width, neck / width);
    match (hx - nx, hy - ny) {
        (0, -1) => Some(Dir::Up),
        (0, 1) => Some(Dir::Down),
        (-1, 0) => Some(Dir::Left),
        (1, 0) => Some(Dir::Right),
        _ => None,
    }
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

    fn config() -> GameConfig {
        GameConfig {
            board_width: 13,
            board_height: 13,
            tick_ms: 1000,
        }
    }

    fn two_player_engine() -> Engine {
        Engine::new(config(), vec!["a".into(), "b".into()], 42)
    }

    fn moves(pairs: &[(&str, Dir)]) -> BTreeMap<PlayerId, Dir> {
        pairs.iter().map(|(p, d)| (p.to_string(), *d)).collect()
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
        if t2.alive.contains(&"a".to_string()) {
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
        assert!(!t.alive.contains(&"a".to_string()));
    }

    #[test]
    fn eating_food_grows_and_heals() {
        let cfg = config();
        let mut e = Engine::new(cfg, vec!["solo".into()], 7);
        let t1 = e.first_turn();
        // Walk the snek onto a known food cell.
        let food = t1.food[0];
        let head = t1.snakes["solo"][0];
        let w = cfg.board_width as i16;
        let (fx, fy) = (food as i16 % w, food as i16 / w);
        let (hx, hy) = (head as i16 % w, head as i16 / w);

        let mut t = t1;
        let mut x = hx;
        let mut y = hy;
        // Move horizontally then vertically toward the food.
        for _ in 0..40 {
            if t.game_over() {
                break;
            }
            let dir = if x != fx {
                if fx > x { Dir::Right } else { Dir::Left }
            } else if y != fy {
                if fy > y { Dir::Down } else { Dir::Up }
            } else {
                break;
            };
            t = e.next_turn(&t, &moves(&[("solo", dir)]));
            if let Some(s) = t.snakes.get("solo") {
                x = s[0] as i16 % w;
                y = s[0] as i16 / w;
            }
            if x == fx && y == fy {
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
                (2, 2) => Dir::Right,   // top-left corner
                (10, 2) => Dir::Down,   // top-right corner
                (2, 10) => Dir::Up,     // bottom-left corner
                (10, 10) => Dir::Left,  // bottom-right corner
                (6, 2) => Dir::Right,   // top mid
                (10, 6) => Dir::Down,   // right mid
                (6, 10) => Dir::Left,   // bottom mid
                (2, 6) => Dir::Up,      // left mid
                other => panic!("unexpected spawn position {other:?}"),
            }
        };
        let heads: std::collections::BTreeMap<PlayerId, Pos> =
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
        let next = engine.next_turn(
            &turn,
            &moves(&[("a", Dir::Right), ("b", Dir::Down)]),
        );
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
            let next = e.next_turn(turns.last().unwrap(), &moves(&[("a", dir), ("b", Dir::Down)]));
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
