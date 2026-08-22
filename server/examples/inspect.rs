//! Print the last turns of the most recent stored game.
fn main() {
    let conn = rusqlite::Connection::open("/home/brensch/snekgame/snek.db").unwrap();
    let (id, blob): (i64, Vec<u8>) = conn
        .query_row(
            "SELECT id, record_blob FROM games ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let record: snek_shared::GameRecord = postcard::from_bytes(&blob).unwrap();
    println!("game #{id}, {} turns, config {:?}", record.turns.len(), record.config);
    for p in &record.players {
        println!("  {} = {} {}", p.id, p.emoji, p.name);
    }
    for turn in record.turns.iter().rev().take(3).rev() {
        println!("--- turn {} alive={:?} moves={:?}", turn.turn_number, turn.alive, turn.moves);
        for (pid, s) in &turn.snakes {
            println!("    snake {pid}: {:?}", s);
        }
        for c in &turn.clashes {
            println!("    clash @{} {:?} {:?}", c.pos, c.reason, c.players);
        }
    }
}
