//! Bot compute on a real background thread.
//!
//! `spawn_local` is not a thread — it is a microtask on the UI thread, so
//! searching there competes with rendering and input no matter how finely
//! it is sliced. A dedicated Web Worker is an actual OS thread: the search
//! runs at full speed with the main thread completely untouched.
//!
//! Each bot gets its own worker, so bots never queue behind one another.
//!
//! The worker is a tiny inline module that imports *this same* wasm bundle
//! (trunk publishes its URL in a `modulepreload` link) and calls the
//! exported [`snek_bot_compute`]. Everything crosses the boundary as
//! postcard bytes, so there is one encoding in the codebase and no shared
//! state to get wrong.

use futures_channel::oneshot;
use js_sys::{Object, Reflect, Uint8Array};
use serde::{Deserialize, Serialize};
use snek_shared::{Dir, GameConfig, PlayerId, Turn};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{JsCast, JsValue};

/// How long to wait on the worker before giving up and rebuilding it.
/// Short: a stalled worker must be discovered inside a tick, not after
/// several of them have already passed with the bot driving straight.
const WORKER_TIMEOUT_MS: u32 = 1_500;

#[derive(Serialize, Deserialize)]
struct Job {
    config: GameConfig,
    turn: Turn,
    bot_id: PlayerId,
    /// Absolute wall-clock deadline (epoch ms), fixed when the request
    /// *arrived*. If this job waits behind another on its worker, that wait
    /// comes out of its thinking time — otherwise a queued job answers
    /// correctly but far too late, which the server can only see as
    /// silence.
    deadline_ms: f64,
    nonce: u64,
}

#[derive(Serialize, Deserialize)]
struct JobResult {
    dir: Dir,
    proof: u64,
    sims: u32,
    slices: u32,
    /// When the worker picked the job up (epoch ms) — the difference from
    /// the request's arrival is time lost to queueing.
    began_ms: f64,
    search_ms: u32,
}

/// Exported into the wasm bundle so the worker can call it. Searches until
/// its deadline and returns the best move found — MCTS is anytime, so the
/// clock decides when to stop rather than a predicted simulation count.
#[wasm_bindgen]
pub fn snek_bot_compute(job: &[u8]) -> Vec<u8> {
    let Some(job) = postcard::from_bytes::<Job>(job).ok() else {
        return Vec::new();
    };
    // Enough work per slice to keep clock checks negligible, small enough
    // to land close to the deadline.
    // The deadline can only be honoured to the granularity of one slice,
    // so the slice has to track how expensive a simulation currently is.
    // A fixed 128 was ~19ms early on but several hundred late in a game
    // (four long snakes make every rollout dearer), which overran the
    // deadline, pushed into the next tick, and left the following job
    // queued behind it with time for a single slice — a spiral that gets
    // worse exactly as the board gets crowded.
    const TARGET_SLICE_MS: f64 = 4.0;
    let began_ms = js_sys::Date::now();
    let mut search =
        snek_shared::bot::begin(&job.config, &job.turn, &job.bot_id, u32::MAX, job.nonce);
    let mut sims = 0u32;
    let mut slices = 0u32;
    let mut slice = 32usize;
    loop {
        let before = js_sys::Date::now();
        // Always take one slice, so even a job that queued past its
        // deadline returns a searched move rather than a default.
        if search.step(slice) {
            break; // nothing left to search
        }
        sims += slice as u32;
        slices += 1;
        let took = js_sys::Date::now() - before;
        if took > 0.5 {
            let scaled = slice as f64 * TARGET_SLICE_MS / took;
            slice = scaled.clamp(8.0, 2_048.0) as usize;
        } else if took == 0.0 {
            slice = (slice * 2).min(2_048);
        }
        if js_sys::Date::now() >= job.deadline_ms {
            break;
        }
    }
    let (dir, proof) = search.result();
    let search_ms = (js_sys::Date::now() - began_ms).max(0.0) as u32;
    postcard::to_allocvec(&JobResult {
        dir,
        proof,
        sims,
        slices,
        began_ms,
        search_ms,
    })
    .unwrap_or_default()
}

thread_local! {
    /// One worker per bot, always. Sharing a worker means sharing a message
    /// queue, and a worker drains that queue strictly in order — so a
    /// second bot on the same worker cannot start until the first finishes,
    /// which on a busy device left it with a single slice of search. With a
    /// worker each there is no queue: every worker holds one job per tick
    /// and the OS scheduler shares the cores, so bots degrade together and
    /// gracefully rather than one starving.
    ///
    /// Workers live for the page — booting a wasm instance is far too
    /// expensive to repeat per tick.
    static WORKERS: RefCell<HashMap<PlayerId, web_sys::Worker>> =
        RefCell::new(HashMap::new());
    static PENDING: RefCell<HashMap<u64, oneshot::Sender<Vec<u8>>>> =
        RefCell::new(HashMap::new());
    static NEXT_ID: Cell<u64> = const { Cell::new(1) };
    /// Set once we know a worker cannot be created here; stops us retrying
    /// on every single move.
    static UNAVAILABLE: Cell<bool> = const { Cell::new(false) };
}

/// The worker this bot thinks on, booting one the first time we see it.
fn worker_for(bot_id: &PlayerId) -> Option<web_sys::Worker> {
    if let Some(w) = WORKERS.with(|w| w.borrow().get(bot_id).cloned()) {
        return Some(w);
    }
    let worker = spawn_worker()?;
    WORKERS.with(|w| {
        w.borrow_mut().insert(bot_id.clone(), worker.clone());
    });
    Some(worker)
}

