//! The serving side: the lock-holder binds a local socket, advertises it
//! via the sidecar, and answers read ops — each as its own redb read
//! transaction, snapshot-isolated against the app's writes for free.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use interprocess::local_socket::prelude::*;
use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, Listener, ListenerOptions, Stream,
};
use serde_json::{json, Value};

use crate::db::{Backend, Core, Db};
use crate::embed::Embedder;
use crate::error::{Result, TepinError};
use crate::serve::{sidecar, wire, PROTOCOL_VERSION};
use crate::vector::{EmbedRuntime, Shared};

pub(crate) struct HostRuntime {
    state: Arc<HostState>,
    accept: Option<std::thread::JoinHandle<()>>,
    sidecar_file: PathBuf,
    endpoint: String,
}

/// The embedder + worker channel of the owning handle's pipeline.
type SearchHandles = (Arc<dyn Embedder>, Arc<Shared>);

struct HostState {
    /// Weak so lingering connections never keep the engine (and the file
    /// lock) alive after the owning Db is dropped.
    core: Weak<Core>,
    nonce: String,
    shutdown: AtomicBool,
    /// Registered by `attach_embedder` on a hosting handle — lets the
    /// host serve real semantic `search` to model-less clients.
    search: Mutex<Option<SearchHandles>>,
}

impl HostRuntime {
    pub(crate) fn register_embedder(&self, embedder: Arc<dyn Embedder>, shared: Arc<Shared>) {
        *self.state.search.lock().unwrap() = Some((embedder, shared));
    }
}

fn bind(endpoint: &str) -> std::io::Result<Listener> {
    let name = if cfg!(windows) {
        endpoint.to_ns_name::<GenericNamespaced>()?
    } else {
        endpoint.to_fs_name::<GenericFilePath>()?
    };
    ListenerOptions::new().name(name).create_sync()
}

pub(crate) fn connect_endpoint(endpoint: &str, windows_pipe: bool) -> std::io::Result<Stream> {
    let name = if windows_pipe {
        endpoint.to_ns_name::<GenericNamespaced>()?
    } else {
        endpoint.to_fs_name::<GenericFilePath>()?
    };
    Stream::connect(name)
}

pub(crate) fn start(core: Arc<Core>, db_path: &Path) -> Result<HostRuntime> {
    let (sidecar_file, canonical) = sidecar::location(db_path)?;
    let dir = sidecar::runtime_dir();
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }

    let nonce = {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("OS RNG");
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    let pid = std::process::id();
    let (endpoint, transport) = if cfg!(windows) {
        (format!("tepindb-{pid}-{}", &nonce[..8]), "windows-pipe")
    } else {
        (
            dir.join(format!("{pid}-{}.sock", &nonce[..8]))
                .to_string_lossy()
                .into_owned(),
            "unix",
        )
    };

    let serve_failed = |e: std::io::Error| {
        TepinError::new(
            "serve_failed",
            format!("could not host reads for {canonical}: {e}"),
            "check the runtime dir (XDG_RUNTIME_DIR/TMPDIR) is writable, or open without ServeMode::Host",
        )
        .with_source(e)
    };
    let listener = bind(&endpoint).map_err(serve_failed)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Same trust boundary as the db file: this user only.
        let _ = std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600));
    }

    sidecar::write(
        &sidecar_file,
        &sidecar::Sidecar {
            pid,
            transport: transport.to_string(),
            endpoint: endpoint.clone(),
            nonce: nonce.clone(),
            protocol_version: PROTOCOL_VERSION,
            format_version: crate::format::FORMAT_VERSION,
            path: canonical,
            started_at_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        },
    )?;

    let state = Arc::new(HostState {
        core: Arc::downgrade(&core),
        nonce,
        shutdown: AtomicBool::new(false),
        search: Mutex::new(None),
    });
    let accept = std::thread::Builder::new()
        .name("tepin-serve-accept".into())
        .spawn({
            let state = Arc::clone(&state);
            move || {
                for conn in listener.incoming() {
                    if state.shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = conn else { continue };
                    let state = Arc::clone(&state);
                    let _ = std::thread::Builder::new()
                        .name("tepin-serve-conn".into())
                        .spawn(move || handle_conn(&state, stream));
                }
            }
        })
        .expect("spawn serve accept thread");

    Ok(HostRuntime {
        state,
        accept: Some(accept),
        sidecar_file,
        endpoint,
    })
}

impl Drop for HostRuntime {
    fn drop(&mut self) {
        self.state.shutdown.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_file(&self.sidecar_file);
        // Wake the blocking accept so the loop observes the flag.
        let _ = connect_endpoint(&self.endpoint, cfg!(windows));
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.endpoint);
    }
}

