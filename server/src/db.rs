//! Game history storage. Games live in server memory while being played and
//! are written here once, on completion.

use rusqlite::{params, Connection};
use snek_shared::protocol::{ComputeDonor, ComputeOverview, ComputeTotals, HistoryEntry, HistoryPage};
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
            DROP INDEX IF EXISTS idx_games_finished;
            -- Per-device compute contributions (bots think on player phones).
            CREATE TABLE IF NOT EXISTS compute_stats (
                id            INTEGER PRIMARY KEY,
                game_id       INTEGER NOT NULL,
                player_id     TEXT NOT NULL,
                name          TEXT NOT NULL,
                ip            TEXT NOT NULL,
                device        TEXT NOT NULL,
                benchmark_sims_per_sec INTEGER NOT NULL,
                moves         INTEGER NOT NULL,
                total_sims    INTEGER NOT NULL,
                total_wall_ms INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_compute_game ON compute_stats (game_id);",
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

    #[allow(clippy::too_many_arguments)]
    pub fn insert_compute_stat(
        &self,
        game_id: i64,
        player_id: &str,
        name: &str,
        ip: &str,
        device: &str,
        benchmark: u32,
        moves: u32,
        total_sims: u64,
        total_wall_ms: u64,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO compute_stats
             (game_id, player_id, name, ip, device, benchmark_sims_per_sec,
              moves, total_sims, total_wall_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                game_id,
                player_id,
                name,
                ip,
                device,
                benchmark,
                moves,
                total_sims as i64,
                total_wall_ms as i64,
            ],
        )?;
        Ok(())
    }

    /// All-time compute stats. One scan of compute_stats (a row per host per
    /// game — thousands of rows even after years), so no indexes needed.
    pub fn compute_overview(&self) -> rusqlite::Result<ComputeOverview> {
        let conn = self.conn.lock().unwrap();
        let totals = conn.query_row(
            "SELECT COUNT(DISTINCT player_id || '|' || device),
                    COUNT(DISTINCT game_id),
                    COALESCE(SUM(moves), 0),
                    COALESCE(SUM(total_sims), 0),
                    COALESCE(SUM(total_wall_ms), 0)
             FROM compute_stats",
            [],
            |r| {
                Ok(ComputeTotals {
                    devices: r.get(0)?,
                    games: r.get(1)?,
                    moves: r.get::<_, i64>(2)? as u64,
                    sims: r.get::<_, i64>(3)? as u64,
                    wall_ms: r.get::<_, i64>(4)? as u64,
                })
            },
        )?;
        let mut stmt = conn.prepare(
            "SELECT name, device, COUNT(DISTINCT game_id), SUM(moves),
                    SUM(total_sims), SUM(total_wall_ms), MAX(benchmark_sims_per_sec)
             FROM compute_stats
             GROUP BY player_id, device
             ORDER BY SUM(total_sims) DESC
             LIMIT 50",
        )?;
        let donors = stmt
            .query_map([], |r| {
                Ok(ComputeDonor {
                    name: r.get(0)?,
                    device: r.get(1)?,
                    games: r.get(2)?,
                    moves: r.get::<_, i64>(3)? as u64,
                    sims: r.get::<_, i64>(4)? as u64,
                    wall_ms: r.get::<_, i64>(5)? as u64,
                    best_benchmark: r.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ComputeOverview { totals, donors })
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
