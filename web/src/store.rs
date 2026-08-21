//! Global reactive state + the player's persistent identity (localStorage).

use leptos::prelude::*;
use snek_shared::protocol::{LobbyState, ProfileInfo};
use snek_shared::{Dir, GameConfig, PlayerId, PlayerProfile, Turn};

#[derive(Clone, Debug, PartialEq)]
pub enum Route {
    Home,
    Session(String),
    History,
    Replay(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnStatus {
    Idle,
    Connecting,
    Connected,
    Reconnecting,
}

/// The game currently on screen (live from the server).
#[derive(Clone, Debug, PartialEq)]
pub struct GameView {
    pub config: GameConfig,
    pub players: Vec<PlayerProfile>,
    pub turns: Vec<Turn>,
}

#[derive(Clone, Copy)]
pub struct Store {
    pub route: RwSignal<Route>,
    /// Editable profile fields; the public id comes back from the server.
    pub profile: RwSignal<ProfileInfo>,
    /// Our server-derived public id (empty until `Welcome` arrives).
    pub me: RwSignal<PlayerId>,
    pub show_profile_editor: RwSignal<bool>,
    pub conn: RwSignal<ConnStatus>,
    pub lobby: RwSignal<Option<LobbyState>>,
    pub game: RwSignal<Option<GameView>>,
    /// History scrubber position when reviewing a finished game (None = live).
    pub view_turn: RwSignal<Option<usize>>,
    /// The direction we're currently steering (highlights the d-pad).
    /// Starts at the clockwise default; corrected from each resolved turn.
    pub selected_dir: RwSignal<Option<Dir>>,
    /// Display name for a session we're about to create (sent with Join).
    pub pending_name: RwSignal<Option<String>>,
    pub saved_id: RwSignal<Option<i64>>,
    /// performance-now ms when the pre-game countdown ends.
    pub countdown_ends: RwSignal<f64>,
    /// Coarse clock for countdown display, ticked by an interval.
    pub now_ms: RwSignal<f64>,
    pub error: RwSignal<Option<String>>,
}

impl Store {
    pub fn new() -> Store {
        Store {
            route: RwSignal::new(Route::Home),
            profile: RwSignal::new(load_profile()),
            me: RwSignal::new(String::new()),
            show_profile_editor: RwSignal::new(false),
            conn: RwSignal::new(ConnStatus::Idle),
            lobby: RwSignal::new(None),
            game: RwSignal::new(None),
            view_turn: RwSignal::new(None),
            selected_dir: RwSignal::new(None),
            pending_name: RwSignal::new(None),
            saved_id: RwSignal::new(None),
            countdown_ends: RwSignal::new(0.0),
            now_ms: RwSignal::new(0.0),
            error: RwSignal::new(None),
        }
    }

    pub fn use_store() -> Store {
        expect_context::<Store>()
    }
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

/// The private secret proving our identity to the server. Never shown to or
/// shared with other players — they only ever see the hash the server derives.
pub fn secret() -> String {
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
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    let ok = web_sys::window()
        .and_then(|w| w.crypto().ok())
        .map(|c| c.get_random_values_with_u8_array(&mut buf).is_ok())
        .unwrap_or(false);
    if !ok {
        for b in buf.iter_mut() {
            *b = (js_sys::Math::random() * 256.0) as u8;
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_secret() -> String {
    random_hex(16)
}

/// Opaque id for a newly created session — this is what goes in the URL.
pub fn new_session_id() -> String {
    random_hex(8)
}

fn session_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.session_storage().ok()?
}

/// Remember a session's display name so a rejoin after the server tidied the
/// empty session away (phone lock, tab sleep) restores it instead of landing
/// in an "unnamed game".
pub fn remember_session_name(session_id: &str, name: &str) {
    if name.is_empty() || name == "unnamed game" {
        return;
    }
    if let Some(s) = session_storage() {
        let _ = s.set_item(&format!("snek_sname_{session_id}"), name);
    }
}

pub fn recall_session_name(session_id: &str) -> Option<String> {
    session_storage()?
        .get_item(&format!("snek_sname_{session_id}"))
        .ok()?
}

fn load_profile() -> ProfileInfo {
    let s = storage();
    let get = |key: &str| -> Option<String> { s.as_ref()?.get_item(key).ok()? };

    let random = || (js_sys::Math::random() * 1e9) as usize;
    let emoji = get("snek_emoji")
        .unwrap_or_else(|| crate::palette::EMOJIS[random() % crate::palette::EMOJIS.len()].to_string());
    let colour =
        get("snek_colour").unwrap_or_else(|| crate::palette::colour((random() % 360) as u32));

    ProfileInfo {
        name: get("snek_name").unwrap_or_default(),
        emoji,
        colour,
    }
}

pub fn save_profile(p: &ProfileInfo) {
    if let Some(s) = storage() {
        let _ = s.set_item("snek_name", &p.name);
        let _ = s.set_item("snek_emoji", &p.emoji);
        let _ = s.set_item("snek_colour", &p.colour);
    }
}
