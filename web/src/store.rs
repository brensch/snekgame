//! Global reactive state + the player's persistent identity (localStorage).

use leptos::prelude::*;
use snek_shared::protocol::{LobbyState, ProfileInfo, SessionSummary};
use snek_shared::{Dir, GameConfig, PlayerId, PlayerProfile, Turn};
use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    Home,
    Session(String),
    History,
    Replay(i64),
    Compute,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnStatus {
    Idle,
    Connecting,
    Connected,
    Reconnecting,
}

/// The game currently on screen (live from the server).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GameView {
    pub config: GameConfig,
    pub players: Vec<PlayerProfile>,
    pub turns: Vec<Turn>,
}

#[derive(Clone, Copy)]
pub(crate) struct Store {
    pub route: RwSignal<Route>,
    /// Editable profile fields; the public id comes back from the server.
    pub profile: RwSignal<ProfileInfo>,
    /// Our server-derived public id (empty until `Welcome` arrives).
    pub me: RwSignal<PlayerId>,
    pub show_profile_editor: RwSignal<bool>,
    pub show_bot_picker: RwSignal<bool>,
    pub conn: RwSignal<ConnStatus>,
    pub lobby: RwSignal<Option<LobbyState>>,
    /// Live games list, pushed by the server while watching (home screen).
    pub sessions: RwSignal<Option<Vec<SessionSummary>>>,
    pub game: RwSignal<Option<GameView>>,
    /// History scrubber position when reviewing a finished game (None = live).
    pub view_turn: RwSignal<Option<usize>>,
    /// Set once we have evidence the network passes TCP but not UDP —
    /// which is a firewall eating QUIC, not a broken site.
    pub quic_blocked: RwSignal<bool>,
    /// When the lobby's settings last changed under us (ms), so the "last
    /// changed by" box can flash — and keep flashing across the lobby being
    /// redrawn for some other reason partway through.
    pub settings_flash_at: RwSignal<f64>,
    /// Someone has held the game. Server-owned: we only ever mirror what it
    /// tells us, so every screen agrees on whether the clock is running.
    pub paused: RwSignal<bool>,
    /// The direction we're currently steering (highlights the d-pad).
    /// Starts at the clockwise default; corrected from each resolved turn.
    pub selected_dir: RwSignal<Option<Dir>>,
    pub saved_id: RwSignal<Option<i64>>,
    /// performance-now ms when the pre-game countdown ends.
    pub countdown_ends: RwSignal<f64>,
    /// Coarse clock for countdown display, ticked by an interval.
    pub now_ms: RwSignal<f64>,
    pub error: RwSignal<Option<String>>,
}

impl Store {
    pub(crate) fn new() -> Self {
        Self {
            route: RwSignal::new(Route::Home),
            profile: RwSignal::new(load_profile()),
            me: RwSignal::new(String::new()),
            show_profile_editor: RwSignal::new(false),
            show_bot_picker: RwSignal::new(false),
            conn: RwSignal::new(ConnStatus::Idle),
            lobby: RwSignal::new(None),
            sessions: RwSignal::new(None),
            game: RwSignal::new(None),
            view_turn: RwSignal::new(None),
            quic_blocked: RwSignal::new(false),
            settings_flash_at: RwSignal::new(0.0),
            paused: RwSignal::new(false),
            selected_dir: RwSignal::new(None),
            saved_id: RwSignal::new(None),
            countdown_ends: RwSignal::new(0.0),
            now_ms: RwSignal::new(0.0),
            error: RwSignal::new(None),
        }
    }

    pub(crate) fn use_store() -> Self {
        expect_context::<Self>()
    }
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

/// The private secret proving our identity to the server. Never shown to or
/// shared with other players — they only ever see the hash the server derives.
pub(crate) fn secret() -> String {
    let get = || -> Option<String> { storage()?.get_item("snek_secret").ok()? };
    get().unwrap_or_else(|| {
        let secret = random_secret();
        if let Some(s) = storage() {
            let _ = s.set_item("snek_secret", &secret);
        }
        secret
    })
}

/// Random hex string. Uses `crypto.getRandomValues`, which (unlike
/// `crypto.randomUUID`) also works in non-secure contexts like plain-http
/// LAN addresses; falls back to Math.random only if even that is missing.
pub(crate) fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    let ok = web_sys::window()
        .and_then(|w| w.crypto().ok())
        .is_some_and(|c| c.get_random_values_with_u8_array(&mut buf).is_ok());
    if !ok {
        for b in &mut buf {
            *b =
                u8::try_from(crate::num::to_u32(js_sys::Math::random() * 256.0)).unwrap_or(u8::MAX);
        }
    }
    buf.iter()
        .fold(String::with_capacity(bytes * 2), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        })
}

fn random_secret() -> String {
    random_hex(16)
}

/// Opaque id for a newly created session — this is what goes in the URL.
pub(crate) fn new_session_id() -> String {
    random_hex(8)
}

fn load_profile() -> ProfileInfo {
    let s = storage();
    let get = |key: &str| -> Option<String> { s.as_ref()?.get_item(key).ok()? };

    let emoji = get("snek_emoji").unwrap_or_else(|| {
        crate::palette::EMOJIS[crate::num::random_index(crate::palette::EMOJIS.len())].to_owned()
    });
    let colour = get("snek_colour").unwrap_or_else(|| {
        crate::palette::colour(crate::num::to_u32(js_sys::Math::random() * 360.0))
    });

    ProfileInfo {
        name: get("snek_name").unwrap_or_default(),
        emoji,
        colour,
    }
}

pub(crate) fn save_profile(p: &ProfileInfo) {
    if let Some(s) = storage() {
        let _ = s.set_item("snek_name", &p.name);
        let _ = s.set_item("snek_emoji", &p.emoji);
        let _ = s.set_item("snek_colour", &p.colour);
    }
}
