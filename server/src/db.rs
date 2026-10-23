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
    ) -> anyhow::Result<i64> {
        let winners: &[Winner] = &record
            .turns
            .last()
            .map(|t| t.winners.clone())
            .unwrap_or_default();
        let num_turns = i64::try_from(record.turns.len())?;
        let players_json = serde_json::to_string(&record.players)?;
        let winners_json = serde_json::to_string(winners)?;
        let blob = postcard::to_allocvec(record)?;
        let id = self.with_conn(|conn| {
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
        })?;
        Ok(id)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{finished_game, profile, temp_path};

    fn stat(name: &str, device: &str, moves: u32, sims: u64) -> ComputeStat {
        ComputeStat {
            name: name.to_owned(),
            device: device.to_owned(),
            moves,
            sims,
            wall_ms: u64::from(moves) * 10,
        }
    }

    /// A second connection to the same file, for doing to the database
    /// what the server never would.
    fn tamper(path: &Path, sql: &str) {
        Connection::open(path).unwrap().execute_batch(sql).unwrap();
    }

    #[test]
    fn a_stored_game_comes_back_as_it_was_saved() {
        let db = Db::open(&temp_path("roundtrip.db")).unwrap();
        let players = [profile("a1", "alice"), profile("b2", "bob")];
        let record = finished_game(&players);
        let id = db.insert_game(1_000, 2_000, &record).unwrap();

        let blob = db.get_game_blob(id).unwrap().expect("blob stored");
        assert_eq!(postcard::from_bytes::<GameRecord>(&blob).unwrap(), record);

        let meta = db.get_game_meta(id).unwrap().expect("meta stored");
        assert_eq!(meta.id, id);
        assert_eq!(meta.finished_at_ms, 2_000);
        assert_eq!(meta.num_turns, u32::try_from(record.turns.len()).unwrap());
        assert_eq!(meta.players, players);
        assert_eq!(meta.winners, record.turns.last().unwrap().winners);
        assert_eq!(meta.winners.len(), 2, "both players ranked");
    }

    #[test]
    fn a_game_that_was_never_stored_is_none_not_an_error() {
        let db = Db::open(&temp_path("missing.db")).unwrap();
        assert_eq!(db.get_game_meta(42).unwrap(), None);
        assert_eq!(db.get_game_blob(42).unwrap(), None);
    }

    #[test]
    fn history_pages_run_newest_first_and_end_cleanly() {
        let db = Db::open(&temp_path("pages.db")).unwrap();
        let record = finished_game(&[profile("a1", "alice")]);
        let ids: Vec<i64> = (0..5)
            .map(|i| db.insert_game(i, i + 1, &record).unwrap())
            .collect();

        let first = db.list_games(None, 2).unwrap();
        let got: Vec<i64> = first.entries.iter().map(|e| e.id).collect();
        assert_eq!(got, [ids[4], ids[3]]);
        assert_eq!(first.next, Some(ids[3]), "an older page exists");

        let second = db.list_games(first.next, 2).unwrap();
        let got: Vec<i64> = second.entries.iter().map(|e| e.id).collect();
        assert_eq!(got, [ids[2], ids[1]]);

        // The last page is exactly as long as what is left, and says so.
        let last = db.list_games(second.next, 2).unwrap();
        let got: Vec<i64> = last.entries.iter().map(|e| e.id).collect();
        assert_eq!(got, [ids[0]]);
        assert_eq!(last.next, None);
    }

    #[test]
    fn page_sizes_are_kept_between_one_and_fifty() {
        let db = Db::open(&temp_path("limits.db")).unwrap();
        let record = finished_game(&[profile("a1", "alice")]);
        for i in 0..60 {
            db.insert_game(i, i, &record).unwrap();
        }
        assert_eq!(db.list_games(None, 0).unwrap().entries.len(), 1);
        let most = db.list_games(None, u32::MAX).unwrap();
        assert_eq!(most.entries.len(), MAX_PAGE);
        assert!(most.next.is_some());
    }

    /// The list is shown even if a row's JSON has been mangled: that row
    /// loses its names, the rest of the page is unaffected.
    #[test]
    fn a_row_with_unreadable_json_still_lists() {
        let path = temp_path("mangled.db");
        let db = Db::open(&path).unwrap();
        let id = db
            .insert_game(0, 0, &finished_game(&[profile("a1", "alice")]))
            .unwrap();
        tamper(
            &path,
            "UPDATE games SET players_json = 'not json', winners_json = '{'",
        );
        let entry = db.get_game_meta(id).unwrap().unwrap();
        assert!(entry.players.is_empty());
        assert!(entry.winners.is_empty());
        assert_eq!(db.list_games(None, 10).unwrap().entries, [entry]);
    }

    #[test]
    fn compute_overview_counts_devices_games_and_ranks_donors() {
        let db = Db::open(&temp_path("compute.db")).unwrap();
        let empty = db.compute_overview().unwrap();
        assert_eq!(
            empty.totals,
            ComputeTotals {
                devices: 0,
                games: 0,
                moves: 0,
                sims: 0,
                wall_ms: 0
            }
        );
        assert!(empty.donors.is_empty());

        // Alice on two devices across two games, bob on one in one game.
        db.insert_compute_stat(1, &stat("alice", "phone", 10, 1_000), "a1", "ip", 900)
            .unwrap();
        db.insert_compute_stat(2, &stat("alice", "phone", 5, 500), "a1", "ip", 1_200)
            .unwrap();
        db.insert_compute_stat(2, &stat("alice", "laptop", 1, 100), "a1", "ip", 5_000)
            .unwrap();
        db.insert_compute_stat(2, &stat("bob", "phone", 20, 9_000), "b2", "ip", 800)
            .unwrap();

        let overview = db.compute_overview().unwrap();
        assert_eq!(
            overview.totals,
            ComputeTotals {
                devices: 3,
                games: 2,
                moves: 36,
                sims: 10_600,
                wall_ms: 360
            }
        );
        let ranked: Vec<(&str, &str)> = overview
            .donors
            .iter()
            .map(|d| (d.name.as_str(), d.device.as_str()))
            .collect();
        assert_eq!(
            ranked,
            [("bob", "phone"), ("alice", "phone"), ("alice", "laptop")],
            "most simulations first"
        );
        let alice_phone = &overview.donors[1];
        assert_eq!(alice_phone.games, 2);
        assert_eq!(alice_phone.moves, 15);
        assert_eq!(alice_phone.best_benchmark, 1_200, "the best benchmark");
    }

    /// `SQLite` integers are signed 64-bit; a counter past that is refused
    /// rather than wrapped into a negative.
    #[test]
    fn a_counter_too_big_to_store_is_an_error() {
        let db = Db::open(&temp_path("huge.db")).unwrap();
        let err = db
            .insert_compute_stat(1, &stat("x", "y", 1, u64::MAX), "p", "ip", 1)
            .unwrap_err();
        assert!(
            matches!(err, rusqlite::Error::ToSqlConversionFailure(_)),
            "{err:?}"
        );
        assert_eq!(db.compute_overview().unwrap().totals.moves, 0);
    }

    /// Older databases had a lobby-name column; opening one sheds it and
    /// keeps the games.
    #[test]
    fn an_older_database_loses_its_session_column() {
        let path = temp_path("old.db");
        tamper(
            &path,
            "CREATE TABLE games (
                id INTEGER PRIMARY KEY,
                session TEXT NOT NULL DEFAULT '',
                started_at_ms INTEGER NOT NULL,
                finished_at_ms INTEGER NOT NULL,
                num_turns INTEGER NOT NULL,
                players_json TEXT NOT NULL,
                winners_json TEXT NOT NULL,
                record_blob BLOB NOT NULL
            );
            INSERT INTO games VALUES (7, 'lobby', 0, 5, 1, '[]', '[]', x'00');",
        );
        let db = Db::open(&path).unwrap();
        let columns: Vec<String> = Connection::open(&path)
            .unwrap()
            .prepare("SELECT name FROM pragma_table_info('games')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(!columns.contains(&"session".to_owned()), "{columns:?}");
        assert_eq!(db.get_game_meta(7).unwrap().unwrap().finished_at_ms, 5);
    }

    #[test]
    fn a_database_that_cannot_be_opened_is_an_error() {
        // A directory that doesn't exist.
        let path = temp_path("no-such-dir").join("snek.db");
        assert!(Db::open(&path).is_err());
        // A file that isn't a database.
        let path = temp_path("garbage.db");
        std::fs::write(&path, vec![b'x'; 4096]).unwrap();
        assert!(Db::open(&path).is_err());
    }

    /// A schema we don't recognise is refused rather than written into.
    #[test]
    fn a_database_with_a_foreign_schema_is_refused() {
        let path = temp_path("foreign.db");
        tamper(&path, "CREATE TABLE compute_stats (something_else TEXT);");
        let err = Db::open(&path).err().expect("opened a foreign schema");
        assert!(err.to_string().contains("game_id"), "{err}");
    }

    /// Every call reports a database that has gone wrong under it, rather
    /// than pretending there is nothing there.
    #[test]
    fn calls_fail_when_the_tables_are_gone() {
        let path = temp_path("broken.db");
        let db = Db::open(&path).unwrap();
        let record = finished_game(&[profile("a1", "alice")]);
        // Warm the cached statement first: it must not outlive the table.
        db.list_games(None, 10).unwrap();
        tamper(&path, "DROP TABLE games; DROP TABLE compute_stats;");

        assert!(db.insert_game(0, 0, &record).is_err());
        assert!(db.list_games(None, 10).is_err());
        assert!(db.get_game_meta(1).is_err());
        assert!(db.get_game_blob(1).is_err());
        assert!(db.compute_overview().is_err());
        assert!(
            db.insert_compute_stat(1, &stat("x", "y", 1, 1), "p", "ip", 1)
                .is_err()
        );
    }
}
