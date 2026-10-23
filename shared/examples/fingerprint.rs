//! The search fingerprint: one hash over the bot's answers (move and
//! root-visit proof) on fixed positions from real playthroughs. Pure
//! performance work must not move it, so run it before and after a change
//! and compare the hashes:
//!
//! ```text
//! cargo run --release -p snek-shared --example fingerprint
//! ```
//!
//! A debug build prints the same hash, only about ten times slower; the
//! time it prints is the release build's speed check.

pub mod common;

use snek_shared::bot::think;
use std::io::Write;
use std::time::Instant;

/// FNV-1a over little-endian words, the same hash the bot's proofs use.
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

fn main() -> std::io::Result<()> {
    let t0 = Instant::now();
    let positions = common::fingerprint_positions();
    let words: Vec<u64> = positions
        .iter()
        .map(|(cfg, turn, me)| think(cfg, turn, me, 3_000, 99).1)
        .collect();
    let fp = fnv1a(&words);
    writeln!(
        std::io::stdout(),
        "FINGERPRINT {fp:#x} over {} positions in {:?}",
        positions.len(),
        t0.elapsed()
    )
}
