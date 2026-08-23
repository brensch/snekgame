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
//! Difficulty is purely thinking budget. The search is **deterministic and
//! bit-identical across platforms** for a fixed simulation count (UCB's
//! `ln` uses a platform-exact approximation), which is what makes
//! distributed compute auditable: the server hands a phone
//! `(turn, sims, nonce)`, the phone returns `(move, proof)` where the
//! proof hashes the nonce with the root visit statistics, and the server
//! can recompute the identical search at any time to catch tampering.

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

/// Deterministic move for a fixed simulation count, plus a tamper-evidence
/// proof: fnv1a over the nonce, the chosen move, and the root visit
/// distribution. Identical on every platform for identical inputs, so any
/// party can re-run the search and verify both outputs.
pub fn think(
    config: &GameConfig,
    turn: &Turn,
    me: &PlayerId,
    sims: u32,
    nonce: u64,
) -> (Dir, u64) {
    // Index the world: alive snakes in turn order, me somewhere among them.
    let order: Vec<&PlayerId> = turn.alive.iter().collect();
    let Some(me_idx) = order.iter().position(|p| *p == me) else {
        return (Dir::Up, fnv1a(&[nonce, 0]));
    };
    let board = FastBoard::from_turn(config, turn, &order);

    let root_cands = safe_candidates(&board, me_idx);
    match root_cands.len() {
        0 => return (Dir::Up, fnv1a(&[nonce, 1])),
        1 => {
            let d = root_cands[0];
            return (d, fnv1a(&[nonce, 2, dir_index(d) as u64]));
        }
        _ => {}
    }
    search(&board, me_idx, sims as usize, nonce, EVAL)
}

/// The response checksum a compute host must include: binds the server's
/// nonce and request id to the chosen move. Verified server-side in
/// nanoseconds; a response without the right binding is discarded and
/// strikes the host. (Proves the request was received and answered
/// deliberately — not that the search ran honestly; the deterministic
/// `think` keeps full recompute-audits available if ever needed.)
pub fn bind_proof(nonce: u64, request_id: u64, dir: Dir) -> u64 {
    fnv1a(&[nonce, request_id, dir_index(dir) as u64, 0x736e_656b])
}

fn dir_index(d: Dir) -> usize {
    DIRS.iter().position(|&x| x == d).unwrap_or(0)
}

fn fnv1a(words: &[u64]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for w in words {
        for b in w.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
    }
    h
}