fn handle_conn(state: &HostState, mut stream: Stream) {
    while let Ok(Some(msg)) = wire::read_frame(&mut stream) {
        if state.shutdown.load(Ordering::SeqCst) {
            break;
        }
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let op = msg["op"].as_str().unwrap_or_default().to_string();
        let args = msg.get("args").cloned().unwrap_or_else(|| json!({}));
        let reply = match dispatch(state, &op, &args) {
            Ok(ok) => json!({"id": id, "ok": ok}),
            Err(e) => json!({"id": id, "error": e.to_json()["error"]}),
        };
        if wire::write_frame(&mut stream, &reply).is_err() {
            break;
        }
    }
}

/// A request-scoped Db view over the host's engine, including a borrowed
/// handle on its embed pipeline when one is attached.
fn shim_db(state: &HostState) -> Result<Db> {
    let core = state.core.upgrade().ok_or_else(|| {
        TepinError::new(
            "serve_disconnected",
            "the serving process is shutting down",
            "reopen the database; the lock may be free now",
        )
    })?;
    let embed = state
        .search
        .lock()
        .unwrap()
        .as_ref()
        .map(|(embedder, shared)| EmbedRuntime::borrowed(Arc::clone(embedder), Arc::clone(shared)));
    Ok(Db {
        backend: Backend::Local(core),
        embed,
        host: None,
    })
}

fn dispatch(state: &HostState, op: &str, args: &Value) -> Result<Value> {
    let collection = || -> Result<&str> {
        args["collection"].as_str().ok_or_else(|| {
            TepinError::new(
                "invalid_filter",
                "missing required string argument 'collection'",
                "pass the collection name",
            )
        })
    };
    let id_arg = || -> Result<&str> {
        args["id"].as_str().ok_or_else(|| {
            TepinError::new(
                "doc_not_found",
                "missing required string argument 'id'",
                "pass the document's _id",
            )
        })
    };
    let limit = || args["limit"].as_u64().unwrap_or(5) as usize;

    match op {
        "hello" => {
            let client_protocol = args["protocol_version"].as_u64().unwrap_or(0) as u32;
            if client_protocol != PROTOCOL_VERSION {
                return Err(TepinError::new(
                    "protocol_mismatch",
                    format!(
                        "client speaks serving protocol v{client_protocol}, this host v{PROTOCOL_VERSION}"
                    ),
                    "upgrade the older side of the connection",
                ));
            }
            Ok(json!({
                "protocol_version": PROTOCOL_VERSION,
                "format_version": crate::format::FORMAT_VERSION,
                "nonce": state.nonce,
                "pid": std::process::id(),
            }))
        }
        "collections" => Ok(json!({"collections": shim_db(state)?.collections()?})),
        "query" => {
            let filter = args.get("filter").cloned().unwrap_or_else(|| json!({}));
            Ok(json!({"docs": shim_db(state)?.find(collection()?, &filter)?}))
        }
        "get" => Ok(json!({"doc": shim_db(state)?.get(collection()?, id_arg()?)?})),
        "search" => {
            let query = args["query"].as_str().unwrap_or_default();
            let hits = shim_db(state)?.search(args["collection"].as_str(), query, limit())?;
            Ok(json!({"hits": hits}))
        }
        "keyword_search" => {
            let query = args["query"].as_str().unwrap_or_default();
            let hits =
                shim_db(state)?.keyword_search(args["collection"].as_str(), query, limit())?;
            Ok(json!({"hits": hits}))
        }
        "search_by_vector" => {
            let vector: Vec<f32> = args["vector"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_f64()).map(|v| v as f32).collect())
                .unwrap_or_default();
            let hits =
                shim_db(state)?.search_by_vector(args["collection"].as_str(), &vector, limit())?;
            Ok(json!({"hits": hits}))
        }
        "get_vectors" => Ok(
            json!({"vectors": shim_db(state)?.get_vectors(collection()?, id_arg()?)?}),
        ),
        "insert" | "upsert" | "update" | "delete" | "purpose" | "embed_fields" => {
            Err(TepinError::new(
                "database_locked",
                "another process holds the write lock; served handles are read-only",
                "writes need the lock — close the writer, or go through its surface (e.g. its MCP server)",
            ))
        }
        other => Err(TepinError::new(
            "not_implemented",
            format!("unknown serve op {other:?}"),
            "the client and host may be different versions; upgrade the older side",
        )),
    }
}
