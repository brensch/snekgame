//! The house snek: a Voronoi-UCT bot.
//!
//! Each tick it runs a tiny Monte-Carlo tree search over its own moves only:
//! opponents are simulated with a cheap "keep going unless that kills me"
//! policy, states are advanced with the real engine (so the sim rules are the
//! game rules), and leaves are scored by Voronoi board control — the share of
//! cells this snek would reach first. UCB1 balances the three possible
//! headings at each ply.
//!
//! Difficulty is lookahead depth (with a matching iteration budget). Budgets
//! are deliberately tiny: worst case is a few hundred engine ticks plus one
//! O(cells) flood fill per iteration — microseconds, not milliseconds.

use crate::engine::Engine;
use crate::rng::Rng;
use crate::types::*;
use std::collections::BTreeMap;

pub fn params(difficulty: BotDifficulty) -> (u8, u32) {
    // (lookahead depth in moves, search iterations)
    match difficulty {
        BotDifficulty::Easy => (1, 24),
        BotDifficulty::Medium => (3, 90),
        BotDifficulty::Hard => (6, 240),
    }
}

const UCB_C: f64 = 1.2;

#[derive(Clone, Copy, Default)]
struct Node {
    visits: f64,
    value: f64,
    children: [Option<usize>; 4], // indexed by Dir as usize
}

fn dir_index(d: Dir) -> usize {
    match d {
        Dir::Up => 0,
        Dir::Down => 1,
        Dir::Left => 2,
        Dir::Right => 3,
    }
}

pub fn choose_move(
    config: &GameConfig,
    turn: &Turn,
    me: &PlayerId,
    difficulty: BotDifficulty,
    seed: u64,
) -> Dir {
    let (depth, iterations) = params(difficulty);
    let mut rng = Rng::new(seed);

    let root_legal = legal_moves(config, turn, me);
    let Some(&first) = root_legal.first() else {
        return Dir::Up; // boxed in — doesn't matter
    };
    if root_legal.len() == 1 {
        return first;
    }

    let mut nodes: Vec<Node> = vec![Node::default()];

    for _ in 0..iterations {
        // A fresh scratch engine per descent keeps the sim honest (food
        // spawns vary between descents; UCT averages over them).
        let mut sim = Engine::new(*config, turn.alive.clone(), rng.next_u64());
        let mut state = turn.clone();
        let mut path: Vec<usize> = vec![0];
        let mut leaf_value = None;

        for _level in 0..depth {
            let node_id = *path.last().unwrap();
            let legal = legal_moves(config, &state, me);
            if legal.is_empty() {
                leaf_value = Some(0.0);
                break;
            }

            // UCB1 over the legal headings.
            let parent_n = nodes[node_id].visits.max(1.0);
            let mut best: Option<(f64, Dir)> = None;
            for &d in &legal {
                let score = match nodes[node_id].children[dir_index(d)] {
                    Some(c) if nodes[c].visits > 0.0 => {
                        nodes[c].value / nodes[c].visits
                            + UCB_C * (parent_n.ln() / nodes[c].visits).sqrt()
                    }
                    // Unvisited: explore first, jittered for tie-breaking.
                    _ => 10.0 + (rng.next_u64() % 8) as f64,
                };
                if best.map(|(s, _)| score > s).unwrap_or(true) {
                    best = Some((score, d));
                }
            }
            let chosen = best.unwrap().1;

            let child_id = match nodes[node_id].children[dir_index(chosen)] {
                Some(c) => c,
                None => {
                    nodes.push(Node::default());
                    let c = nodes.len() - 1;
                    nodes[node_id].children[dir_index(chosen)] = Some(c);
                    c
                }
            };
            path.push(child_id);

            // Advance the world one tick: my move + timid opponents.
            let mut moves: BTreeMap<PlayerId, Dir> = BTreeMap::new();
            moves.insert(me.clone(), chosen);
            for opp in state.alive.iter().filter(|p| *p != me) {
                moves.insert(opp.clone(), opponent_policy(config, &state, opp));
            }
            state = sim.next_turn(&state, &moves);

            if !state.alive.contains(me) {
                leaf_value = Some(0.0);
                break;
            }
            if state.game_over() {
                leaf_value = Some(1.0); // over and we're alive: we won
                break;
            }
        }

        let value = leaf_value.unwrap_or_else(|| voronoi_share(config, &state, me));
        for &n in &path {
            nodes[n].visits += 1.0;
            nodes[n].value += value;
        }
    }

    // Most-visited root move wins.
    root_legal
        .into_iter()
        .max_by(|&a, &b| {
            let va = nodes[0].children[dir_index(a)].map(|c| nodes[c].visits).unwrap_or(0.0);
            let vb = nodes[0].children[dir_index(b)].map(|c| nodes[c].visits).unwrap_or(0.0);
            va.total_cmp(&vb)
        })
        .unwrap_or(first)
}

