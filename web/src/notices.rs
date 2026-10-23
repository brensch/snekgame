//! Toasts: short notes that stack at the bottom of the screen and fade on
//! their own. Each one stays for [`TOAST_MS`], the newest nearest the edge.
//!
//! Hand-rolled rather than a library: the Leptos toast crates either draw
//! their own look in inline styles or no longer build against current
//! web-sys, and all this needs is a list, a timer and a CSS animation.

use crate::session_page::{BOARD_SIZES, speed_name};
use crate::store::Store;
use gloo_timers::callback::Timeout;
use leptos::prelude::*;
use snek_shared::GameConfig;
use snek_shared::protocol::LobbyState;
use std::cell::Cell;

/// How long each toast is on screen, fade included. styles.css times the
/// fade to match.
const TOAST_MS: u32 = 3_000;

/// At most this many toasts at once: a new one pushes the oldest off, so a
/// burst of changes never buries the page.
const MAX_TOASTS: usize = 3;

/// One note: who it's about, in their colour, and what to say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Toast {
    id: u64,
    who: String,
    colour: String,
    text: String,
}

thread_local! {
    static NEXT_ID: Cell<u64> = const { Cell::new(0) };
}

/// Put a toast up, and take it down again after [`TOAST_MS`].
fn raise(store: Store, who: String, colour: String, text: String) {
    let id = NEXT_ID.with(|n| {
        let id = n.get();
        n.set(id + 1);
        id
    });
    store.toasts.update(|t| {
        t.insert(
            0,
            Toast {
                id,
                who,
                colour,
                text,
            },
        );
        t.truncate(MAX_TOASTS);
    });
    Timeout::new(TOAST_MS, move || {
        store.toasts.update(|t| t.retain(|toast| toast.id != id));
    })
    .forget();
}

/// The stack of toasts, pinned to the bottom of the screen. Keyed by id, so a
/// toast already showing keeps its element (and its place in its fade) when
/// another arrives.
pub(crate) fn toaster(store: Store) -> impl IntoView {
    view! {
        <ol class="toasts" aria-live="polite">
            <For
                each=move || store.toasts.get()
                key=|toast| toast.id
                children=|toast| {
                    view! {
                        <li class="snek-toast">
                            <span class="who" style=format!("background: {};", toast.colour)>
                                {toast.who}
                            </span>
                            <span>{toast.text}</span>
                        </li>
                    }
                }
            />
        </ol>
    }
}

/// Someone else changed the lobby's settings: say who, and what to. Nothing
/// is stored about it; what changed is read off the old and new settings.
/// Our own changes need no telling.
pub(crate) fn settings_changed(store: Store, old: &LobbyState, new: &LobbyState) {
    let Some(by) = &new.config_changed_by else {
        return;
    };
    if *by == store.me.get_untracked() {
        return;
    }
    let (who, colour) = new.players.iter().find(|p| &p.id == by).map_or_else(
        || ("someone who left".to_owned(), "#fff".to_owned()),
        |p| (format!("{} {}", p.emoji, p.name), p.colour.clone()),
    );
    let text = if old.name == new.name {
        format!("changed {}", changes(&old.config, &new.config).join(", "))
    } else {
        format!("renamed the game to {}", new.name)
    };
    raise(store, who, colour, text);
}

/// We were removed from a lobby, by `by` if we know who.
pub(crate) fn removed(store: Store, by: Option<&snek_shared::PlayerProfile>) {
    let (who, colour) = by.map_or_else(
        || ("someone".to_owned(), "#fff".to_owned()),
        |p| (format!("{} {}", p.emoji, p.name), p.colour.clone()),
    );
    raise(store, who, colour, "removed you from the lobby".to_owned());
}

/// Each setting that differs, as it now reads ("speed to Fast").
fn changes(old: &GameConfig, new: &GameConfig) -> Vec<String> {
    let mut out = Vec::new();
    if old.board_width != new.board_width {
        out.push(format!(
            "board to {}",
            label(BOARD_SIZES, &new.board_width, &new.board_width.to_string())
        ));
    }
    if speed_name(old) != speed_name(new) {
        out.push(format!("speed to {}", speed_name(new)));
    }
    if old.food_emoji != new.food_emoji {
        out.push(format!("food to {}", new.food_emoji));
    }
    if out.is_empty() {
        // Only the height can differ alone, and nothing in the UI sets it
        // apart from the width; say something rather than nothing.
        out.push("the settings".to_owned());
    }
    out
}

/// The lobby's name for a setting's value, or `fallback` if it has none.
fn label<T: PartialEq>(options: &[(&str, T)], value: &T, fallback: &str) -> String {
    options
        .iter()
        .find(|(_, v)| v == value)
        .map_or_else(|| fallback.to_owned(), |(name, _)| (*name).to_owned())
}
