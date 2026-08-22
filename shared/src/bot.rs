//! The house sneks: a port of the strongest heuristic agent from the
//! author's snek3 training league (`snek-heuristic`, "ucb voronoi").
//!
//! Architecture:
//! - **Decoupled-UCT MCTS over joint moves.** At every node, *every* living
//!   snake runs its own UCB1 selection over its candidate moves; the child
//!   is keyed by the joint choice. Opponents are searched, not assumed —
//!   the tree discovers their best responses, which is what makes this
//!   agent actually fight for space.
//! - **Time-expanded Voronoi evaluation.** All heads flood outward together,
//!   one turn per BFS layer, with body cells unblocking as tails vacate
//!   (segment k of an L-long snake frees after L−k turns). A cell belongs
//!   to whoever can be there first; same-turn contests go to the *longer*
//!   snake (it wins the head-to-head), equal lengths kill the claim.
//!   Blended with health, relative length, a hunger term (can I win the
//!   race to food I own before starving?) and a trap term (a reachable
//!   region smaller than my body is a death sentence).
//! - **Guaranteed-loss pruning.** Candidate moves stepping onto a square a
//!   longer-or-equal opponent head can also reach next turn are dropped
//!   before the search ever sees them (unless everything loses).
//! - **A fast internal board.** Search steps run on an index-based board
//!   mirroring the real engine's rules (food-spawn-free lookahead), so a
//!   descent costs microseconds instead of cloning string-keyed maps.
//!
//! Difficulty is purely thinking time: easy 8ms, medium 35ms, hard 100ms
//! per move (the server caps by tick length and runs bots on blocking
//! threads). Hard sustains tens of thousands of simulations per move.

use crate::types::*;
use std::collections::{HashMap, VecDeque};

/// Per-move thinking budget in milliseconds.
pub fn budget_ms(difficulty: BotDifficulty) -> u32 {
    match difficulty {
        BotDifficulty::Easy => 8,
        BotDifficulty::Medium => 35,
        BotDifficulty::Hard => 100,
    }
}

const C_UCT: f32 = 1.2;
const DRAW_VALUE: f32 = -0.25;
const MAX_SIMS: usize = 200_000;
const MAX_SNAKES: usize = 8;

// Voronoi blend weights (positive terms sum to 0.85; trap only subtracts).
const W_AREA: f32 = 0.40;
const W_HEALTH: f32 = 0.10;
const W_LENGTH: f32 = 0.15;
const W_FOOD: f32 = 0.20;
const W_TRAP: f32 = 0.35;

const DIRS: [Dir; 4] = [Dir::Up, Dir::Down, Dir::Left, Dir::Right];

// ---------------------------------------------------------------------------
// Public entry
// ---------------------------------------------------------------------------

pub fn choose_move(
    config: &GameConfig,
    turn: &Turn,
    me: &PlayerId,
    difficulty: BotDifficulty,
    _seed: u64,
    budget_ms_override: Option<u32>,
) -> Dir {
    let budget = budget_ms_override.unwrap_or_else(|| budget_ms(difficulty));

    // Index the world: alive snakes in turn order, me somewhere among them.
    let order: Vec<&PlayerId> = turn.alive.iter().collect();
    let Some(me_idx) = order.iter().position(|p| *p == me) else {
        return Dir::Up; // dead already
    };
    let board = FastBoard::from_turn(config, turn, &order);

    let root_cands = safe_candidates(&board, me_idx);
    match root_cands.len() {
        0 => return Dir::Up,
        1 => return root_cands[0],
        _ => {}
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(budget as u64);
    search(&board, me_idx, deadline)
}

// ---------------------------------------------------------------------------
// Fast board: the engine's rules on indexed state (no food spawns)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Snake {
    body: VecDeque<u16>,
    health: u8,
    alive: bool,
}

impl Snake {
    fn head(&self) -> u16 {
        self.body[0]
    }
    fn len(&self) -> usize {
        self.body.len()
    }
}

#[derive(Clone)]
struct FastBoard {
    w: i16,
    h: i16,
    snakes: Vec<Snake>,
    food: Vec<u16>,
    alive_count: usize,
    /// Terminal threshold: multi games end at 1 alive, solo games at 0.
    min_alive: usize,
}