/// This bundle's JS glue and wasm URLs, as published by trunk, made
/// **fully qualified**. A blob-URL module has no origin to resolve a
/// path-absolute specifier like `/snek-web-abc.js` against, so importing
/// one fails on load — which looks exactly like "the worker never
/// answers".
fn bundle_urls() -> Option<(String, String)> {
    let window = web_sys::window()?;
    let origin = window.location().origin().ok()?;
    let doc = window.document()?;
    let absolute = |href: String| {
        if href.starts_with("http") {
            href
        } else {
            format!("{origin}{href}")
        }
    };
    let glue = absolute(
        doc.query_selector("link[rel=modulepreload]")
            .ok()??
            .get_attribute("href")?,
    );
    let wasm = doc
        .query_selector("link[rel=preload][as=fetch]")
        .ok()
        .flatten()
        .and_then(|l| l.get_attribute("href"))
        .map(absolute)
        .unwrap_or_else(|| glue.replace(".js", "_bg.wasm"));
    Some((glue, wasm))
}

fn spawn_worker() -> Option<web_sys::Worker> {
    let (glue, wasm) = bundle_urls()?;
    // A module worker that boots the same bundle and answers jobs.
    let script = format!(
        r#"let ready = null;
const boot = async () => {{
  const b = await import('{glue}');
  await b.default({{ module_or_path: '{wasm}' }});
  return b;
}};
self.onmessage = async (e) => {{
  try {{
    ready = ready || boot();
    const b = await ready;
    const out = b.snek_bot_compute(new Uint8Array(e.data.job));
    self.postMessage({{ id: e.data.id, out }}, [out.buffer]);
  }} catch (err) {{
    // Surface the reason instead of dying silently.
    self.postMessage({{ id: e.data.id, error: String((err && err.stack) || err) }});
  }}
}};"#
    );
    let parts = js_sys::Array::of1(&JsValue::from_str(&script));
    let props = web_sys::BlobPropertyBag::new();
    props.set_type("text/javascript");
    let blob = web_sys::Blob::new_with_str_sequence_and_options(&parts, &props).ok()?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).ok()?;

    let opts = web_sys::WorkerOptions::new();
    opts.set_type(web_sys::WorkerType::Module);
    let worker = web_sys::Worker::new_with_options(&url, &opts).ok()?;

    let on_message =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |ev: web_sys::MessageEvent| {
            let data = ev.data();
            let id = Reflect::get(&data, &"id".into())
                .ok()
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as u64;
            if let Some(err) = Reflect::get(&data, &"error".into())
                .ok()
                .and_then(|v| v.as_string())
            {
                web_sys::console::error_1(&JsValue::from_str(&format!("bot worker failed: {err}")));
            }
            let bytes = Reflect::get(&data, &"out".into())
                .ok()
                .and_then(|v| v.dyn_into::<Uint8Array>().ok())
                .map(|a| a.to_vec())
                .unwrap_or_default();
            if let Some(tx) = PENDING.with(|p| p.borrow_mut().remove(&id)) {
                let _ = tx.send(bytes);
            }
        });
    worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();

    // A worker that dies takes its pending jobs with it; drop the handle so
    // the next request builds a fresh one.
    let on_error = Closure::<dyn FnMut(JsValue)>::new(move |_| {
        WORKERS.with(|w| w.borrow_mut().clear());
        PENDING.with(|p| p.borrow_mut().clear());
    });
    worker.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    on_error.forget();

    Some(worker)
}

/// Run one bot search on the worker thread. `None` means no worker is
/// available here and the caller should fall back to the main thread.
pub async fn compute(
    config: GameConfig,
    turn: Turn,
    bot_id: PlayerId,
    think_ms: u32,
    nonce: u64,
) -> Option<(Dir, u64, u32, u32, f64, u32)> {
    if UNAVAILABLE.with(|u| u.get()) {
        return None;
    }
    let Some(worker) = worker_for(&bot_id) else {
        UNAVAILABLE.with(|u| u.set(true));
        return None;
    };

    // The clock starts now, when the request arrived — not when a worker
    // happens to get to it.
    let deadline_ms = js_sys::Date::now() + think_ms as f64;
    let job = postcard::to_allocvec(&Job {
        config,
        turn,
        bot_id,
        deadline_ms,
        nonce,
    })
    .ok()?;

    let id = NEXT_ID.with(|n| {
        let id = n.get();
        n.set(id + 1);
        id
    });
    let (tx, rx) = oneshot::channel();
    PENDING.with(|p| p.borrow_mut().insert(id, tx));

    let msg = Object::new();
    let _ = Reflect::set(&msg, &"id".into(), &JsValue::from_f64(id as f64));
    let _ = Reflect::set(&msg, &"job".into(), &Uint8Array::from(job.as_slice()));
    if worker.post_message(&msg).is_err() {
        PENDING.with(|p| p.borrow_mut().remove(&id));
        WORKERS.with(|w| w.borrow_mut().clear());
        return None;
    }

    // A worker that goes quiet must not strand the caller forever: time it
    // out and rebuild it on the next request.
    let bytes = match futures_util::future::select(
        rx,
        Box::pin(gloo_timers::future::TimeoutFuture::new(WORKER_TIMEOUT_MS)),
    )
    .await
    {
        futures_util::future::Either::Left((Ok(bytes), _)) => bytes,
        _ => {
            PENDING.with(|p| p.borrow_mut().remove(&id));
            WORKERS.with(|w| w.borrow_mut().clear());
            return None;
        }
    };
    if bytes.is_empty() {
        return None;
    }
    let result = postcard::from_bytes::<JobResult>(&bytes).ok()?;
    Some((
        result.dir,
        result.proof,
        result.sims,
        result.slices,
        result.began_ms,
        result.search_ms,
    ))
}
