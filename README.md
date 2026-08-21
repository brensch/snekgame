# snekgame

A rebuild of [tac](https://github.com/brensch/tac)'s **snek** variant, all in
Rust: one shared crate holds the game engine and wire types, the server runs
them natively, and the frontend runs the very same primitives compiled to wasm.

- **shared/** — game types, deterministic snek engine, postcard wire protocol
- **server/** — axum: websocket sessions, real-time tick loop, sqlite history,
  serves the embedded frontend (single-binary deploy)
- **web/** — Leptos CSR frontend built with trunk

## How it plays

Same concepts as tac's snek: pick a session word, share it with friends, ready
up, and steer your snek on a shared board. Real-time ticks (configurable
0.5–2s); missed ticks continue your last direction. Eat 🎃 to grow and refill
health, avoid walls/bodies/bigger heads, outlive everyone. Finished games are
written to sqlite and replayable from the history page.

## Architecture notes

- **Interface**: a single websocket per client carrying postcard-encoded
  binary frames of shared Rust enums (`ClientMsg`/`ServerMsg`). No JSON, no
  schema drift — both ends compile the same types.
- **Connection model**: one actor task per session owns all state; sockets are
  dumb pipes to it. Clients auto-reconnect with backoff and are re-sent full
  snapshots on join, so no incremental state survives a gap. Slow clients are
  dropped rather than ever blocking the tick loop.
- **Security**: your identity is a client secret that never leaves the server;
  everyone else sees only its hash. Moves are bound server-side to the
  connection that sent them, and pending moves are never broadcast — you learn
  other players' moves only in the resolved turn.
- **Storage**: games live in server memory while running; on completion the
  full `GameRecord` (players, seed, every turn) is written to sqlite. The
  engine is deterministic given seed + moves, so records can be re-simulated.

## Dev

```
cargo test                                   # engine + e2e (ws clients, sqlite)
cd web && trunk build --release              # build frontend into web/dist
cargo run -p snek-server                     # serves embedded dist on :8080
```

Env: `SNEK_PORT` (default 8080), `SNEK_DB` (default ./snek.db).
