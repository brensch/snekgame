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
//! - **The tree carries over between turns** ([`begin_from`]): the subtree
//!   under the joint move that actually happened is already a search of
//!   the new position, so each turn starts thousands of simulations in.
//!   It is dropped when food spawns or a snake dies — the search never
//!   models either, so no child matches.
//!
//! Difficulty is purely thinking budget. A fresh search ([`think`],
//! [`begin`]) is deterministic for a fixed simulation count; one that
//! carries a tree over depends on what came before, so moves are no longer
//! re-derivable from a single turn. Nothing relies on that: the server
//! checks only [`bind_proof`], which binds its nonce and request id to the
//! move.

use crate::engine::{dir_between, step};
use crate::types::{BotDifficulty, Dir, GameConfig, PlayerId, Turn};
use std::cell::RefCell;
use std::cmp::{Ordering, Reverse};
use std::collections::VecDeque;

/// Per-move thinking budget in milliseconds.
#[must_use]
pub const fn budget_ms(difficulty: BotDifficulty) -> u32 {
    match difficulty {
        BotDifficulty::Easy => 8,
        BotDifficulty::Medium => 35,
        BotDifficulty::Hard => 100,
    }
}

const C_UCT: f32 = 1.2;
const DRAW_VALUE: f32 = -0.25;
const MAX_SIMS: usize = 200_000;
/// Tree size cap, counting nodes carried over from earlier turns: a node is
/// a few hundred bytes, and a phone's tab must not balloon.
const MAX_NODES: usize = 150_000;
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
/// proof.
///
/// The proof is fnv1a over the nonce, the chosen move, and the root visit
/// distribution. Both are identical on every platform for identical inputs,
/// so any party can re-run the search and verify them.
#[must_use]
pub fn think(config: &GameConfig, turn: &Turn, me: &PlayerId, sims: u32, nonce: u64) -> (Dir, u64) {
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
            return (d, fnv1a(&[nonce, 2, dir_index(d)]));
        }
        _ => {}
    }
    search(&board, me_idx, sims as usize, nonce, EVAL)
}

/// The response checksum a compute host must include: binds the server's
/// nonce and request id to the chosen move.
///
/// Verified server-side in nanoseconds; a response without the right
/// binding is discarded and strikes the host. (Proves the request was
/// received and answered deliberately — not that the search ran honestly;
/// the deterministic `think` keeps full recompute-audits available if ever
/// needed.)
#[must_use]
pub fn bind_proof(nonce: u64, request_id: u64, dir: Dir) -> u64 {
    fnv1a(&[nonce, request_id, dir_index(dir), 0x736e_656b])
}

/// The move's position in [`DIRS`], as a proof word.
const fn dir_index(d: Dir) -> u64 {
    match d {
        Dir::Up => 0,
        Dir::Down => 1,
        Dir::Left => 2,
        Dir::Right => 3,
    }
}

fn fnv1a(words: &[u64]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for w in words {
        for b in w.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
    }
    h
}

/// Bit-exact `ln` across native and wasm: IEEE mul/add/div/sqrt are exactly
/// rounded everywhere, but libm's `ln` is not — so UCB uses this polynomial
/// (exponent split + cubic in the mantissa). Accuracy ~1e-3, plenty for
/// exploration terms; determinism is the point.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add rounds differently (moving every search result) and is a slow libm call on wasm"
)]
fn fast_ln(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    let bits = x.to_bits();
    // Shifting out the sign bit leaves the biased exponent in the top byte.
    let exponent = f32::from((bits << 1).to_be_bytes()[0]) - 127.0;
    // The mantissa, in [1, 2).
    let mantissa = f32::from_bits((bits & 0x007f_ffff) | 0x3f80_0000);
    // Least-squares cubic fit of log2(m) on [1,2); max error ~1.3e-3.
    let (c3, c2, c1, c0) = (0.153_937f32, -1.029_631_1, 3.010_985, -2.133_964_3);
    let l2 = ((c3 * mantissa + c2) * mantissa + c1) * mantissa + c0;
    core::f32::consts::LN_2 * (exponent + l2)
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
    w: u16,
    h: u16,
    snakes: Vec<Snake>,
    food: Vec<u16>,
    alive_count: usize,
    /// Terminal threshold: multi games end at 1 alive, solo games at 0.
    min_alive: usize,
}

impl FastBoard {
    fn from_turn(config: &GameConfig, turn: &Turn, order: &[&PlayerId]) -> Self {
        let snakes = order
            .iter()
            .map(|p| Snake {
                body: turn.snakes[*p].iter().copied().collect(),
                health: turn.health.get(*p).copied().unwrap_or(100),
                alive: true,
            })
            .collect::<Vec<_>>();
        let alive_count = snakes.len();
        Self {
            w: u16::from(config.board_width),
            h: u16::from(config.board_height),
            snakes,
            food: turn.food.clone(),
            alive_count,
            min_alive: usize::from(alive_count > 1),
        }
    }

    /// The same position: every snake's body, health and liveness, and the
    /// same food (in any order — eating reorders the list). Food is what
    /// makes reuse safe: the search never spawns any, so a tree whose child
    /// matches the new turn exactly was built for the world that happened.
    fn same_position(&self, other: &Self) -> bool {
        if self.w != other.w
            || self.h != other.h
            || self.snakes.len() != other.snakes.len()
            || self.food.len() != other.food.len()
        {
            return false;
        }
        let snakes_match = self
            .snakes
            .iter()
            .zip(&other.snakes)
            .all(|(a, b)| a.alive == b.alive && a.health == b.health && a.body == b.body);
        if !snakes_match {
            return false;
        }
        let mut a = self.food.clone();
        let mut b = other.food.clone();
        a.sort_unstable();
        b.sort_unstable();
        a == b
    }

    const fn is_wall(&self, pos: u16) -> bool {
        let (x, y) = (pos % self.w, pos / self.w);
        x == 0 || y == 0 || x == self.w - 1 || y == self.h - 1
    }

    fn step_dir(&self, from: u16, dir: Dir) -> Option<u16> {
        step(from, dir, self.w, self.h)
    }

    const fn is_terminal(&self) -> bool {
        self.alive_count <= self.min_alive
    }

    /// Sole survivor of a multi game, if the board is terminal that way.
    fn winner(&self) -> Option<usize> {
        if self.min_alive == 1 && self.alive_count == 1 {
            self.snakes.iter().position(|s| s.alive)
        } else {
            None
        }
    }

    /// One tick of the real rules (minus random food spawns): wall/self
    /// deaths resolve first and their corpses leave the board (a snake that
    /// died this tick cannot kill anyone), then head-on/body collisions
    /// among the survivors, then eating and starvation.
    fn step(&mut self, moves: &[Dir]) {
        let n = self.snakes.len();

        // Movement (tail pops before self-check, exactly like the engine);
        // phase-1 deaths are removed immediately.
        for (i, &dir) in moves.iter().enumerate().take(n) {
            if !self.snakes[i].alive {
                continue;
            }
            let head = self.snakes[i].head();
            match self.step_dir(head, dir) {
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
        for (i, dead) in died.iter_mut().enumerate().take(n) {
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
                        *dead = true;
                    }
                } else if self.snakes[j].body.iter().skip(1).any(|&c| c == head) {
                    *dead = true;
                }
            }
        }