/// Headings that don't reverse into the neck and don't hit a wall.
fn legal_moves(config: &GameConfig, turn: &Turn, player: &PlayerId) -> Vec<Dir> {
    let Some(snake) = turn.snakes.get(player) else {
        return Vec::new();
    };
    let head = snake[0];
    let last = last_direction(snake, config.board_width as i16);
    Dir::ALL
        .iter()
        .copied()
        .filter(|&d| Some(d.opposite()) != last)
        .filter(|&d| {
            step(config, head, d)
                .map(|t| !config.is_wall(t))
                .unwrap_or(false)
        })
        .collect()
}

/// Opponents in the sim keep going straight unless that's immediately fatal,
/// then take the first non-lethal turn. Cheap and predictable.
fn opponent_policy(config: &GameConfig, turn: &Turn, player: &PlayerId) -> Dir {
    let Some(snake) = turn.snakes.get(player) else {
        return Dir::Up;
    };
    let head = snake[0];
    let last = last_direction(snake, config.board_width as i16);
    let deadly = |d: Dir| -> bool {
        match step(config, head, d) {
            Some(t) if !config.is_wall(t) => {
                turn.snakes.values().any(|s| s.contains(&t))
            }
            _ => true,
        }
    };
    if let Some(l) = last {
        if !deadly(l) {
            return l;
        }
    }
    Dir::ALL
        .iter()
        .copied()
        .filter(|&d| Some(d.opposite()) != last)
        .find(|&d| !deadly(d))
        .or(last)
        .unwrap_or(Dir::Up)
}

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

/// Voronoi board control: multi-source BFS from every living head; each free
/// cell belongs to whoever reaches it first. Returns our share of claimed
/// cells (0..1); sole survivor scores 1, dead scores 0.
fn voronoi_share(config: &GameConfig, turn: &Turn, me: &PlayerId) -> f64 {
    if !turn.alive.contains(me) {
        return 0.0;
    }
    if turn.alive.len() <= 1 {
        return 1.0;
    }

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

    let mut queue: std::collections::VecDeque<(Pos, u8)> = std::collections::VecDeque::new();
    let mut my_idx = 0u8;
    for (i, p) in turn.alive.iter().enumerate() {
        if p == me {
            my_idx = i as u8;
        }
        if let Some(snake) = turn.snakes.get(p) {
            queue.push_back((snake[0], i as u8));
        }
    }

    let mut counts = vec![0u32; turn.alive.len()];
    while let Some((pos, who)) = queue.pop_front() {
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
                queue.push_back((np as Pos, who));
            }
        }
    }

    let total: u32 = counts.iter().sum();
    if total == 0 {
        return 0.5;
    }
    counts[my_idx as usize] as f64 / total as f64
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

    /// A solo hard bot should comfortably survive a long while.
    #[test]
    fn hard_bot_survives_solo() {
        let me: PlayerId = "bot".to_string();
        let mut engine = Engine::new(config(), vec![me.clone()], 9);
        let mut turn = engine.first_turn();
        for i in 0..60 {
            assert!(
                turn.alive.contains(&me),
                "hard bot died at turn {i}"
            );
            let mv = choose_move(&config(), &turn, &me, BotDifficulty::Hard, 1000 + i);
            let mut moves = BTreeMap::new();
            moves.insert(me.clone(), mv);
            turn = engine.next_turn(&turn, &moves);
        }
    }

    /// Two bots fight to a finish without breaking the engine.
    #[test]
    fn bots_finish_a_game() {
        let a: PlayerId = "a".into();
        let b: PlayerId = "b".into();
        let mut engine = Engine::new(config(), vec![a.clone(), b.clone()], 17);
        let mut turn = engine.first_turn();
        for i in 0..500 {
            if turn.game_over() {
                assert_eq!(turn.winners.len(), 2);
                return;
            }
            let mut moves = BTreeMap::new();
            for (p, d) in [(&a, BotDifficulty::Medium), (&b, BotDifficulty::Easy)] {
                if turn.alive.contains(p) {
                    moves.insert(p.clone(), choose_move(&config(), &turn, p, d, 40 + i));
                }
            }
            turn = engine.next_turn(&turn, &moves);
        }
        // Extremely unlikely, but a 500-turn stalemate is not a failure of
        // the bot API itself.
    }

    /// The whole point: bot moves must be extremely cheap.
    #[test]
    fn bot_moves_are_cheap() {
        let me: PlayerId = "bot".to_string();
        let mut engine = Engine::new(config(), vec![me.clone(), "x".into()], 3);
        let turn = engine.first_turn();
        let start = std::time::Instant::now();
        for i in 0..100 {
            choose_move(&config(), &turn, &me, BotDifficulty::Hard, i);
        }
        let per_move = start.elapsed() / 100;
        // The real bar is the release build (server runs release); debug
        // builds are ~20x slower so get a looser one.
        let limit_ms = if cfg!(debug_assertions) { 120 } else { 10 };
        assert!(
            per_move.as_millis() < limit_ms,
            "hard bot took {per_move:?} per move"
        );
    }
}
