pub mod db;
pub mod session;
pub mod wt;

use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use db::Db;
use session::Registry;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

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
    // Compress the frontend now, off the async runtime, so the first visitor
    // doesn't wait for it.
    std::thread::spawn(brotli_assets);
    let db = Arc::new(Db::open(db_path).expect("failed to open sqlite db"));
    let registry = Registry::new(db.clone());
    let state = AppState {
        registry: registry.clone(),
        db,
    };
    let router = Router::new()
        .route("/api/sessions", get(session_list))
        .route("/api/compute", get(compute_overview))
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

/// All-time client-compute statistics.
async fn compute_overview(State(state): State<AppState>) -> Response {
    let db = state.db.clone();
    match tokio::task::spawn_blocking(move || db.compute_overview()).await {
        Ok(Ok(overview)) => Json(overview).into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Who/where/when for one stored game (JSON) — the replay page shows this
/// alongside the postcard turn data.
async fn history_meta(State(state): State<AppState>, AxumPath(id): AxumPath<i64>) -> Response {
    let db = state.db.clone();
    match tokio::task::spawn_blocking(move || db.get_game_meta(id)).await {
        Ok(Ok(Some(meta))) => Json(meta).into_response(),
        Ok(Ok(None)) => StatusCode::NOT_FOUND.into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Replay data as a postcard blob — the frontend decodes it with the same
/// shared types the engine produced it with.
async fn history_game(State(state): State<AppState>, AxumPath(id): AxumPath<i64>) -> Response {
    let db = state.db.clone();
    match tokio::task::spawn_blocking(move || db.get_game_blob(id)).await {
        Ok(Ok(Some(blob))) => {
            ([(header::CONTENT_TYPE, "application/octet-stream")], blob).into_response()
        }
        Ok(Ok(None)) => StatusCode::NOT_FOUND.into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// The shell must be revalidated every load; everything else it names is
/// content-hashed and can be kept forever. Served without either header, a
/// phone will hold the shell on heuristic caching alone, keep loading an
/// old bundle from cache, and be turned away by the version check on every
/// join — which looks like the site hanging rather than a stale page.
const SHELL_CACHE: &str = "no-cache";
const HASHED_CACHE: &str = "public, max-age=31536000, immutable";

fn cache_policy(path: &str) -> &'static str {
    // trunk stamps a content hash into every asset filename it emits.
    if path.ends_with(".html") {
        SHELL_CACHE
    } else {
        HASHED_CACHE
    }
}

/// Text-like assets worth compressing. woff2 and images are compressed
/// already.
fn compressible(path: &str) -> bool {
    [".wasm", ".js", ".css", ".html", ".svg", ".json", ".txt"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

/// Brotli copies of the compressible assets at the highest quality, made
/// once when the server starts (see `build`) rather than per request: the
/// files never change while it runs, and quality 11 is far smaller than the
/// fast setting a proxy uses on the fly — but too slow to do per request.
/// Release builds only; in debug builds rust-embed reads the files from
/// disk, so a cache would go stale with every `trunk build`.
fn brotli_assets() -> &'static HashMap<String, Vec<u8>> {
    static BROTLI: OnceLock<HashMap<String, Vec<u8>>> = OnceLock::new();
    BROTLI.get_or_init(|| {
        if cfg!(debug_assertions) {
            return HashMap::new();
        }
        let params = brotli::enc::BrotliEncoderParams {
            quality: 11,
            lgwin: 24,
            ..Default::default()
        };
        Assets::iter()
            .filter(|path| compressible(path))
            .filter_map(|path| {
                let file = Assets::get(&path)?;
                let mut out = Vec::new();
                brotli::BrotliCompress(&mut &file.data[..], &mut out, &params).ok()?;
                Some((path.into_owned(), out))
            })
            .collect()
    })
}

fn accepts_brotli(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|enc| enc.split(';').next().map(str::trim) == Some("br"))
}

/// One embedded file, Brotli-encoded when the client takes it.
fn asset_response(path: &str, mime: &str, cache: &'static str, headers: &HeaderMap) -> Response {
    let Some(file) = Assets::get(path) else {
        return (
            StatusCode::NOT_FOUND,
            "frontend not built — run `trunk build` in web/",
        )
            .into_response();
    };
    let base = [
        (header::CONTENT_TYPE, mime.to_string()),
        (header::CACHE_CONTROL, cache.to_string()),
        (header::VARY, "accept-encoding".to_string()),
    ];
    match brotli_assets().get(path) {
        Some(br) if accepts_brotli(headers) => {
            (base, [(header::CONTENT_ENCODING, "br")], br.clone()).into_response()
        }
        _ => (base, file.data.into_owned()).into_response(),
    }
}

async fn static_handler(uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if Assets::get(path).is_some() {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        asset_response(path, mime.as_ref(), cache_policy(path), &headers)
    } else {
        // Unknown paths fall back to the app shell (hash routing).
        asset_response("index.html", "text/html", SHELL_CACHE, &headers)
    }
}
