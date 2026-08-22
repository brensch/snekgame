pub mod db;
pub mod session;
pub mod wt;

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

/// Build the HTTP app (static frontend + read-only APIs) and the shared
/// session registry that the WebTransport endpoint plugs into.
pub fn build(db_path: &Path) -> (Router, Registry) {
    let db = Arc::new(Db::open(db_path).expect("failed to open sqlite db"));
    let registry = Registry::new(db.clone());
    let state = AppState {
        registry: registry.clone(),
        db,
    };
    let router = Router::new()
        .route("/api/sessions", get(session_list))
        .route("/api/history", get(history_list))
        .route("/api/history/{id}", get(history_game))
        .route("/api/history/{id}/meta", get(history_meta))
        .fallback(static_handler)
        .with_state(state);
    (router, registry)
}

/// Active games (someone in the lobby or playing right now).
async fn session_list(State(state): State<AppState>) -> Response {
    Json(state.registry.active_sessions()).into_response()
}

async fn history_list(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let before = q.get("before").and_then(|v| v.parse::<i64>().ok());
    let limit = q
        .get("limit")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(10);
    let db = state.db.clone();
    match tokio::task::spawn_blocking(move || db.list_games(before, limit)).await {
        Ok(Ok(page)) => Json(page).into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Who/where/when for one stored game (JSON) — the replay page shows this
/// alongside the postcard turn data.
async fn history_meta(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<i64>,
) -> Response {
    let db = state.db.clone();
    match tokio::task::spawn_blocking(move || db.get_game_meta(id)).await {
        Ok(Ok(Some(meta))) => Json(meta).into_response(),
        Ok(Ok(None)) => StatusCode::NOT_FOUND.into_response(),
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
