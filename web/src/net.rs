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

use crate::num;
use crate::store::{ConnStatus, Countdown, GameView, Store};
use crate::trail;
use futures_util::StreamExt;
use gloo_timers::callback::{Interval, Timeout};
use js_sys::Uint8Array;
use leptos::prelude::*;
use leptos::task::spawn_local;
use snek_shared::protocol::{
    ClientMsg, ComputeTelemetry, ConnectReport, LobbyState, PROTOCOL_VERSION, ServerMsg, decode,
    encode,
};
use snek_shared::{Dir, GameConfig, PlayerId, PlayerProfile, Turn};
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
    /// Held only to keep the keepalive running; dropping it stops it.
    ping: Option<Interval>,
    /// Likewise the move repeater (session mode).
    repeat: Option<Interval>,
    backoff_ms: u32,
    /// When this attempt's connection became ready, once it has.
    ready_at: Option<f64>,
    /// When this connection attempt began, while it is still trying: from
    /// `open` until the server first says something on it. None once it is
    /// live, and while waiting out a backoff.
    attempt_started: Option<f64>,
}

thread_local! {
    static ACTIVE: RefCell<Option<Conn>> = const { RefCell::new(None) };
    /// When the last server message arrived (any kind — pongs count).
    static LAST_RX: Cell<f64> = const { Cell::new(0.0) };
    /// Newest turn we have seen. Compute requests older than this are
    /// already obsolete — the tick they were for has resolved — so running
    /// them would only push the queue further behind.
    static LATEST_TURN: Cell<u32> = const { Cell::new(0) };
    /// This device's measured search speed, once it has been measured.
    static BENCHMARK: Cell<Option<u32>> = const { Cell::new(None) };
    /// The connection we lost and are getting back, while we are.
    static OUTAGE: Cell<Option<Outage>> = const { Cell::new(None) };
    /// Where we're going once the server confirms we've left a game.
    static LEAVING_FOR: RefCell<Option<String>> = const { RefCell::new(None) };
    /// The next move away from a game has been agreed to (see
    /// `app::read_route`).
    static LEAVE_ALLOWED: Cell<bool> = const { Cell::new(false) };
    /// When the page last came back into view.
    static VISIBLE_SINCE: Cell<f64> = const { Cell::new(0.0) };
    /// Whether the page was last seen hidden.
    static HIDDEN: Cell<bool> = const { Cell::new(false) };
}

/// A live connection lost, timed until it is back so the server can be
/// told how the reconnect went (see [`ConnectReport`]).
#[derive(Clone, Copy)]
struct Outage {
    /// Last thing heard on the connection that went.
    last_rx: f64,
    attempts: u32,
}

/// Marker for the one automatic reload we allow ourselves.
const RELOAD_KEY: &str = "snek-reloaded-at";
const RELOAD_COOLDOWN_MS: f64 = 30_000.0;

fn reloaded_recently() -> bool {
    web_sys::window()
        .and_then(|w| w.session_storage().ok().flatten())
        .and_then(|s| s.get_item(RELOAD_KEY).ok().flatten())
        .and_then(|v| v.parse::<f64>().ok())
        .is_some_and(|at| now() - at < RELOAD_COOLDOWN_MS)
}

thread_local! {
    /// Attempts in a row that never reached Connected.
    static FAILED_ATTEMPTS: Cell<u32> = const { Cell::new(0) };
}

/// Attempts before we stop assuming it is us and start suspecting the
/// network. Two is enough to rule out a single unlucky handshake.
const ATTEMPTS_BEFORE_DIAGNOSING: u32 = 2;

