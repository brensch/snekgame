//! The house sneks: time-budgeted PUCT search.
//!
//! Architecture (AlphaZero-shaped, minus the network):
//! - **PUCT selection** over the bot's own four headings:
//!   `Q(a) + c · P(a) · √N / (1 + n(a))`, with heuristic priors standing in
//!   for a policy net.
//! - **Priors** encode instinct: never reverse, near-zero for walls/bodies,
//!   prefer straight, steer toward food when hungry, prefer open space.
//! - **Opponents are sampled from their own priors** each descent (a cheap
//!   decoupled-UCT): the search averages over plausible opponent play
//!   instead of assuming they never turn.
//! - **Simulation is the real engine**, so the rules can't drift.
//! - **Leaf evaluation** blends survival, Voronoi board control, health
//!   against distance-to-food, and length dominance (head-on threat).
//! - **Time budgets, not iteration counts**: easy 8ms, medium 35ms, hard
//!   100ms per move (the server also caps by tick length and runs bots on
//!   blocking threads off the game loop). On this hardware an iteration
//!   costs tens of microseconds, so hard runs a few thousand descents up to
//!   32 plies deep — it is genuinely looking ~half a board-crossing ahead.

use crate::engine::Engine;
use crate::rng::Rng;
use crate::types::*;
use std::collections::BTreeMap;

/// Per-move thinking budget in milliseconds.
pub fn budget_ms(difficulty: BotDifficulty) -> u32 {
    match difficulty {
        BotDifficulty::Easy => 8,
        BotDifficulty::Medium => 35,
        BotDifficulty::Hard => 100,
    }
}

/// How deep a single descent may go.
fn depth_cap(difficulty: BotDifficulty) -> u32 {
    match difficulty {
        BotDifficulty::Easy => 8,
        BotDifficulty::Medium => 16,
        BotDifficulty::Hard => 32,
    }
}

const C_PUCT: f32 = 1.5;
/// First-play urgency: optimistic-neutral value for unvisited children.
const FPU: f32 = 0.5;
/// Runaway guard — budgets should stop us long before this.
const MAX_ITERS: u32 = 100_000;

// ---------------------------------------------------------------------------
// Public entry
// ---------------------------------------------------------------------------

/// Pick a move within `budget_ms_override` (or the difficulty default).
/// Runs on native server threads; compiled but never called in wasm.
pub fn choose_move(
    config: &GameConfig,
    turn: &Turn,
    me: &PlayerId,
    difficulty: BotDifficulty,
    seed: u64,
    budget_ms_override: Option<u32>,
) -> Dir {
    let budget = budget_ms_override.unwrap_or_else(|| budget_ms(difficulty)) as u128;
    let cap = depth_cap(difficulty);
    let mut rng = Rng::new(seed);

    let root_priors = priors(config, turn, me);
    let legal: Vec<usize> = (0..4).filter(|&a| root_priors[a] > 0.0).collect();
    match legal.len() {
        0 => return Dir::Up, // boxed in; doesn't matter
        1 => return DIRS[legal[0]],
        _ => {}
    }

    let mut arena: Vec<Node> = vec![Node::new(root_priors)];
    let start = std::time::Instant::now();
    let mut iters = 0u32;

    while start.elapsed().as_millis() < budget && iters < MAX_ITERS {
        iters += 1;
        run_descent(config, turn, me, cap, &mut arena, &mut rng);
    }

    // Most-visited root action wins.
    let root = &arena[0];
    let best = (0..4)
        .filter(|&a| root.priors[a] > 0.0)
        .max_by(|&a, &b| {
            let na = root.child(a).map(|c| arena[c].visits).unwrap_or(0.0);
            let nb = root.child(b).map(|c| arena[c].visits).unwrap_or(0.0);
            na.total_cmp(&nb)
        })
        .unwrap_or(legal[0]);
    DIRS[best]
}

// ---------------------------------------------------------------------------
// Tree
// ---------------------------------------------------------------------------

const DIRS: [Dir; 4] = [Dir::Up, Dir::Down, Dir::Left, Dir::Right];
const NONE: u32 = u32::MAX;

struct Node {
    visits: f32,
    value_sum: f32,
    priors: [f32; 4],
    children: [u32; 4],
}

impl Node {
    fn new(priors: [f32; 4]) -> Node {
        Node {
            visits: 0.0,
            value_sum: 0.0,
            priors,
            children: [NONE; 4],
        }
    }

    fn child(&self, a: usize) -> Option<usize> {
        (self.children[a] != NONE).then_some(self.children[a] as usize)
    }

    fn q(&self) -> f32 {
        if self.visits > 0.0 {
            self.value_sum / self.visits
        } else {
            FPU
        }
    }
}

