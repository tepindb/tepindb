//! The discovering side: a process that lost the file lock reads the
//! sidecar, connects to the advertised host, and turns its read calls
//! into wire requests. Stale sidecars (dead host, recycled endpoint,
//! wrong nonce) are cleaned up and reported as plain `database_locked`.

use std::path::Path;
use std::sync::Mutex;

use interprocess::local_socket::Stream;
use serde_json::{json, Value};

use crate::db::CollectionInfo;
use crate::error::{Result, TepinError};
use crate::serve::{host, sidecar, wire, PROTOCOL_VERSION};
use crate::vector::{KeywordHit, SearchHit, VectorHit};

pub(crate) struct RemoteClient {
    inner: Mutex<ClientInner>,
}

struct ClientInner {
    stream: Stream,
    next_id: u64,
}

/// Try to find and validate a host for `db_path`. `Ok(None)` means "no
/// usable host" — the caller falls back to `database_locked`.
pub(crate) fn discover(db_path: &Path) -> Result<Option<RemoteClient>> {
    let Ok((sidecar_file, canonical)) = sidecar::location(db_path) else {
        return Ok(None);
    };
    let Some(sc) = sidecar::read(&sidecar_file) else {
        return Ok(None);
    };
    // The filename hash is not collision-proof; the path inside is.
    if sc.path != canonical {
        return Ok(None);
    }
    // A live host on an incompatible protocol: leave its sidecar alone,
    // just don't talk to it.
    if sc.protocol_version != PROTOCOL_VERSION {
        return Ok(None);
    }
    // Same rule as opening a too-new file directly.
    if sc.format_version > crate::format::FORMAT_VERSION {
        return Err(TepinError::new(
            "format_too_new",
            format!(
                "the serving process uses format v{}, this build reads up to v{}",
                sc.format_version,
                crate::format::FORMAT_VERSION
            ),
            "upgrade tepindb (`npm i -g tepindb` / `cargo install tepin-cli`) and retry",
        ));
    }

    let Ok(stream) = host::connect_endpoint(&sc.endpoint, sc.transport == "windows-pipe") else {
        // Dead pid or recycled endpoint — clean the stale sidecar so the
        // next opener doesn't retry it.
        let _ = std::fs::remove_file(&sidecar_file);
        return Ok(None);
    };
    let client = RemoteClient {
        inner: Mutex::new(ClientInner { stream, next_id: 0 }),
    };
    // The nonce round-trip proves the listener is the advertised process,
    // not an unrelated service on a recycled endpoint.
    match client.call("hello", json!({"protocol_version": PROTOCOL_VERSION})) {
        Ok(hello) if hello["nonce"] == json!(sc.nonce) => Ok(Some(client)),
        _ => {
            let _ = std::fs::remove_file(&sidecar_file);
            Ok(None)
        }
    }
}

impl RemoteClient {
    fn call(&self, op: &str, args: Value) -> Result<Value> {
        let disconnected = || {
            TepinError::new(
                "serve_disconnected",
                "lost the connection to the serving process",
                "reopen the database; the lock may be free now, or a new host may be up",
            )
        };
        let mut inner = self.inner.lock().unwrap();
        inner.next_id += 1;
        let id = inner.next_id;
        wire::write_frame(
            &mut inner.stream,
            &json!({"id": id, "op": op, "args": args}),
        )
        .map_err(|e| disconnected().with_source(e))?;
        let reply = wire::read_frame(&mut inner.stream)
            .map_err(|e| disconnected().with_source(e))?
            .ok_or_else(disconnected)?;
        if reply["id"] != json!(id) {
            return Err(disconnected());
        }
        if let Some(err) = reply.get("error") {
            return Err(TepinError::new(
                intern_code(err["code"].as_str().unwrap_or_default()),
                err["message"].as_str().unwrap_or_default().to_string(),
                err["hint"].as_str().unwrap_or_default().to_string(),
            ));
        }
        Ok(reply.get("ok").cloned().unwrap_or(Value::Null))
    }

    pub(crate) fn collections(&self) -> Result<Vec<CollectionInfo>> {
        let v = self.call("collections", json!({}))?;
        Ok(serde_json::from_value(v["collections"].clone())?)
    }

    pub(crate) fn get(&self, collection: &str, id: &str) -> Result<Option<Value>> {
        let v = self.call("get", json!({"collection": collection, "id": id}))?;
        match v["doc"].clone() {
            Value::Null => Ok(None),
            doc => Ok(Some(doc)),
        }
    }

    pub(crate) fn find(&self, collection: &str, filter: &Value) -> Result<Vec<Value>> {
        let v = self.call("query", json!({"collection": collection, "filter": filter}))?;
        Ok(serde_json::from_value(v["docs"].clone())?)
    }

    pub(crate) fn search(
        &self,
        collection: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let v = self.call(
            "search",
            json!({"collection": collection, "query": query, "limit": limit}),
        )?;
        Ok(serde_json::from_value(v["hits"].clone())?)
    }

    pub(crate) fn keyword_search(
        &self,
        collection: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<KeywordHit>> {
        let v = self.call(
            "keyword_search",
            json!({"collection": collection, "query": query, "limit": limit}),
        )?;
        Ok(serde_json::from_value(v["hits"].clone())?)
    }

    pub(crate) fn search_by_vector(
        &self,
        collection: Option<&str>,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<VectorHit>> {
        let v = self.call(
            "search_by_vector",
            json!({"collection": collection, "vector": query, "limit": limit}),
        )?;
        Ok(serde_json::from_value(v["hits"].clone())?)
    }

    pub(crate) fn get_vectors(&self, collection: &str, id: &str) -> Result<Vec<Vec<f32>>> {
        let v = self.call("get_vectors", json!({"collection": collection, "id": id}))?;
        Ok(serde_json::from_value(v["vectors"].clone())?)
    }
}

/// Error codes cross the wire as strings but live as `&'static str`;
/// map them back onto the registry (docs/errors.md).
fn intern_code(code: &str) -> &'static str {
    const KNOWN: &[&str] = &[
        "file_not_found",
        "not_a_tepin_file",
        "invalid_preamble",
        "format_too_new",
        "database_locked",
        "storage_error",
        "io_error",
        "invalid_json",
        "invalid_document",
        "invalid_filter",
        "invalid_collection_name",
        "duplicate_id",
        "unique_violation",
        "collection_not_found",
        "doc_not_found",
        "not_implemented",
        "destination_exists",
        "migration_failed",
        "model_download_failed",
        "checksum_mismatch",
        "model_load_failed",
        "embedding_failed",
        "embedder_already_attached",
        "embedder_not_attached",
        "embedder_mismatch",
        "collection_not_embedded",
        "invalid_vector",
        "manual_vectors_disabled",
        "serve_failed",
        "serve_disconnected",
        "protocol_mismatch",
    ];
    KNOWN
        .iter()
        .find(|k| **k == code)
        .copied()
        .unwrap_or("remote_error")
}
