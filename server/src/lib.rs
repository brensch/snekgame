//! The snek server: the embedded web frontend and read-only history APIs
//! over HTTP, and the games themselves over WebTransport — one actor per
//! session, finished games kept in `SQLite`.

pub mod db;
pub mod session;
pub mod wt;

// Public only so its helpers can be shared between the modules' tests
// without lint gymnastics; it exists in test builds alone.
#[cfg(test)]
pub mod testing;

use anyhow::Context;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use db::Db;
use session::Registry;
use std::borrow::Cow;
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use wtransport::endpoint::endpoint_side;
use wtransport::{Endpoint, Identity, VarInt};

/// Frontend build output, embedded so deployment is a single binary.
/// (In debug builds rust-embed reads from disk, so `trunk build` output is
/// picked up without recompiling the server.)
#[derive(rust_embed::Embed)]
#[folder = "../web/dist"]
struct Assets;

/// How the server is set up: where it listens, where history is kept, and
/// what WebTransport presents as its TLS identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// HTTP: the frontend and the read-only APIs. 0 picks a free port.
    pub http_port: u16,
    /// WebTransport (UDP): the games. 0 picks a free port.
    pub wt_port: u16,
    /// The `SQLite` history database, created if missing.
    pub db: PathBuf,
    /// The certificate and key, or `None` for a self-signed identity (local
    /// dev), which also serves the browser its hash at `/api/dev/wt`.
    pub tls: Option<TlsFiles>,
}

/// PEM files for the WebTransport TLS identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsFiles {
    /// The certificate chain.
    pub cert: PathBuf,
    /// Its private key.
    pub key: PathBuf,
}

impl Config {
    /// The configuration that `var` describes, `var` being the environment
    /// in the binary: `SNEK_PORT` (default 8080), `SNEK_WT_PORT` (4433),
    /// `SNEK_DB` (`snek.db`), and `SNEK_CERT` with `SNEK_KEY` (both, or a
    /// self-signed identity). A port that doesn't parse gets its default.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Self {
        let port = |name, default| var(name).and_then(|p| p.parse().ok()).unwrap_or(default);
        Self {
            http_port: port("SNEK_PORT", 8080),
            wt_port: port("SNEK_WT_PORT", 4433),
            db: PathBuf::from(var("SNEK_DB").unwrap_or_else(|| "snek.db".to_owned())),
            tls: var("SNEK_CERT")
                .zip(var("SNEK_KEY"))
                .map(|(cert, key)| TlsFiles {
                    cert: cert.into(),
                    key: key.into(),
                }),
        }
    }
}

/// A server with its database open and both ports bound, ready to serve.
pub struct Server {
    app: Router,
    registry: Registry,
    listener: tokio::net::TcpListener,
    endpoint: Endpoint<endpoint_side::Server>,
    http_addr: SocketAddr,
    wt_port: u16,
}

