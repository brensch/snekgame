//! Bot compute on a real background thread.
//!
//! `spawn_local` is not a thread — it is a microtask on the UI thread, so
//! searching there competes with rendering and input no matter how finely
//! it is sliced. A dedicated Web Worker is an actual OS thread: the search
//! runs at full speed with the main thread completely untouched.
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

/// How long to wait on the worker before rebuilding it.
const WORKER_TIMEOUT_MS: u32 = 5_000;

#[derive(Serialize, Deserialize)]
struct Job {
    config: GameConfig,
    turn: Turn,
    bot_id: PlayerId,
    sims: u32,
    nonce: u64,
}

#[derive(Serialize, Deserialize)]
struct JobResult {
    dir: Dir,
    proof: u64,
}

/// Exported into the wasm bundle so the worker can call it. Runs the full
/// deterministic search — no chunking needed, nothing else lives on this
/// thread.
#[wasm_bindgen]
pub fn snek_bot_compute(job: &[u8]) -> Vec<u8> {
    let Some(job) = postcard::from_bytes::<Job>(job).ok() else {
        return Vec::new();
    };
    let (dir, proof) =
        snek_shared::bot::think(&job.config, &job.turn, &job.bot_id, job.sims, job.nonce);
    postcard::to_allocvec(&JobResult { dir, proof }).unwrap_or_default()
}

thread_local! {
    static WORKER: RefCell<Option<web_sys::Worker>> = const { RefCell::new(None) };
    static PENDING: RefCell<HashMap<u64, oneshot::Sender<Vec<u8>>>> =
        RefCell::new(HashMap::new());
    static NEXT_ID: Cell<u64> = const { Cell::new(1) };
    /// Set once we know a worker cannot be created here; stops us retrying
    /// on every single move.
    static UNAVAILABLE: Cell<bool> = const { Cell::new(false) };
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

    let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(
        move |ev: web_sys::MessageEvent| {
            let data = ev.data();
            let id = Reflect::get(&data, &"id".into())
                .ok()
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as u64;
            if let Some(err) = Reflect::get(&data, &"error".into())
                .ok()
                .and_then(|v| v.as_string())
            {
                web_sys::console::error_1(&JsValue::from_str(&format!(
                    "bot worker failed: {err}"
                )));
            }
            let bytes = Reflect::get(&data, &"out".into())
                .ok()
                .and_then(|v| v.dyn_into::<Uint8Array>().ok())
                .map(|a| a.to_vec())
                .unwrap_or_default();
            if let Some(tx) = PENDING.with(|p| p.borrow_mut().remove(&id)) {
                let _ = tx.send(bytes);
            }
        },
    );
    worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();

    // A worker that dies takes its pending jobs with it; drop the handle so
    // the next request builds a fresh one.
    let on_error = Closure::<dyn FnMut(JsValue)>::new(move |_| {
        WORKER.with(|w| *w.borrow_mut() = None);
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
    sims: u32,
    nonce: u64,
) -> Option<(Dir, u64)> {
    if UNAVAILABLE.with(|u| u.get()) {
        return None;
    }
    let worker = WORKER.with(|w| {
        let mut w = w.borrow_mut();
        if w.is_none() {
            *w = spawn_worker();
        }
        w.clone()
    });
    let Some(worker) = worker else {
        UNAVAILABLE.with(|u| u.set(true));
        return None;
    };

    let job = postcard::to_allocvec(&Job {
        config,
        turn,
        bot_id,
        sims,
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
        WORKER.with(|w| *w.borrow_mut() = None);
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
            WORKER.with(|w| *w.borrow_mut() = None);
            return None;
        }
    };
    if bytes.is_empty() {
        return None;
    }
    let result = postcard::from_bytes::<JobResult>(&bytes).ok()?;
    Some((result.dir, result.proof))
}
