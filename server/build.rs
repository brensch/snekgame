//! The frontend is embedded into this binary at compile time (`rust_embed`
//! over `../web/dist`). Cargo cannot see that dependency by itself, so a
//! `trunk build` followed by `cargo build` would happily relink the *old*
//! wasm into the new server — a stale client against a bumped
//! `PROTOCOL_VERSION`, which looks like the whole site hanging on connect.
//! Declaring the directory makes the staleness impossible.
fn main() {
    println!("cargo:rerun-if-changed=../web/dist");
    if let Ok(entries) = std::fs::read_dir("../web/dist") {
        for entry in entries.flatten() {
            println!("cargo:rerun-if-changed={}", entry.path().display());
        }
    }
}