impl FastBoard {
    fn from_turn(config: &GameConfig, turn: &Turn, order: &[&PlayerId]) -> FastBoard {
        let snakes = order
            .iter()
            .map(|p| Snake {
                body: turn.snakes[*p].iter().copied().collect(),
                health: turn.health.get(*p).copied().unwrap_or(100),
                alive: true,
            })
            .collect::<Vec<_>>();
        let alive_count = snakes.len();
        FastBoard {
            w: config.board_width as i16,
            h: config.board_height as i16,
            snakes,
            food: turn.food.clone(),
            alive_count,
            min_alive: if alive_count > 1 { 1 } else { 0 },
        }
    }

    fn is_wall(&self, pos: u16) -> bool {
        let (x, y) = (pos as i16 % self.w, pos as i16 / self.w);
        x == 0 || y == 0 || x == self.w - 1 || y == self.h - 1
    }

    fn step_dir(&self, from: u16, dir: Dir) -> Option<u16> {
        let (x, y) = (from as i16 % self.w, from as i16 / self.w);
        let (dx, dy) = dir.delta();
        let (nx, ny) = (x + dx, y + dy);
        if nx < 0 || nx >= self.w || ny < 0 || ny >= self.h {
            None
        } else {
            Some((ny * self.w + nx) as u16)
        }
    }

    fn is_terminal(&self) -> bool {
        self.alive_count <= self.min_alive
    }

    /// Sole survivor of a multi game, if the board is terminal that way.
    fn winner(&self) -> Option<usize> {
        (self.min_alive == 1 && self.alive_count == 1)
            .then(|| self.snakes.iter().position(|s| s.alive).unwrap())
    }