fn run_descent(
    config: &GameConfig,
    root_turn: &Turn,
    me: &PlayerId,
    cap: u32,
    arena: &mut Vec<Node>,
    rng: &mut Rng,
) {
    let mut sim = Engine::new(*config, root_turn.alive.clone(), rng.next_u64());
    let mut state = root_turn.clone();
    let mut path: Vec<usize> = vec![0];
    let mut value = None;

    for depth in 0..cap {
        let node_id = *path.last().unwrap();

        // PUCT over actions with non-zero prior.
        let sqrt_n = (arena[node_id].visits + 1.0).sqrt();
        let mut best: Option<(f32, usize)> = None;
        for a in 0..4 {
            let p = arena[node_id].priors[a];
            if p <= 0.0 {
                continue;
            }
            let (q, n) = match arena[node_id].child(a) {
                Some(c) => (arena[c].q(), arena[c].visits),
                None => (FPU, 0.0),
            };
            let score = q + C_PUCT * p * sqrt_n / (1.0 + n);
            if best.map(|(s, _)| score > s).unwrap_or(true) {
                best = Some((score, a));
            }
        }
        let Some((_, action)) = best else {
            value = Some(0.0); // nowhere to go
            break;
        };

        // Joint move: mine chosen, opponents sampled from their instincts.
        let mut moves: BTreeMap<PlayerId, Dir> = BTreeMap::new();
        moves.insert(me.clone(), DIRS[action]);
        for opp in state.alive.clone() {
            if &opp != me {
                let p = priors(config, &state, &opp);
                moves.insert(opp, sample(&p, rng));
            }
        }
        state = sim.next_turn(&state, &moves);

        // Materialize the child and walk into it.
        let child_id = match arena[path[path.len() - 1]].child(action) {
            Some(c) => c,
            None => {
                let priors = if state.alive.contains(me) {
                    priors(config, &state, me)
                } else {
                    [0.25; 4] // unused — terminal below
                };
                arena.push(Node::new(priors));
                let c = arena.len() - 1;
                let n = path[path.len() - 1];
                arena[n].children[action] = c as u32;
                c
            }
        };
        path.push(child_id);

        if !state.alive.contains(me) {
            // Dying later is marginally better than dying now.
            value = Some(0.04 * depth as f32 / cap as f32);
            break;
        }
        if state.game_over() {
            value = Some(1.0);
            break;
        }
        // Fresh leaf: evaluate here rather than descending blind.
        if arena[child_id].visits == 0.0 {
            value = Some(evaluate(config, &state, me));
            break;
        }
    }

    let value = value.unwrap_or_else(|| evaluate(config, &state, me));
    for &n in &path {
        arena[n].visits += 1.0;
        arena[n].value_sum += value;
    }
}

fn sample(priors: &[f32; 4], rng: &mut Rng) -> Dir {
    let total: f32 = priors.iter().sum();
    if total <= 0.0 {
        return DIRS[rng.below(4)];
    }
    let mut roll = (rng.next_u64() % 10_000) as f32 / 10_000.0 * total;
    for a in 0..4 {
        roll -= priors[a];
        if roll <= 0.0 {
            return DIRS[a];
        }
    }
    DIRS[3]
}

// ---------------------------------------------------------------------------
// Priors: instinct without a network
// ---------------------------------------------------------------------------

fn priors(config: &GameConfig, turn: &Turn, player: &PlayerId) -> [f32; 4] {
    let mut out = [0.0f32; 4];
    let Some(snake) = turn.snakes.get(player) else {
        return out;
    };
    let head = snake[0];
    let last = last_direction(snake, config.board_width as i16);
    let health = turn.health.get(player).copied().unwrap_or(100);
    let hunger = match health {
        0..=25 => 1.0,
        26..=60 => 0.5,
        _ => 0.15,
    };

    for (a, &dir) in DIRS.iter().enumerate() {
        if Some(dir.opposite()) == last {
            continue; // never reverse
        }
        let Some(target) = step(config, head, dir) else {
            out[a] = 0.0;
            continue;
        };
        if config.is_wall(target) {
            out[a] = 0.0;
            continue;
        }
        // Body cells: lethal unless it's a tail that will vacate.
        let mut blocked = false;
        let mut tail_chase = false;
        for s in turn.snakes.values() {
            if let Some(i) = s.iter().position(|&c| c == target) {
                if i == s.len() - 1 && s[s.len() - 1] != s[s.len() - 2] {
                    tail_chase = true; // tail vacates unless they just ate
                } else {
                    blocked = true;
                }
                break;
            }
        }
        if blocked {
            out[a] = 0.02; // effectively never, but not structurally zero
            continue;
        }

        let mut p = 1.0;
        if tail_chase {
            p *= 0.3;
        }
        if Some(dir) == last {
            p += 0.35; // keep momentum
        }
        // Food pull, scaled by hunger.
        if let Some(d) = nearest_food_dist(config, turn, target) {
            p += hunger * 1.6 / (1.0 + d as f32);
        }
        // Breathing room.
        p += 0.15 * free_neighbours(config, turn, target) as f32;
        out[a] = p;
    }

    let total: f32 = out.iter().sum();
    if total > 0.0 {
        for v in &mut out {
            *v /= total;
        }
    } else if let Some(l) = last {
        // Boxed in: at least keep the engine's straight-line default legal.
        out[DIRS.iter().position(|&d| d == l).unwrap_or(0)] = 1.0;
    } else {
        out = [0.25; 4];
    }
    out
}

