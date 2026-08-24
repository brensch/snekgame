//! WebTransport (QUIC) client — the only transport.
//!
//! One connection at a time, in one of two modes:
//! - **Session**: joins a lobby. Control traffic (join, lobby, game
//!   start/end, catch-ups) rides one reliable bidirectional stream;
//!   `Turn` snapshots arrive as datagrams (deduped by turn number, so the
//!   server's redundant sends and any reordering are harmless); `Move` and
//!   `Ping` go up as datagrams, with the current heading re-sent every
//!   100ms so a lost input costs 100ms, not a turn.
//! - **Watch**: the home screen's live games list over the control stream.
//!
//! Automatic reconnect with backoff; on rejoin the server replays full
//! state, so no incremental state is ever trusted across a gap.

use crate::store::{ConnStatus, GameView, Store};
use futures_util::StreamExt;
use gloo_timers::callback::{Interval, Timeout};
use js_sys::Uint8Array;
use leptos::prelude::*;
use leptos::task::spawn_local;
use snek_shared::protocol::*;
use std::cell::{Cell, RefCell};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{WebTransport, WritableStreamDefaultWriter};

// UDP 443 — pairs with the HTTPS port-forward that already exists for the
// domain (Caddy only occupies TCP 443).
const WT_PORT: u16 = 443;
const PING_INTERVAL_MS: u32 = 2_000;
/// A connection is only "live" if the server said something this recently.
/// Pings every 2s guarantee traffic, so 6s = three strikes.
const STALE_MS: f64 = 6_000.0;
const MOVE_REPEAT_MS: u32 = 100;
const BACKOFF_START_MS: u32 = 300;
const BACKOFF_MAX_MS: u32 = 3_000;
/// Every connect attempt must resolve within this or it is killed and
/// retried — QUIC handshakes started while offline can otherwise hang for
/// ages with no error, stalling the whole backoff loop.
const CONNECT_TIMEOUT_MS: u32 = 4_000;

#[derive(Clone, PartialEq)]
enum Mode {
    Session(String),
    Watch,
}

struct Conn {
    mode: Mode,
    /// Generation guard: bumping it orphans callbacks of older connections.
    generation: u64,
    transport: Option<WebTransport>,
    control: Option<WritableStreamDefaultWriter>,
    dgrams: Option<WritableStreamDefaultWriter>,
    _ping: Option<Interval>,
    _repeat: Option<Interval>,
    backoff_ms: u32,
}