        for (snake, &dead) in self.snakes.iter_mut().zip(&died) {
            if dead && snake.alive {
                snake.alive = false;
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
                if let Some(&tail) = self.snakes[i].body.back() {
                    self.snakes[i].body.push_back(tail);
                }
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
    dir_between(s.body[1], s.body[0], board.w)
}

/// Up to four moves, inline: candidate lists are built for every snake at
/// every node, so they must never touch the heap.
#[derive(Clone, Copy)]
struct Cands {
    dirs: [Dir; 4],
    n: u8,
}

impl Cands {
    const fn empty() -> Self {
        Self {
            dirs: [Dir::Up; 4],
            n: 0,
        }
    }
    const fn push(&mut self, d: Dir) {
        self.dirs[self.n as usize] = d;
        self.n += 1;
    }
}

impl std::ops::Deref for Cands {
    type Target = [Dir];
    fn deref(&self) -> &[Dir] {
        &self.dirs[..self.n as usize]
    }
}

impl IntoIterator for Cands {
    type Item = Dir;
    type IntoIter = std::iter::Take<std::array::IntoIter<Dir, 4>>;
    fn into_iter(self) -> Self::IntoIter {
        self.dirs.into_iter().take(self.n as usize)
    }
}

/// Non-reversing, on-board, non-wall moves; a trapped snake keeps all four
/// (the search picks the least bad); the dead get a dummy.
fn candidates(board: &FastBoard, i: usize) -> Cands {
    let mut v = Cands::empty();
    if !board.snakes[i].alive {
        v.push(Dir::Up);
        return v;
    }
    let head = board.snakes[i].head();
    let reverse = last_dir(board, i).map(Dir::opposite);
    for &m in &DIRS {
        if Some(m) == reverse {
            continue;
        }
        match board.step_dir(head, m) {
            Some(t) if !board.is_wall(t) => v.push(m),
            _ => {}
        }
    }
    if v.n == 0 {
        v = Cands { dirs: DIRS, n: 4 };
    }
    v
}

/// [`candidates`] minus guaranteed head-to-head losses: squares a
/// longer-or-equal live opponent's head can also reach next turn. If every
/// move is such a square, keep them all and let the search pick.
fn safe_candidates(board: &FastBoard, i: usize) -> Cands {
    let base = candidates(board, i);
    if base.len() <= 1 || !board.snakes[i].alive {
        return base;
    }
    let my_len = board.snakes[i].len();
    // Every square a big-enough rival head can step onto next turn.
    let mut danger = [0u16; 4 * MAX_SNAKES];
    let mut n_danger = 0;
    for (j, s) in board.snakes.iter().enumerate() {
        if j == i || !s.alive || s.len() < my_len {
            continue;
        }
        for m in candidates(board, j) {
            if let Some(t) = board.step_dir(s.head(), m) {
                danger[n_danger] = t;
                n_danger += 1;
            }
        }
    }
    let danger = &danger[..n_danger];
    let head = board.snakes[i].head();
    let mut safe = Cands::empty();
    for &m in base.iter() {
        if board
            .step_dir(head, m)
            .is_some_and(|t| !danger.contains(&t))
        {
            safe.push(m);
        }
    }
    if safe.n == 0 { base } else { safe }
}

// ---------------------------------------------------------------------------
// Decoupled-UCT search over joint moves
// ---------------------------------------------------------------------------

/// Per-snake statistics live inline (four moves at most); children are a
/// short list keyed by the joint move — a node rarely has more than a
/// dozen, and a linear scan of a few words beats hashing.
struct Node {
    board: FastBoard,
    cands: [Cands; MAX_SNAKES],
    /// (visits, summed value) per snake per candidate, at `i * 4 + c` —
    /// sized to the snakes actually playing rather than the maximum, since
    /// it is most of a node and a tree holds a hundred thousand of them.
    stats: Vec<[f32; 2]>,
    total: f32,
    children: Vec<(u32, u32)>,
    terminal: bool,
}

impl Node {
    fn new(board: FastBoard) -> Self {
        let terminal = board.is_terminal();
        let mut cands = [Cands::empty(); MAX_SNAKES];
        for (i, c) in cands.iter_mut().enumerate().take(board.snakes.len()) {
            *c = safe_candidates(&board, i);
        }
        Self {
            stats: vec![[0.0; 2]; board.snakes.len() * 4],
            board,
            cands,
            total: 0.0,
            children: Vec::new(),
            terminal,
        }
    }

    fn child(&self, key: u32) -> Option<usize> {
        self.children
            .iter()
            .find(|&&(k, _)| k == key)
            .map(|&(_, id)| id as usize)
    }

    /// UCB1 for one snake: first unvisited candidate, else
    /// argmax Q + c·√(ln(total+1)/n). Deterministic tie-break. Returns the
    /// candidate's index (below 4, so it packs into the joint-move key).
    #[expect(
        clippy::suboptimal_flops,
        reason = "mul_add rounds differently (moving every search result) and is a slow libm call on wasm"
    )]
    fn select(&self, i: usize, c_uct: f32) -> u8 {
        let n_cands = self.cands[i].n;
        if n_cands == 1 {
            return 0;
        }
        let stats = &self.stats[i * 4..i * 4 + 4];
        if let Some(c) = (0..n_cands).find(|&c| stats[usize::from(c)][0] == 0.0) {
            return c;
        }
        let ln_total = fast_ln(self.total + 1.0);
        let mut best = 0;
        let mut best_score = f32::NEG_INFINITY;
        for (c, &[n, v]) in (0..n_cands).zip(stats) {
            let q = v / n;
            let score = q + c_uct * (ln_total / n).sqrt();
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

/// A search that can be run in pieces. The browser's bot worker drives it
/// in slices against a deadline. Chunking cannot change the answer —
/// simulations are sequential, so N+M sims equals one run of N+M.
pub struct Search {
    nodes: Vec<Node>,
    /// The current descent: each node and the candidate every snake chose
    /// there.
    path: Vec<(usize, [u8; MAX_SNAKES])>,
    /// Who each board index is (the turn's alive list), so a later turn can
    /// tell whether this tree still describes its world.
    order: Vec<PlayerId>,
    me: usize,
    remaining: usize,
    nonce: u64,
    eval: EvalFn,
    /// UCB exploration constant.
    c_uct: f32,
    /// Set when the position needed no search at all.
    immediate: Option<(Dir, u64)>,
}

impl Search {
    /// Run at most `chunk` simulations. Returns true when finished.
    pub fn step(&mut self, chunk: usize) -> bool {
        if self.immediate.is_some() {
            return true;
        }
        let n = chunk.min(self.remaining);
        for _ in 0..n {
            self.simulate();
        }
        self.remaining -= n;
        self.remaining == 0
    }

    /// Nodes in the tree, including any carried over from the last turn.
    #[must_use]
    pub const fn tree_size(&self) -> usize {
        self.nodes.len()
    }

    /// The chosen move and its proof. Valid once `step` has returned true.
    #[must_use]
    pub fn result(&self) -> (Dir, u64) {
        if let Some(done) = self.immediate {
            return done;
        }
        readout(&self.nodes, self.me, self.nonce)
    }

    fn simulate(&mut self) {
        let (nodes, path, eval, c_uct) = (&mut self.nodes, &mut self.path, self.eval, self.c_uct);
        path.clear();
        let mut cur = 0usize;
        let leaf_vals = loop {
            if nodes[cur].terminal {
                break eval(&nodes[cur].board);
            }
            let n = nodes[cur].board.snakes.len();
            let mut cidx = [0u8; MAX_SNAKES];
            let mut key = 0u32;
            for (i, slot) in cidx.iter_mut().enumerate().take(n) {
                let c = nodes[cur].select(i, c_uct);
                *slot = c;
                key = key * 4 + u32::from(c);
            }
            path.push((cur, cidx));
            let Some(child) = nodes[cur].child(key) else {
                let mut child = nodes[cur].board.clone();
                let mut moves = [Dir::Up; MAX_SNAKES];
                for (i, m) in moves.iter_mut().enumerate().take(n) {
                    *m = nodes[cur].cands[i][usize::from(cidx[i])];
                }
                child.step(&moves[..n]);
                let vals = eval(&child);
                // At the size cap the tree stops growing but the search
                // doesn't: the leaf is scored and backed up, unstored.
                if nodes.len() < MAX_NODES
                    && let Ok(id) = u32::try_from(nodes.len())
                {
                    nodes.push(Node::new(child));
                    nodes[cur].children.push((key, id));
                }
                break vals;
            };
            cur = child;
        };
        for &(nid, cidx) in path.iter() {
            let node = &mut nodes[nid];
            node.total += 1.0;
            for i in 0..node.board.snakes.len() {
                let st = &mut node.stats[i * 4 + usize::from(cidx[i])];
                st[0] += 1.0;
                st[1] += leaf_vals[i];
            }
        }
    }
}

/// Argmax visits (Q breaks ties), plus the proof binding the nonce to the
/// work actually performed.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "visit counts are whole numbers in [0, 2^24], exact in f32 and in u64; Rust has no From<f32> for integers"
)]
fn readout(nodes: &[Node], me: usize, nonce: u64) -> (Dir, u64) {
    let root_node = &nodes[0];
    let q = |c: usize| {
        let [n, v] = root_node.stats[me * 4 + c];
        if n > 0.0 { v / n } else { f32::NEG_INFINITY }
    };
    let mut best = 0usize;
    for c in 1..root_node.cands[me].len() {
        let visits = |c: usize| root_node.stats[me * 4 + c][0];
        if (visits(c), q(c)) > (visits(best), q(best)) {
            best = c;
        }
    }
    let dir = root_node.cands[me][best];
    let mut words: Vec<u64> = vec![nonce, dir_index(dir)];
    for c in 0..root_node.cands[me].len() {
        words.push(root_node.stats[me * 4 + c][0] as u64);
    }
    (dir, fnv1a(&words))
}

fn search(root: &FastBoard, me: usize, max_sims: usize, nonce: u64, eval: EvalFn) -> (Dir, u64) {
    let mut s = Search {
        nodes: vec![Node::new(root.clone())],
        path: Vec::with_capacity(64),
        order: Vec::new(),
        me,
        remaining: max_sims.min(MAX_SIMS),
        nonce,
        eval,
        c_uct: C_UCT,
        immediate: None,
    };
    while !s.step(usize::MAX) {}
    s.result()
}

/// Start a search that the caller drives with [`Search::step`]. Trivial
/// positions (boxed in, or a single legal move) finish immediately.
#[must_use]
pub fn begin(config: &GameConfig, turn: &Turn, me: &PlayerId, sims: u32, nonce: u64) -> Search {
    begin_from(None, config, turn, me, sims, nonce)
}

/// [`begin`], but starting from last turn's tree when it still applies.
///
/// The subtree under the joint move that actually happened is already a
/// search of this exact position, often thousands of simulations deep. It
/// applies only if that child's board *is* this turn — same snakes, same
/// health, same food. New food (the one thing the search never imagines)
/// or a death changing who is playing means no child matches, and the
/// search starts fresh.
#[must_use]
pub fn begin_from(
    prev: Option<Search>,
    config: &GameConfig,
    turn: &Turn,
    me: &PlayerId,
    sims: u32,
    nonce: u64,
) -> Search {
    let finished = |immediate: (Dir, u64)| Search {
        nodes: Vec::new(),
        path: Vec::new(),
        order: Vec::new(),
        me: 0,
        remaining: 0,
        nonce,
        eval: EVAL,
        c_uct: C_UCT,
        immediate: Some(immediate),
    };

    let order: Vec<PlayerId> = turn.alive.clone();
    let Some(me_idx) = order.iter().position(|p| p == me) else {
        return finished((Dir::Up, fnv1a(&[nonce, 0])));
    };
    let refs: Vec<&PlayerId> = order.iter().collect();
    let board = FastBoard::from_turn(config, turn, &refs);
    let cands = safe_candidates(&board, me_idx);
    match cands.len() {
        0 => finished((Dir::Up, fnv1a(&[nonce, 1]))),
        1 => {
            let d = cands[0];
            finished((d, fnv1a(&[nonce, 2, dir_index(d)])))
        }
        _ => {
            let nodes = prev
                .filter(|p| p.immediate.is_none() && p.order == order && !p.nodes.is_empty())
                .and_then(|p| {
                    let next = p.nodes[0]
                        .children
                        .iter()
                        .map(|&(_, id)| id as usize)
                        .find(|&id| p.nodes[id].board.same_position(&board))?;
                    Some(reroot(p.nodes, next))
                })
                .unwrap_or_else(|| vec![Node::new(board)]);
            Search {
                nodes,
                path: Vec::with_capacity(64),
                order,
                me: me_idx,
                remaining: (sims as usize).min(MAX_SIMS),
                nonce,
                eval: EVAL,
                c_uct: C_UCT,
                immediate: None,
            }
        }
    }
}

/// The subtree under `root`, moved (not copied) into a fresh arena with
/// `root` at index 0. Everything else is dropped.
fn reroot(nodes: Vec<Node>, root: usize) -> Vec<Node> {
    let mut slots: Vec<Option<Node>> = nodes.into_iter().map(Some).collect();
    let mut out: Vec<Node> = Vec::new();
    out.extend(slots[root].take());
    let mut i = 0;
    while i < out.len() {
        let kids = std::mem::take(&mut out[i].children);
        let mut moved = Vec::with_capacity(kids.len());
        for (key, old) in kids {
            // Ids were u32 in the old arena, and this one holds fewer nodes.
            if let Ok(id) = u32::try_from(out.len())
                && let Some(node) = slots[old as usize].take()
            {
                moved.push((key, id));
                out.push(node);
            }
        }
        out[i].children = moved;
        i += 1;
    }
    out
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
    /// Treat "not strictly longer than the biggest rival" as maximum
    /// growth urgency: +1 is the head-on threshold (equal length kills
    /// both, +1 wins outright), so parity is not good enough.
    bully_target: bool,
    /// Multiplier on growth urgency while everyone is still short — the
    /// early game is when a length lead is cheapest to win and most
    /// punishing to hold (1.0 = off).
    early_boost: f32,
    /// Explicit value for standing at the head-on threshold: rewards being
    /// strictly longer, penalises exact parity (mutual death) (0.0 = off).
    dominance: f32,
    /// Longest snake on the board below which the early boost applies.
    early_until: f32,
    /// Continuous food model: appetite (baseline + hunger + length
    /// deficit) times a smooth proximity gradient, instead of the old
    /// saturating can-I-reach-it-before-starving term.
    gradient_food: bool,
    /// Appetite of a healthy snake at parity — what makes it eat
    /// opportunistically while doing space control.
    appetite_base: f32,
    /// Distance scale of the proximity gradient (cells).
    food_scale: f32,
    /// Weight of food we only *tie* the race to (contested) versus food we
    /// outright own. Contested food is risky, not invisible — at a
    /// symmetric start the centre food is contested by everyone.
    contested_food: f32,
}

/// Shipping tuning: one continuous appetite, no thresholds. Every value
/// earned in two-seat deterministic duels (see the `arena_*` tests).
///
/// The food term is `appetite × proximity`, where appetite = a baseline
/// (opportunistic eating while doing space control) + hunger + length
/// deficit, and proximity is a smooth `1/(1+d/scale)` pull. The old term
/// asked "can I reach food before starving?", which saturated at 1.0 for
/// every distance under ~75 — the search had no slope to climb, which is
/// why a healthy snake never ate. Early-game aggression now *emerges*
/// from the deficit term rather than a hard-coded turn window.
///
/// - `appetite_base: 0.45` — 64W-16L-0D over 80 games vs the previous
///   tuning, snakes 20.7 vs 16.4 long; also 40W-20L on 11x11. 0.22 and
///   0.32 were weaker, 0.60 slightly past the peak.
/// - `contested_food: 0.6` — food we only *tie* the race to is risky, not
///   invisible. At a symmetric start the centre food is contested by
///   everyone, so the old code could not see it at all: 54W-24L with it
///   visible versus 30W-20L blind.
/// - `food_scale: 5.0` — cells over which the pull decays.
///
/// The pre-gradient knobs below it (`growth`, `early_boost`, `dominance`,
/// `length_heavy`, `bully_target`) are inert here and survive only as
/// arena benchmarks documenting what lost.
const TUNING: Tuning = Tuning {
    growth: 0.0,
    length_heavy: false,
    bully_target: false,
    early_boost: 1.0,
    dominance: 0.0,
    early_until: 9.0,
    gradient_food: true,
    appetite_base: 0.45,
    food_scale: 5.0,
    contested_food: 0.6,
};

/// The threshold-based tuning this replaced (growth urgency + a 1.6x boost
/// under length 9), kept as the arena's benchmark opponent.
#[cfg(test)]
const PRE_GRADIENT: Tuning = Tuning {
    growth: 0.7,
    early_boost: 1.6,
    gradient_food: false,
    appetite_base: 0.0,
    contested_food: 0.0,
    ..TUNING
};

/// The original evaluation, before any length awareness.
#[cfg(test)]
const LEGACY: Tuning = Tuning {
    growth: 0.0,
    length_heavy: false,
    bully_target: false,
    early_boost: 1.0,
    dominance: 0.0,
    early_until: 9.0,
    gradient_food: false,
    appetite_base: 0.0,
    food_scale: 5.0,
    contested_food: 0.0,
};

// Experiments, each isolated against the tuning it challenged.
#[cfg(test)]
const APPETITE_022: Tuning = Tuning {
    appetite_base: 0.22,
    ..TUNING
};
#[cfg(test)]
const APPETITE_060: Tuning = Tuning {
    appetite_base: 0.60,
    ..TUNING
};
#[cfg(test)]
const BLIND_CONTESTED: Tuning = Tuning {
    contested_food: 0.0,
    ..TUNING
};
#[cfg(test)]
const LENGTH_HEAVY_ONLY: Tuning = Tuning {
    length_heavy: true,
    ..LEGACY
};
#[cfg(test)]
const GROWTH_ONLY: Tuning = Tuning {
    growth: 0.7,
    ..LEGACY
};
#[cfg(test)]
const BULLY: Tuning = Tuning {
    bully_target: true,
    ..PRE_GRADIENT
};

fn evaluate(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_impl(board, TUNING)
}

#[cfg(test)]
fn evaluate_pre_gradient(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_impl(board, PRE_GRADIENT)
}

#[cfg(test)]
fn evaluate_legacy(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_impl(board, LEGACY)
}

#[cfg(test)]
fn evaluate_appetite_022(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_impl(board, APPETITE_022)
}

#[cfg(test)]
fn evaluate_appetite_060(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_impl(board, APPETITE_060)
}

#[cfg(test)]
fn evaluate_blind_contested(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_impl(board, BLIND_CONTESTED)
}

#[cfg(test)]
fn evaluate_growth_only(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_impl(board, GROWTH_ONLY)
}

#[cfg(test)]
fn evaluate_length_heavy(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_impl(board, LENGTH_HEAVY_ONLY)
}

#[cfg(test)]
fn evaluate_bully(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_impl(board, BULLY)
}

/// Buffers the evaluation reuses on every call (one set per thread). An
/// evaluation runs once per simulation — tens of thousands of times a move
/// — and allocating these afresh each time was most of its cost.
#[derive(Default)]
struct Scratch {
    blocked_until: Vec<u16>,
    owner: Vec<u8>,
    arrival: Vec<u16>,
    is_food: Vec<bool>,
    frontier: Vec<Vec<usize>>,
    next: Vec<Vec<usize>>,
    /// `seen[c] == stamp` marks c visited in the current flood; bumping the
    /// stamp clears the whole array at once.
    seen: Vec<u32>,
    stamp: u32,
    queue: Vec<(usize, u16)>,
    /// `blocked_until` for an empty board of `walls_for` — the wall ring —
    /// built once per board size and copied in.
    walls: Vec<u16>,
    walls_for: (u16, u16),
    /// Non-wall cells on that board.
    interior: u16,
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch::default());
}