/// Work out why connecting keeps failing, and do something about it.
///
/// This is answerable rather than guessable. The page itself arrived over
/// TCP, and the API lives on the same host, so if a plain HTTP request
/// succeeds while WebTransport will not connect, then the server is
/// reachable and UDP is not. Corporate and school networks routinely block
/// UDP/443 outright, because QUIC is encrypted end to end and their
/// middleboxes cannot read it. Ordinary sites just fall back to TCP and
/// nobody notices; WebTransport has no TCP to fall back to.
fn diagnose_after_failure(store: Store, generation: u64) {
    let attempts = FAILED_ATTEMPTS.with(|c| {
        let n = c.get() + 1;
        c.set(n);
        n
    });
    if attempts != ATTEMPTS_BEFORE_DIAGNOSING {
        return; // once, on the way past
    }
    spawn_local(async move {
        let tcp_works = gloo_net::http::Request::get("/api/sessions")
            .send()
            .await
            .is_ok_and(|r| r.ok());
        // Only if TCP really did answer and we're still not through: else
        // this is just an offline phone. (Not "is this attempt still the
        // live one": the attempt that timed out has always been replaced by
        // the time the answer comes back.)
        if !tcp_works || store.conn.get_untracked() == ConnStatus::Connected {
            return;
        }
        // The server is there and QUIC won't reach it. Seen on phones after
        // a sleep: every new connection the page opens hangs, while the
        // same page freshly loaded connects at once. So the first remedy is
        // the one that works, a reload. Only if that didn't help (we
        // reloaded moments ago) is it the network itself blocking UDP.
        if reloaded_recently() {
            store.quic_blocked.set(true);
            return;
        }
        if let Some(w) = web_sys::window() {
            trail::note(&format!(
                "reloading: #{generation} and the one before hung, http fine"
            ));
            if let Ok(Some(s)) = w.session_storage() {
                let _ = s.set_item(RELOAD_KEY, &now().to_string());
            }
            let _ = w.location().reload();
        }
    });
}

fn mark_rx() {
    LAST_RX.with(|c| c.set(now()));
}

fn is_stale() -> bool {
    LAST_RX.with(|c| now() - c.get() > STALE_MS)
}

pub(crate) fn now() -> f64 {
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
pub(crate) fn ensure_session(store: Store, session: &str) {
    ensure(store, Mode::Session(session.to_owned()));
}

/// Subscribe to the live games list (idempotent).
pub(crate) fn ensure_watch(store: Store) {
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
        OUTAGE.with(|o| o.set(None));
    }
    trail::want_connection(true);
    if need_new {
        trail::note(if mode_changed {
            "navigated"
        } else {
            "reopen: no connection"
        });
        open(store, mode, BACKOFF_START_MS);
    }
}

