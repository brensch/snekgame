//! Helpers for the unit tests: scratch files, finished games to store, and
//! a look at what got logged. They panic on anything going wrong, as a
//! test should.

use snek_shared::{Engine, GameConfig, GameRecord, PlayerProfile};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

/// A path no other test is using, in a directory of this process's own.
/// Nothing is created at it.
///
/// # Panics
///
/// If the directory can't be made.
#[must_use]
pub fn temp_path(name: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!("snek-unit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("{}-{name}", NEXT.fetch_add(1, Ordering::Relaxed)))
}

/// A player as the lobby would hold them.
#[must_use]
pub fn profile(id: &str, name: &str) -> PlayerProfile {
    PlayerProfile {
        id: id.to_owned(),
        name: name.to_owned(),
        emoji: "🐍".to_owned(),
        colour: "hsl(120, 85%, 60%)".to_owned(),
    }
}

/// A real game played to the end by `players`, nobody steering: everyone
/// drives straight on until the walls settle it.
///
/// # Panics
///
/// Never: a game always has a turn to go on from.
#[must_use]
pub fn finished_game(players: &[PlayerProfile]) -> GameRecord {
    let config = GameConfig {
        board_width: 7,
        board_height: 7,
        tick_ms: 250,
        food_emoji: "🍕".to_owned(),
        cerebral: false,
    };
    let ids = players.iter().map(|p| p.id.clone()).collect();
    let mut engine = Engine::new(config.clone(), ids, 7);
    let mut turns = vec![engine.first_turn()];
    while !turns.last().unwrap().game_over() {
        let next = engine.next_turn(turns.last().unwrap(), &BTreeMap::new());
        turns.push(next);
    }
    GameRecord {
        config,
        players: players.to_vec(),
        seed: engine.initial_seed(),
        turns,
    }
}

thread_local! {
    static CAPTURED: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// What this thread has logged since [`Logs::capture`], as text.
///
/// One global subscriber, installed on first use, writes every thread's
/// logs to that thread's own buffer. (A subscriber per test, set as the
/// thread's default, races with other tests' threads over tracing's
/// per-callsite cache, and now and then misses a line.) A current-thread
/// runtime runs every task on the test's own thread, the session actor
/// included, so this sees what they log too.
pub struct Logs(());

impl Logs {
    /// Start afresh: forget what this thread logged before.
    ///
    /// # Panics
    ///
    /// If something other than this installed a global subscriber.
    #[must_use]
    pub fn capture() -> Self {
        static INSTALLED: OnceLock<()> = OnceLock::new();
        INSTALLED.get_or_init(|| {
            tracing_subscriber::fmt()
                .with_writer(|| ThreadBuffer)
                .with_ansi(false)
                .with_max_level(tracing::Level::DEBUG)
                .init();
        });
        CAPTURED.with_borrow_mut(Vec::clear);
        Self(())
    }

    /// The log lines so far that mention every one of `parts`.
    #[must_use]
    pub fn lines_with(&self, parts: &[&str]) -> Vec<String> {
        CAPTURED.with_borrow(|bytes| {
            String::from_utf8_lossy(bytes)
                .lines()
                .filter(|line| parts.iter().all(|p| line.contains(p)))
                .map(str::to_owned)
                .collect()
        })
    }

    /// Whether some line mentions every one of `parts`.
    #[must_use]
    pub fn has(&self, parts: &[&str]) -> bool {
        !self.lines_with(parts).is_empty()
    }
}

struct ThreadBuffer;

impl Write for ThreadBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        CAPTURED.with_borrow_mut(|bytes| bytes.extend_from_slice(buf));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
