pub mod db;
pub mod session;
pub mod ws;

use axum::extract::{Path as AxumPath, State};
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use db::Db;
use session::Registry;
use std::path::Path;
use std::sync::Arc;

/// Frontend build output, embedded so deployment is a single binary.
/// (In debug builds rust-embed reads from disk, so `trunk build` output is
/// picked up without recompiling the server.)
#[derive(rust_embed::Embed)]
#[folder = "../web/dist"]
struct Assets;

#[derive(Clone)]
pub struct AppState {
    pub registry: Registry,
    pub db: Arc<Db>,
}

pub fn build_app(db_path: &Path) -> Router {
    let db = Arc::new(Db::open(db_path).expect("failed to open sqlite db"));
    let state = AppState {
        registry: Registry::new(db.clone()),
        db,
    };
    Router::new()
        .route("/ws", get(ws_route))
        .route("/ws/games", get(games_watch))
        .route("/api/sessions", get(session_list))
        .route("/api/history", get(history_list))
        .route("/api/history/{id}", get(history_game))
        .fallback(static_handler)
        .with_state(state)
}

/// Active games (someone in the lobby or playing right now).
async fn session_list(State(state): State<AppState>) -> Response {
    Json(state.registry.active_sessions()).into_response()
}

/// Push-updated active-games list: sends the current list on connect and a
/// fresh postcard frame every time it changes.
async fn games_watch(
    ws: axum::extract::ws::WebSocketUpgrade,
    State(state): State<AppState>,
) -> Response {
    use axum::extract::ws::Message;
    ws.on_upgrade(move |mut sock| async move {
        let mut rx = state.registry.watch_games();
        loop {
            let list = rx.borrow_and_update().clone();
            let frame = postcard::to_allocvec(&list).unwrap();
            if sock.send(Message::Binary(frame.into())).await.is_err() {
                break;
            }
            tokio::select! {
                changed = rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                msg = sock.recv() => match msg {
                    None | Some(Err(_)) => break,
                    Some(Ok(Message::Close(_))) => break,
                    _ => {}
                },
            }
        }
    })
}

async fn ws_route(
    ws: axum::extract::ws::WebSocketUpgrade,
    State(state): State<AppState>,
) -> Response {
    ws::ws_handler(ws, State(state.registry)).await
}

async fn history_list(State(state): State<AppState>) -> Response {
    let db = state.db.clone();
    match tokio::task::spawn_blocking(move || db.list_games(100)).await {
        Ok(Ok(list)) => Json(list).into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Replay data as a postcard blob — the frontend decodes it with the same
/// shared types the engine produced it with.
async fn history_game(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<i64>,
) -> Response {
    let db = state.db.clone();
    match tokio::task::spawn_blocking(move || db.get_game_blob(id)).await {
        Ok(Ok(Some(blob))) => (
            [(header::CONTENT_TYPE, "application/octet-stream")],
            blob,
        )
            .into_response(),
        Ok(Ok(None)) => StatusCode::NOT_FOUND.into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match Assets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, mime.as_ref())],
                file.data.into_owned(),
            )
                .into_response()
        }
        // Unknown paths fall back to the app shell (hash routing).
        None => match Assets::get("index.html") {
            Some(file) => (
                [(header::CONTENT_TYPE, "text/html")],
                file.data.into_owned(),
            )
                .into_response(),
            None => (
                StatusCode::NOT_FOUND,
                "frontend not built — run `trunk build` in web/",
            )
                .into_response(),
        },
    }
}