/// The app came back to the foreground (or the network returned): don't
/// wait out a (possibly throttled) backoff timer — reconnect right now if
/// the connection isn't healthy. `open` bumps the generation, so any
/// pending backoff timers and the old transport's callbacks go stale.
pub(crate) fn nudge(store: Store) {
    let reopen = ACTIVE.with(|a| {
        let mut a = a.borrow_mut();
        match &mut *a {
            Some(conn) => {
                conn.backoff_ms = BACKOFF_START_MS;
                // Waiting out a backoff: skip the rest of it. A transport
                // that looks fine but has gone silent is a zombie
                // (suspended tabs often never fire `closed`). An attempt
                // that has run past its deadline is a hang. An attempt
                // still inside its deadline is none of these: restarting
                // it only throws away a handshake already under way.
                let waiting = conn.transport.is_none() && conn.attempt_started.is_none();
                let hung = conn
                    .attempt_started
                    .is_some_and(|t| now() - t > f64::from(CONNECT_TIMEOUT_MS));
                let stale = is_stale();
                (waiting || hung || stale).then(|| {
                    trail::note(&format!(
                        "reopen: {}",
                        if waiting {
                            "skipping backoff"
                        } else if hung {
                            "attempt hung"
                        } else {
                            "silent"
                        }
                    ));
                    conn.mode.clone()
                })
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
pub(crate) fn check_liveness(store: Store) {
    if store.conn.get_untracked() != ConnStatus::Idle && is_stale() {
        nudge(store);
    }
}

/// Tear down (leaving for an unconnected page).
pub(crate) fn disconnect(store: Store) {
    ACTIVE.with(|a| {
        if let Some(mut conn) = a.borrow_mut().take() {
            conn.generation += 1;
            if let Some(t) = conn.transport.take() {
                t.close();
            }
        }
    });
    store.conn.set(ConnStatus::Idle);
    trail::want_connection(false);
    OUTAGE.with(|o| o.set(None));
    store.lobby.set(None);
    store.game.set(None);
    store.saved_id.set(None);
    store.view_turn.set(None);
}

/// A message as the control stream carries it: its length (u32, little
/// endian), then the message.
fn framed(msg: &ClientMsg) -> Option<Vec<u8>> {
    let bytes = encode(msg);
    let len = u32::try_from(bytes.len()).ok()?;
    let mut framed = Vec::with_capacity(bytes.len() + 4);
    framed.extend_from_slice(&len.to_le_bytes());
    framed.extend_from_slice(&bytes);
    Some(framed)
}

/// Write one framed message to a control stream.
fn write_framed(writer: &WritableStreamDefaultWriter, msg: &ClientMsg) {
    if let Some(framed) = framed(msg) {
        let _ = writer.write_with_chunk(&Uint8Array::from(framed.as_slice()).into());
    }
}

/// Control-class message over the reliable stream.
pub(crate) fn send(msg: &ClientMsg) {
    ACTIVE.with(|a| {
        if let Some(conn) = &*a.borrow()
            && let Some(writer) = &conn.control
        {
            write_framed(writer, msg);
        }
    });
}

fn send_datagram(msg: &ClientMsg) {
    let bytes = encode(msg);
    ACTIVE.with(|a| {
        if let Some(conn) = &*a.borrow()
            && let Some(writer) = &conn.dgrams
        {
            let _ = writer.write_with_chunk(&Uint8Array::from(bytes.as_slice()).into());
        }
    });
}

/// Steer: light the key you pressed, send the move up, and keep re-sending
/// it until the server has resolved the turn. The lit key is your own
/// input, not game state: pending moves are private (nobody else ever sees
/// them), and the next snapshot shows where your snek actually went. Not
/// connected, nothing happens at all, so the pad never shows a move the
/// server can't have.
pub(crate) fn send_move(store: Store, dir: Dir) {
    if store.conn.get_untracked() != ConnStatus::Connected {
        return;
    }
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
            .is_some_and(|c| c.generation == generation)
    })
}

fn open(store: Store, mode: Mode, backoff_ms: u32) {
    // A reconnect, as opposed to a first connect or a navigation: time it.
    if matches!(
        store.conn.get_untracked(),
        ConnStatus::Connected | ConnStatus::Reconnecting
    ) {
        let last_rx = LAST_RX.with(Cell::get);
        OUTAGE.with(|o| {
            let outage = o.get().unwrap_or(Outage {
                last_rx,
                attempts: 0,
            });
            o.set(Some(Outage {
                attempts: outage.attempts + 1,
                ..outage
            }));
        });
    }
    let generation = ACTIVE.with(|a| {
        let mut a = a.borrow_mut();
        let generation = a.as_ref().map_or(1, |c| c.generation + 1);
        if let Some(mut old) = a.take()
            && let Some(t) = old.transport.take()
        {
            t.close();
        }
        *a = Some(Conn {
            mode: mode.clone(),
            generation,
            transport: None,
            control: None,
            dgrams: None,
            ping: None,
            repeat: None,
            backoff_ms,
            ready_at: None,
            attempt_started: Some(now()),
        });
        generation
    });

    trail::note(&format!("open #{generation}"));
    mark_rx(); // fresh attempt starts with a fresh liveness clock
    // Losing a connection is reconnecting until we're back, however many
    // attempts that takes; anything else is a first connect.
    store.conn.update(|c| {
        *c = if matches!(*c, ConnStatus::Connected | ConnStatus::Reconnecting) {
            ConnStatus::Reconnecting
        } else {
            ConnStatus::Connecting
        }
    });

    let attempt_mode = mode.clone();
    spawn_local(async move {
        if let Err(e) = connect(store, attempt_mode, generation).await {
            // A thrown WebTransportError carries its reason in `message`.
            let reason = js_sys::Reflect::get(&e, &"message".into())
                .ok()
                .and_then(|m| m.as_string())
                .or_else(|| e.as_string())
                .unwrap_or_default();
            trail::note(&format!("#{generation} failed: {reason}"));
            tracing_debug(&e);
            diagnose_after_failure(store, generation);
            schedule_reconnect(store, generation);
        }
    });

    // Hung-attempt guard: if this attempt is still trying after the
    // deadline, abandon it (open bumps the generation, orphaning the hung
    // future and closing its transport) and try again. It asks whether
    // *this attempt* got through, not what the status is now: a phone that
    // sleeps fires this timer late, long after the attempt connected, and
    // the status it would see is the one the sleep itself caused.
    Timeout::new(CONNECT_TIMEOUT_MS, move || {
        if still_trying(generation) {
            trail::note(&format!("#{generation} timed out"));
            diagnose_after_failure(store, generation);
            let next = (backoff_ms * 2).min(BACKOFF_MAX_MS);
            open(store, mode, next);
        }
    })
    .forget();
}

/// Whether this generation is the live one and still hasn't heard from the
/// server.
fn still_trying(generation: u64) -> bool {
    ACTIVE.with(|a| {
        a.borrow()
            .as_ref()
            .is_some_and(|c| c.generation == generation && c.attempt_started.is_some())
    })
}

/// The server has answered a new session connection: tell it how getting
/// here went (see [`ConnectReport`]): how long this attempt's handshake and
/// join took, and for a reconnect, how long the player was cut off and how
/// many attempts it took, with the trail of what happened on the way.
fn report_connect(started: f64, ready_at: f64) {
    let at = now();
    let outage = OUTAGE.with(Cell::take);
    let visible_since = VISIBLE_SINCE.with(Cell::get);
    send(&ClientMsg::Connected(ConnectReport {
        reconnect: outage.is_some(),
        attempts: outage.map_or(1, |o| o.attempts),
        silent_ms: outage.map(|o| num::to_u32(at - o.last_rx)),
        // Only if the page was hidden after the old connection went quiet:
        // otherwise it was in view throughout and there is no wake to time.
        since_visible_ms: outage
            .filter(|o| visible_since > o.last_rx)
            .map(|_| num::to_u32(at - visible_since)),
        page_ms: num::to_u32(trail::page_ms()),
        handshake_ms: num::to_u32(ready_at - started),
        join_ms: num::to_u32(at - ready_at),
        trail: trail::events(),
    }));
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

/// What a dev server publishes at /api/dev/wt (see [`dev_wt`]).
#[derive(serde::Deserialize)]
struct DevWt {
    port: u16,
    #[serde(rename = "certHash")]
    cert_hash: Vec<u8>,
}

/// Local dev: a page served from localhost talks to a snek-server that has
/// no real certificate, only a self-signed one. Browsers accept that over
/// WebTransport only if handed the certificate's hash up front, which that
/// server publishes at /api/dev/wt along with its port. Anywhere else this
/// is never asked and the normal :443 connection is made.
async fn dev_wt() -> Option<(u16, Vec<u8>)> {
    let host = web_sys::window()?.location().hostname().ok()?;
    if host != "localhost" && host != "127.0.0.1" {
        return None;
    }
    let resp = gloo_net::http::Request::get("/api/dev/wt")
        .send()
        .await
        .ok()?;
    if !resp.ok() {
        return None;
    }
    let dev: DevWt = resp.json().await.ok()?;
    Some((dev.port, dev.cert_hash))
}

fn open_transport(dev: Option<(u16, Vec<u8>)>) -> Result<WebTransport, JsValue> {
    let Some((port, cert_hash)) = dev else {
        return WebTransport::new(&wt_url());
    };
    let host = web_sys::window()
        .and_then(|w| w.location().hostname().ok())
        .unwrap_or_default();
    let hash = web_sys::WebTransportHash::new();
    hash.set_algorithm("sha-256");
    hash.set_value_u8_array(&Uint8Array::from(&cert_hash[..]));
    let opts = web_sys::WebTransportOptions::new();
    opts.set_server_certificate_hashes(&[hash]);
    WebTransport::new_with_options(&format!("https://{host}:{port}"), &opts)
}

fn webtransport_missing() -> bool {
    web_sys::window()
        .is_none_or(|w| !js_sys::Reflect::has(&w, &"WebTransport".into()).unwrap_or(false))
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
        trail::want_connection(false);
        return Ok(());
    }
    let transport = open_transport(dev_wt().await)?;
    // Register immediately so the attempt-timeout guard (and any newer
    // generation) can close a handshake that never resolves.
    ACTIVE.with(|a| {
        if let Some(conn) = &mut *a.borrow_mut()
            && conn.generation == generation
        {
            conn.transport = Some(transport.clone());
        }
    });
    JsFuture::from(transport.ready()).await?;
    if !is_current(generation) {
        transport.close();
        return Ok(());
    }
    ACTIVE.with(|a| {
        if let Some(conn) = &mut *a.borrow_mut()
            && conn.generation == generation
        {
            conn.ready_at = Some(now());
        }
    });

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
    write_framed(&control_writer, &first);

    // Keepalive ping (both modes — liveness needs traffic) + input
    // redundancy (session mode only).
    let ping = keepalive();
    let repeat = match mode {
        Mode::Session(_) => Some(move_repeater(store)),
        Mode::Watch => None,
    };

    ACTIVE.with(|a| {
        if let Some(conn) = &mut *a.borrow_mut() {
            if conn.generation == generation {
                conn.transport = Some(transport.clone());
                conn.control = Some(control_writer);
                conn.dgrams = Some(dgram_writer);
                conn.ping = Some(ping);
                conn.repeat = repeat;
                conn.backoff_ms = BACKOFF_START_MS; // healthy again
            } else {
                transport.close();
            }
        }
    });
    // Not Connected yet: that is the server talking to us (see `apply`),
    // not a handshake finishing.
    read_control(
        store,
        generation,
        control_readable.unchecked_into::<web_sys::ReadableStream>(),
    );
    read_datagrams(store, generation, dgram_readable);

    // Session hosts contribute compute: measure this device's real search
    // speed (~600ms of actual search, chunked so the UI stays alive) and
    // report it. The server assigns bots to the fastest phones.
    if matches!(mode, Mode::Session(_)) {
        spawn_local(volunteer_compute(generation));
    }

    // Reconnect when the transport dies.
    spawn_local(async move {
        let _ = JsFuture::from(transport.closed()).await;
        if is_current(generation) {
            trail::note(&format!("#{generation} closed"));
            store.conn.set(ConnStatus::Reconnecting);
            schedule_reconnect(store, generation);
        }
    });

    Ok(())
}

/// Ping every couple of seconds, so a live connection is never silent.
fn keepalive() -> Interval {
    let mut n = 0u32;
    Interval::new(PING_INTERVAL_MS, move || {
        n = n.wrapping_add(1);
        send_datagram(&ClientMsg::Ping(n));
    })
}

/// Re-send our heading while we are alive in a running game, so a lost
/// input costs one repeat, not a turn.
fn move_repeater(store: Store) -> Interval {
    Interval::new(MOVE_REPEAT_MS, move || {
        let live = store.game.with_untracked(|g| {
            g.as_ref()
                .and_then(|g| g.turns.last())
                .is_some_and(|t| !t.game_over() && t.alive.contains(&store.me.get_untracked()))
        });
        if live && let Some(dir) = store.selected_dir.get_untracked() {
            send_datagram(&ClientMsg::Move(dir));
        }
    })
}

/// Reader: control stream (length-prefixed frames).
fn read_control(store: Store, generation: u64, readable: web_sys::ReadableStream) {
    let mut stream = wasm_streams::ReadableStream::from_raw(readable).into_stream();
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

/// Reader: datagrams (one message per datagram).
fn read_datagrams(store: Store, generation: u64, readable: web_sys::ReadableStream) {
    let mut stream = wasm_streams::ReadableStream::from_raw(readable).into_stream();
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

/// Benchmark this device and offer it to the server as a bot host.
///
/// Measured once per page load: the device doesn't get slower or faster
/// between connections, and a reconnect is exactly when a phone that has
/// just woken up should be answering the server, not searching a test
/// board.
async fn volunteer_compute(generation: u64) {
    let mut rate = BENCHMARK.with(Cell::get);
    if rate.is_none() {
        rate = benchmark_sims_per_sec(generation).await;
        BENCHMARK.with(|b| b.set(rate));
    }
    if is_current(generation)
        && let Some(rate) = rate
    {
        send(&ClientMsg::Benchmark {
            sims_per_sec: rate,
            device: device_description(),
            cores: web_sys::window()
                .map_or(4, |w| num::to_u32(w.navigator().hardware_concurrency())),
        });
    }
}

thread_local! {
    /// Requests already taken up. Each one is sent three times — twice as a
    /// datagram and once on the stream — so a lost packet cannot hold it up.
    /// Only the copy that gets here first is worth searching.
    static HANDLED: RefCell<std::collections::VecDeque<u64>> =
        const { RefCell::new(std::collections::VecDeque::new()) };
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
pub(crate) fn watch_main_thread() {
    let mut last = now();
    let beat = Interval::new(50, move || {
        let t = now();
        let lag = num::to_u32((t - last) - 50.0);
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
pub(crate) fn report_availability(visible: bool) {
    // Focus and pageshow report "visible" too; only a return from hidden
    // is a wake.
    let was_hidden = HIDDEN.with(|h| h.replace(!visible));
    if was_hidden && visible {
        VISIBLE_SINCE.with(|v| v.set(now()));
        trail::note("visible");
    } else if !was_hidden && !visible {
        trail::note("hidden");
    }
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
        .map(|(inner, _)| inner.to_owned())
        .unwrap_or(ua);
    inner.chars().take(80).collect()
}

/// ~600ms of genuine search on a synthetic board, yielding between chunks.
async fn benchmark_sims_per_sec(generation: u64) -> Option<u32> {
    use snek_shared::{Engine, GameConfig};
    // One timed search on the worker: it both proves a worker can run here
    // (no worker, no volunteering) and gives an initial speed for ranking
    // hosts. Everything after this is deadline-driven, so nothing is
    // predicted from it.
    const PROBE_MS: u32 = 300;
    let config = GameConfig {
        board_width: 13,
        board_height: 13,
        tick_ms: 1000,
        food_emoji: snek_shared::DEFAULT_FOOD.to_owned(),
    };
    let a: PlayerId = "bench-a".into();
    let mut engine = Engine::new(config.clone(), vec![a.clone(), "bench-b".into()], 1);
    let turn = engine.first_turn();

    let (_dir, _proof, sims, _slices, _began, _search) =
        crate::botworker::compute(config, turn, a.clone(), PROBE_MS, 0).await?;
    if !is_current(generation) {
        return None;
    }
    Some(num::to_u32(f64::from(sims) * 1000.0 / f64::from(PROBE_MS)))
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
        conn.ping = None;
        conn.repeat = None;
        conn.attempt_started = None;
        let mode = conn.mode.clone();
        let backoff = conn.backoff_ms;
        conn.backoff_ms = (backoff * 2).min(BACKOFF_MAX_MS);
        trail::note(&format!("retry in {backoff}ms"));
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
    // Only the server's own words count for the watchdog: a page busy
    // retrying, however hard, hasn't heard from it.
    trail::heard_from_server();
    // Connected means the server is actually talking to us on this
    // connection — not merely that a QUIC handshake finished. This is what
    // lets the hung-attempt guard recycle a stalled join.
    if store.conn.get_untracked() != ConnStatus::Connected {
        let attempt = ACTIVE.with(|a| {
            a.borrow_mut().as_mut().map(|conn| {
                let times = (conn.attempt_started.take(), conn.ready_at);
                (times, matches!(conn.mode, Mode::Session(_)))
            })
        });
        store.conn.set(ConnStatus::Connected);
        trail::note("connected");
        if let Some(((Some(started), ready_at), true)) = attempt {
            report_connect(started, ready_at.unwrap_or(started));
        }
        FAILED_ATTEMPTS.with(|c| c.set(0));
        store.quic_blocked.set(false);
    }
    match msg {
        ServerMsg::Welcome { your_id } => {
            store.me.set(your_id);
        }
        ServerMsg::Sessions(list) => store.sessions.set(Some(list)),
        ServerMsg::Lobby(lobby) => on_lobby(store, lobby),
        ServerMsg::GameStart {
            config,
            players,
            turn,
            countdown_ms,
        } => on_game_start(store, config, players, turn, countdown_ms),
        ServerMsg::Turn(turn) => on_turn(store, turn),
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
        } => on_compute_request(BotJob {
            request_id,
            bot_id,
            config,
            turn,
            think_ms,
            nonce,
        }),
        ServerMsg::GameSaved { id, .. } => store.saved_id.set(Some(id)),
        ServerMsg::Error(e) => on_error(store, e),
        ServerMsg::Hold(hold) => {
            // Lifted: the countdown runs again before play does.
            if let Some(ms) = hold.resume_in_ms {
                let began = now();
                store.countdown.set(Countdown {
                    began,
                    ends: began + f64::from(ms),
                    resume: true,
                });
            }
            store.hold.set(hold);
        }
        ServerMsg::Kicked(by) => on_kicked(store, by.as_ref()),
        ServerMsg::Left => finish_leaving(),
        ServerMsg::Pong(_) => {}
    }
}

fn on_lobby(store: Store, lobby: LobbyState) {
    // Server phase truth: no game running + we hold an *unfinished*
    // game means we missed the ending while away — fall back to the
    // lobby. A finished game (results screen) stays until the user
    // leaves it.
    if !lobby.game_running {
        let stale_unfinished = store.game.with_untracked(|g| {
            g.as_ref()
                .and_then(|g| g.turns.last())
                .is_some_and(|t| !t.game_over())
        });
        if stale_unfinished {
            store.game.set(None);
            store.view_turn.set(None);
            store.selected_dir.set(None);
        }
    }
    // Our hand of foods holds the lobby's pick, whoever made it.
    store
        .food_hand
        .update(|hand| hand.show_pick(&lobby.config.food_emoji));
    // Someone else changed the settings: say who, and what. The first lobby
    // of a session is not a change.
    store.lobby.with_untracked(|old| {
        if let Some(old) = old
            && old.session == lobby.session
            && old.config_rev != lobby.config_rev
        {
            crate::notices::settings_changed(store, old, &lobby);
        }
    });
    store.lobby.set(Some(lobby));
}

/// Whether we're in a game (its lobby or the game itself), so leaving
/// would mean something.
pub(crate) fn in_a_game(store: Store) -> bool {
    store.lobby.with_untracked(Option::is_some) || store.game.with_untracked(Option::is_some)
}

/// Agree to the next move away from a game, once.
pub(crate) fn take_leave_allowed() -> bool {
    LEAVE_ALLOWED.with(|l| l.replace(false))
}

/// Go to `target` (a URL hash) from a game, taking ourselves out of it on
/// the way. The server is told first and we go when it confirms (or after
/// a moment regardless): closing the connection straight after sending
/// could lose the message, and the seat would only time out.
pub(crate) fn leave_to(store: Store, target: String) {
    store.popup.set(None);
    LEAVING_FOR.with(|l| *l.borrow_mut() = Some(target));
    if store.conn.get_untracked() == ConnStatus::Connected {
        send(&ClientMsg::Leave);
        Timeout::new(LEAVE_WAIT_MS, finish_leaving).forget();
    } else {
        finish_leaving();
    }
}

/// How long to wait for the server to confirm a leave before going anyway.
const LEAVE_WAIT_MS: u32 = 1_500;

/// Go wherever we were leaving for, if we still are.
fn finish_leaving() {
    if let Some(target) = LEAVING_FOR.with(|l| l.borrow_mut().take()) {
        LEAVE_ALLOWED.with(|l| l.set(true));
        crate::app::navigate(&target);
    }
}

/// We were taken out of the lobby: back to the home screen, with a toast
/// saying who did it. Leaving the session's route is what drops the
/// connection (the server has already let it go).
fn on_kicked(store: Store, by: Option<&PlayerProfile>) {
    store.popup.set(None);
    crate::notices::removed(store, by);
    LEAVE_ALLOWED.with(|l| l.set(true));
    crate::app::navigate("#/");
}

fn on_game_start(
    store: Store,
    config: GameConfig,
    players: Vec<PlayerProfile>,
    turn: Turn,
    countdown_ms: u32,
) {
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
    let began = now();
    store.countdown.set(Countdown {
        began,
        ends: began + f64::from(countdown_ms),
        resume: false,
    });
    // Pre-select the clockwise default heading on the d-pad.
    let me = store.me.get_untracked();
    // A fresh game always runs; never inherit the last one's hold.
    store.hold.set(snek_shared::protocol::Hold::default());
    // And the lobby's popups belong to the lobby: back from the game, it
    // starts closed.
    store.popup.set(None);
    store.selected_dir.set(
        turn.snakes
            .get(&me)
            .and_then(|s| s.first())
            .map(|&head| snek_shared::clockwise_start_dir(&config, head)),
    );
    store.game.set(Some(GameView {
        config,
        players,
        turns: vec![turn],
    }));
}

fn on_turn(store: Store, turn: Turn) {
    LATEST_TURN.with(|c| c.set(c.get().max(turn.turn_number)));
    // Datagram-class: dedup the redundant copies, drop stale
    // reorderings, accept gaps (snapshots are full state).
    let me = store.me.get_untracked();
    // Play has begun: whatever this phone's clock makes of the countdown,
    // it's over. (It counts from when the start reached it, so it runs a
    // little behind the server's, and the first move would otherwise be
    // drawn with the last of the countdown still on it.)
    let newer = store.game.with_untracked(|g| {
        g.as_ref().is_some_and(|g| {
            g.turns
                .last()
                .is_none_or(|t| turn.turn_number > t.turn_number)
        })
    });
    if newer && store.countdown.with_untracked(|c| c.ends > now()) {
        store.countdown.update(|c| c.ends = now());
    }
    let finished = newer && turn.game_over();
    store.game.update(|g| {
        if let Some(g) = g {
            let last = g.turns.last().map_or(0, |t| t.turn_number);
            if turn.turn_number > last {
                if let Some(&dir) = turn.moves.get(&me) {
                    store.selected_dir.set(Some(dir));
                }
                g.turns.push(turn);
            }
        }
    });
    // It's over: play the highlights.
    if finished {
        crate::autoplay::start(store, None);
    }
}

/// A `ComputeRequest`: one bot move the server wants from this device.
struct BotJob {
    request_id: u64,
    bot_id: PlayerId,
    config: GameConfig,
    turn: Turn,
    think_ms: u32,
    nonce: u64,
}

fn on_compute_request(job: BotJob) {
    // All search happens on the worker thread; the UI thread never
    // touches it. A device that cannot spawn a worker never
    // benchmarks, so it is never assigned a bot in the first place
    // — and if one somehow arrives, staying silent makes the server
    // hand that bot to another phone.
    if !first_sighting(job.request_id) {
        return; // another copy of this request got here first
    }
    if job.turn.turn_number < LATEST_TURN.with(Cell::get) {
        web_sys::console::debug_1(&JsValue::from_str(&format!(
            "compute skipped as stale bot={} turn={}",
            job.bot_id, job.turn.turn_number
        )));
        return; // superseded before we even started
    }
    spawn_local(think_for_bot(job));
}

/// Search for the bot's move, on the worker if there is one, and answer.
async fn think_for_bot(job: BotJob) {
    let requested_for = job.turn.turn_number;
    let arrived = now();
    let searched = crate::botworker::compute(
        job.config.clone(),
        job.turn.clone(),
        job.bot_id.clone(),
        job.think_ms,
        job.nonce,
    )
    .await;
    let (dir, tm) = if let Some((dir, _proof, sims, slices, began_ms, search_ms)) = searched {
        (
            dir,
            ComputeTelemetry {
                queue_ms: num::to_u32(began_ms - arrived),
                search_ms,
                total_ms: num::to_u32(now() - arrived),
                think_ms: job.think_ms,
                sims,
                slices,
                fallback: false,
                loop_lag_ms: take_loop_lag(),
            },
        )
    } else {
        search_here(&job, arrived)
    };
    web_sys::console::debug_1(&JsValue::from_str(&format!(
        "compute bot={} turn={requested_for} think={}ms queue={}ms search={}ms total={}ms sims={} slices={} fallback={}",
        job.bot_id,
        tm.think_ms,
        tm.queue_ms,
        tm.search_ms,
        tm.total_ms,
        tm.sims,
        tm.slices,
        tm.fallback
    )));
    // Always report, even if the tick already resolved: the
    // server discards a stale move but keeps the timings, and
    // silence is indistinguishable from a broken client.
    let proof = snek_shared::bot::bind_proof(job.nonce, job.request_id, dir);
    // Datagrams, not the control stream: a bot move is only
    // useful for one tick, and on the reliable stream a single
    // lost packet head-of-line blocks every bot's answer at
    // once — which is exactly how three independent workers end
    // up defaulting on the same turn. Sent twice; the server
    // ignores the duplicate.
    let msg = ClientMsg::BotMove {
        request_id: job.request_id,
        dir,
        proof,
        tm,
    };
    send_datagram(&msg);
    send_datagram(&msg);
}

/// The worker is gone. Say so — silence here is indistinguishable from a
/// dead client — then take a short, bounded turn on this thread so the bot
/// still plays. A brief hitch beats a snake driving into a wall.
fn search_here(job: &BotJob, arrived: f64) -> (Dir, ComputeTelemetry) {
    send_datagram(&ClientMsg::ComputeFailed {
        request_id: job.request_id,
        reason: "worker unavailable".into(),
    });
    let mut search =
        snek_shared::bot::begin(&job.config, &job.turn, &job.bot_id, u32::MAX, job.nonce);
    let cutoff = now() + f64::from(job.think_ms).min(60.0);
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
            search_ms: num::to_u32(now() - cutoff + 60.0),
            total_ms: num::to_u32(now() - arrived),
            think_ms: job.think_ms,
            sims,
            slices: sims / 64,
            fallback: true,
            loop_lag_ms: take_loop_lag(),
        },
    )
}

fn on_error(store: Store, e: String) {
    // A version mismatch means this page is running code the server
    // has moved past, and no amount of reconnecting will fix it —
    // it just retries forever behind "Joining session…". Telling a
    // stuck page to refresh itself is the only thing that works, and
    // the shell is served no-cache so the reload picks up the new
    // build. Once only: if a reload doesn't resolve it, looping
    // would be worse than showing the error.
    if e.contains("version")
        && !reloaded_recently()
        && let Some(w) = web_sys::window()
    {
        let _ = w
            .session_storage()
            .map(|s| s.map(|s| s.set_item(RELOAD_KEY, &now().to_string())));
        let _ = w.location().reload();
        return;
    }
    store.error.set(Some(e));
}
