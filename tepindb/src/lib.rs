//! # tepindb — the Rust driver
//!
//! One `.tepin` file holds your documents, indexes, vectors, and its own
//! documentation. Zero config: everything about a database lives inside it.
//!
//! ```no_run
//! use serde_json::json;
//!
//! // Vector search wired up (bge-small, lazy-downloaded on first embed):
//! let db = tepindb::open_auto("memory.tepin")?;
//! db.set_embed_fields("notes", &["title", "body"])?;
//! db.insert("notes", json!({"title": "reset flow", "body": "how we reset passwords"}))?;
//!
//! let hits = db.search(None, "how do I reset my password", 5)?;
//! assert_eq!(hits[0].doc["title"], "reset flow");
//!
//! // Plain document store, no model anywhere:
//! let plain = tepindb::open("plain.tepin")?;
//! plain.insert("kv", json!({"k": "v"}))?;
//! # Ok::<(), tepindb::TepinError>(())
//! ```
//!
//! `open()` never touches the network or loads a model. `open_auto()` is
//! the explicit opt-in: it attaches the default model, downloading it once
//! (SHA-256-pinned) into the shared cache on first use — and even then,
//! opening stays instant; only `embed`/`search` wait for readiness.

pub use tepin_core::embed::{cosine, Embedder, Embedding, MockEmbedder};
pub use tepin_core::{CollectionInfo, CollectionMeta, Db, Result, SearchHit, TepinError};
pub use tepin_embed::{EmbedderStatus, LazyEmbedder};

#[cfg(feature = "embedding")]
pub use tepin_embed::{fetch, OnnxEmbedder};

use std::path::Path;

/// Open (or create) a database. No model, no network — pure document store
/// until an embedder is attached.
pub fn open(path: impl AsRef<Path>) -> Result<Db> {
    Db::open(path)
}

/// Open a database that must already exist (read-path semantics: a typo'd
/// path is an error, never a silently created empty db).
pub fn open_existing(path: impl AsRef<Path>) -> Result<Db> {
    Db::open_existing(path)
}

/// Open (or create) a database with the default embedding model attached —
/// the explicit auto mode. Returns instantly: bge-small loads (and on the
/// very first use, downloads into the shared cache) on a background thread,
/// and only `embed`/`search` wait for it.
#[cfg(feature = "embedding")]
pub fn open_auto(path: impl AsRef<Path>) -> Result<Db> {
    let mut db = Db::open(path)?;
    attach_default_embedder(&mut db)?;
    Ok(db)
}

/// Attach the default model (bge-small) to an already-open database.
#[cfg(feature = "embedding")]
pub fn attach_default_embedder(db: &mut Db) -> Result<()> {
    let cache = tepin_embed::fetch::default_cache_dir()?;
    let lazy = OnnxEmbedder::spawn_lazy(&tepin_embed::fetch::BGE_SMALL, cache);
    db.attach_embedder(std::sync::Arc::new(lazy))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn slim_open_is_a_full_document_store() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::open(dir.path().join("d.tepin")).unwrap();
        let id = db.insert("notes", json!({"title": "plain"})).unwrap();
        assert_eq!(db.get("notes", &id).unwrap().unwrap()["title"], "plain");
        assert!(!db.embedder_attached());
    }

    #[test]
    fn custom_embedders_plug_in() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = crate::open(dir.path().join("d.tepin")).unwrap();
        db.attach_embedder(Arc::new(crate::MockEmbedder::new(8)))
            .unwrap();
        db.set_embed_fields("notes", &["title"]).unwrap();
        db.insert("notes", json!({"title": "findable"})).unwrap();
        let hits = db.search(None, "findable", 1).unwrap();
        assert_eq!(hits[0].doc["title"], "findable");
    }
}
