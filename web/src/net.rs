//! Bulletproof-io websocket client: binary postcard frames, automatic
//! reconnect with backoff, app-level keepalive pings. All server messages are
//! folded straight into the reactive store; on reconnect the server replays a
//! full snapshot so no incremental state is ever trusted across a gap.

use crate::store::{ConnStatus, GameView, Store};
use gloo_timers::callback::{Interval, Timeout};
use leptos::prelude::*;
use snek_shared::protocol::*;
use std::cell::RefCell;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{BinaryType, MessageEvent, WebSocket};

const PING_INTERVAL_MS: u32 = 15_000;
const BACKOFF_START_MS: u32 = 400;
const BACKOFF_MAX_MS: u32 = 5_000;

thread_local! {
    static ACTIVE: RefCell<Option<Conn>> = const { RefCell::new(None) };
}

struct Conn {
    session: String,
    /// Generation counter: callbacks from stale sockets check this and bail.
    generation: u64,
    socket: Option<WebSocket>,
    _ping: Option<Interval>,
    _reconnect: Option<Timeout>,
    backoff_ms: u32,
}

/// Connect to a session lobby (idempotent — reconnects only if the session
/// changed or the socket is gone).
pub fn ensure_connected(store: Store, session: &str) {
    let need_new = ACTIVE.with(|a| {
        let a = a.borrow();
        match &*a {
            Some(c) if c.session == session && c.socket.is_some() => false,
            _ => true,
        }
    });
    if need_new {
        open_socket(store, session.to_string(), BACKOFF_START_MS);
    }
}

/// Tear down (leaving the session page).
pub fn disconnect(store: Store) {
    ACTIVE.with(|a| {
        if let Some(mut conn) = a.borrow_mut().take() {
            conn.generation += 1; // invalidate callbacks
            if let Some(sock) = conn.socket.take() {
                let _ = sock.close();
            }
        }
    });
    store.conn.set(ConnStatus::Idle);
    store.lobby.set(None);
    store.game.set(None);
    store.saved_id.set(None);
    store.view_turn.set(None);
}

/// Steer: optimistic d-pad highlight now, server truth on the next turn.
pub fn send_move(store: Store, dir: snek_shared::Dir) {
    store.selected_dir.set(Some(dir));
    send(&ClientMsg::Move(dir));
}

pub fn send(msg: &ClientMsg) {
    ACTIVE.with(|a| {
        if let Some(conn) = &*a.borrow() {
            if let Some(sock) = &conn.socket {
                if sock.ready_state() == WebSocket::OPEN {
                    let _ = sock.send_with_u8_array(&encode(msg));
                }
            }
        }
    });
}

fn ws_url() -> String {
    let loc = web_sys::window().unwrap().location();
    let proto = if loc.protocol().unwrap_or_default() == "https:" {
        "wss"
    } else {
        "ws"
    };
    format!("{proto}://{}/ws", loc.host().unwrap_or_default())
}

