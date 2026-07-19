//! # tepin-core
//!
//! AI-first single-file database for CLI tools and agents.
//!
//! A `.tepin` file is a 4 KiB human/LLM-readable preamble followed by a redb
//! payload — one file holds documents, indexes, vectors, and its own
//! description. Zero external config: everything about a database lives
//! inside the database.
//!
//! ```no_run
//! use tepin_core::Db;
//! use serde_json::json;
//!
//! let db = Db::open("memory.tepin")?;
//! let id = db.insert("notes", json!({"title": "hello tepin"}))?;
//! let hits = db.find("notes", &json!({"title": "hello tepin"}))?;
//! # Ok::<(), tepin_core::TepinError>(())
//! ```

mod db;
pub mod embed;
mod error;
pub mod format;
mod fts;
mod id;
mod vector;

pub use db::{CollectionInfo, CollectionMeta, Db};
pub use error::{Result, TepinError};
pub use vector::SearchHit;
