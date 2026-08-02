//! The language-neutral driver core shared by every FFI shell (the C ABI
//! in `lib.rs`, the Node addon in `tepin-node`): a process-local handle
//! registry over `Db`, JSON open options, and dispatch into
//! `tepin_core::ops`. No C types here — shells only marshal strings.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};
use tepin_core::{Db, TepinError};

/// Open handles. An `Arc<Db>` is cloned out per call so long operations
/// never hold the registry lock.
fn registry() -> &'static Mutex<HashMap<u64, Arc<Db>>> {
    static REG: OnceLock<Mutex<HashMap<u64, Arc<Db>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

// The whole design leans on the engine being thread-safe; make that a
// compile error instead of a runtime surprise if it ever regresses.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Db>();
};

/// Library, format, and op-surface introspection — what a driver prints
/// when asked to debug itself.
pub fn version_info() -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "format_version": tepin_core::format::FORMAT_VERSION,
        "embedding": cfg!(feature = "embedding"),
        "ops": tepin_core::ops::OPS,
    })
}

/// Open a database from JSON options:
///
/// ```jsonc
/// {
///   "path": "memory.tepin",       // or "in_memory": true
///   "existing": false,            // true = error instead of create
///   "serve": "off",               // "host" | "discover" | "host_or_discover"
///   "embedder": "off",            // "auto" = bge-small, lazy download
///   "retry_ms": 0                 // retry a locked open with backoff
/// }
/// ```
///
/// Returns `{"handle": N, "served": bool}`.
pub fn open(options_json: &str) -> Result<Value, TepinError> {
    let opts: Value = serde_json::from_str(options_json)?;

    #[allow(unused_mut)]
    let mut db = if opts["in_memory"].as_bool().unwrap_or(false) {
        Db::open_in_memory()?
    } else {
        let path = opts["path"].as_str().ok_or_else(|| {
            TepinError::new(
                "file_not_found",
                "open options need a string 'path' (or \"in_memory\": true)",
                "pass {\"path\": \"your.tepin\"}",
            )
        })?;
        let mut builder = Db::options();
        if let Some(ms) = opts["retry_ms"].as_u64().filter(|ms| *ms > 0) {
            builder = builder.retry_for(std::time::Duration::from_millis(ms));
        }
        builder = builder.serve(parse_serve_mode(&opts["serve"])?);
        if opts["existing"].as_bool().unwrap_or(false) {
            builder.open_existing(path)?
        } else {
            builder.open(path)?
        }
    };

    match opts["embedder"].as_str().unwrap_or("off") {
        "off" => {}
        "auto" => attach_auto_embedder(&mut db)?,
        other => {
            return Err(TepinError::new(
                "embedder_mismatch",
                format!("unknown embedder mode {other:?}"),
                "use \"off\" (default) or \"auto\" (bge-small, lazy SHA-256-pinned download)",
            ))
        }
    }

    let served = db.is_served();
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    registry().lock().unwrap().insert(handle, Arc::new(db));
    Ok(json!({"handle": handle, "served": served}))
}

fn parse_serve_mode(v: &Value) -> Result<tepin_core::ServeMode, TepinError> {
    use tepin_core::ServeMode;
    Ok(match v.as_str().unwrap_or("off") {
        "off" => ServeMode::Off,
        "host" => ServeMode::Host,
        "discover" => ServeMode::Discover,
        "host_or_discover" => ServeMode::HostOrDiscover,
        other => {
            return Err(TepinError::new(
                "serve_failed",
                format!("unknown serve mode {other:?}"),
                "use \"off\", \"host\", \"discover\", or \"host_or_discover\" (docs/serving.md)",
            ))
        }
    })
}

#[cfg(feature = "embedding")]
fn attach_auto_embedder(db: &mut Db) -> Result<(), TepinError> {
    // A served (remote) handle searches through the host's model.
    if db.is_served() {
        return Ok(());
    }
    let cache = tepin_embed::fetch::default_cache_dir()?;
    let lazy = tepin_embed::OnnxEmbedder::spawn_lazy(&tepin_embed::fetch::BGE_SMALL, cache);
    db.attach_embedder(Arc::new(lazy))
}

#[cfg(not(feature = "embedding"))]
fn attach_auto_embedder(_db: &mut Db) -> Result<(), TepinError> {
    Err(TepinError::new(
        "not_implemented",
        "this libtepin build has no embedding support (slim library)",
        "use the full libtepin from GitHub releases for semantic search; every other op works",
    ))
}

fn invalid_handle(handle: u64) -> TepinError {
    TepinError::new(
        "invalid_handle",
        format!("no open database for handle {handle}"),
        "the handle was never opened, or was already closed; open the database again",
    )
}

/// Run one op against an open handle — op names, argument names, and
/// result shapes are exactly the CLI/MCP surface (`tepin_core::ops`).
pub fn call(handle: u64, op: &str, args: &Value) -> Result<Value, TepinError> {
    let db = registry()
        .lock()
        .unwrap()
        .get(&handle)
        .cloned()
        .ok_or_else(|| invalid_handle(handle))?;
    tepin_core::ops::dispatch(&db, op, args)
}

/// Close a handle, releasing the file lock (and stopping its serve host,
/// when it had one). In-flight calls on other threads may briefly hold
/// the `Arc`; the engine drops (and the lock frees) when the last one
/// finishes.
pub fn close(handle: u64) -> Result<Value, TepinError> {
    match registry().lock().unwrap().remove(&handle) {
        Some(_db) => Ok(json!({"closed": handle})),
        None => Err(invalid_handle(handle)),
    }
}