fn free_neighbours(config: &GameConfig, turn: &Turn, pos: Pos) -> u32 {
    DIRS.iter()
        .filter_map(|&d| step(config, pos, d))
        .filter(|&t| !config.is_wall(t))
        .filter(|&t| !turn.snakes.values().any(|s| s.contains(&t)))
        .count() as u32
}

fn nearest_food_dist(config: &GameConfig, turn: &Turn, from: Pos) -> Option<u32> {
    let w = config.board_width as i32;
    let (fx, fy) = (from as i32 % w, from as i32 / w);
    turn.food
        .iter()
        .map(|&f| {
            let (x, y) = (f as i32 % w, f as i32 / w);
            ((x - fx).abs() + (y - fy).abs()) as u32
        })
        .min()
}

// ---------------------------------------------------------------------------
// Evaluation: how good is this world for me?
// ---------------------------------------------------------------------------

fn evaluate(config: &GameConfig, turn: &Turn, me: &PlayerId) -> f32 {
    if !turn.alive.contains(me) {
        return 0.0;
    }
    if turn.game_over() {
        return 1.0;
    }

    let (share, my_food_dist) = voronoi(config, turn, me);

    // Health sustainability: fine above 60hp; below that, reachable food is
    // what matters.
    let health = turn.health.get(me).copied().unwrap_or(100) as f32;
    let sustain = if health > 60.0 {
        1.0
    } else {
        let h = health / 60.0;
        let food_ok = match my_food_dist {
            Some(d) => 1.0 / (1.0 + d as f32 / 4.0),
            None => 0.1, // no food reachable first: bad place to be hungry
        };
        (0.35 * h + 0.65 * food_ok).min(1.0)
    };

    // Length dominance: longer than every opponent wins head-ons.
    let my_len = turn.snakes.get(me).map(|s| s.len() as f32).unwrap_or(0.0);
    let max_opp = turn
        .snakes
        .iter()
        .filter(|(p, _)| *p != me)
        .map(|(_, s)| s.len() as f32)
        .fold(0.0, f32::max);
    let dominance = 0.5 + 0.5 * ((my_len - max_opp) * 0.25).tanh();

    0.20 + 0.40 * share + 0.20 * sustain + 0.20 * dominance
}