impl Server {
    /// Open the database, load the TLS identity and bind both ports.
    ///
    /// # Errors
    ///
    /// Fails if any of those can't be done.
    pub async fn bind(config: &Config) -> anyhow::Result<Self> {
        let (app, registry) = build(&config.db)
            .with_context(|| format!("failed to open the database at {}", config.db.display()))?;

        // WebTransport (QUIC) — the game transport.
        let identity = wt::load_identity(config.tls.as_ref())
            .await
            .context("failed to load TLS identity")?;
        let endpoint =
            wt::build_endpoint(config.wt_port, &identity).context("failed to bind WT endpoint")?;
        let wt_port = endpoint.local_addr()?.port();

        // Local dev only: with no real certificate the identity is
        // self-signed, which a browser will accept only if told its hash up
        // front. A page served from localhost asks for it here (see
        // wt_target in net.rs). A server with real certs never registers
        // the route.
        let app = if config.tls.is_none() {
            app.route("/api/dev/wt", get(dev_wt(wt_port, &identity)?))
        } else {
            app
        };

        // HTTP — static frontend + read-only APIs (behind Caddy).
        let addr = SocketAddr::from(([0, 0, 0, 0], config.http_port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("failed to bind http on {addr}"))?;
        let http_addr = listener.local_addr()?;
        Ok(Self {
            app,
            registry,
            listener,
            endpoint,
            http_addr,
            wt_port,
        })
    }

    /// Where HTTP is listening.
    #[must_use]
    pub const fn http_addr(&self) -> SocketAddr {
        self.http_addr
    }

    /// The UDP port WebTransport is listening on.
    #[must_use]
    pub const fn wt_port(&self) -> u16 {
        self.wt_port
    }

    /// Serve until `shutdown` resolves, then stop: HTTP finishes the
    /// requests it has, and every WebTransport client is told the server
    /// is going rather than left to time out. Games still being played are
    /// lost, as they always were when the process stopped.
    ///
    /// # Errors
    ///
    /// Fails if the HTTP server does.
    pub async fn serve(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> anyhow::Result<()> {
        tracing::info!("webtransport listening on udp {}", self.wt_port);
        tracing::info!("snek listening on http://{}", self.http_addr);
        let endpoint = Arc::new(self.endpoint);
        let accept = tokio::spawn(wt::serve(Arc::clone(&endpoint), self.registry));
        axum::serve(self.listener, self.app)
            .with_graceful_shutdown(shutdown)
            .await
            .context("http server failed")?;
        accept.abort();
        endpoint.close(VarInt::from_u32(0), b"server shutting down");
        endpoint.wait_idle().await;
        tracing::info!("snek stopped");
        Ok(())
    }
}

/// The handler for `/api/dev/wt`: where WebTransport is and the hash of its
/// self-signed certificate.
fn dev_wt(
    wt_port: u16,
    identity: &Identity,
) -> anyhow::Result<impl Fn() -> std::future::Ready<Json<serde_json::Value>> + Clone + use<>> {
    let cert_hash: Vec<u8> = identity
        .certificate_chain()
        .as_slice()
        .first()
        .context("self-signed identity has no certificate")?
        .hash()
        .as_ref()
        .to_vec();
    let body = serde_json::json!({ "port": wt_port, "certHash": cert_hash });
    Ok(move || std::future::ready(Json(body.clone())))
}

/// Run the server described by `config` until `shutdown` resolves.
///
/// # Errors
///
/// Fails if the server can't be set up (see [`Server::bind`]) or the HTTP
/// server fails.
pub async fn run(
    config: &Config,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    Server::bind(config).await?.serve(shutdown).await
}

/// Resolves when the process is asked to stop: SIGTERM, which is how
/// systemd stops the service, or ctrl-c. Both are hooked before this
/// returns, so a signal that arrives early isn't missed.
///
/// # Errors
///
/// Fails if the signal handlers can't be installed.
pub fn shutdown_signal() -> std::io::Result<impl Future<Output = ()> + Send + 'static> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = terminate.recv() => tracing::info!("SIGTERM — shutting down"),
            _ = interrupt.recv() => tracing::info!("interrupted — shutting down"),
        }
    })
}

/// What every HTTP handler can reach.
#[derive(Clone)]
struct AppState {
    registry: Registry,
    db: Arc<Db>,
    frontend: Arc<Frontend>,
}

/// Lock a mutex, carrying on past poisoning. A panic elsewhere while it was
/// held leaves nothing half-done behind it here: the registry maps are only
/// ever changed by single inserts and removes, and the database rolls back a
/// statement that didn't finish. Refusing every later lock would turn one
/// panicked task into a dead server.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Run a database call on a blocking thread, off the async runtime. A call
/// that never finished (its thread panicked, or the runtime is shutting
/// down under it) is as much a failure as one that returned an error.
pub(crate) async fn blocking<T: Send + 'static, E: Into<anyhow::Error> + Send + 'static>(
    call: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(call).await?.map_err(Into::into)
}