/// Bit-exact `ln` across native and wasm: IEEE mul/add/div/sqrt are exactly
/// rounded everywhere, but libm's `ln` is not — so UCB uses this polynomial
/// (exponent split + cubic in the mantissa). Accuracy ~1e-3, plenty for
/// exploration terms; determinism is the point.
fn fast_ln(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    let bits = x.to_bits();
    let e = ((bits >> 23) & 0xff) as i32 - 127;
    let m = f32::from_bits((bits & 0x007f_ffff) | 0x3f80_0000); // [1, 2)
    // Least-squares cubic fit of log2(m) on [1,2); max error ~1.3e-3.
    let (a, b, c, d) = (0.153_937f32, -1.029_631_1, 3.010_984_8, -2.133_964_3);
    let l2 = ((a * m + b) * m + c) * m + d;
    core::f32::consts::LN_2 * (e as f32 + l2)
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
    /// deaths resolve first and their corpses leave the board (a snake that
    /// died this tick cannot kill anyone), then head-on/body collisions
    /// among the survivors, then eating and starvation.
    fn step(&mut self, moves: &[Dir]) {
        let n = self.snakes.len();

        // Movement (tail pops before self-check, exactly like the engine);
        // phase-1 deaths are removed immediately.
        for i in 0..n {
            if !self.snakes[i].alive {
                continue;
            }
            let head = self.snakes[i].head();
            match self.step_dir(head, moves[i]) {
                Some(t) if !self.is_wall(t) => {
                    self.snakes[i].body.pop_back();
                    if self.snakes[i].body.contains(&t) {
                        self.snakes[i].alive = false; // self-collision
                        self.alive_count -= 1;
                    } else {
                        self.snakes[i].body.push_front(t);
                    }
                }
                _ => {
                    self.snakes[i].alive = false; // wall
                    self.alive_count -= 1;
                }
            }
        }

        // Cross-snake collisions among survivors, simultaneously.
        let mut died = [false; MAX_SNAKES];
        for i in 0..n {
            if !self.snakes[i].alive {
                continue;
            }
            let head = self.snakes[i].head();
            let my_len = self.snakes[i].len();
            for j in 0..n {
                if j == i || !self.snakes[j].alive {
                    continue;
                }
                if self.snakes[j].head() == head {
                    // Head-on: shortest dies, ties kill both.
                    if my_len <= self.snakes[j].len() {
                        died[i] = true;
                    }
                } else if self.snakes[j].body.iter().skip(1).any(|&c| c == head) {
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
        let ln_total = fast_ln(self.total + 1.0);
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

type EvalFn = fn(&FastBoard) -> [f32; MAX_SNAKES];
/// The production evaluation.
const EVAL: EvalFn = evaluate;

fn search(root: &FastBoard, me: usize, max_sims: usize, nonce: u64, eval: EvalFn) -> (Dir, u64) {
    let mut nodes = vec![Node::new(root.clone())];
    let mut path: Vec<(usize, [usize; MAX_SNAKES])> = Vec::with_capacity(64);
    let mut sims = 0usize;

    while sims < max_sims.min(MAX_SIMS) {
        path.clear();
        let mut cur = 0usize;
        let leaf_vals = loop {
            if nodes[cur].terminal {
                break eval(&nodes[cur].board);
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
                    let vals = eval(&child);
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
    let dir = root_node.cands[me][best];
    // Proof binds the nonce to the work actually performed.
    let mut words: Vec<u64> = vec![nonce, dir_index(dir) as u64];
    for c in 0..root_node.cands[me].len() {
        words.push(root_node.visits[me][c] as u64);
    }
    (dir, fnv1a(&words))
}

// ---------------------------------------------------------------------------
// Time-expanded Voronoi evaluation (per-snake values in [-1, 1])
// ---------------------------------------------------------------------------

const UNCLAIMED: u8 = u8::MAX;
const CONTESTED: u8 = u8::MAX - 1;

/// Length-awareness experiments, ablated: `growth` makes food urgency
/// scale with a length deficit (not just hunger); `length_heavy` shifts
/// weight from health onto relative length. `evaluate` is the incumbent.
#[derive(Clone, Copy, PartialEq)]
struct Tuning {
    /// Strength of length-deficit food urgency (0.0 = hunger only).
    growth: f32,
    length_heavy: bool,
}

/// Shipping tuning. `growth: 0.7` earned its place in the arena: 82W-54L-4D
/// over 140 games against `LEGACY` across 11x11 and 13x13 (~60%, p≈0.008),
/// with longer snakes on average. `length_heavy` stays off — re-weighting
/// relative length at health's expense lost 18W-40L and produced *shorter*
/// snakes (it starved). See `arena_ablation` / `arena_confirm_growth`.
const TUNING: Tuning = Tuning { growth: 0.7, length_heavy: false };
/// The pre-tuning evaluation, kept as the arena's benchmark opponent.
#[cfg(test)]
const LEGACY: Tuning = Tuning { growth: 0.0, length_heavy: false };
#[cfg(test)]
const GROWTH_05: Tuning = Tuning { growth: 0.5, length_heavy: false };
#[cfg(test)]
const GROWTH_10: Tuning = Tuning { growth: 1.0, length_heavy: false };
#[cfg(test)]
const LENGTH_HEAVY_ONLY: Tuning = Tuning { growth: 0.0, length_heavy: true };
#[cfg(test)]
const BOTH: Tuning = Tuning { growth: 0.7, length_heavy: true };

fn evaluate(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_impl(board, TUNING)
}

#[cfg(test)]
fn evaluate_legacy(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_impl(board, LEGACY)
}

#[cfg(test)]
fn evaluate_growth_05(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_impl(board, GROWTH_05)
}

#[cfg(test)]
fn evaluate_growth_10(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_impl(board, GROWTH_10)
}

#[cfg(test)]
fn evaluate_length_heavy(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_impl(board, LENGTH_HEAVY_ONLY)
}

#[cfg(test)]
fn evaluate_v2(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_impl(board, BOTH)
}

fn evaluate_impl(board: &FastBoard, tuning: Tuning) -> [f32; MAX_SNAKES] {
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
        let hunger = ((100.0 - s.health as f32) / 60.0).clamp(0.0, 1.0);
        let urgency = if tuning.growth > 0.0 {
            // Shorter (or merely equal — ties die too) than the biggest
            // rival: food matters even on full health.
            let growth_need = ((longest_other + 2.0 - len) / 4.0).clamp(0.0, 1.0);
            hunger.max(tuning.growth * growth_need)
        } else {
            hunger
        };
        let margin = if food_dist[i] == u16::MAX {
            -1.0
        } else {
            ((s.health as f32 - food_dist[i] as f32) / 25.0).clamp(-1.0, 1.0)
        };
        let food = urgency * margin;
        // Trap: a reachable region smaller than the body is a death sentence.
        let reach = reachable_cells(board, i, &blocked_until);
        let trap = ((reach as f32 - len) / len).clamp(-1.0, 0.0);
        let (w_health, w_length) = if tuning.length_heavy {
            (0.05, W_LENGTH + 0.05)
        } else {
            (W_HEALTH, W_LENGTH)
        };
        vals[i] = (W_AREA * area_score
            + w_health * health
            + w_length * length
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

    fn play_duel(a_sims: u32, b_sims: u32, seed: u64) -> (u32, u32) {
        let a: PlayerId = "a".into();
        let b: PlayerId = "b".into();
        let mut engine = Engine::new(config(), vec![a.clone(), b.clone()], seed);
        let mut turn = engine.first_turn();
        for _ in 0..300 {
            if turn.game_over() {
                break;
            }
            let mut moves = BTreeMap::new();
            for (p, sims) in [(&a, a_sims), (&b, b_sims)] {
                if turn.alive.contains(p) {
                    moves.insert(p.clone(), think(&config(), &turn, p, sims, 0).0);
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
    fn bot_survives_solo() {
        let me: PlayerId = "bot".to_string();
        let mut engine = Engine::new(config(), vec![me.clone()], 9);
        let mut turn = engine.first_turn();
        for i in 0..150 {
            assert!(turn.alive.contains(&me), "bot died at turn {i}");
            let (mv, _) = think(&config(), &turn, &me, 1500, 0);
            let mut moves = BTreeMap::new();
            moves.insert(me.clone(), mv);
            turn = engine.next_turn(&turn, &moves);
        }
    }

    /// The whole distributed-compute scheme rests on this: identical inputs
    /// must give identical moves AND identical proofs, every time.
    #[test]
    fn deterministic_move_and_proof() {
        let a: PlayerId = "a".into();
        let mut engine = Engine::new(config(), vec![a.clone(), "b".into()], 5);
        let turn = engine.first_turn();
        let one = think(&config(), &turn, &a, 4000, 777);
        let two = think(&config(), &turn, &a, 4000, 777);
        assert_eq!(one, two, "search must be deterministic");
        // Different nonce -> same move, different proof (nonce is bound in).
        let three = think(&config(), &turn, &a, 4000, 778);
        assert_eq!(one.0, three.0);
        assert_ne!(one.1, three.1, "proof must bind the nonce");
    }

    /// More simulations must buy strength.
    #[test]
    fn more_sims_win() {
        let mut strong_wins = 0;
        let mut weak_wins = 0;
        for seed in 0..5u64 {
            let (strong, weak) = play_duel(6000, 300, 42 + seed);
            if strong > weak {
                strong_wins += 1;
            } else if weak > strong {
                weak_wins += 1;
            }
        }
        assert!(
            strong_wins > weak_wins,
            "more sims should outlast fewer (strong {strong_wins} vs weak {weak_wins})"
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
        let _ = engine;
    }

    /// The fast board must agree with the engine: same-tick corpses are
    /// not lethal, so the search doesn't flee survivable squares.
    #[test]
    fn fast_board_corpse_is_not_lethal() {
        let a: PlayerId = "a".into();
        let b: PlayerId = "b".into();
        let mut snakes = BTreeMap::new();
        snakes.insert(a.clone(), vec![50u16, 49, 48, 47, 60, 59, 58, 57]);
        snakes.insert(b.clone(), vec![44u16, 31, 18, 19, 20, 33]);
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
        let order: Vec<&PlayerId> = turn.alive.iter().collect();
        let mut board = FastBoard::from_turn(&config(), &turn, &order);
        board.step(&[Dir::Right, Dir::Down]);
        assert!(!board.snakes[0].alive, "A hit the wall");
        assert!(board.snakes[1].alive, "B must survive the corpse");
        assert_eq!(board.snakes[1].head(), 57);
        assert!(board.is_terminal());
        assert_eq!(board.winner(), Some(1));
    }

    fn think_with(
        config: &GameConfig,
        turn: &Turn,
        me: &PlayerId,
        sims: u32,
        eval: EvalFn,
    ) -> Dir {
        let order: Vec<&PlayerId> = turn.alive.iter().collect();
        let Some(me_idx) = order.iter().position(|p| *p == me) else {
            return Dir::Up;
        };
        let board = FastBoard::from_turn(config, turn, &order);
        let cands = safe_candidates(&board, me_idx);
        match cands.len() {
            0 => Dir::Up,
            1 => cands[0],
            _ => search(&board, me_idx, sims as usize, 0, eval).0,
        }
    }

    fn arena_config() -> GameConfig {
        board_config(11)
    }

    fn board_config(size: u8) -> GameConfig {
        GameConfig {
            board_width: size,
            board_height: size,
            tick_ms: 1000,
        }
    }

    /// One deterministic duel. Returns (challenger_score, incumbent_score)
    /// where a score is (survival_turns, final_length) — length breaks the
    /// "both alive at the cap" ties that make survival-only tests blind.
    fn duel(
        challenger: EvalFn,
        incumbent: EvalFn,
        sims: u32,
        seed: u64,
        swap: bool,
        board: u8,
    ) -> ((u32, usize), (u32, usize)) {
        let cfg = board_config(board);
        let a: PlayerId = "a".into();
        let b: PlayerId = "b".into();
        let mut engine = Engine::new(cfg, vec![a.clone(), b.clone()], seed);
        let mut turn = engine.first_turn();
        for _ in 0..250 {
            if turn.game_over() {
                break;
            }
            let mut moves = BTreeMap::new();
            for p in [&a, &b] {
                if !turn.alive.contains(p) {
                    continue;
                }
                let is_challenger = (p == &a) ^ swap;
                let eval = if is_challenger { challenger } else { incumbent };
                moves.insert(p.clone(), think_with(&cfg, &turn, p, sims, eval));
            }
            turn = engine.next_turn(&turn, &moves);
        }
        let score = |p: &PlayerId| {
            let survived = turn
                .winners
                .iter()
                .find(|w| &w.player == p)
                .map(|w| w.survival_turns)
                .unwrap_or(turn.turn_number);
            let len = turn.snakes.get(p).map(|s| s.len()).unwrap_or(0);
            (survived, len)
        };
        if swap {
            (score(&b), score(&a))
        } else {
            (score(&a), score(&b))
        }
    }

    /// Head-to-head arena, parallel across cores. Run with
    /// `--ignored --nocapture` to see the score.
    fn run_arena(name: &str, challenger: EvalFn, incumbent: EvalFn, seeds: u64, sims: u32) {
        run_arena_on(name, challenger, incumbent, seeds, sims, 11)
    }

    fn run_arena_on(
        name: &str,
        challenger: EvalFn,
        incumbent: EvalFn,
        seeds: u64,
        sims: u32,
        board: u8,
    ) {
        let jobs: Vec<(u64, bool)> = (0..seeds)
            .flat_map(|s| [(100 + s, false), (100 + s, true)])
            .collect();
        let results: Vec<((u32, usize), (u32, usize))> = std::thread::scope(|scope| {
            let chunks: Vec<_> = jobs
                .chunks(jobs.len().div_ceil(12).max(1))
                .map(|chunk| {
                    scope.spawn(move || {
                        chunk
                            .iter()
                            .map(|&(seed, swap)| {
                                duel(challenger, incumbent, sims, seed, swap, board)
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            chunks.into_iter().flat_map(|h| h.join().unwrap()).collect()
        });

        let (mut wins, mut losses, mut draws) = (0, 0, 0);
        let (mut ch_len, mut in_len) = (0usize, 0usize);
        for (ch, inc) in &results {
            ch_len += ch.1;
            in_len += inc.1;
            match ch.cmp(inc) {
                std::cmp::Ordering::Greater => wins += 1,
                std::cmp::Ordering::Less => losses += 1,
                std::cmp::Ordering::Equal => draws += 1,
            }
        }
        let n = results.len();
        println!(
            "ARENA {name}: {wins}W {losses}L {draws}D of {n} @ {sims} sims, {board}x{board} \
             | mean final length {:.1} vs {:.1}",
            ch_len as f32 / n as f32,
            in_len as f32 / n as f32,
        );
    }

    /// Do the two evals even disagree? (Sanity check for arena power.)
    #[test]
    #[ignore]
    fn evals_disagree_often_enough() {
        let cfg = arena_config();
        let a: PlayerId = "a".into();
        let b: PlayerId = "b".into();
        let mut engine = Engine::new(cfg, vec![a.clone(), b.clone()], 7);
        let mut turn = engine.first_turn();
        let (mut same, mut diff) = (0, 0);
        for _ in 0..60 {
            if turn.game_over() {
                break;
            }
            let m1 = think_with(&cfg, &turn, &a, 1_200, evaluate_legacy);
            let m2 = think_with(&cfg, &turn, &a, 1_200, evaluate);
            if m1 == m2 { same += 1 } else { diff += 1 }
            let mut moves = BTreeMap::new();
            moves.insert(a.clone(), m1);
            if turn.alive.contains(&b) {
                moves.insert(b.clone(), think_with(&cfg, &turn, &b, 1_200, evaluate));
            }
            turn = engine.next_turn(&turn, &moves);
        }
        println!("EVAL DISAGREEMENT: {diff} differ, {same} same");
    }

    #[test]
    #[ignore]
    fn arena_ablation() {
        run_arena("growth-urgency only", evaluate, evaluate_legacy, 30, 1_200);
        run_arena("length-heavy only  ", evaluate_length_heavy, evaluate_legacy, 30, 1_200);
        run_arena("both               ", evaluate_v2, evaluate_legacy, 30, 1_200);
    }

    /// Confirmation: does growth-urgency hold up at higher power, on the
    /// default board, and across strengths?
    #[test]
    #[ignore]
    fn arena_confirm_growth() {
        run_arena_on("shipped 0.7 @13x13", evaluate, evaluate_legacy, 40, 1_200, 13);
        run_arena("growth 0.5        ", evaluate_growth_05, evaluate_legacy, 30, 1_200);
        run_arena("growth 1.0        ", evaluate_growth_10, evaluate_legacy, 30, 1_200);
    }

    /// fast_ln tracks real ln closely enough for UCB.
    #[test]
    fn fast_ln_is_close() {
        for i in 1..2000 {
            let x = i as f32 * 0.37;
            let err = (fast_ln(x) - x.ln()).abs();
            assert!(err < 0.02, "fast_ln({x}) off by {err}");
        }
    }
}