/// Multi-source BFS from every living head: each free cell belongs to
/// whoever reaches it first. Returns (my share of claimed cells, my BFS
/// distance to the nearest food I reach first).
fn voronoi(config: &GameConfig, turn: &Turn, me: &PlayerId) -> (f32, Option<u32>) {
    let cells = config.cells();
    let w = config.board_width as i16;
    const BLOCKED: u8 = 254;
    const FREE: u8 = 255;
    let mut owner = vec![FREE; cells];
    for pos in 0..cells as u16 {
        if config.is_wall(pos) {
            owner[pos as usize] = BLOCKED;
        }
    }
    for snake in turn.snakes.values() {
        for &pos in snake {
            owner[pos as usize] = BLOCKED;
        }
    }

    let mut queue: std::collections::VecDeque<(Pos, u8, u32)> = std::collections::VecDeque::new();
    let mut my_idx = u8::MAX;
    for (i, p) in turn.alive.iter().enumerate() {
        if p == me {
            my_idx = i as u8;
        }
        if let Some(snake) = turn.snakes.get(p) {
            queue.push_back((snake[0], i as u8, 0));
        }
    }

    let mut counts = vec![0u32; turn.alive.len()];
    let mut my_food_dist: Option<u32> = None;
    while let Some((pos, who, dist)) = queue.pop_front() {
        let (x, y) = (pos as i16 % w, pos as i16 / w);
        for (dx, dy) in [(0i16, -1i16), (0, 1), (-1, 0), (1, 0)] {
            let (nx, ny) = (x + dx, y + dy);
            if nx < 0 || nx >= w || ny < 0 || ny >= config.board_height as i16 {
                continue;
            }
            let np = (ny * w + nx) as usize;
            if owner[np] == FREE {
                owner[np] = who;
                counts[who as usize] += 1;
                if who == my_idx && my_food_dist.is_none() && turn.food.contains(&(np as Pos)) {
                    my_food_dist = Some(dist + 1);
                }
                queue.push_back((np as Pos, who, dist + 1));
            }
        }
    }

    let total: u32 = counts.iter().sum();
    let share = if total == 0 || my_idx == u8::MAX {
        0.5
    } else {
        counts[my_idx as usize] as f32 / total as f32
    };
    (share, my_food_dist)
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn step(config: &GameConfig, from: Pos, dir: Dir) -> Option<Pos> {
    let w = config.board_width as i16;
    let h = config.board_height as i16;
    let (x, y) = (from as i16 % w, from as i16 / w);
    let (dx, dy) = dir.delta();
    let (nx, ny) = (x + dx, y + dy);
    if nx < 0 || nx >= w || ny < 0 || ny >= h {
        None
    } else {
        Some((ny * w + nx) as Pos)
    }
}

fn last_direction(snake: &[Pos], width: i16) -> Option<Dir> {
    let (head, neck) = (snake[0] as i16, *snake.get(1)? as i16);
    if head == neck {
        return None;
    }
    match (head % width - neck % width, head / width - neck / width) {
        (0, -1) => Some(Dir::Up),
        (0, 1) => Some(Dir::Down),
        (-1, 0) => Some(Dir::Left),
        (1, 0) => Some(Dir::Right),
        _ => None,
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

    fn play_duel(
        a_diff: BotDifficulty,
        a_budget: u32,
        b_diff: BotDifficulty,
        b_budget: u32,
        seed: u64,
    ) -> (u32, u32) {
        let a: PlayerId = "a".into();
        let b: PlayerId = "b".into();
        let mut engine = Engine::new(config(), vec![a.clone(), b.clone()], seed);
        let mut turn = engine.first_turn();
        for i in 0..300 {
            if turn.game_over() {
                break;
            }
            let mut moves = BTreeMap::new();
            for (p, d, budget) in [(&a, a_diff, a_budget), (&b, b_diff, b_budget)] {
                if turn.alive.contains(p) {
                    moves.insert(
                        p.clone(),
                        choose_move(&config(), &turn, p, d, seed * 1000 + i, Some(budget)),
                    );
                }
            }
            turn = engine.next_turn(&turn, &moves);
        }
        let survival = |p: &PlayerId| {
            turn.winners
                .iter()
                .find(|w| &w.player == p)
                .map(|w| w.survival_turns)
                .unwrap_or(turn.turn_number)
        };
        (survival(&a), survival(&b))
    }

    /// A solo hard bot must cruise indefinitely.
    #[test]
    fn hard_bot_survives_solo() {
        let me: PlayerId = "bot".to_string();
        let mut engine = Engine::new(config(), vec![me.clone()], 9);
        let mut turn = engine.first_turn();
        for i in 0..120 {
            assert!(turn.alive.contains(&me), "hard bot died at turn {i}");
            let mv = choose_move(&config(), &turn, &me, BotDifficulty::Hard, 1000 + i, Some(20));
            let mut moves = BTreeMap::new();
            moves.insert(me.clone(), mv);
            turn = engine.next_turn(&turn, &moves);
        }
    }

    /// The budget is honoured (this is the whole point of the design).
    #[test]
    fn budget_is_respected() {
        let me: PlayerId = "bot".to_string();
        let mut engine = Engine::new(config(), vec![me.clone(), "x".into()], 3);
        let turn = engine.first_turn();
        let start = std::time::Instant::now();
        for i in 0..10 {
            choose_move(&config(), &turn, &me, BotDifficulty::Hard, i, Some(100));
        }
        let per_move = start.elapsed().as_millis() / 10;
        // Generous slack for debug builds; release is comfortably under.
        assert!(per_move < 400, "hard move took {per_move}ms against 100ms budget");
    }

    /// More thinking time must actually buy strength: hard (40ms) should
    /// dominate easy (4ms) across a handful of duels.
    #[test]
    fn hard_beats_easy() {
        let mut hard_wins = 0;
        let mut easy_wins = 0;
        for seed in 0..5u64 {
            let (hard, easy) = play_duel(
                BotDifficulty::Hard,
                40,
                BotDifficulty::Easy,
                4,
                42 + seed,
            );
            if hard > easy {
                hard_wins += 1;
            } else if easy > hard {
                easy_wins += 1;
            }
        }
        assert!(
            hard_wins > easy_wins,
            "hard should outlast easy (hard {hard_wins} vs easy {easy_wins})"
        );
    }
}