    /// One tick of the real rules (minus random food spawns): wall/self
    /// deaths leave the body blocking this tick, head-on kills the shorter,
    /// then eating and starvation.
    fn step(&mut self, moves: &[Dir]) {
        let n = self.snakes.len();
        let mut died = [false; MAX_SNAKES];

        // Movement (tail pops before self-check, exactly like the engine).
        for i in 0..n {
            if !self.snakes[i].alive {
                continue;
            }
            let head = self.snakes[i].head();
            match self.step_dir(head, moves[i]) {
                Some(t) if !self.is_wall(t) => {
                    self.snakes[i].body.pop_back();
                    if self.snakes[i].body.contains(&t) {
                        died[i] = true; // self-collision; body blocks this tick
                    } else {
                        self.snakes[i].body.push_front(t);
                    }
                }
                _ => died[i] = true, // wall; body blocks this tick
            }
        }

        // Cross-snake collisions (bodies of this tick's dead still block).
        for i in 0..n {
            if !self.snakes[i].alive || died[i] {
                continue;
            }
            let head = self.snakes[i].head();
            let my_len = self.snakes[i].len();
            for j in 0..n {
                if j == i || !self.snakes[j].alive {
                    continue;
                }
                if self.snakes[j].head() == head && !died[j] {
                    // Head-on: shortest dies, ties kill both.
                    if my_len <= self.snakes[j].len() {
                        died[i] = true;
                    }
                } else if self.snakes[j].body.iter().skip(1).any(|&c| c == head)
                    || (died[j] && self.snakes[j].head() == head)
                {
                    died[i] = true;
                }
            }
        }

        for i in 0..n {
            if died[i] && self.snakes[i].alive {
                self.snakes[i].alive = false;
                self.alive_count -= 1;
            }
        }

        // Food + starvation.
        for i in 0..n {
            if !self.snakes[i].alive {
                continue;
            }
            let head = self.snakes[i].head();
            if let Some(f) = self.food.iter().position(|&c| c == head) {
                self.food.swap_remove(f);
                let tail = *self.snakes[i].body.back().unwrap();
                self.snakes[i].body.push_back(tail);
                self.snakes[i].health = 100;
            } else {
                self.snakes[i].health = self.snakes[i].health.saturating_sub(1);
                if self.snakes[i].health == 0 {
                    self.snakes[i].alive = false;
                    self.alive_count -= 1;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Candidates + guaranteed-loss pruning
// ---------------------------------------------------------------------------

fn last_dir(board: &FastBoard, i: usize) -> Option<Dir> {
    let s = &board.snakes[i];
    if s.len() < 2 || s.body[0] == s.body[1] {
        return None;
    }
    let (head, neck) = (s.body[0] as i16, s.body[1] as i16);
    match (head % board.w - neck % board.w, head / board.w - neck / board.w) {
        (0, -1) => Some(Dir::Up),
        (0, 1) => Some(Dir::Down),
        (-1, 0) => Some(Dir::Left),
        (1, 0) => Some(Dir::Right),
        _ => None,
    }
}

/// Non-reversing, on-board, non-wall moves; a trapped snake keeps all four
/// (the search picks the least bad); the dead get a dummy.
fn candidates(board: &FastBoard, i: usize) -> Vec<Dir> {
    if !board.snakes[i].alive {
        return vec![Dir::Up];
    }
    let head = board.snakes[i].head();
    let reverse = last_dir(board, i).map(|d| d.opposite());
    let mut v = Vec::with_capacity(4);
    for &m in &DIRS {
        if Some(m) == reverse {
            continue;
        }
        match board.step_dir(head, m) {
            Some(t) if !board.is_wall(t) => v.push(m),
            _ => {}
        }
    }
    if v.is_empty() {
        v.extend_from_slice(&DIRS);
    }
    v
}

/// [`candidates`] minus guaranteed head-to-head losses: squares a
/// longer-or-equal live opponent's head can also reach next turn. If every
/// move is such a square, keep them all and let the search pick.
fn safe_candidates(board: &FastBoard, i: usize) -> Vec<Dir> {
    let base = candidates(board, i);
    if base.len() <= 1 || !board.snakes[i].alive {
        return base;
    }
    let my_len = board.snakes[i].len();
    let mut danger: Vec<u16> = Vec::new();
    for (j, s) in board.snakes.iter().enumerate() {
        if j == i || !s.alive || s.len() < my_len {
            continue;
        }
        for m in candidates(board, j) {
            if let Some(t) = board.step_dir(s.head(), m) {
                danger.push(t);
            }
        }
    }
    let head = board.snakes[i].head();
    let safe: Vec<Dir> = base
        .iter()
        .copied()
        .filter(|&m| {
            board
                .step_dir(head, m)
                .map(|t| !danger.contains(&t))
                .unwrap_or(false)
        })
        .collect();
    if safe.is_empty() {
        base
    } else {
        safe
    }
}

// ---------------------------------------------------------------------------
// Decoupled-UCT search over joint moves
// ---------------------------------------------------------------------------

struct Node {
    board: FastBoard,
    cands: Vec<Vec<Dir>>,
    visits: Vec<Vec<f32>>,
    values: Vec<Vec<f32>>,
    total: f32,
    children: HashMap<u32, usize>,
    terminal: bool,
}

impl Node {
    fn new(board: FastBoard) -> Node {
        let terminal = board.is_terminal();
        let cands: Vec<Vec<Dir>> = (0..board.snakes.len())
            .map(|i| safe_candidates(&board, i))
            .collect();
        let visits = cands.iter().map(|c| vec![0.0; c.len()]).collect();
        let values = cands.iter().map(|c| vec![0.0; c.len()]).collect();
        Node {
            board,
            cands,
            visits,
            values,
            total: 0.0,
            children: HashMap::new(),
            terminal,
        }
    }

    /// UCB1 for one snake: first unvisited candidate, else
    /// argmax Q + c·√(ln(total+1)/n). Deterministic tie-break.
    fn select(&self, i: usize) -> usize {
        let n_cands = self.cands[i].len();
        if n_cands == 1 {
            return 0;
        }
        if let Some(c) = (0..n_cands).find(|&c| self.visits[i][c] == 0.0) {
            return c;
        }
        let ln_total = (self.total + 1.0).ln();
        let mut best = 0;
        let mut best_score = f32::NEG_INFINITY;
        for c in 0..n_cands {
            let n = self.visits[i][c];
            let q = self.values[i][c] / n;
            let score = q + C_UCT * (ln_total / n).sqrt();
            if score > best_score {
                best_score = score;
                best = c;
            }
        }
        best
    }
}

fn search(root: &FastBoard, me: usize, deadline: std::time::Instant) -> Dir {
    let mut nodes = vec![Node::new(root.clone())];
    let mut path: Vec<(usize, [usize; MAX_SNAKES])> = Vec::with_capacity(64);
    let mut sims = 0usize;

    while sims < MAX_SIMS {
        if sims % 32 == 0 && std::time::Instant::now() >= deadline {
            break;
        }
        path.clear();
        let mut cur = 0usize;
        let leaf_vals = loop {
            if nodes[cur].terminal {
                break evaluate(&nodes[cur].board);
            }
            let n = nodes[cur].board.snakes.len();
            let mut cidx = [0usize; MAX_SNAKES];
            let mut key = 0u32;
            for (i, slot) in cidx.iter_mut().enumerate().take(n) {
                let c = nodes[cur].select(i);
                *slot = c;
                key = key * 4 + c as u32;
            }
            path.push((cur, cidx));
            match nodes[cur].children.get(&key) {
                Some(&child) => cur = child,
                None => {
                    let mut child = nodes[cur].board.clone();
                    let moves: Vec<Dir> =
                        (0..n).map(|i| nodes[cur].cands[i][cidx[i]]).collect();
                    child.step(&moves);
                    let vals = evaluate(&child);
                    let id = nodes.len();
                    nodes.push(Node::new(child));
                    nodes[cur].children.insert(key, id);
                    break vals;
                }
            }
        };
        for &(nid, cidx) in &path {
            let node = &mut nodes[nid];
            node.total += 1.0;
            for i in 0..node.board.snakes.len() {
                node.visits[i][cidx[i]] += 1.0;
                node.values[i][cidx[i]] += leaf_vals[i];
            }
        }
        sims += 1;
    }

    // Argmax visits (Q breaks ties).
    let root_node = &nodes[0];
    let q = |c: usize| {
        let n = root_node.visits[me][c];
        if n > 0.0 {
            root_node.values[me][c] / n
        } else {
            f32::NEG_INFINITY
        }
    };
    let mut best = 0usize;
    for c in 1..root_node.cands[me].len() {
        if (root_node.visits[me][c], q(c)) > (root_node.visits[me][best], q(best)) {
            best = c;
        }
    }
    root_node.cands[me][best]
}

// ---------------------------------------------------------------------------
// Time-expanded Voronoi evaluation (per-snake values in [-1, 1])
// ---------------------------------------------------------------------------

const UNCLAIMED: u8 = u8::MAX;
const CONTESTED: u8 = u8::MAX - 1;

fn evaluate(board: &FastBoard) -> [f32; MAX_SNAKES] {
    let mut vals = [-1.0f32; MAX_SNAKES];
    if board.is_terminal() {
        match board.winner() {
            Some(w) => vals[w] = 1.0,
            None => {
                for v in vals.iter_mut().take(board.snakes.len()) {
                    *v = DRAW_VALUE;
                }
            }
        }
        return vals;
    }

    let w = board.w as usize;
    let cells = w * board.h as usize;
    let alive: Vec<usize> = (0..board.snakes.len())
        .filter(|&i| board.snakes[i].alive)
        .collect();

    // Turns until each cell is vacated (walls never are).
    let mut blocked_until = vec![0u16; cells];
    for pos in 0..cells as u16 {
        if board.is_wall(pos) {
            blocked_until[pos as usize] = u16::MAX;
        }
    }
    for &i in &alive {
        let s = &board.snakes[i];
        let len = s.len();
        for (k, &seg) in s.body.iter().enumerate() {
            let c = seg as usize;
            blocked_until[c] = blocked_until[c].max((len - k) as u16);
        }
    }

    // Ownership race: longer snakes claim first within a turn; equal-length
    // same-turn contests void the cell and stop both floods.
    let mut owner = vec![UNCLAIMED; cells];
    let mut arrival = vec![0u16; cells];
    let mut race_order = alive.clone();
    race_order.sort_by_key(|&i| std::cmp::Reverse(board.snakes[i].len()));
    let mut frontier: Vec<Vec<usize>> = vec![Vec::new(); board.snakes.len()];
    for &i in &alive {
        frontier[i].push(board.snakes[i].head() as usize);
    }
    let mut t: u16 = 0;
    while frontier.iter().any(|f| !f.is_empty()) && (t as usize) <= cells {
        t += 1;
        let mut next: Vec<Vec<usize>> = vec![Vec::new(); board.snakes.len()];
        for &i in &race_order {
            let my_len = board.snakes[i].len();
            for &c in &frontier[i] {
                if t > 1 && owner[c] != i as u8 {
                    continue; // this frontier cell got contested away
                }
                let (x, y) = ((c % w) as i32, (c / w) as i32);
                for (dx, dy) in [(0, 1), (0, -1), (1, 0), (-1, 0)] {
                    let (nx, ny) = (x + dx, y + dy);
                    if nx < 0 || nx >= w as i32 || ny < 0 || ny >= board.h as i32 {
                        continue;
                    }
                    let nc = ny as usize * w + nx as usize;
                    if blocked_until[nc] > t {
                        continue;
                    }
                    match owner[nc] {
                        UNCLAIMED => {
                            owner[nc] = i as u8;
                            arrival[nc] = t;
                            next[i].push(nc);
                        }
                        o if o != CONTESTED
                            && o != i as u8
                            && arrival[nc] == t
                            && board.snakes[o as usize].len() == my_len =>
                        {
                            owner[nc] = CONTESTED;
                        }
                        _ => {}
                    }
                }
            }
        }
        frontier = next;
    }

    let mut area = [0usize; MAX_SNAKES];
    for c in 0..cells {
        if owner[c] != UNCLAIMED && owner[c] != CONTESTED {
            area[owner[c] as usize] += 1;
        }
    }
    // Earliest arrival at food this snake wins the race to.
    let mut food_dist = [u16::MAX; MAX_SNAKES];
    for &f in &board.food {
        let c = f as usize;
        let o = owner[c];
        if o != UNCLAIMED && o != CONTESTED {
            let o = o as usize;
            food_dist[o] = food_dist[o].min(arrival[c]);
        }
    }

    let free = (0..cells).filter(|&c| blocked_until[c] == 0).count().max(1);
    let n_alive = alive.len() as f32;
    for &i in &alive {
        let s = &board.snakes[i];
        let len = s.len() as f32;
        let area_score = ((area[i] as f32 / free as f32) * n_alive - 1.0).clamp(-1.0, 1.0);
        let health = (s.health as f32 / 100.0) * 2.0 - 1.0;
        let longest_other = alive
            .iter()
            .filter(|&&j| j != i)
            .map(|&j| board.snakes[j].len())
            .max()
            .unwrap_or(0) as f32;
        let length = ((len - longest_other) / 4.0).clamp(-1.0, 1.0);
        // Hunger: the race between health and the nearest food we can win,
        // weighted by how hungry we actually are.
        let urgency = ((100.0 - s.health as f32) / 60.0).clamp(0.0, 1.0);
        let margin = if food_dist[i] == u16::MAX {
            -1.0
        } else {
            ((s.health as f32 - food_dist[i] as f32) / 25.0).clamp(-1.0, 1.0)
        };
        let food = urgency * margin;
        // Trap: a reachable region smaller than the body is a death sentence.
        let reach = reachable_cells(board, i, &blocked_until);
        let trap = ((reach as f32 - len) / len).clamp(-1.0, 0.0);
        vals[i] = (W_AREA * area_score
            + W_HEALTH * health
            + W_LENGTH * length
            + W_FOOD * food
            + W_TRAP * trap)
            .clamp(-1.0, 1.0);
    }
    vals
}

/// Cells snake `i` can reach at all, opponents parked and tails decaying:
/// enterable at step t+1 if vacated by then.
fn reachable_cells(board: &FastBoard, i: usize, blocked_until: &[u16]) -> usize {
    let w = board.w as usize;
    let mut seen = vec![false; blocked_until.len()];
    let mut queue = VecDeque::new();
    let start = board.snakes[i].head() as usize;
    seen[start] = true;
    queue.push_back((start, 0u16));
    let mut count = 0usize;
    while let Some((c, t)) = queue.pop_front() {
        let (x, y) = ((c % w) as i32, (c / w) as i32);
        for (dx, dy) in [(0, 1), (0, -1), (1, 0), (-1, 0)] {
            let (nx, ny) = (x + dx, y + dy);
            if nx < 0 || nx >= w as i32 || ny < 0 || ny >= board.h as i32 {
                continue;
            }
            let nc = ny as usize * w + nx as usize;
            if seen[nc] || blocked_until[nc] > t + 1 {
                continue;
            }
            seen[nc] = true;
            count += 1;
            queue.push_back((nc, t + 1));
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use std::collections::BTreeMap;

    fn config() -> GameConfig {
        GameConfig {
            board_width: 13,
            board_height: 13,
            tick_ms: 1000,
        }
    }

    fn play_duel(a_budget: u32, b_budget: u32, seed: u64) -> (u32, u32) {
        let a: PlayerId = "a".into();
        let b: PlayerId = "b".into();
        let mut engine = Engine::new(config(), vec![a.clone(), b.clone()], seed);
        let mut turn = engine.first_turn();
        for i in 0..300 {
            if turn.game_over() {
                break;
            }
            let mut moves = BTreeMap::new();
            for (p, budget) in [(&a, a_budget), (&b, b_budget)] {
                if turn.alive.contains(p) {
                    moves.insert(
                        p.clone(),
                        choose_move(
                            &config(),
                            &turn,
                            p,
                            BotDifficulty::Hard,
                            seed * 1000 + i,
                            Some(budget),
                        ),
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

    /// A solo hard bot must cruise indefinitely (food discipline included).
    #[test]
    fn hard_bot_survives_solo() {
        let me: PlayerId = "bot".to_string();
        let mut engine = Engine::new(config(), vec![me.clone()], 9);
        let mut turn = engine.first_turn();
        for i in 0..150 {
            assert!(turn.alive.contains(&me), "hard bot died at turn {i}");
            let mv = choose_move(&config(), &turn, &me, BotDifficulty::Hard, i, Some(15));
            let mut moves = BTreeMap::new();
            moves.insert(me.clone(), mv);
            turn = engine.next_turn(&turn, &moves);
        }
    }

    /// The budget is honoured.
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
        assert!(per_move < 400, "hard move took {per_move}ms against 100ms budget");
    }

    /// Thinking time must buy strength: 40ms should dominate 3ms.
    #[test]
    fn more_time_wins() {
        let mut strong_wins = 0;
        let mut weak_wins = 0;
        for seed in 0..5u64 {
            let (strong, weak) = play_duel(40, 3, 42 + seed);
            if strong > weak {
                strong_wins += 1;
            } else if weak > strong {
                weak_wins += 1;
            }
        }
        assert!(
            strong_wins > weak_wins,
            "more time should outlast less (strong {strong_wins} vs weak {weak_wins})"
        );
    }

    /// The pruner never leaves a losable head-to-head when an out exists.
    #[test]
    fn avoids_guaranteed_headon_loss() {
        let me: PlayerId = "a".into();
        let mut engine = Engine::new(config(), vec![me.clone(), "b".into()], 21);
        let turn = engine.first_turn();
        let order: Vec<&PlayerId> = turn.alive.iter().collect();
        let board = FastBoard::from_turn(&config(), &turn, &order);
        let my_idx = order.iter().position(|p| **p == me).unwrap();
        for m in safe_candidates(&board, my_idx) {
            let t = board.step_dir(board.snakes[my_idx].head(), m).unwrap();
            for (j, s) in board.snakes.iter().enumerate() {
                if j == my_idx || s.len() < board.snakes[my_idx].len() {
                    continue;
                }
                for om in candidates(&board, j) {
                    assert_ne!(
                        Some(t),
                        board.step_dir(s.head(), om),
                        "unsafe candidate survived pruning"
                    );
                }
            }
        }
        let _ = engine; // keep the engine alive for symmetry
    }
}
