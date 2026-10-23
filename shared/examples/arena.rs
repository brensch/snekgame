//! Head-to-head arenas: deterministic two-seat duels between evaluation
//! tunings or search settings, every position played from both seats. This
//! is where every value in [`TUNING`] was earned.
//!
//! ```text
//! cargo run --release -p snek-shared --example arena -- <benchmark>
//! ```
//!
//! Each benchmark is minutes of CPU across every core; run it without a
//! name to list them. A line reads challenger-first: wins, losses, draws,
//! and the mean final lengths (length breaks the "both alive at the cap"
//! ties that make survival-only scores blind).

use snek_shared::bot::{
    C_UCT, EvalFn, FastBoard, MAX_SNAKES, Search, TUNING, Tuning, begin, begin_from, evaluate,
    evaluate_tuned,
};
use snek_shared::{Dir, Engine, GameConfig, PlayerId, Turn};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::{self, AtomicUsize};
use std::sync::{Mutex, PoisonError};

/// The threshold-based tuning the shipped one replaced (growth urgency + a
/// 1.6x boost under length 9), kept as the benchmark opponent.
const PRE_GRADIENT: Tuning = Tuning {
    growth: 0.7,
    early_boost: 1.6,
    gradient_food: false,
    appetite_base: 0.0,
    contested_food: 0.0,
    ..TUNING
};

/// The original evaluation, before any length awareness.
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
const APPETITE_022: Tuning = Tuning {
    appetite_base: 0.22,
    ..TUNING
};
const APPETITE_060: Tuning = Tuning {
    appetite_base: 0.60,
    ..TUNING
};
const BLIND_CONTESTED: Tuning = Tuning {
    contested_food: 0.0,
    ..TUNING
};
const LENGTH_HEAVY_ONLY: Tuning = Tuning {
    length_heavy: true,
    ..LEGACY
};
const GROWTH_ONLY: Tuning = Tuning {
    growth: 0.7,
    ..LEGACY
};
const BULLY: Tuning = Tuning {
    bully_target: true,
    ..PRE_GRADIENT
};

fn evaluate_pre_gradient(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_tuned(board, PRE_GRADIENT)
}

fn evaluate_legacy(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_tuned(board, LEGACY)
}

fn evaluate_appetite_022(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_tuned(board, APPETITE_022)
}

fn evaluate_appetite_060(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_tuned(board, APPETITE_060)
}

fn evaluate_blind_contested(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_tuned(board, BLIND_CONTESTED)
}

fn evaluate_growth_only(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_tuned(board, GROWTH_ONLY)
}

fn evaluate_length_heavy(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_tuned(board, LENGTH_HEAVY_ONLY)
}

fn evaluate_bully(board: &FastBoard) -> [f32; MAX_SNAKES] {
    evaluate_tuned(board, BULLY)
}

fn board_config(size: u8) -> GameConfig {
    GameConfig {
        board_width: size,
        board_height: size,
        ..GameConfig::default()
    }
}

/// A fresh search of `turn` under `eval`, run to its budget.
fn think_with(config: &GameConfig, turn: &Turn, me: &PlayerId, sims: u32, eval: EvalFn) -> Dir {
    let mut s = begin(config, turn, me, sims, 0).with_eval(eval);
    while !s.step(usize::MAX) {}
    s.result().0
}

/// (challenger, incumbent) scores from one game, each (survival turns,
/// final length).
type Duel = ((u32, u32), (u32, u32));

/// Final length of `p`'s snake (0 if dead).
fn final_length(turn: &Turn, p: &PlayerId) -> u32 {
    turn.snakes
        .get(p)
        .map_or(0, |s| u32::try_from(s.len()).unwrap_or(u32::MAX))
}

/// (survival turns, final length) for `p` once the duel is over.
fn score(turn: &Turn, p: &PlayerId) -> (u32, u32) {
    let survived = turn
        .winners
        .iter()
        .find(|w| &w.player == p)
        .map_or(turn.turn_number, |w| w.survival_turns);
    (survived, final_length(turn, p))
}

/// Mean of `total` over `n` games.
fn mean(total: u32, n: u32) -> f64 {
    f64::from(total) / f64::from(n)
}

/// Wins, losses and draws for the challenger, and both sides' summed final
/// lengths.
fn tally(results: &[Duel]) -> ([u32; 3], u32, u32) {
    let mut wld = [0u32; 3];
    let (mut ch_len, mut in_len) = (0u32, 0u32);
    for (ch, inc) in results {
        ch_len += ch.1;
        in_len += inc.1;
        match ch.cmp(inc) {
            Ordering::Greater => wld[0] += 1,
            Ordering::Less => wld[1] += 1,
            Ordering::Equal => wld[2] += 1,
        }
    }
    (wld, ch_len, in_len)
}

