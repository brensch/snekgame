//! Global reactive state + the player's persistent identity (localStorage).

use leptos::prelude::*;
use snek_shared::protocol::{LobbyState, ProfileInfo};
use snek_shared::{GameConfig, PlayerId, PlayerProfile, Turn};

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
        let secret = web_sys::window()
            .map(|w| w.crypto().unwrap().random_uuid())
            .unwrap_or_else(|| format!("anon-{}", js_sys::Math::random()));
        if let Some(s) = storage() {
            let _ = s.set_item("snek_secret", &secret);
        }
        secret
    })
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