fn open_socket(store: Store, session: String, backoff_ms: u32) {
    let generation = ACTIVE.with(|a| {
        let mut a = a.borrow_mut();
        let gen = a.as_ref().map(|c| c.generation + 1).unwrap_or(1);
        if let Some(old) = a.take() {
            if let Some(sock) = old.socket {
                let _ = sock.close();
            }
        }
        *a = Some(Conn {
            session: session.clone(),
            generation: gen,
            socket: None,
            _ping: None,
            _reconnect: None,
            backoff_ms,
        });
        gen
    });

    store.conn.update(|c| {
        *c = if *c == ConnStatus::Connected {
            ConnStatus::Reconnecting
        } else {
            ConnStatus::Connecting
        }
    });

    let socket = match WebSocket::new(&ws_url()) {
        Ok(s) => s,
        Err(_) => {
            schedule_reconnect(store, generation);
            return;
        }
    };
    socket.set_binary_type(BinaryType::Arraybuffer);

    // -- open: join + start keepalive -------------------------------------
    let on_open = {
        let session = session.clone();
        Closure::<dyn FnMut()>::new(move || {
            if !is_current(generation) {
                return;
            }
            send(&ClientMsg::Join {
                version: PROTOCOL_VERSION,
                session: session.clone(),
                secret: crate::store::secret(),
                profile: store.profile.get_untracked(),
            });
            store.conn.set(ConnStatus::Connected);
            let mut n = 0u32;
            let ping = Interval::new(PING_INTERVAL_MS, move || {
                n = n.wrapping_add(1);
                send(&ClientMsg::Ping(n));
            });
            ACTIVE.with(|a| {
                if let Some(conn) = &mut *a.borrow_mut() {
                    if conn.generation == generation {
                        conn._ping = Some(ping);
                        conn.backoff_ms = BACKOFF_START_MS; // healthy again
                    }
                }
            });
        })
    };
    socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    on_open.forget();

    // -- message ----------------------------------------------------------
    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
        if !is_current(generation) {
            return;
        }
        let Ok(buf) = ev.data().dyn_into::<js_sys::ArrayBuffer>() else {
            return;
        };
        let bytes = js_sys::Uint8Array::new(&buf).to_vec();
        if let Some(msg) = decode::<ServerMsg>(&bytes) {
            apply(store, msg);
        }
    });
    socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();

    // -- close / error -----------------------------------------------------
    let on_close = Closure::<dyn FnMut(JsValue)>::new(move |_| {
        if !is_current(generation) {
            return;
        }
        store.conn.set(ConnStatus::Reconnecting);
        schedule_reconnect(store, generation);
    });
    socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    socket.set_onerror(Some(on_close.as_ref().unchecked_ref()));
    on_close.forget();

    ACTIVE.with(|a| {
        if let Some(conn) = &mut *a.borrow_mut() {
            if conn.generation == generation {
                conn.socket = Some(socket);
            }
        }
    });
}

fn is_current(generation: u64) -> bool {
    ACTIVE.with(|a| {
        a.borrow()
            .as_ref()
            .map(|c| c.generation == generation)
            .unwrap_or(false)
    })
}

fn schedule_reconnect(store: Store, generation: u64) {
    ACTIVE.with(|a| {
        let mut a = a.borrow_mut();
        let Some(conn) = &mut *a else { return };
        if conn.generation != generation {
            return;
        }
        conn.socket = None;
        conn._ping = None;
        let session = conn.session.clone();
        let backoff = conn.backoff_ms;
        let next_backoff = (backoff * 2).min(BACKOFF_MAX_MS);
        conn.backoff_ms = next_backoff;
        conn._reconnect = Some(Timeout::new(backoff, move || {
            if is_current(generation) {
                open_socket(store, session, next_backoff);
            }
        }));
    });
}

fn apply(store: Store, msg: ServerMsg) {
    match msg {
        ServerMsg::Welcome { your_id } => store.me.set(your_id),
        ServerMsg::Lobby(lobby) => store.lobby.set(Some(lobby)),
        ServerMsg::GameStart {
            config,
            players,
            turn,
            countdown_ms,
        } => {
            store.saved_id.set(None);
            store.view_turn.set(None);
            store.countdown_ends.set(now() + countdown_ms as f64);
            // Pre-select the clockwise default heading on the d-pad.
            let me = store.me.get_untracked();
            store.selected_dir.set(
                turn.snakes
                    .get(&me)
                    .map(|s| snek_shared::clockwise_start_dir(&config, s[0])),
            );
            store.game.set(Some(GameView {
                config,
                players,
                turns: vec![turn],
            }));
        }
        ServerMsg::Turn(turn) => {
            // The applied move is the truth (covers ignored reversals and
            // auto-continue) — sync the d-pad highlight to it.
            let me = store.me.get_untracked();
            if let Some(&dir) = turn.moves.get(&me) {
                store.selected_dir.set(Some(dir));
            }
            store.game.update(|g| {
                if let Some(g) = g {
                    g.turns.push(turn);
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
        ServerMsg::GameSaved { id } => store.saved_id.set(Some(id)),
        ServerMsg::Error(e) => store.error.set(Some(e)),
        ServerMsg::Pong(_) => {}
    }
}

pub fn now() -> f64 {
    js_sys::Date::now()
}