/// One deterministic duel of two evaluations at a fixed budget. Returns
/// (challenger score, incumbent score).
fn duel(
    challenger: EvalFn,
    incumbent: EvalFn,
    sims: u32,
    seed: u64,
    swap: bool,
    board: u8,
) -> Duel {
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
    if swap {
        (score(&turn, &b), score(&turn, &a))
    } else {
        (score(&turn, &a), score(&turn, &b))
    }
}

/// Evaluation against evaluation over `seeds` positions, parallel across
/// cores.
fn run_arena_on(
    name: &str,
    challenger: EvalFn,
    incumbent: EvalFn,
    seeds: u64,
    sims: u32,
    board: u8,
) -> String {
    let jobs: Vec<(u64, bool)> = (0..seeds)
        .flat_map(|s| [(100 + s, false), (100 + s, true)])
        .collect();
    let results: Vec<Duel> = std::thread::scope(|scope| {
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
            .flat_map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    });
    let ([wins, losses, draws], ch_len, in_len) = tally(&results);
    let n = wins + losses + draws;
    format!(
        "ARENA {name}: {wins}W {losses}L {draws}D of {n} @ {sims} sims, {board}x{board} \
         | mean final length {:.1} vs {:.1}",
        mean(ch_len, n),
        mean(in_len, n),
    )
}

/// [`run_arena_on`] on the 11x11 board.
fn run_arena(name: &str, challenger: EvalFn, incumbent: EvalFn, seeds: u64, sims: u32) -> String {
    run_arena_on(name, challenger, incumbent, seeds, sims, 11)
}

/// A bot as it plays a whole game: a search budget per move, and whether
/// it carries its tree from one turn to the next.
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
        let mut s = begin_from(prev, cfg, turn, me, self.spec.sims, 0)
            .with_eval(self.spec.eval)
            .with_c_uct(self.spec.c_uct);
        while !s.step(usize::MAX) {}
        let dir = s.result().0;
        self.prev = Some(s);
        dir
    }
}

/// One game between two specs; (challenger, incumbent) scores.
fn agent_duel(challenger: Spec, incumbent: Spec, seed: u64, swap: bool, board: u8) -> Duel {
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
    if swap {
        (score(&turn, &b), score(&turn, &a))
    } else {
        (score(&turn, &a), score(&turn, &b))
    }
}

/// Challenger vs incumbent over `seeds` positions, each played from both
/// seats, across all cores. Reports wins/losses/draws and the challenger's
/// score (draws half) with a ±2σ band.
fn agent_arena(name: &str, challenger: Spec, incumbent: Spec, seeds: u64, board: u8) -> String {
    let jobs: Vec<(u64, bool)> = (0..seeds)
        .flat_map(|s| [(1_000 + s, false), (1_000 + s, true)])
        .collect();
    let threads = std::thread::available_parallelism().map_or(8, std::num::NonZero::get);
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                while let Some(&(seed, swap)) =
                    jobs.get(next.fetch_add(1, atomic::Ordering::Relaxed))
                {
                    let r = agent_duel(challenger, incumbent, seed, swap, board);
                    results
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(r);
                }
            });
        }
    });
    let results = results.into_inner().unwrap_or_else(PoisonError::into_inner);
    let ([w, l, d], ch_len, in_len) = tally(&results);
    let games = w + l + d;
    // Draws count half.
    let score = mean(2 * w + d, games) / 2.0;
    let band = 2.0 * (score * (1.0 - score) / f64::from(games)).sqrt();
    format!(
        "ARENA {name:<28} {w:>3}W {l:>3}L {d:>2}D  score {:.1}% ±{:.1}  len {:.1} vs {:.1}  ({board}x{board})",
        100.0 * score,
        100.0 * band,
        mean(ch_len, games),
        mean(in_len, games),
    )
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

/// Do the two evaluations even disagree? (A sanity check for arena power:
/// two tunings that pick the same moves can't separate in a duel.)
fn evals_disagree() -> String {
    let cfg = board_config(11);
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
    format!("EVAL DISAGREEMENT: {diff} differ, {same} same")
}

