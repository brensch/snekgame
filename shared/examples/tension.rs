//! Prints a stored game's tense moments, turn by turn, to tune the
//! highlights against real games:
//! `cargo run -p snek-shared --example tension -- game.bin`
//! (a game's record from `/api/history/{id}`).

use snek_shared::GameRecord;
use snek_shared::highlights::{plan, tense_moments};
use std::io::Write as _;

fn main() -> std::io::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "game.bin".to_owned());
    let bytes = std::fs::read(path)?;
    let Ok(record) = postcard::from_bytes::<GameRecord>(&bytes) else {
        return Err(std::io::Error::other("not a game record"));
    };
    let moments = tense_moments(&record.config, &record.turns);
    let timeline = plan(record.turns.len(), &moments);
    let total: f64 = timeline.iter().map(|b| b.ms).sum();
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "{} turns, {} tense, loop {total:.0} ms",
        record.turns.len(),
        moments.len()
    )?;
    for (turn, tension) in &moments {
        writeln!(out, "  {turn:>4} {tension:?}")?;
    }
    Ok(())
}
