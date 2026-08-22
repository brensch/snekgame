//! Game history storage. Games live in server memory while being played and
//! are written here once, on completion.

use rusqlite::{params, Connection};
use snek_shared::protocol::HistoryEntry;
use snek_shared::{GameRecord, PlayerProfile, Winner};
use std::path::Path;
use std::sync::Mutex;

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Db> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS games (
                id            INTEGER PRIMARY KEY,
                started_at_ms INTEGER NOT NULL,
                finished_at_ms INTEGER NOT NULL,
                num_turns     INTEGER NOT NULL,
                players_json  TEXT NOT NULL,
                winners_json  TEXT NOT NULL,
                record_blob   BLOB NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_games_finished
                ON games (finished_at_ms DESC);",
        )?;
        // Lobby names never existed. Shed the column from older databases.
        let _ = conn.execute_batch("ALTER TABLE games DROP COLUMN session;");
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    pub fn insert_game(
        &self,
        started_at_ms: i64,
        finished_at_ms: i64,
        record: &GameRecord,
    ) -> rusqlite::Result<i64> {
        let winners: &[Winner] = &record.turns.last().map(|t| t.winners.clone()).unwrap_or_default();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO games
             (started_at_ms, finished_at_ms, num_turns, players_json, winners_json, record_blob)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                started_at_ms,
                finished_at_ms,
                record.turns.len() as i64,
                serde_json::to_string(&record.players).unwrap(),
                serde_json::to_string(winners).unwrap(),
                postcard::to_allocvec(record).unwrap(),
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn list_games(&self, limit: u32) -> rusqlite::Result<Vec<HistoryEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, finished_at_ms, num_turns, players_json, winners_json
             FROM games ORDER BY finished_at_ms DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |row| {
            let players_json: String = row.get(3)?;
            let winners_json: String = row.get(4)?;
            Ok(HistoryEntry {
                id: row.get(0)?,
                finished_at_ms: row.get(1)?,
                num_turns: row.get::<_, i64>(2)? as u32,
                players: serde_json::from_str::<Vec<PlayerProfile>>(&players_json)
                    .unwrap_or_default(),
                winners: serde_json::from_str::<Vec<Winner>>(&winners_json).unwrap_or_default(),
            })
        })?;
        rows.collect()
    }

    pub fn get_game_meta(&self, id: i64) -> rusqlite::Result<Option<HistoryEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, finished_at_ms, num_turns, players_json, winners_json
             FROM games WHERE id = ?1",
        )?;
        let mut rows = stmt.query([id])?;
        match rows.next()? {
            Some(row) => {
                let players_json: String = row.get(3)?;
                let winners_json: String = row.get(4)?;
                Ok(Some(HistoryEntry {
                    id: row.get(0)?,
                    finished_at_ms: row.get(1)?,
                    num_turns: row.get::<_, i64>(2)? as u32,
                    players: serde_json::from_str::<Vec<PlayerProfile>>(&players_json)
                        .unwrap_or_default(),
                    winners: serde_json::from_str::<Vec<Winner>>(&winners_json)
                        .unwrap_or_default(),
                }))
            }
            None => Ok(None),
        }
    }

    pub fn get_game_blob(&self, id: i64) -> rusqlite::Result<Option<Vec<u8>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT record_blob FROM games WHERE id = ?1")?;
        let mut rows = stmt.query([id])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }
}