/// Build the HTTP app (static frontend + read-only APIs) and the shared
/// session registry that the WebTransport endpoint plugs into.
///
/// # Errors
///
/// Fails if the history database at `db_path` can't be opened.
pub fn build(db_path: &Path) -> anyhow::Result<(Router, Registry)> {
    let db = Arc::new(Db::open(db_path)?);
    // Precompressed in release builds only: in debug builds rust-embed
    // reads the files from disk, so a cache would go stale with every
    // `trunk build`.
    let frontend = Arc::new(Frontend::new::<Assets>(!cfg!(debug_assertions)));
    Ok(app(db, frontend))
}

fn app(db: Arc<Db>, frontend: Arc<Frontend>) -> (Router, Registry) {
    // Compress the frontend now, off the async runtime, so the first visitor
    // doesn't wait for it.
    let warm = Arc::clone(&frontend);
    std::thread::spawn(move || warm.brotli().len());
    let registry = Registry::new(Arc::clone(&db));
    let state = AppState {
        registry: registry.clone(),
        db,
        frontend,
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

fn server_error() -> Response {
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

/// Active games (someone in the lobby or playing right now).
async fn session_list(State(state): State<AppState>) -> Response {
    Json(state.registry.active_sessions()).into_response()
}

async fn history_list(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let before = q.get("before").and_then(|v| v.parse::<i64>().ok());
    let limit = q
        .get("limit")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(10);
    let db = Arc::clone(&state.db);
    blocking(move || db.list_games(before, limit))
        .await
        .map_or_else(|_| server_error(), |page| Json(page).into_response())
}

/// All-time client-compute statistics.
async fn compute_overview(State(state): State<AppState>) -> Response {
    let db = Arc::clone(&state.db);
    blocking(move || db.compute_overview()).await.map_or_else(
        |_| server_error(),
        |overview| Json(overview).into_response(),
    )
}

/// Who/where/when for one stored game (JSON) — the replay page shows this
/// alongside the postcard turn data.
async fn history_meta(State(state): State<AppState>, AxumPath(id): AxumPath<i64>) -> Response {
    let db = Arc::clone(&state.db);
    match blocking(move || db.get_game_meta(id)).await {
        Ok(Some(meta)) => Json(meta).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => server_error(),
    }
}

/// Replay data as a postcard blob — the frontend decodes it with the same
/// shared types the engine produced it with.
async fn history_game(State(state): State<AppState>, AxumPath(id): AxumPath<i64>) -> Response {
    let db = Arc::clone(&state.db);
    match blocking(move || db.get_game_blob(id)).await {
        Ok(Some(blob)) => {
            ([(header::CONTENT_TYPE, "application/octet-stream")], blob).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => server_error(),
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
    if Path::new(path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("html"))
    {
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

/// The frontend's files, and Brotli copies of the ones worth compressing.
struct Frontend {
    get: fn(&str) -> Option<rust_embed::EmbeddedFile>,
    paths: fn() -> Vec<String>,
    /// Whether to make the Brotli copies at all.
    precompress: bool,
    brotli: OnceLock<HashMap<String, Vec<u8>>>,
}

impl Frontend {
    /// The files embedded as `E`.
    fn new<E: rust_embed::Embed>(precompress: bool) -> Self {
        Self {
            get: E::get,
            paths: || E::iter().map(Cow::into_owned).collect(),
            precompress,
            brotli: OnceLock::new(),
        }
    }

    /// Brotli copies of the compressible files at the highest quality, made
    /// once (see `app`) rather than per request: the files never change
    /// while the server runs, and quality 11 is far smaller than the fast
    /// setting a proxy uses on the fly — but too slow to do per request.
    fn brotli(&self) -> &HashMap<String, Vec<u8>> {
        self.brotli.get_or_init(|| {
            if !self.precompress {
                return HashMap::new();
            }
            let params = brotli::enc::BrotliEncoderParams {
                quality: 11,
                lgwin: 24,
                ..Default::default()
            };
            (self.paths)()
                .into_iter()
                .filter(|path| compressible(path))
                .filter_map(|path| {
                    let file = (self.get)(&path)?;
                    let mut out = Vec::new();
                    brotli::BrotliCompress(&mut &file.data[..], &mut out, &params).ok()?;
                    Some((path, out))
                })
                .collect()
        })
    }

    /// One file, Brotli-encoded when the client takes it.
    fn response(
        &self,
        path: &str,
        mime: &str,
        cache: &'static str,
        headers: &HeaderMap,
    ) -> Response {
        let Some(file) = (self.get)(path) else {
            return (
                StatusCode::NOT_FOUND,
                "frontend not built — run `trunk build` in web/",
            )
                .into_response();
        };
        let base = [
            (header::CONTENT_TYPE, mime.to_owned()),
            (header::CACHE_CONTROL, cache.to_owned()),
            (header::VARY, "accept-encoding".to_owned()),
        ];
        match self.brotli().get(path) {
            Some(br) if accepts_brotli(headers) => {
                (base, [(header::CONTENT_ENCODING, "br")], br.clone()).into_response()
            }
            _ => (base, file.data.into_owned()).into_response(),
        }
    }
}

fn accepts_brotli(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|enc| enc.split(';').next().map(str::trim) == Some("br"))
}

async fn static_handler(State(state): State<AppState>, uri: Uri, headers: HeaderMap) -> Response {
    let frontend = &state.frontend;
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if (frontend.get)(path).is_some() {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        frontend.response(path, mime.as_ref(), cache_policy(path), &headers)
    } else {
        // Unknown paths fall back to the app shell (hash routing).
        frontend.response("index.html", "text/html", SHELL_CACHE, &headers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{finished_game, profile, temp_path};
    use axum::body::Body;
    use axum::http::Request;
    use snek_shared::GameRecord;
    use snek_shared::protocol::{ComputeOverview, ComputeStat, HistoryEntry, HistoryPage};
    use tower::ServiceExt;

    /// A small frontend standing in for `trunk build` output: the shell, a
    /// hashed script and an already-compressed font.
    #[derive(rust_embed::Embed)]
    #[folder = "tests/fixtures/web"]
    struct Fixture;

    fn test_app(precompress: bool) -> (Router, Arc<Db>, PathBuf) {
        let path = temp_path("http.db");
        let db = Arc::new(Db::open(&path).unwrap());
        let frontend = Arc::new(Frontend::new::<Fixture>(precompress));
        let (router, _) = app(Arc::clone(&db), frontend);
        (router, db, path)
    }

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    impl Reply {
        fn header(&self, name: header::HeaderName) -> Option<&str> {
            self.headers.get(name).map(|v| v.to_str().unwrap())
        }

        fn json<T: serde::de::DeserializeOwned>(&self) -> T {
            assert_eq!(self.status, StatusCode::OK);
            serde_json::from_slice(&self.body).unwrap()
        }
    }

    async fn get_with(router: &Router, uri: &str, accept_encoding: Option<&str>) -> Reply {
        let mut request = Request::get(uri);
        if let Some(enc) = accept_encoding {
            request = request.header(header::ACCEPT_ENCODING, enc);
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        Reply {
            status,
            headers,
            body,
        }
    }

    async fn get(router: &Router, uri: &str) -> Reply {
        get_with(router, uri, None).await
    }

    fn unbrotli(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        brotli::BrotliDecompress(&mut &bytes[..], &mut out).unwrap();
        out
    }

    #[test]
    fn config_comes_from_the_environment_with_defaults() {
        let vars: HashMap<&str, &str> = HashMap::new();
        assert_eq!(
            Config::from_vars(|k| vars.get(k).map(|v| (*v).to_owned())),
            Config {
                http_port: 8080,
                wt_port: 4433,
                db: "snek.db".into(),
                tls: None,
            }
        );

        // What deploy/snekgame.service sets.
        let vars = HashMap::from([
            ("SNEK_PORT", "8081"),
            ("SNEK_WT_PORT", "443"),
            ("SNEK_DB", "/opt/snekgame/snek.db"),
            ("SNEK_CERT", "/opt/snekgame/certs/cert.pem"),
            ("SNEK_KEY", "/opt/snekgame/certs/key.pem"),
        ]);
        assert_eq!(
            Config::from_vars(|k| vars.get(k).map(|v| (*v).to_owned())),
            Config {
                http_port: 8081,
                wt_port: 443,
                db: "/opt/snekgame/snek.db".into(),
                tls: Some(TlsFiles {
                    cert: "/opt/snekgame/certs/cert.pem".into(),
                    key: "/opt/snekgame/certs/key.pem".into(),
                }),
            }
        );

        // A port that doesn't parse gets its default; a cert without its key
        // is no identity at all.
        let vars = HashMap::from([("SNEK_PORT", "eighty"), ("SNEK_CERT", "cert.pem")]);
        let config = Config::from_vars(|k| vars.get(k).map(|v| (*v).to_owned()));
        assert_eq!(config.http_port, 8080);
        assert_eq!(config.tls, None);
    }

    #[tokio::test]
    async fn history_lists_pages_and_serves_each_game() {
        let (router, db, _) = test_app(false);
        let record = finished_game(&[profile("a1", "alice"), profile("b2", "bob")]);
        let ids: Vec<i64> = (0..3)
            .map(|i| db.insert_game(i, i, &record).unwrap())
            .collect();

        // Ten to a page by default.
        let page: HistoryPage = get(&router, "/api/history").await.json();
        assert_eq!(page.entries.len(), 3);
        assert_eq!(page.next, None);

        let page: HistoryPage = get(&router, "/api/history?limit=2").await.json();
        assert_eq!(page.entries[0].id, ids[2], "newest first");
        assert_eq!(page.next, Some(ids[1]));
        let uri = format!("/api/history?limit=2&before={}", ids[1]);
        let page: HistoryPage = get(&router, &uri).await.json();
        let got: Vec<i64> = page.entries.iter().map(|e| e.id).collect();
        assert_eq!(got, [ids[0]]);
        // Junk paging falls back to the defaults.
        let page: HistoryPage = get(&router, "/api/history?limit=lots&before=x")
            .await
            .json();
        assert_eq!(page.entries.len(), 3);

        let meta: HistoryEntry = get(&router, &format!("/api/history/{}/meta", ids[0]))
            .await
            .json();
        assert_eq!(meta.players, record.players);

        let blob = get(&router, &format!("/api/history/{}", ids[0])).await;
        assert_eq!(blob.status, StatusCode::OK);
        assert_eq!(
            blob.header(header::CONTENT_TYPE),
            Some("application/octet-stream")
        );
        assert_eq!(
            postcard::from_bytes::<GameRecord>(&blob.body).unwrap(),
            record
        );

        assert_eq!(
            get(&router, "/api/history/999").await.status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get(&router, "/api/history/999/meta").await.status,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn compute_and_sessions_report_what_there_is() {
        let (router, db, _) = test_app(false);
        let overview: ComputeOverview = get(&router, "/api/compute").await.json();
        assert_eq!(overview.totals.devices, 0);

        let stat = ComputeStat {
            name: "🐍 alice".to_owned(),
            device: "Pixel 9".to_owned(),
            moves: 12,
            sims: 3_400,
            wall_ms: 900,
        };
        db.insert_compute_stat(1, &stat, "a1", "10.0.0.1", 40_000)
            .unwrap();
        let overview: ComputeOverview = get(&router, "/api/compute").await.json();
        assert_eq!(overview.totals.sims, 3_400);
        assert_eq!(overview.donors[0].device, "Pixel 9");

        // Nobody is playing.
        let sessions: Vec<serde_json::Value> = get(&router, "/api/sessions").await.json();
        assert!(sessions.is_empty());
    }

    /// A database gone wrong is a server error, never an empty history that
    /// looks like everything was lost.
    #[tokio::test]
    async fn a_broken_database_is_a_server_error() {
        let (router, _db, path) = test_app(false);
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch("DROP TABLE games; DROP TABLE compute_stats;")
            .unwrap();
        for uri in [
            "/api/history",
            "/api/history/1",
            "/api/history/1/meta",
            "/api/compute",
        ] {
            assert_eq!(
                get(&router, uri).await.status,
                StatusCode::INTERNAL_SERVER_ERROR,
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn the_shell_is_revalidated_and_hashed_files_kept_forever() {
        let (router, _, _) = test_app(false);
        let shell = get(&router, "/").await;
        assert_eq!(shell.status, StatusCode::OK);
        assert_eq!(shell.header(header::CACHE_CONTROL), Some(SHELL_CACHE));
        assert_eq!(shell.header(header::CONTENT_TYPE), Some("text/html"));
        assert!(String::from_utf8_lossy(&shell.body).contains("<title>snek</title>"));

        let script = get(&router, "/app-1a2b3c.js").await;
        assert_eq!(script.header(header::CACHE_CONTROL), Some(HASHED_CACHE));
        assert_eq!(script.header(header::CONTENT_TYPE), Some("text/javascript"));

        // Hash routes and anything unknown get the shell, uncached.
        let deep = get(&router, "/history/12").await;
        assert_eq!(deep.status, StatusCode::OK);
        assert_eq!(deep.header(header::CACHE_CONTROL), Some(SHELL_CACHE));
        assert_eq!(deep.body, shell.body);
    }

    #[tokio::test]
    async fn text_assets_go_out_brotli_encoded_to_clients_that_take_it() {
        let (router, _, _) = test_app(true);
        let plain = get(&router, "/app-1a2b3c.js").await;
        assert_eq!(plain.header(header::CONTENT_ENCODING), None);
        assert_eq!(plain.header(header::VARY), Some("accept-encoding"));

        for accept in ["br", "gzip, deflate, br;q=0.9", "gzip,br"] {
            let br = get_with(&router, "/app-1a2b3c.js", Some(accept)).await;
            assert_eq!(br.header(header::CONTENT_ENCODING), Some("br"), "{accept}");
            assert_eq!(unbrotli(&br.body), plain.body, "{accept}");
        }
        // "brotli" is not "br".
        let other = get_with(&router, "/app-1a2b3c.js", Some("gzip, brotli")).await;
        assert_eq!(other.header(header::CONTENT_ENCODING), None);

        // The shell is compressed too; the font, compressed already, isn't.
        let shell = get_with(&router, "/", Some("br")).await;
        assert_eq!(shell.header(header::CONTENT_ENCODING), Some("br"));
        let font = get_with(&router, "/font-9f8e7d.woff2", Some("br")).await;
        assert_eq!(font.status, StatusCode::OK);
        assert_eq!(font.header(header::CONTENT_ENCODING), None);
        assert_eq!(font.header(header::CONTENT_TYPE), Some("font/woff2"));
    }

    /// Debug builds read the files straight from disk, so nothing is
    /// precompressed and a fresh `trunk build` shows up at once.
    #[tokio::test]
    async fn without_precompression_everything_goes_out_as_is() {
        let (router, _, _) = test_app(false);
        let script = get_with(&router, "/app-1a2b3c.js", Some("br")).await;
        assert_eq!(script.header(header::CONTENT_ENCODING), None);
        assert!(script.body.starts_with(b"console.log"));
    }

    #[tokio::test]
    async fn an_unbuilt_frontend_says_so() {
        #[derive(rust_embed::Embed)]
        #[folder = "tests/fixtures/empty"]
        struct Unbuilt;

        let db = Arc::new(Db::open(&temp_path("unbuilt.db")).unwrap());
        let (router, _) = app(db, Arc::new(Frontend::new::<Unbuilt>(true)));
        let reply = get(&router, "/").await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert!(String::from_utf8_lossy(&reply.body).contains("trunk build"));
    }
}
