//! Print turns of a stored game.
//! `inspect [--db path] [game_id] [from] [to]` — the database defaults to
//! `./snek.db`, the game to the most recent one, and the turns to its last
//! two.
use anyhow::Context;
use std::io::Write;

fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let db_path = match args.iter().position(|a| a == "--db") {
        Some(i) => {
            let path = args.get(i + 1).context("--db needs a path")?.clone();
            args.drain(i..=i + 1);
            path
        }
        None => "snek.db".to_owned(),
    };
    let conn = rusqlite::Connection::open(&db_path)
        .with_context(|| format!("failed to open {db_path}"))?;
    let want_game: Option<i64> = args.first().and_then(|a| a.parse().ok());
    let range: Option<(u32, u32)> = match (args.get(1), args.get(2)) {
        (Some(a), Some(b)) => Some((
            a.parse().with_context(|| format!("bad turn number {a}"))?,
            b.parse().with_context(|| format!("bad turn number {b}"))?,
        )),
        _ => None,
    };
    let (id, blob): (i64, Vec<u8>) = match want_game {
        Some(g) => conn
            .query_row(
                "SELECT id, record_blob FROM games WHERE id = ?1",
                [g],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .with_context(|| format!("no game #{g}"))?,
        None => conn
            .query_row(
                "SELECT id, record_blob FROM games ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .context("no games stored")?,
    };
    let record: snek_shared::GameRecord =
        postcard::from_bytes(&blob).with_context(|| format!("game #{id} doesn't decode"))?;

    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "game #{id}, {} turns, config {:?}",
        record.turns.len(),
        record.config
    )?;
    for p in &record.players {
        writeln!(out, "  {} = {} {}", p.id, p.emoji, p.name)?;
    }
    let selected: Vec<&snek_shared::Turn> = match range {
        Some((from, to)) => record
            .turns
            .iter()
            .filter(|t| t.turn_number >= from && t.turn_number <= to)
            .collect(),
        None => record.turns.iter().rev().take(2).rev().collect(),
    };
    let w = u16::from(record.config.board_width);
    for turn in selected {
        writeln!(
            out,
            "--- turn {} alive={:?} moves={:?}",
            turn.turn_number, turn.alive, turn.moves
        )?;
        for (pid, s) in &turn.snakes {
            // Head as (x, y) too: a wall death is only obvious in coordinates.
            let head = s
                .first()
                .map_or_else(String::new, |p| format!(" head=({},{})", p % w, p / w));
            writeln!(out, "    snake {pid}:{head} {s:?}")?;
        }
        for c in &turn.clashes {
            writeln!(out, "    clash @{} {:?} {:?}", c.pos, c.reason, c.players)?;
        }
        if turn.game_over() {
            writeln!(out, "    winners: {:?}", turn.winners)?;
        }
    }
    Ok(())
}