fn evaluate_impl(board: &FastBoard, tuning: Tuning) -> [f32; MAX_SNAKES] {
    SCRATCH.with(|scratch| evaluate_with(&mut scratch.borrow_mut(), board, tuning))
}

/// A body or board count as the u16 the evaluation works in. Nothing on a
/// board of at most 255 x 255 cells counts past that.
fn count(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

fn evaluate_with(sc: &mut Scratch, board: &FastBoard, tuning: Tuning) -> [f32; MAX_SNAKES] {
    let mut vals = [-1.0f32; MAX_SNAKES];
    if board.is_terminal() {
        match board.winner() {
            Some(winner) => vals[winner] = 1.0,
            None => {
                for v in vals.iter_mut().take(board.snakes.len()) {
                    *v = DRAW_VALUE;
                }
            }
        }
        return vals;
    }

    // Living snakes by index, and every snake's length.
    let mut lens = [0u16; MAX_SNAKES];
    let mut alive_buf = [0u8; MAX_SNAKES];
    let mut n_alive = 0u8;
    for (id, s) in (0u8..).zip(&board.snakes) {
        lens[usize::from(id)] = count(s.len());
        if s.alive {
            alive_buf[usize::from(n_alive)] = id;
            n_alive += 1;
        }
    }
    let alive = &alive_buf[..usize::from(n_alive)];

    // Turns until each cell is vacated (walls never are).
    let cells = usize::from(board.w) * usize::from(board.h);
    if sc.walls_for != (board.w, board.h) || sc.walls.len() != cells {
        sc.walls = (0..board.w * board.h)
            .map(|pos| if board.is_wall(pos) { u16::MAX } else { 0 })
            .collect();
        sc.walls_for = (board.w, board.h);
        sc.interior = sc.walls.iter().fold(0, |n, &b| n + u16::from(b == 0));
    }
    let blocked_until = &mut sc.blocked_until;
    blocked_until.clear();
    blocked_until.extend_from_slice(&sc.walls);
    // Distinct cells under living bodies (stacked tail segments share one).
    let mut occupied = 0u16;
    for &id in alive {
        // Segment k of an L-long body frees after L - k turns.
        let mut frees_after = lens[usize::from(id)];
        for &seg in &board.snakes[usize::from(id)].body {
            let c = usize::from(seg);
            if blocked_until[c] == 0 {
                occupied += 1;
            }
            blocked_until[c] = blocked_until[c].max(frees_after);
            frees_after = frees_after.saturating_sub(1);
        }
    }

    let flood = ownership_race(sc, board, alive, &lens);

    let free = sc.interior.saturating_sub(occupied).max(1);
    for &id in alive {
        let i = usize::from(id);
        let s = &board.snakes[i];
        // Trap: only "is there room for my body?" matters (anything at or
        // past `len` scores 0), so the flood stops once it has counted
        // that much.
        let reach = reachable_cells(
            board,
            i,
            &sc.blocked_until,
            lens[i],
            (&mut sc.seen, &mut sc.stamp, &mut sc.queue),
        );
        let outlook = Outlook {
            len: lens[i],
            longest_other: alive
                .iter()
                .filter(|&&j| j != id)
                .map(|&j| lens[usize::from(j)])
                .max()
                .unwrap_or(0),
            health: s.health,
            area: flood.area[i],
            free,
            n_alive,
            food_dist: flood.food_dist[i],
            food_any: flood.food_any[i],
            reach,
        };
        vals[i] = snake_value(&outlook, tuning);
    }
    vals
}

/// What the ownership race found, per snake.
struct Flood {
    /// Cells owned outright.
    area: [u16; MAX_SNAKES],
    /// Earliest arrival at food the snake wins the race to (`u16::MAX`:
    /// none).
    food_dist: [u16; MAX_SNAKES],
    /// Earliest arrival at food it reaches first *or ties for*.
    food_any: [u16; MAX_SNAKES],
}

/// The time-expanded Voronoi flood over `sc.blocked_until`: all heads at
/// once, one turn per layer. Longer snakes claim first within a turn;
/// equal-length same-turn contests void the cell and stop both floods.
fn ownership_race(
    sc: &mut Scratch,
    board: &FastBoard,
    alive: &[u8],
    lens: &[u16; MAX_SNAKES],
) -> Flood {
    let width = usize::from(board.w);
    let cells = sc.blocked_until.len();
    let n = board.snakes.len();
    let blocked_until = &sc.blocked_until;
    let owner = &mut sc.owner;
    owner.clear();
    owner.resize(cells, UNCLAIMED);
    let arrival = &mut sc.arrival;
    arrival.clear();
    arrival.resize(cells, 0);
    // Food lookup + earliest arrival at any food a snake reaches first *or
    // ties for*; contested food is discounted later, never ignored.
    let is_food = &mut sc.is_food;
    is_food.clear();
    is_food.resize(cells, false);
    for &f in &board.food {
        is_food[usize::from(f)] = true;
    }
    let mut food_any = [u16::MAX; MAX_SNAKES];
    // Cells owned outright, tallied as the flood claims (and contests) them.
    let mut area = [0u16; MAX_SNAKES];
    let mut race_buf = [0u8; MAX_SNAKES];
    let race_order = &mut race_buf[..alive.len()];
    race_order.copy_from_slice(alive);
    race_order.sort_by_key(|&id| Reverse(lens[usize::from(id)]));
    let race_order = &*race_order;
    if sc.frontier.len() < n {
        sc.frontier.resize_with(n, Vec::new);
        sc.next.resize_with(n, Vec::new);
    }
    let (frontier, next) = (&mut sc.frontier, &mut sc.next);
    for f in frontier.iter_mut().take(n) {
        f.clear();
    }
    for &id in alive {
        let i = usize::from(id);
        frontier[i].push(usize::from(board.snakes[i].head()));
    }
    let mut turn: u16 = 0;
    while frontier[..n].iter().any(|f| !f.is_empty()) && usize::from(turn) <= cells {
        turn += 1;
        for f in next.iter_mut().take(n) {
            f.clear();
        }
        for &me in race_order {
            let i = usize::from(me);
            let my_len = lens[i];
            for &c in &frontier[i] {
                if turn > 1 && owner[c] != me {
                    continue; // this frontier cell got contested away
                }
                // Flood cells are never walls and the board is ringed by
                // walls, so all four neighbours are in range: no
                // coordinates, no bounds checks.
                for nc in [c + width, c - width, c + 1, c - 1] {
                    if blocked_until[nc] > turn {
                        continue;
                    }
                    match owner[nc] {
                        UNCLAIMED => {
                            owner[nc] = me;
                            area[i] += 1;
                            arrival[nc] = turn;
                            next[i].push(nc);
                            if is_food[nc] {
                                food_any[i] = food_any[i].min(turn);
                            }
                        }
                        o if o != CONTESTED
                            && o != me
                            && arrival[nc] == turn
                            && lens[usize::from(o)] == my_len =>
                        {
                            let o = usize::from(o);
                            owner[nc] = CONTESTED;
                            area[o] -= 1;
                            if is_food[nc] {
                                // Both can be here at t; both should see it.
                                food_any[i] = food_any[i].min(turn);
                                food_any[o] = food_any[o].min(turn);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        std::mem::swap(frontier, next);
    }

    // Earliest arrival at food this snake wins the race to.
    let mut food_dist = [u16::MAX; MAX_SNAKES];
    for &f in &board.food {
        let c = usize::from(f);
        let o = owner[c];
        if o != UNCLAIMED && o != CONTESTED {
            let o = usize::from(o);
            food_dist[o] = food_dist[o].min(arrival[c]);
        }
    }
    Flood {
        area,
        food_dist,
        food_any,
    }
}

/// One living snake's position, as the blend scores it.
struct Outlook {
    len: u16,
    /// Longest living rival (0 when alone).
    longest_other: u16,
    health: u8,
    /// Cells owned outright, out of `free` unoccupied ones, shared among
    /// `n_alive` snakes.
    area: u16,
    free: u16,
    n_alive: u8,
    /// See [`Flood`].
    food_dist: u16,
    food_any: u16,
    /// Cells reachable at all, counted up to `len`.
    reach: u16,
}

/// The Voronoi blend for one snake, in [-1, 1].
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add rounds differently (moving every search result) and is a slow libm call on wasm"
)]
fn snake_value(o: &Outlook, tuning: Tuning) -> f32 {
    let len = f32::from(o.len);
    let longest_other = f32::from(o.longest_other);
    let area_score =
        ((f32::from(o.area) / f32::from(o.free)) * f32::from(o.n_alive) - 1.0).clamp(-1.0, 1.0);
    let health = (f32::from(o.health) / 100.0) * 2.0 - 1.0;
    let length = ((len - longest_other) / 4.0).clamp(-1.0, 1.0);
    // Hunger: the race between health and the nearest food we can win,
    // weighted by how hungry we actually are.
    let hunger = ((100.0 - f32::from(o.health)) / 60.0).clamp(0.0, 1.0);
    let urgency = if tuning.growth > 0.0 {
        // Shorter (or merely equal — ties die too) than the biggest
        // rival: food matters even on full health.
        let growth_need = if tuning.bully_target {
            // Parity is worthless: full urgency until strictly ahead,
            // tapering once the head-on lead is banked.
            if o.len <= o.longest_other {
                1.0
            } else {
                ((longest_other + 2.0 - len) / 2.0).clamp(0.0, 1.0)
            }
        } else {
            ((longest_other + 2.0 - len) / 4.0).clamp(0.0, 1.0)
        };
        // Early game: leads are cheap to take and expensive to concede.
        let early = if len.max(longest_other) < tuning.early_until {
            tuning.early_boost
        } else {
            1.0
        };
        hunger.max((tuning.growth * early).min(1.0) * growth_need)
    } else {
        hunger
    };
    let margin = if o.food_dist == u16::MAX {
        -1.0
    } else {
        ((f32::from(o.health) - f32::from(o.food_dist)) / 25.0).clamp(-1.0, 1.0)
    };
    let food = if tuning.gradient_food {
        // Appetite: a healthy snake at parity still wants food a bit
        // (that is opportunistic eating), more when hungry, more when
        // behind on length. Everything continuous — no switches.
        let deficit = ((longest_other + 1.0 - len) / 4.0).clamp(0.0, 1.0);
        let appetite = (tuning.appetite_base + 0.60 * hunger + 0.50 * deficit).min(1.0);
        // Prefer food we own outright; fall back to food we merely tie
        // the race to, at a discount.
        let (dist, share) = if o.food_dist == u16::MAX {
            (o.food_any, tuning.contested_food)
        } else {
            (o.food_dist, 1.0)
        };
        match dist {
            // Nothing reachable at all: bad in proportion to hunger.
            u16::MAX => -0.5 * hunger,
            _ if share <= 0.0 => -0.5 * hunger,
            d if d > u16::from(o.health) => -1.0, // cannot make it
            d => {
                // Smooth pull, so there is always a slope to climb.
                let attraction = 1.0 / (1.0 + f32::from(d) / tuning.food_scale);
                appetite * attraction * share
            }
        }
    } else {
        urgency * margin
    };
    // Trap: a reachable region smaller than the body is a death sentence.
    let trap = ((f32::from(o.reach) - len) / len).clamp(-1.0, 0.0);
    let (w_health, w_length) = if tuning.length_heavy {
        (0.05, W_LENGTH + 0.05)
    } else {
        (W_HEALTH, W_LENGTH)
    };
    // Head-on standing: strictly longer wins the collision, exact
    // parity kills us both, shorter simply loses it.
    let dominance = if tuning.dominance > 0.0 {
        let standing = match o.len.cmp(&o.longest_other) {
            Ordering::Greater => 1.0,
            Ordering::Equal => -1.0,
            Ordering::Less => -0.5,
        };
        tuning.dominance * standing
    } else {
        0.0
    };
    (W_AREA * area_score
        + w_health * health
        + w_length * length
        + W_FOOD * food
        + W_TRAP * trap
        + dominance)
        .clamp(-1.0, 1.0)
}

/// Cells snake `i` can reach at all, opponents parked and tails decaying:
/// enterable at step t+1 if vacated by then. Counted only up to `enough` —
/// the caller just needs to know whether there are at least that many.
fn reachable_cells(
    board: &FastBoard,
    i: usize,
    blocked_until: &[u16],
    enough: u16,
    (seen, stamp, queue): (&mut Vec<u32>, &mut u32, &mut Vec<(usize, u16)>),
) -> u16 {
    let width = usize::from(board.w);
    if seen.len() != blocked_until.len() || *stamp == u32::MAX {
        seen.clear();
        seen.resize(blocked_until.len(), 0);
        *stamp = 0;
    }
    *stamp += 1;
    let mark = *stamp;
    // A Vec read from the front is the same FIFO as a VecDeque, minus the
    // allocation.
    queue.clear();
    let start = usize::from(board.snakes[i].head());
    seen[start] = mark;
    queue.push((start, 0u16));
    let mut reached = 0u16;
    let mut head = 0;
    while head < queue.len() {
        let (c, t) = queue[head];
        head += 1;
        // Never a wall (see the ownership flood), so no bounds checks.
        for nc in [c + width, c - width, c + 1, c - 1] {
            if seen[nc] == mark || blocked_until[nc] > t + 1 {
                continue;
            }
            seen[nc] = mark;
            reached += 1;
            if reached >= enough {
                return reached;
            }
            queue.push((nc, t + 1));
        }
    }
    reached
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::types::DEFAULT_FOOD;
    use std::collections::BTreeMap;
    use std::hint::black_box;

    fn config() -> GameConfig {
        GameConfig {
            board_width: 13,
            board_height: 13,
            tick_ms: 1000,
            food_emoji: DEFAULT_FOOD.to_owned(),
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
                .map_or(turn.turn_number, |w| w.survival_turns)
        };
        (survival(&a), survival(&b))
    }

    /// A solo hard bot must cruise indefinitely (food discipline included).
    #[test]
    fn bot_survives_solo() {
        let me: PlayerId = "bot".to_owned();
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

    /// Chunking must be invisible: a search driven in small pieces (as the
    /// browser does, yielding between them) must give bit-identical
    /// results to one uninterrupted run.
    #[test]
    fn chunked_search_matches_unchunked() {
        let a: PlayerId = "a".into();
        let mut engine = Engine::new(config(), vec![a.clone(), "b".into()], 11);
        let turn = engine.first_turn();
        let whole = think(&config(), &turn, &a, 3_000, 4242);
        for chunk in [1usize, 7, 128, 999] {
            let mut s = begin(&config(), &turn, &a, 3_000, 4242);
            let mut guard = 0;
            while !s.step(chunk) {
                guard += 1;
                assert!(guard < 100_000, "chunked search failed to finish");
            }
            assert_eq!(s.result(), whole, "chunk size {chunk} changed the result");
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
            alive: vec![a, b],
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

    fn think_with(config: &GameConfig, turn: &Turn, me: &PlayerId, sims: u32, eval: EvalFn) -> Dir {
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
            food_emoji: DEFAULT_FOOD.to_owned(),
        }
    }

    /// Final length of `p`'s snake (0 if dead), for arena scores.
    fn final_length(turn: &Turn, p: &PlayerId) -> u32 {
        turn.snakes
            .get(p)
            .map_or(0, |s| u32::try_from(s.len()).unwrap())
    }

    /// Mean of `total` over `n` games, for arena printouts.
    fn mean(total: u32, n: usize) -> f64 {
        f64::from(total) / f64::from(u32::try_from(n).unwrap())
    }

    /// One deterministic duel. Returns (challenger score, incumbent score)
    /// where a score is (survival turns, final length) — length breaks the
    /// "both alive at the cap" ties that make survival-only tests blind.
    fn duel(
        challenger: EvalFn,
        incumbent: EvalFn,
        sims: u32,
        seed: u64,
        swap: bool,
        board: u8,
    ) -> ((u32, u32), (u32, u32)) {
        let cfg = board_config(board);
        let a: PlayerId = "a".into();
        let b: PlayerId = "b".into();
        let mut engine = Engine::new(cfg.clone(), vec![a.clone(), b.clone()], seed);
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
                .map_or(turn.turn_number, |w| w.survival_turns);
            (survived, final_length(&turn, p))
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
        run_arena_on(name, challenger, incumbent, seeds, sims, 11);
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
        let results: Vec<((u32, u32), (u32, u32))> = std::thread::scope(|scope| {
            // Every thread starts before the first join.
            let mut handles = Vec::new();
            for chunk in jobs.chunks(jobs.len().div_ceil(12).max(1)) {
                handles.push(scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|&(seed, swap)| duel(challenger, incumbent, sims, seed, swap, board))
                        .collect::<Vec<_>>()
                }));
            }
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect()
        });

        let (mut wins, mut losses, mut draws) = (0, 0, 0);
        let (mut ch_len, mut in_len) = (0u32, 0u32);
        for (ch, inc) in &results {
            ch_len += ch.1;
            in_len += inc.1;
            match ch.cmp(inc) {
                Ordering::Greater => wins += 1,
                Ordering::Less => losses += 1,
                Ordering::Equal => draws += 1,
            }
        }
        let n = results.len();
        println!(
            "ARENA {name}: {wins}W {losses}L {draws}D of {n} @ {sims} sims, {board}x{board} \
             | mean final length {:.1} vs {:.1}",
            mean(ch_len, n),
            mean(in_len, n),
        );
    }

    /// Do the two evals even disagree? (Sanity check for arena power.)
    #[test]
    #[ignore = "diagnostic: only prints; run with --ignored --nocapture"]
    fn evals_disagree_often_enough() {
        let cfg = arena_config();
        let a: PlayerId = "a".into();
        let b: PlayerId = "b".into();
        let mut engine = Engine::new(cfg.clone(), vec![a.clone(), b.clone()], 7);
        let mut turn = engine.first_turn();
        let (mut same, mut diff) = (0, 0);
        for _ in 0..60 {
            if turn.game_over() {
                break;
            }
            let m1 = think_with(&cfg, &turn, &a, 1_200, evaluate_pre_gradient);
            let m2 = think_with(&cfg, &turn, &a, 1_200, evaluate);
            if m1 == m2 {
                same += 1;
            } else {
                diff += 1;
            }
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
    #[ignore = "arena benchmark: minutes of CPU; run with --ignored --nocapture"]
    fn arena_ablation() {
        run_arena(
            "growth-urgency only",
            evaluate_growth_only,
            evaluate_legacy,
            30,
            1_200,
        );
        run_arena(
            "length-heavy only  ",
            evaluate_length_heavy,
            evaluate_legacy,
            30,
            1_200,
        );
    }

    /// Bully candidates: each isolated against the shipped tuning.
    #[test]
    #[ignore = "arena benchmark: minutes of CPU; run with --ignored --nocapture"]
    fn arena_bully_candidates() {
        run_arena_on(
            "bully-target",
            evaluate_bully,
            evaluate_pre_gradient,
            30,
            1_200,
            13,
        );
    }

    /// Does a continuous appetite + proximity gradient beat the shipped
    /// threshold-based tuning? (And how hungry should a healthy snake be?)
    #[test]
    #[ignore = "arena benchmark: minutes of CPU; run with --ignored --nocapture"]
    fn arena_gradient_food() {
        // The shipped model against the tuning it replaced.
        run_arena_on(
            "shipped (gradient)  ",
            evaluate,
            evaluate_pre_gradient,
            40,
            1_200,
            13,
        );
        run_arena_on(
            "contested food blind",
            evaluate_blind_contested,
            evaluate_pre_gradient,
            30,
            1_200,
            13,
        );
        run_arena_on(
            "appetite 0.22       ",
            evaluate_appetite_022,
            evaluate_pre_gradient,
            30,
            1_200,
            13,
        );
        run_arena_on(
            "appetite 0.60       ",
            evaluate_appetite_060,
            evaluate_pre_gradient,
            30,
            1_200,
            13,
        );
    }

    /// Where does appetite peak now that contested food is visible?
    #[test]
    #[ignore = "arena benchmark: minutes of CPU; run with --ignored --nocapture"]
    fn arena_appetite_sweep() {
        run_arena_on(
            "shipped @11x11",
            evaluate,
            evaluate_pre_gradient,
            30,
            1_200,
            11,
        );
    }

    /// How long should "early" last, and does the boost hold up?
    #[test]
    #[ignore = "arena benchmark: minutes of CPU; run with --ignored --nocapture"]
    fn arena_early_sweep() {
        run_arena_on(
            "pre-gradient vs legacy",
            evaluate_pre_gradient,
            evaluate_legacy,
            30,
            1_200,
            13,
        );
    }

    /// Confirmation: does growth-urgency hold up at higher power, on the
    /// default board, and across strengths?
    #[test]
    #[ignore = "arena benchmark: minutes of CPU; run with --ignored --nocapture"]
    fn arena_confirm_growth() {
        run_arena_on(
            "shipped 0.7 @13x13",
            evaluate,
            evaluate_legacy,
            40,
            1_200,
            13,
        );
    }

    /// Positions from real playthroughs: 2-, 3- and 4-snake games on three
    /// board sizes, sampled every few turns.
    fn fingerprint_positions() -> Vec<(GameConfig, Turn, PlayerId)> {
        let mut out = Vec::new();
        for (size, players, seed) in [(11u8, 2usize, 1u64), (13, 3, 2), (17, 4, 3), (13, 2, 4)] {
            let cfg = board_config(size);
            let ids: Vec<PlayerId> = (0..players).map(|i| format!("p{i}")).collect();
            let mut engine = Engine::new(cfg.clone(), ids.clone(), seed);
            let mut turn = engine.first_turn();
            for t in 0..60 {
                if turn.game_over() {
                    break;
                }
                if t % 5 == 0 {
                    out.push((cfg.clone(), turn.clone(), turn.alive[0].clone()));
                }
                let moves = turn
                    .alive
                    .iter()
                    .map(|p| (p.clone(), think(&cfg, &turn, p, 400, 1).0))
                    .collect();
                turn = engine.next_turn(&turn, &moves);
            }
        }
        out
    }

    /// One hash over the bot's answers (move + root-visit proof) on the
    /// fingerprint positions. Pure performance work must not move it.
    fn search_fingerprint() -> u64 {
        let words: Vec<u64> = fingerprint_positions()
            .iter()
            .map(|(cfg, turn, me)| think(cfg, turn, me, 3_000, 99).1)
            .collect();
        fnv1a(&words)
    }

    #[test]
    #[ignore = "diagnostic: only prints; run with --ignored --nocapture"]
    fn print_search_fingerprint() {
        let t0 = std::time::Instant::now();
        let fp = search_fingerprint();
        println!(
            "FINGERPRINT {fp:#x} over {} positions in {:?}",
            fingerprint_positions().len(),
            t0.elapsed()
        );
    }

    /// Where a simulation's time goes: evaluation, board copy + step, and
    /// node setup, each timed alone over the fingerprint positions.
    #[test]
    #[ignore = "diagnostic: only prints; run with --ignored --nocapture"]
    fn profile_parts() {
        let boards: Vec<FastBoard> = fingerprint_positions()
            .iter()
            .map(|(cfg, turn, _)| {
                let order: Vec<&PlayerId> = turn.alive.iter().collect();
                FastBoard::from_turn(cfg, turn, &order)
            })
            .collect();
        let reps = 4_000u32;
        let time = |name: &str, f: &mut dyn FnMut(&FastBoard)| {
            let t0 = std::time::Instant::now();
            for _ in 0..reps {
                for b in &boards {
                    f(b);
                }
            }
            let calls = f64::from(reps) * f64::from(u32::try_from(boards.len()).unwrap());
            let per = t0.elapsed().as_secs_f64() * 1e9 / calls;
            println!("PART {name:<12} {per:>8.0} ns");
        };
        time("evaluate", &mut |b| {
            black_box(evaluate(b));
        });
        time("clone+step", &mut |b| {
            let mut c = b.clone();
            let moves: Vec<Dir> = (0..c.snakes.len()).map(|i| candidates(b, i)[0]).collect();
            c.step(&moves);
            black_box(c);
        });
        time("node", &mut |b| {
            black_box(Node::new(b.clone()));
        });
    }

    /// A bot as it plays a whole game: a search budget per move, and
    /// whether it carries its tree from one turn to the next.
    #[derive(Clone, Copy)]
    struct Spec {
        sims: u32,
        reuse: bool,
        eval: EvalFn,
        c_uct: f32,
    }

    struct Agent {
        spec: Spec,
        prev: Option<Search>,
    }

    impl Agent {
        const fn new(spec: Spec) -> Self {
            Self { spec, prev: None }
        }
        fn act(&mut self, cfg: &GameConfig, turn: &Turn, me: &PlayerId) -> Dir {
            let prev = if self.spec.reuse {
                self.prev.take()
            } else {
                None
            };
            let mut s = begin_from(prev, cfg, turn, me, self.spec.sims, 0);
            s.eval = self.spec.eval;
            s.c_uct = self.spec.c_uct;
            while !s.step(usize::MAX) {}
            let dir = s.result().0;
            self.prev = Some(s);
            dir
        }
    }

    /// One game between two specs; (challenger, incumbent) scores as
    /// (survival turns, final length).
    fn agent_duel(
        challenger: Spec,
        incumbent: Spec,
        seed: u64,
        swap: bool,
        board: u8,
    ) -> ((u32, u32), (u32, u32)) {
        let cfg = board_config(board);
        let a: PlayerId = "a".into();
        let b: PlayerId = "b".into();
        let (spec_a, spec_b) = if swap {
            (incumbent, challenger)
        } else {
            (challenger, incumbent)
        };
        let mut agents = [Agent::new(spec_a), Agent::new(spec_b)];
        let mut engine = Engine::new(cfg.clone(), vec![a.clone(), b.clone()], seed);
        let mut turn = engine.first_turn();
        for _ in 0..300 {
            if turn.game_over() {
                break;
            }
            let mut moves = BTreeMap::new();
            for (p, agent) in [&a, &b].into_iter().zip(agents.iter_mut()) {
                if turn.alive.contains(p) {
                    moves.insert(p.clone(), agent.act(&cfg, &turn, p));
                }
            }
            turn = engine.next_turn(&turn, &moves);
        }
        let score = |p: &PlayerId| {
            let survived = turn
                .winners
                .iter()
                .find(|w| &w.player == p)
                .map_or(turn.turn_number, |w| w.survival_turns);
            (survived, final_length(&turn, p))
        };
        if swap {
            (score(&b), score(&a))
        } else {
            (score(&a), score(&b))
        }
    }

    /// Challenger vs incumbent over `seeds` positions, each played from
    /// both seats, across all cores. Prints wins/losses/draws and the
    /// challenger's score (draws half) with a ±2σ band.
    fn agent_arena(name: &str, challenger: Spec, incumbent: Spec, seeds: u64, board: u8) -> f64 {
        let jobs: Vec<(u64, bool)> = (0..seeds)
            .flat_map(|s| [(1_000 + s, false), (1_000 + s, true)])
            .collect();
        let threads = std::thread::available_parallelism().map_or(8, std::num::NonZero::get);
        let next = std::sync::atomic::AtomicUsize::new(0);
        let results = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    loop {
                        let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(&(seed, swap)) = jobs.get(k) else {
                            break;
                        };
                        let r = agent_duel(challenger, incumbent, seed, swap, board);
                        results.lock().unwrap().push(r);
                    }
                });
            }
        });
        let results = results.into_inner().unwrap();
        let (mut w, mut l, mut d) = (0u32, 0u32, 0u32);
        let (mut ch_len, mut in_len) = (0u32, 0u32);
        for (ch, inc) in &results {
            ch_len += ch.1;
            in_len += inc.1;
            match ch.cmp(inc) {
                Ordering::Greater => w += 1,
                Ordering::Less => l += 1,
                Ordering::Equal => d += 1,
            }
        }
        let games = results.len();
        let n = f64::from(u32::try_from(games).unwrap());
        // Draws count half.
        let score = mean(2 * w + d, games) / 2.0;
        let band = 2.0 * (score * (1.0 - score) / n).sqrt();
        println!(
            "ARENA {name:<28} {w:>3}W {l:>3}L {d:>2}D  score {:.1}% ±{:.1}  len {:.1} vs {:.1}  ({board}x{board})",
            100.0 * score,
            100.0 * band,
            mean(ch_len, games),
            mean(in_len, games),
        );
        score
    }

    const BASE: Spec = Spec {
        sims: 20_000,
        reuse: false,
        eval: evaluate,
        c_uct: C_UCT,
    };
    const REUSE: Spec = Spec {
        reuse: true,
        ..BASE
    };

    /// The shipped bot against the one it replaced, at equal thinking time:
    /// the browser measured 2.8x the simulations per ms (~93k vs ~30k a
    /// move at the hard budget) plus tree reuse. The old bot's play is the
    /// fresh search here — `print_search_fingerprint` shows the speedups
    /// left its moves identical.
    #[test]
    #[ignore = "arena benchmark: minutes of CPU; run with --ignored --nocapture"]
    fn arena_new_vs_old() {
        let old = Spec {
            sims: 30_000,
            ..BASE
        };
        let new = Spec {
            sims: 84_000,
            ..REUSE
        };
        agent_arena("new vs old, equal time", new, old, 40, 11);
    }

    /// Exploration constant, both sides reusing trees.
    #[test]
    #[ignore = "arena benchmark: minutes of CPU; run with --ignored --nocapture"]
    fn arena_c_uct() {
        for c in [0.5, 0.8, 1.7] {
            agent_arena(
                &format!("c_uct {c} vs 1.2"),
                Spec { c_uct: c, ..REUSE },
                REUSE,
                40,
                11,
            );
        }
    }

    /// Does carrying the tree between turns win, at equal fresh work?
    #[test]
    #[ignore = "arena benchmark: minutes of CPU; run with --ignored --nocapture"]
    fn arena_tree_reuse() {
        agent_arena(
            "reuse vs fresh @11",
            Spec {
                reuse: true,
                ..BASE
            },
            BASE,
            40,
            11,
        );
        agent_arena(
            "reuse vs fresh @13",
            Spec {
                reuse: true,
                ..BASE
            },
            BASE,
            40,
            13,
        );
    }

    #[test]
    #[ignore = "diagnostic: only prints; run with --ignored --nocapture"]
    fn print_node_size() {
        let (cfg, turn, me) = fingerprint_positions().remove(8);
        let mut s = begin(&cfg, &turn, &me, 50_000, 0);
        while !s.step(usize::MAX) {}
        let heap: usize = s
            .nodes
            .iter()
            .map(|n| {
                n.children.capacity() * 8
                    + n.board.snakes.capacity() * size_of::<Snake>()
                    + n.board
                        .snakes
                        .iter()
                        .map(|x| x.body.capacity() * 2)
                        .sum::<usize>()
                    + n.board.food.capacity() * 2
            })
            .sum();
        println!(
            "NODE inline {} B, heap ~{} B/node over {} nodes",
            size_of::<Node>(),
            heap / s.nodes.len(),
            s.nodes.len()
        );
    }

    /// `fast_ln` tracks real ln closely enough for UCB.
    #[test]
    fn fast_ln_is_close() {
        for i in 1..2000u16 {
            let x = f32::from(i) * 0.37;
            let err = (fast_ln(x) - x.ln()).abs();
            assert!(err < 0.02, "fast_ln({x}) off by {err}");
        }
    }
}
