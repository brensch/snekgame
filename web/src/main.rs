mod app;
mod board;
mod history_page;
mod net;
mod palette;
mod session_page;
mod store;

use leptos::prelude::*;

fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(app::app);
}
