//! Game history storage. Games live in server memory while being played and
//! are written here once, on completion.

use crate::lock;
use rusqlite::{Connection, OptionalExtension, Row, params};
use snek_shared::protocol::{
    ComputeDonor, ComputeOverview, ComputeStat, ComputeTotals, HistoryEntry, HistoryPage,
};
use snek_shared::{GameRecord, PlayerProfile, Winner};
use std::path::Path;
use std::sync::Mutex;

/// Most history entries one page may ask for.
const MAX_PAGE: usize = 50;

/// The history database: one connection, shared by every request and
/// session.
///
/// Calls are short and run on blocking threads, so a single locked
/// connection is plenty, and it keeps the order of writes trivial.
pub struct Db {
    conn: Mutex<Connection>,
}

/// A value that has to be turned into something the database can store
/// (JSON, a postcard blob, a row count) and could not be, reported the way
/// rusqlite reports its own conversion failures.
fn to_sql_failure(e: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
}

/// A `games` row as the history list and the replay page show it. Columns:
/// id, `finished_at_ms`, `num_turns`, `players_json`, `winners_json`.
fn history_entry(row: &Row<'_>) -> rusqlite::Result<HistoryEntry> {
    let players_json: String = row.get(3)?;
    let winners_json: String = row.get(4)?;
    Ok(HistoryEntry {
        id: row.get(0)?,
        finished_at_ms: row.get(1)?,
        num_turns: row.get(2)?,
        players: serde_json::from_str::<Vec<PlayerProfile>>(&players_json).unwrap_or_default(),
        winners: serde_json::from_str::<Vec<Winner>>(&winners_json).unwrap_or_default(),
    })
}

impl Db {
    /// Run `f` with the connection held. Several statements that belong
    /// together (an insert and its rowid, the two halves of an overview)
    /// run under one lock.
    fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        f(&lock(&self.conn))
    }

    /// Open (or create) the database at `path` and bring its schema up to
    /// date.
    ///
    /// # Errors
    ///
    /// Fails if the file can't be opened or the schema can't be created.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
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
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Store a finished game and return its id.
    ///
    /// # Errors
    ///
    /// Fails if the record can't be encoded or the insert fails.
    pub fn insert_game(
        &self,
        started_at_ms: i64,
        finished_at_ms: i64,
        record: &GameRecord,
    ) -> rusqlite::Result<i64> {
        let winners: &[Winner] = &record
            .turns
            .last()
            .map(|t| t.winners.clone())
            .unwrap_or_default();
        let num_turns = i64::try_from(record.turns.len()).map_err(to_sql_failure)?;
        let players_json = serde_json::to_string(&record.players).map_err(to_sql_failure)?;
        let winners_json = serde_json::to_string(winners).map_err(to_sql_failure)?;
        let blob = postcard::to_allocvec(record).map_err(to_sql_failure)?;
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO games
             (started_at_ms, finished_at_ms, num_turns, players_json, winners_json, record_blob)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    started_at_ms,
                    finished_at_ms,
                    num_turns,
                    players_json,
                    winners_json,
                    blob
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// Keyset-paginated history: newest first, resuming below `before`.
    ///
    /// Cost: one clustered B-tree descent + `limit+1` sequential leaf reads,
    /// independent of page depth (OFFSET would scan everything skipped).
    /// The replay blob is the last column and is never selected here, so
    /// its overflow pages are never touched by listing.
    ///
    /// # Errors
    ///
    /// Fails if the query does.
    pub fn list_games(&self, before: Option<i64>, limit: u32) -> rusqlite::Result<HistoryPage> {
        let limit = usize::try_from(limit).map_or(MAX_PAGE, |l| l.clamp(1, MAX_PAGE));
        let cursor = before.unwrap_or(i64::MAX);
        // rowid order == finish order; fetch one extra to learn if there is
        // an older page.
        let mut entries: Vec<HistoryEntry> = lock(&self.conn)
            .prepare_cached(
                "SELECT id, finished_at_ms, num_turns, players_json, winners_json
                 FROM games WHERE id < ?1 ORDER BY id DESC LIMIT ?2",
            )?
            .query_map(params![cursor, limit + 1], history_entry)?
            .collect::<Result<_, _>>()?;
        let next = if entries.len() > limit {
            entries.truncate(limit);
            entries.last().map(|e| e.id)
        } else {
            None
        };
        Ok(HistoryPage { entries, next })
    }

    /// Record what one device computed for the bots in game `game_id`:
    /// `stat` as the players saw it, plus who it was (`player_id`, `ip`) and
    /// the lobby benchmark it reported.
    ///
    /// # Errors
    ///
    /// Fails if a counter doesn't fit the database's integers or the insert
    /// fails.
    pub fn insert_compute_stat(
        &self,
        game_id: i64,
        stat: &ComputeStat,
        player_id: &str,
        ip: &str,
        benchmark: u32,
    ) -> rusqlite::Result<()> {
        lock(&self.conn).execute(
            "INSERT INTO compute_stats
             (game_id, player_id, name, ip, device, benchmark_sims_per_sec,
              moves, total_sims, total_wall_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                game_id,
                player_id,
                stat.name,
                ip,
                stat.device,
                benchmark,
                stat.moves,
                stat.sims,
                stat.wall_ms,
            ],
        )?;
        Ok(())
    }

    /// All-time compute stats. One scan of `compute_stats` (a row per host
    /// per game — thousands of rows even after years), so no indexes needed.
    ///
    /// # Errors
    ///
    /// Fails if the query does.
    pub fn compute_overview(&self) -> rusqlite::Result<ComputeOverview> {
        self.with_conn(Self::compute_overview_on)
    }

    fn compute_overview_on(conn: &Connection) -> rusqlite::Result<ComputeOverview> {
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
                    moves: r.get(2)?,
                    sims: r.get(3)?,
                    wall_ms: r.get(4)?,
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
                    moves: r.get(3)?,
                    sims: r.get(4)?,
                    wall_ms: r.get(5)?,
                    best_benchmark: r.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ComputeOverview { totals, donors })
    }

    /// Who/where/when for one stored game, or `None` if there is no such
    /// game.
    ///
    /// # Errors
    ///
    /// Fails if the query does.
    pub fn get_game_meta(&self, id: i64) -> rusqlite::Result<Option<HistoryEntry>> {
        lock(&self.conn)
            .query_row(
                "SELECT id, finished_at_ms, num_turns, players_json, winners_json
                 FROM games WHERE id = ?1",
                [id],
                history_entry,
            )
            .optional()
    }

    /// The postcard-encoded `GameRecord` of one stored game, or `None` if
    /// there is no such game.
    ///
    /// # Errors
    ///
    /// Fails if the query does.
    pub fn get_game_blob(&self, id: i64) -> rusqlite::Result<Option<Vec<u8>>> {
        lock(&self.conn)
            .query_row("SELECT record_blob FROM games WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()
    }
}
