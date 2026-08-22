//! Game history storage. Games live in server memory while being played and
//! are written here once, on completion.

use rusqlite::{params, Connection};
use snek_shared::protocol::{HistoryEntry, HistoryPage};
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
            -- rowid order IS finish order (rows insert at game end), so the
            -- primary key serves list ordering; no secondary index needed.
            DROP INDEX IF EXISTS idx_games_finished;",
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

    /// Keyset-paginated history: newest first, resuming below `before`.
    ///
    /// Cost: one clustered B-tree descent + `limit+1` sequential leaf reads,
    /// independent of page depth (OFFSET would scan everything skipped).
    /// The replay blob is the last column and is never selected here, so
    /// its overflow pages are never touched by listing.
    pub fn list_games(
        &self,
        before: Option<i64>,
        limit: u32,
    ) -> rusqlite::Result<HistoryPage> {
        let limit = limit.clamp(1, 50) as i64;
        let conn = self.conn.lock().unwrap();
        // rowid order == finish order; fetch one extra to learn if there is
        // an older page.
        let mut stmt = conn.prepare_cached(
            "SELECT id, finished_at_ms, num_turns, players_json, winners_json
             FROM games WHERE id < ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let cursor = before.unwrap_or(i64::MAX);
        let rows = stmt.query_map([cursor, limit + 1], |row| {
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
        let mut entries: Vec<HistoryEntry> = rows.collect::<Result<_, _>>()?;
        let next = if entries.len() as i64 > limit {
            entries.truncate(limit as usize);
            entries.last().map(|e| e.id)
        } else {
            None
        };
        Ok(HistoryPage { entries, next })
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