thread_local! {
    static ACTIVE: RefCell<Option<Conn>> = const { RefCell::new(None) };
    /// When the last server message arrived (any kind — pongs count).
    static LAST_RX: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
    /// Newest turn we have seen. Compute requests older than this are
    /// already obsolete — the tick they were for has resolved — so running
    /// them would only push the queue further behind.
    static LATEST_TURN: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn mark_rx() {
    LAST_RX.with(|c| c.set(now()));
}

fn is_stale() -> bool {
    LAST_RX.with(|c| now() - c.get() > STALE_MS)
}

pub fn now() -> f64 {
    js_sys::Date::now()
}

fn wt_url() -> String {
    let host = web_sys::window()
        .and_then(|w| w.location().hostname().ok())
        .unwrap_or_default();
    format!("https://{host}:{WT_PORT}")
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Connect to a session lobby (idempotent).
pub fn ensure_session(store: Store, session: &str) {
    ensure(store, Mode::Session(session.to_string()));
}

/// Subscribe to the live games list (idempotent).
pub fn ensure_watch(store: Store) {
    ensure(store, Mode::Watch);
}

fn ensure(store: Store, mode: Mode) {
    let (need_new, mode_changed) = ACTIVE.with(|a| {
        let a = a.borrow();
        match &*a {
            Some(c) if c.mode == mode && c.transport.is_some() => (false, false),
            Some(c) => (true, c.mode != mode),
            None => (true, true),
        }
    });
    if mode_changed {
        // A navigation, not a reconnect: fresh joins must not flash the
        // reconnect banner just because the previous mode was connected.
        store.conn.set(ConnStatus::Connecting);
        // Session-scoped state must not leak between sessions (or linger
        // after going home) — a stale game would freeze over the new lobby.
        store.lobby.set(None);
        store.game.set(None);
        store.saved_id.set(None);
        store.view_turn.set(None);
        store.selected_dir.set(None);
    }
    if need_new {
        open(store, mode, BACKOFF_START_MS);
    }
}

/// The app came back to the foreground (or the network returned): don't
/// wait out a (possibly throttled) backoff timer — reconnect right now if
/// the connection isn't healthy. `open` bumps the generation, so any
/// pending backoff timers and the old transport's callbacks go stale.
pub fn nudge(store: Store) {
    let reopen = ACTIVE.with(|a| {
        let mut a = a.borrow_mut();
        match &mut *a {
            Some(conn) => {
                conn.backoff_ms = BACKOFF_START_MS;
                // A transport that looks fine but has gone silent is a
                // zombie (suspended tabs often never fire `closed`), and a
                // connect/join that never completes is a hang.
                let unhealthy = conn.transport.is_none()
                    || store.conn.get_untracked() != ConnStatus::Connected
                    || is_stale();
                unhealthy.then(|| conn.mode.clone())
            }
            None => None,
        }
    });
    if let Some(mode) = reopen {
        open(store, mode, BACKOFF_START_MS);
    }
}

/// Watchdog (driven by the shared clock while the tab is visible): any
/// non-idle connection that has been silent past the staleness window gets
/// reopened — zombie "Connected" transports and stalled connect/join
/// cycles alike. Every reopen resets the clock, so healthy attempts are
/// never interrupted.
pub fn check_liveness(store: Store) {
    if store.conn.get_untracked() != ConnStatus::Idle && is_stale() {
        nudge(store);
    }
}

/// Tear down (leaving for an unconnected page).
pub fn disconnect(store: Store) {
    ACTIVE.with(|a| {
        if let Some(mut conn) = a.borrow_mut().take() {
            conn.generation += 1;
            if let Some(t) = conn.transport.take() {
                t.close();
            }
        }
    });
    store.conn.set(ConnStatus::Idle);
    store.lobby.set(None);
    store.game.set(None);
    store.saved_id.set(None);
    store.view_turn.set(None);
}

/// Control-class message over the reliable stream.
pub fn send(msg: &ClientMsg) {
    let bytes = encode(msg);
    ACTIVE.with(|a| {
        if let Some(conn) = &*a.borrow() {
            if let Some(writer) = &conn.control {
                let mut framed = Vec::with_capacity(bytes.len() + 4);
                framed.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                framed.extend_from_slice(&bytes);
                let _ = writer.write_with_chunk(&Uint8Array::from(framed.as_slice()).into());
            }
        }
    });
}

fn send_datagram(msg: &ClientMsg) {
    let bytes = encode(msg);
    ACTIVE.with(|a| {
        if let Some(conn) = &*a.borrow() {
            if let Some(writer) = &conn.dgrams {
                let _ = writer.write_with_chunk(&Uint8Array::from(bytes.as_slice()).into());
            }
        }
    });
}

/// Steer: optimistic d-pad highlight now, datagram up, server truth on the
/// next snapshot. The repeat timer keeps re-sending until then.
pub fn send_move(store: Store, dir: snek_shared::Dir) {
    store.selected_dir.set(Some(dir));
    send_datagram(&ClientMsg::Move(dir));
}

// ---------------------------------------------------------------------------
// Connection lifecycle
// ---------------------------------------------------------------------------

fn is_current(generation: u64) -> bool {
    ACTIVE.with(|a| {
        a.borrow()
            .as_ref()
            .map(|c| c.generation == generation)
            .unwrap_or(false)
    })
}

fn open(store: Store, mode: Mode, backoff_ms: u32) {
    let generation = ACTIVE.with(|a| {
        let mut a = a.borrow_mut();
        let generation = a.as_ref().map(|c| c.generation + 1).unwrap_or(1);
        if let Some(mut old) = a.take() {
            if let Some(t) = old.transport.take() {
                t.close();
            }
        }
        *a = Some(Conn {
            mode: mode.clone(),
            generation,
            transport: None,
            control: None,
            dgrams: None,
            _ping: None,
            _repeat: None,
            backoff_ms,
        });
        generation
    });

    mark_rx(); // fresh attempt starts with a fresh liveness clock
    store.conn.update(|c| {
        *c = if *c == ConnStatus::Connected {
            ConnStatus::Reconnecting
        } else {
            ConnStatus::Connecting
        }
    });

    let attempt_mode = mode.clone();
    spawn_local(async move {
        if let Err(e) = connect(store, attempt_mode, generation).await {
            tracing_debug(&e);
            schedule_reconnect(store, generation);
        }
    });

    // Hung-attempt guard: if this generation still isn't Connected after the
    // deadline, abandon it (open bumps the generation, orphaning the hung
    // future and closing its transport) and try again.
    Timeout::new(CONNECT_TIMEOUT_MS, move || {
        if is_current(generation) && store.conn.get_untracked() != ConnStatus::Connected {
            let next = (backoff_ms * 2).min(BACKOFF_MAX_MS);
            open(store, mode, next);
        }
    })
    .forget();
}

fn tracing_debug(e: &JsValue) {
    web_sys::console::debug_1(e);
}

/// The datagram writer, across the spec rename: Safari (26.4+) implements
/// only the standard `createWritable()`; Chrome still carries the legacy
/// `writable` attribute. Feature-detect instead of picking a side.
fn datagram_writer(
    dgrams: &web_sys::WebTransportDatagramDuplexStream,
) -> Result<WritableStreamDefaultWriter, JsValue> {
    let obj: &JsValue = dgrams.as_ref();
    let create = js_sys::Reflect::get(obj, &"createWritable".into())?;
    let stream = if create.is_function() {
        js_sys::Function::from(create).call0(obj)?
    } else {
        js_sys::Reflect::get(obj, &"writable".into())?
    };
    let stream: web_sys::WritableStream = stream.dyn_into()?;
    stream.get_writer()
}

fn webtransport_missing() -> bool {
    web_sys::window()
        .map(|w| {
            !js_sys::Reflect::has(&w, &"WebTransport".into()).unwrap_or(false)
        })
        .unwrap_or(true)
}

async fn connect(store: Store, mode: Mode, generation: u64) -> Result<(), JsValue> {
    if webtransport_missing() {
        // Don't spin a silent reconnect loop on browsers that can't ever
        // succeed — say what's needed.
        store.error.set(Some(
            "this browser can't play snek — it needs WebTransport \
             (iOS/Safari 26.4+, or any current Chrome/Edge/Firefox)"
                .into(),
        ));
        store.conn.set(ConnStatus::Idle);
        return Ok(());
    }
    let transport = WebTransport::new(&wt_url())?;
    // Register immediately so the attempt-timeout guard (and any newer
    // generation) can close a handshake that never resolves.
    ACTIVE.with(|a| {
        if let Some(conn) = &mut *a.borrow_mut() {
            if conn.generation == generation {
                conn.transport = Some(transport.clone());
            }
        }
    });
    JsFuture::from(transport.ready()).await?;
    if !is_current(generation) {
        transport.close();
        return Ok(());
    }

    // Control stream.
    let stream: web_sys::WebTransportBidirectionalStream =
        JsFuture::from(transport.create_bidirectional_stream())
            .await?
            .dyn_into()?;
    let control_writer = stream.writable().get_writer()?;
    let control_readable = stream.readable();

    // Datagrams.
    let dgram_duplex = transport.datagrams();
    let dgram_writer = datagram_writer(&dgram_duplex)?;
    let dgram_readable = dgram_duplex.readable();

    // First frame declares intent.
    let first = match &mode {
        Mode::Watch => ClientMsg::WatchGames,
        Mode::Session(session) => ClientMsg::Join {
            version: PROTOCOL_VERSION,
            session: session.clone(),
            secret: crate::store::secret(),
            profile: store.profile.get_untracked(),
        },
    };
    {
        let bytes = encode(&first);
        let mut framed = Vec::with_capacity(bytes.len() + 4);
        framed.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        framed.extend_from_slice(&bytes);
        let _ = control_writer.write_with_chunk(&Uint8Array::from(framed.as_slice()).into());
    }

    // Keepalive ping (both modes — liveness needs traffic) + input
    // redundancy (session mode only).
    let ping = {
        let mut n = 0u32;
        Interval::new(PING_INTERVAL_MS, move || {
            n = n.wrapping_add(1);
            send_datagram(&ClientMsg::Ping(n));
        })
    };
    let repeat = match mode {
        Mode::Session(_) => {
            let repeat = Interval::new(MOVE_REPEAT_MS, move || {
                let live = store
                    .game
                    .with_untracked(|g| {
                        g.as_ref()
                            .and_then(|g| g.turns.last())
                            .map(|t| {
                                !t.game_over()
                                    && t.alive.contains(&store.me.get_untracked())
                            })
                            .unwrap_or(false)
                    });
                if live {
                    if let Some(dir) = store.selected_dir.get_untracked() {
                        send_datagram(&ClientMsg::Move(dir));
                    }
                }
            });
            Some(repeat)
        }
        Mode::Watch => None,
    };

    ACTIVE.with(|a| {
        if let Some(conn) = &mut *a.borrow_mut() {
            if conn.generation == generation {
                conn.transport = Some(transport.clone());
                conn.control = Some(control_writer);
                conn.dgrams = Some(dgram_writer);
                conn._ping = Some(ping);
                conn._repeat = repeat;
                conn.backoff_ms = BACKOFF_START_MS; // healthy again
            } else {
                transport.close();
            }
        }
    });
    store.conn.set(ConnStatus::Connected);

    // Reader: control stream (length-prefixed frames).
    {
        let mut stream = wasm_streams::ReadableStream::from_raw(
            control_readable.unchecked_into::<web_sys::ReadableStream>().into(),
        )
        .into_stream();
        spawn_local(async move {
            let mut buf: Vec<u8> = Vec::new();
            while let Some(Ok(chunk)) = stream.next().await {
                if !is_current(generation) {
                    return;
                }
                buf.extend_from_slice(&Uint8Array::new(&chunk).to_vec());
                loop {
                    if buf.len() < 4 {
                        break;
                    }
                    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                    if buf.len() < 4 + len {
                        break;
                    }
                    let frame: Vec<u8> = buf.drain(..4 + len).skip(4).collect();
                    if let Some(msg) = decode::<ServerMsg>(&frame) {
                        apply(store, msg);
                    }
                }
            }
        });
    }

    // Reader: datagrams (one message per datagram).
    {
        let mut stream = wasm_streams::ReadableStream::from_raw(
            dgram_readable.unchecked_into::<web_sys::ReadableStream>().into(),
        )
        .into_stream();
        spawn_local(async move {
            while let Some(Ok(chunk)) = stream.next().await {
                if !is_current(generation) {
                    return;
                }
                if let Some(msg) = decode::<ServerMsg>(&Uint8Array::new(&chunk).to_vec()) {
                    apply(store, msg);
                }
            }
        });
    }

    // Session hosts contribute compute: measure this device's real search
    // speed (~600ms of actual search, chunked so the UI stays alive) and
    // report it. The server assigns bots to the fastest phones.
    if matches!(mode, Mode::Session(_)) {
        spawn_local(async move {
            let rate = benchmark_sims_per_sec(generation).await;
            if is_current(generation) {
                if let Some(rate) = rate {
                    send(&ClientMsg::Benchmark {
                        sims_per_sec: rate,
                        device: device_description(),
                        cores: web_sys::window()
                            .map(|w| w.navigator().hardware_concurrency() as u32)
                            .unwrap_or(4),
                    });
                }
            }
        });
    }

    // Reconnect when the transport dies.
    spawn_local(async move {
        let _ = JsFuture::from(transport.closed()).await;
        if is_current(generation) {
            store.conn.set(ConnStatus::Reconnecting);
            schedule_reconnect(store, generation);
        }
    });

    Ok(())
}

thread_local! {
    /// Requests already taken up. Each one is sent three times — twice as a
    /// datagram and once on the stream — so a lost packet cannot hold it up.
    /// Only the copy that gets here first is worth searching.
    static HANDLED: RefCell<std::collections::VecDeque<u64>> =
        RefCell::new(std::collections::VecDeque::new());
}

/// True the first time this request is seen, false for its other copies.
fn first_sighting(request_id: u64) -> bool {
    HANDLED.with(|h| {
        let mut h = h.borrow_mut();
        if h.contains(&request_id) {
            return false;
        }
        h.push_back(request_id);
        // A handful of ticks is plenty: copies arrive within milliseconds of
        // each other, and ids only ever go up.
        while h.len() > 32 {
            h.pop_front();
        }
        true
    })
}

thread_local! {
    /// Worst gap between when a 50ms heartbeat should have run and when it
    /// did. Nothing else can see this: while the thread is busy drawing, an
    /// arriving request just sits in the queue, and every timing we take
    /// starts after we pick it up.
    static LOOP_LAG_MS: Cell<u32> = const { Cell::new(0) };
}

/// Start the heartbeat. Sampling costs a timer callback every 50ms.
pub fn watch_main_thread() {
    let mut last = now();
    let beat = Interval::new(50, move || {
        let t = now();
        let lag = ((t - last) - 50.0).max(0.0) as u32;
        last = t;
        LOOP_LAG_MS.with(|c| c.set(c.get().max(lag)));
    });
    beat.forget();
}

/// Worst stall since the last answer, and reset.
fn take_loop_lag() -> u32 {
    LOOP_LAG_MS.with(|c| c.replace(0))
}

/// Tell the server whether this device can think right now. A hidden page
/// has its workers suspended — saying nothing would leave its bots holding
/// their heading into a wall while the phone sleeps.
pub fn report_availability(visible: bool) {
    send(&ClientMsg::HostAvailable {
        can_compute: visible,
    });
}

/// Short device description from the user agent: the parenthesised
/// platform segment carries the model on Android and the OS on iOS.
fn device_description() -> String {
    let ua = web_sys::window()
        .map(|w| w.navigator().user_agent().unwrap_or_default())
        .unwrap_or_default();
    let inner = ua
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(inner, _)| inner.to_string())
        .unwrap_or(ua);
    inner.chars().take(80).collect()
}

/// ~600ms of genuine search on a synthetic board, yielding between chunks.
async fn benchmark_sims_per_sec(generation: u64) -> Option<u32> {
    use snek_shared::{Engine, GameConfig};
    let config = GameConfig {
        board_width: 13,
        board_height: 13,
        tick_ms: 1000,
        food_emoji: snek_shared::DEFAULT_FOOD.to_string(),
    };
    let a: snek_shared::PlayerId = "bench-a".into();
    let mut engine = Engine::new(config.clone(), vec![a.clone(), "bench-b".into()], 1);
    let turn = engine.first_turn();

    // One timed search on the worker: it both proves a worker can run here
    // (no worker, no volunteering) and gives an initial speed for ranking
    // hosts. Everything after this is deadline-driven, so nothing is
    // predicted from it.
    const PROBE_MS: u32 = 300;
    let (_dir, _proof, sims, _slices, _began, _search) =
        crate::botworker::compute(config, turn, a.clone(), PROBE_MS, 0).await?;
    if !is_current(generation) {
        return None;
    }
    Some(((sims as f64) * 1000.0 / PROBE_MS as f64) as u32)
}

fn schedule_reconnect(store: Store, generation: u64) {
    ACTIVE.with(|a| {
        let mut a = a.borrow_mut();
        let Some(conn) = &mut *a else { return };
        if conn.generation != generation {
            return;
        }
        conn.transport = None;
        conn.control = None;
        conn.dgrams = None;
        conn._ping = None;
        conn._repeat = None;
        let mode = conn.mode.clone();
        let backoff = conn.backoff_ms;
        conn.backoff_ms = (backoff * 2).min(BACKOFF_MAX_MS);
        Timeout::new(backoff, move || {
            if is_current(generation) {
                open(store, mode, backoff);
            }
        })
        .forget();
    });
}

// ---------------------------------------------------------------------------
// Applying server messages
// ---------------------------------------------------------------------------

fn apply(store: Store, msg: ServerMsg) {
    mark_rx();
    // Connected means the server is actually talking to us on this
    // connection — not merely that a QUIC handshake finished. This is what
    // lets the hung-attempt guard recycle a stalled join.
    if store.conn.get_untracked() != ConnStatus::Connected {
        store.conn.set(ConnStatus::Connected);
    }
    match msg {
        ServerMsg::Welcome { your_id } => store.me.set(your_id),
        ServerMsg::Sessions(list) => store.sessions.set(Some(list)),
        ServerMsg::Lobby(lobby) => {
            // Server phase truth: no game running + we hold an *unfinished*
            // game means we missed the ending while away — fall back to the
            // lobby. A finished game (results screen) stays until the user
            // leaves it.
            if !lobby.game_running {
                let stale_unfinished = store
                    .game
                    .with_untracked(|g| {
                        g.as_ref()
                            .and_then(|g| g.turns.last())
                            .map(|t| !t.game_over())
                            .unwrap_or(false)
                    });
                if stale_unfinished {
                    store.game.set(None);
                    store.view_turn.set(None);
                    store.selected_dir.set(None);
                }
            }
            store.lobby.set(Some(lobby));
        }
        ServerMsg::GameStart {
            config,
            players,
            turn,
            countdown_ms,
        } => {
            LATEST_TURN.with(|c| c.set(turn.turn_number));
            // Boot a worker for every bot now, during the countdown. Each
            // worker compiles its own wasm instance on first use (~100ms+);
            // paying that inside a move's deadline is what makes the first
            // ticks of a game miss.
            for bot in players.iter().filter(|p| snek_shared::is_bot(&p.id)) {
                let (cfg, t, id) = (config.clone(), turn.clone(), bot.id.clone());
                spawn_local(async move {
                    let _ = crate::botworker::compute(cfg, t, id, 1, 0).await;
                });
            }
            store.saved_id.set(None);
            store.view_turn.set(None);
            store.countdown_ends.set(now() + countdown_ms as f64);
            // Pre-select the clockwise default heading on the d-pad.
            let me = store.me.get_untracked();
            // A fresh game always runs; never inherit the last one's hold.
            store.paused.set(false);
            store.selected_dir.set(
                turn.snakes
                    .get(&me)
                    .map(|s| snek_shared::clockwise_start_dir(&config.clone(), s[0])),
            );
            store.game.set(Some(GameView {
                config,
                players,
                turns: vec![turn],
            }));
        }
        ServerMsg::Turn(turn) => {
            LATEST_TURN.with(|c| c.set(c.get().max(turn.turn_number)));
            // Datagram-class: dedup the redundant copies, drop stale
            // reorderings, accept gaps (snapshots are full state).
            let me = store.me.get_untracked();
            store.game.update(|g| {
                if let Some(g) = g {
                    let last = g.turns.last().map(|t| t.turn_number).unwrap_or(0);
                    if turn.turn_number > last {
                        if let Some(&dir) = turn.moves.get(&me) {
                            store.selected_dir.set(Some(dir));
                        }
                        g.turns.push(turn);
                    }
                }
            });
        }
        ServerMsg::GameInProgress {
            config,
            players,
            turns,
        } => {
            store.view_turn.set(None);
            store.game.set(Some(GameView {
                config,
                players,
                turns,
            }));
        }
        ServerMsg::ComputeRequest {
            request_id,
            bot_id,
            config,
            turn,
            think_ms,
            nonce,
            ..
        } => {
            // All search happens on the worker thread; the UI thread never
            // touches it. A device that cannot spawn a worker never
            // benchmarks, so it is never assigned a bot in the first place
            // — and if one somehow arrives, staying silent makes the server
            // hand that bot to another phone.
            if !first_sighting(request_id) {
                return; // another copy of this request got here first
            }
            if turn.turn_number < LATEST_TURN.with(|c| c.get()) {
                web_sys::console::debug_1(&JsValue::from_str(&format!(
                    "compute skipped as stale bot={bot_id} turn={}",
                    turn.turn_number
                )));
                return; // superseded before we even started
            }
            let bot_label = bot_id.clone();
            spawn_local(async move {
                let requested_for = turn.turn_number;
                let arrived = now();
                let (dir, tm) = match crate::botworker::compute(
                    config.clone(),
                    turn.clone(),
                    bot_id.clone(),
                    think_ms,
                    nonce,
                )
                .await
                {
                    Some((dir, _proof, sims, slices, began_ms, search_ms)) => (
                        dir,
                        ComputeTelemetry {
                            queue_ms: (began_ms - arrived).max(0.0) as u32,
                            search_ms,
                            total_ms: (now() - arrived).max(0.0) as u32,
                            think_ms,
                            sims,
                            slices,
                            fallback: false,
                            loop_lag_ms: take_loop_lag(),
                        },
                    ),
                    None => {
                        // The worker is gone. Say so — silence here is
                        // indistinguishable from a dead client — then take
                        // a short, bounded turn on this thread so the bot
                        // still plays. A brief hitch beats a snake driving
                        // into a wall.
                        send_datagram(&ClientMsg::ComputeFailed {
                            request_id,
                            reason: "worker unavailable".into(),
                        });
                        let mut search =
                            snek_shared::bot::begin(&config, &turn, &bot_id, u32::MAX, nonce);
                        let cutoff = now() + (think_ms as f64).min(60.0);
                        let mut sims = 0u32;
                        while !search.step(64) {
                            sims += 64;
                            if now() >= cutoff {
                                break;
                            }
                        }
                        (
                            search.result().0,
                            ComputeTelemetry {
                                queue_ms: 0,
                                search_ms: (now() - cutoff + 60.0).max(0.0) as u32,
                                total_ms: (now() - arrived).max(0.0) as u32,
                                think_ms,
                                sims,
                                slices: sims / 64,
                                fallback: true,
                                loop_lag_ms: take_loop_lag(),
                            },
                        )
                    }
                };
                web_sys::console::debug_1(&JsValue::from_str(&format!(
                    "compute bot={bot_label} turn={requested_for} think={}ms queue={}ms search={}ms total={}ms sims={} slices={} fallback={}",
                    tm.think_ms, tm.queue_ms, tm.search_ms, tm.total_ms, tm.sims,
                    tm.slices, tm.fallback
                )));
                // Always report, even if the tick already resolved: the
                // server discards a stale move but keeps the timings, and
                // silence is indistinguishable from a broken client.
                let proof = snek_shared::bot::bind_proof(nonce, request_id, dir);
                // Datagrams, not the control stream: a bot move is only
                // useful for one tick, and on the reliable stream a single
                // lost packet head-of-line blocks every bot's answer at
                // once — which is exactly how three independent workers end
                // up defaulting on the same turn. Sent twice; the server
                // ignores the duplicate.
                let msg = ClientMsg::BotMove { request_id, dir, proof, tm };
                send_datagram(&msg);
                send_datagram(&msg);
            });
        }
        ServerMsg::GameSaved { id, .. } => store.saved_id.set(Some(id)),
        ServerMsg::Error(e) => store.error.set(Some(e)),
        ServerMsg::Paused(paused) => store.paused.set(paused),
        ServerMsg::Pong(_) => {}
    }
}
