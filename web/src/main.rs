//! The snek frontend: a Leptos app compiled to wasm. It talks to the server
//! over WebTransport, draws the board as SVG, and — when the server asks —
//! thinks for the bots in Web Workers that boot this same bundle.
#![expect(
    clippy::redundant_pub_crate,
    reason = "a binary crate: rustc's unreachable_pub wants `pub(crate)` on \
              items shared between modules, and this lint wants the opposite; \
              `pub(crate)` says what is true"
)]
#![expect(
    clippy::future_not_send,
    reason = "wasm has one thread and every future here runs on it through \
              spawn_local; they hold JS handles, which are never Send"
)]

mod app;
mod board;
mod botworker;
mod compute_page;
mod games_page;
mod history_page;
mod net;
mod notices;
mod num;
mod palette;
mod rules;
mod session_page;
mod store;
mod trail;

use leptos::prelude::*;

fn main() {
    // A panic stops this page's code dead while the screen stays as it was,
    // so it goes on the connection trail too (the watchdog in index.html
    // reloads the page, and the trail tells the server why).
    std::panic::set_hook(Box::new(|info| {
        trail::note(&format!("crashed: {info}"));
        console_error_panic_hook::hook(info);
    }));
    // The bot worker boots this same bundle to reach `snek_bot_compute`.
    // There is no DOM on that thread, so the UI must not mount there.
    if web_sys::window().is_none() {
        return;
    }
    mount_to_body(app::app);
}
