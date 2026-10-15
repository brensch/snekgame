//! Print turns of a stored game. `inspect [game_id] [from] [to]` — defaults
//! to the last two turns of the most recent game.
fn main() {
    let conn = rusqlite::Connection::open("/home/brensch/snekgame/snek.db").unwrap();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let want_game: Option<i64> = args.first().and_then(|a| a.parse().ok());
    let range: Option<(u32, u32)> = match (args.get(1), args.get(2)) {
        (Some(a), Some(b)) => Some((a.parse().unwrap(), b.parse().unwrap())),
        _ => None,
    };
    let (id, blob): (i64, Vec<u8>) = match want_game {
        Some(g) => conn
            .query_row(
                "SELECT id, record_blob FROM games WHERE id = ?1",
                [g],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap(),
        None => conn
            .query_row(
                "SELECT id, record_blob FROM games ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap(),
    };
    let record: snek_shared::GameRecord = postcard::from_bytes(&blob).unwrap();
    println!(
        "game #{id}, {} turns, config {:?}",
        record.turns.len(),
        record.config
    );
    for p in &record.players {
        println!("  {} = {} {}", p.id, p.emoji, p.name);
    }
    let selected: Vec<&snek_shared::Turn> = match range {
        Some((from, to)) => record
            .turns
            .iter()
            .filter(|t| t.turn_number >= from && t.turn_number <= to)
            .collect(),
        None => record.turns.iter().rev().take(2).rev().collect(),
    };
    let w = record.config.board_width as i32;
    for turn in selected {
        println!(
            "--- turn {} alive={:?} moves={:?}",
            turn.turn_number, turn.alive, turn.moves
        );
        for (pid, s) in &turn.snakes {
            // Head as (x, y) too: a wall death is only obvious in coordinates.
            let head = s
                .first()
                .map(|p| {
                    let p = *p as i32;
                    format!(" head=({},{})", p % w, p / w)
                })
                .unwrap_or_default();
            println!("    snake {pid}:{head} {:?}", s);
        }
        for c in &turn.clashes {
            println!("    clash @{} {:?} {:?}", c.pos, c.reason, c.players);
        }
        if turn.game_over() {
            println!("    winners: {:?}", turn.winners);
        }
    }
}
