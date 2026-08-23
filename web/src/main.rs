mod app;
mod board;
mod botworker;
mod compute_page;
mod games_page;
mod history_page;
mod net;
mod palette;
mod session_page;
mod store;

use leptos::prelude::*;

fn main() {
    console_error_panic_hook::set_once();
    // The bot worker boots this same bundle to reach `snek_bot_compute`.
    // There is no DOM on that thread, so the UI must not mount there.
    if web_sys::window().is_none() {
        return;
    }
    mount_to_body(app::app);
}