/// Every benchmark by name, with what it asks.
const BENCHMARKS: &[(&str, &str)] = &[
    (
        "disagree",
        "how often the pre-gradient and shipped evals pick different moves",
    ),
    (
        "ablation",
        "growth urgency and length weighting, each alone against legacy",
    ),
    ("bully", "bully targeting against pre-gradient"),
    (
        "gradient-food",
        "the shipped food model, blind contested food and appetite 0.22 / 0.60 against pre-gradient",
    ),
    (
        "appetite-sweep",
        "the shipped tuning against pre-gradient on 11x11",
    ),
    ("early-sweep", "pre-gradient against legacy"),
    (
        "confirm-growth",
        "the shipped tuning against legacy on 13x13",
    ),
    (
        "new-vs-old",
        "the shipped bot against the one it replaced, at equal thinking time",
    ),
    (
        "c-uct",
        "exploration constants 0.5 / 0.8 / 1.7 against 1.2, both reusing trees",
    ),
    (
        "tree-reuse",
        "carrying the tree between turns against fresh searches, at equal work",
    ),
];

/// [`run`]'s benchmarks of whole bots rather than evaluations.
fn run_agent_arena(
    name: &str,
    emit: &mut dyn FnMut(String) -> std::io::Result<()>,
) -> std::io::Result<bool> {
    match name {
        // The browser measured 2.8x the simulations per ms (~93k vs ~30k a
        // move at the hard budget) plus tree reuse. The old bot's play is
        // the fresh search here: the fingerprint example shows the
        // speedups left its moves identical.
        "new-vs-old" => {
            let old = Spec {
                sims: 30_000,
                ..BASE
            };
            let new = Spec {
                sims: 84_000,
                ..REUSE
            };
            emit(agent_arena("new vs old, equal time", new, old, 40, 11))?;
        }
        "c-uct" => {
            for c in [0.5, 0.8, 1.7] {
                emit(agent_arena(
                    &format!("c_uct {c} vs 1.2"),
                    Spec { c_uct: c, ..REUSE },
                    REUSE,
                    40,
                    11,
                ))?;
            }
        }
        "tree-reuse" => {
            emit(agent_arena("reuse vs fresh @11", REUSE, BASE, 40, 11))?;
            emit(agent_arena("reuse vs fresh @13", REUSE, BASE, 40, 13))?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Runs benchmark `name`, handing each result line to `emit` as it lands
/// (they take minutes apiece). False if there is no such benchmark.
fn run(name: &str, emit: &mut dyn FnMut(String) -> std::io::Result<()>) -> std::io::Result<bool> {
    match name {
        "disagree" => emit(evals_disagree())?,
        "ablation" => {
            emit(run_arena(
                "growth-urgency only",
                evaluate_growth_only,
                evaluate_legacy,
                30,
                1_200,
            ))?;
            emit(run_arena(
                "length-heavy only  ",
                evaluate_length_heavy,
                evaluate_legacy,
                30,
                1_200,
            ))?;
        }
        "bully" => emit(run_arena_on(
            "bully-target",
            evaluate_bully,
            evaluate_pre_gradient,
            30,
            1_200,
            13,
        ))?,
        "gradient-food" => {
            // The shipped model against the tuning it replaced.
            emit(run_arena_on(
                "shipped (gradient)  ",
                evaluate,
                evaluate_pre_gradient,
                40,
                1_200,
                13,
            ))?;
            emit(run_arena_on(
                "contested food blind",
                evaluate_blind_contested,
                evaluate_pre_gradient,
                30,
                1_200,
                13,
            ))?;
            emit(run_arena_on(
                "appetite 0.22       ",
                evaluate_appetite_022,
                evaluate_pre_gradient,
                30,
                1_200,
                13,
            ))?;
            emit(run_arena_on(
                "appetite 0.60       ",
                evaluate_appetite_060,
                evaluate_pre_gradient,
                30,
                1_200,
                13,
            ))?;
        }
        "appetite-sweep" => emit(run_arena_on(
            "shipped @11x11",
            evaluate,
            evaluate_pre_gradient,
            30,
            1_200,
            11,
        ))?,
        "early-sweep" => emit(run_arena_on(
            "pre-gradient vs legacy",
            evaluate_pre_gradient,
            evaluate_legacy,
            30,
            1_200,
            13,
        ))?,
        "confirm-growth" => emit(run_arena_on(
            "shipped 0.7 @13x13",
            evaluate,
            evaluate_legacy,
            40,
            1_200,
            13,
        ))?,
        _ => return run_agent_arena(name, emit),
    }
    Ok(true)
}

fn main() -> std::io::Result<()> {
    let name = std::env::args().nth(1).unwrap_or_default();
    let mut emit = |line: String| writeln!(std::io::stdout(), "{line}");
    if !run(&name, &mut emit)? {
        let mut out = std::io::stdout().lock();
        writeln!(
            out,
            "usage: cargo run --release -p snek-shared --example arena -- <benchmark>\n"
        )?;
        for (name, what) in BENCHMARKS {
            writeln!(out, "  {name:<16} {what}")?;
        }
    }
    Ok(())
}
