//! Where a simulation's time and a tree's memory go.
//!
//! ```text
//! cargo run --release -p snek-shared --example profile
//! ```
//!
//! Times evaluation, board copy + step, and node setup, each alone over the
//! fingerprint positions; then grows a 50k-simulation tree and reports a
//! node's inline size and the heap it holds on average.

pub mod common;

use snek_shared::PlayerId;
use snek_shared::bot::{FastBoard, Node, begin, candidates, evaluate};
use std::hint::black_box;
use std::io::Write;
use std::time::Instant;

fn main() -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    let positions = common::fingerprint_positions();
    // Each board with its snake count (everyone alive in its turn).
    let boards: Vec<(FastBoard, usize)> = positions
        .iter()
        .map(|(cfg, turn, _)| {
            let order: Vec<&PlayerId> = turn.alive.iter().collect();
            (FastBoard::from_turn(cfg, turn, &order), order.len())
        })
        .collect();
    let reps = 4_000u32;
    let calls = f64::from(reps) * f64::from(u32::try_from(boards.len()).unwrap_or(u32::MAX));
    let mut time = |name: &str, f: &mut dyn FnMut(&FastBoard, usize)| {
        let t0 = Instant::now();
        for _ in 0..reps {
            for (b, n) in &boards {
                f(b, *n);
            }
        }
        let per = t0.elapsed().as_secs_f64() * 1e9 / calls;
        writeln!(out, "PART {name:<12} {per:>8.0} ns")
    };
    time("evaluate", &mut |b, _| {
        black_box(evaluate(b));
    })?;
    time("clone+step", &mut |b, n| {
        let mut c = b.clone();
        let moves: Vec<_> = (0..n).map(|i| candidates(b, i)[0]).collect();
        c.step(&moves);
        black_box(c);
    })?;
    time("node", &mut |b, _| {
        black_box(Node::new(b.clone()));
    })?;

    let (cfg, turn, me) = &positions[8];
    let mut s = begin(cfg, turn, me, 50_000, 0);
    while !s.step(usize::MAX) {}
    writeln!(
        out,
        "NODE inline {} B, heap ~{} B/node over {} nodes",
        size_of::<Node>(),
        s.heap_bytes() / s.tree_size(),
        s.tree_size()
    )
}
