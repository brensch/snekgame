//! What the connection has been doing, for when it goes wrong on somebody's
//! phone. Two pieces:
//!
//! - **The trail**: the last few connection events (attempts, failures,
//!   timeouts, closes, the page hiding and coming back, a crash), each
//!   stamped with seconds since the page loaded. It is kept in
//!   sessionStorage, so it outlives a reload of the tab, and goes to the
//!   server with every connect report: a page that got stuck and was
//!   reloaded tells the server what it was stuck doing.
//! - **The heartbeat** (`window.__snek`): whether this page wants a
//!   connection and when it last heard from the server. The watchdog in
//!   index.html reads it, so it can still rescue the page by reloading it
//!   when this code itself is wedged or has crashed.

use std::cell::RefCell;
use std::collections::VecDeque;
use wasm_bindgen::JsValue;

/// Events kept, across reloads.
const KEPT: usize = 30;
const STORAGE_KEY: &str = "snek-trail";

thread_local! {
    static TRAIL: RefCell<Option<VecDeque<String>>> = const { RefCell::new(None) };
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.session_storage().ok()?
}

/// Milliseconds since this page started loading.
pub(crate) fn page_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map_or(0.0, |p| p.now())
}

/// Note an event. The first one of a page loads whatever the page before it
/// (in this tab) left behind.
pub(crate) fn note(event: &str) {
    TRAIL.with(|t| {
        let mut t = t.borrow_mut();
        let trail = t.get_or_insert_with(|| {
            storage()
                .and_then(|s| s.get_item(STORAGE_KEY).ok().flatten())
                .map(|saved| saved.lines().map(str::to_owned).collect())
                .unwrap_or_default()
        });
        trail.push_back(format!("{:.1}s {event}", page_ms() / 1000.0));
        while trail.len() > KEPT {
            trail.pop_front();
        }
        if let Some(s) = storage() {
            let joined: Vec<&str> = trail.iter().map(String::as_str).collect();
            let _ = s.set_item(STORAGE_KEY, &joined.join("\n"));
        }
    });
}

/// The trail so far, oldest first.
pub(crate) fn events() -> Vec<String> {
    TRAIL.with(|t| {
        t.borrow()
            .as_ref()
            .map(|t| t.iter().cloned().collect())
            .unwrap_or_default()
    })
}

/// Tell the watchdog whether this page wants a connection right now.
pub(crate) fn want_connection(want: bool) {
    set_heartbeat("want", &JsValue::from_bool(want));
}

/// Tell the watchdog the server was just heard from.
pub(crate) fn heard_from_server() {
    set_heartbeat("lastRx", &JsValue::from_f64(js_sys::Date::now()));
}

fn set_heartbeat(field: &str, value: &JsValue) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let key = JsValue::from_str("__snek");
    let beat = match js_sys::Reflect::get(&window, &key) {
        Ok(b) if b.is_object() => b,
        _ => {
            let b: JsValue = js_sys::Object::new().into();
            let _ = js_sys::Reflect::set(&window, &key, &b);
            b
        }
    };
    let _ = js_sys::Reflect::set(&beat, &JsValue::from_str(field), value);
}
