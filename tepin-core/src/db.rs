//! The document layer: collections of JSON documents on top of redb.
//!
//! Collections appear lazily on first insert (MongoDB-style). Each carries
//! a small meta record — free-text purpose, embed config — stored inside
//! the database file itself: there is no external config, ever.

use std::io::Read;
use std::path::Path;

use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, TepinError};
use crate::format::{self, PreambleBackend, PREAMBLE_LEN};
use crate::id;

pub(crate) const META: TableDefinition<&str, &str> = TableDefinition::new("__tepin_meta");
pub(crate) const COLLECTION_PREFIX: &str = "collection:";

/// Per-collection metadata, stored in the meta table.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CollectionMeta {
    /// Free-text description of what this collection is for — surfaced by
    /// `tepin inspect` so an LLM knows the intent, not just the shape.
    pub purpose: Option<String>,
    /// Fields to embed automatically on write (vector search). Empty = none.
    /// The same fields feed the keyword index in both modes.
    #[serde(default)]
    pub embed: Vec<String>,
    /// Manual vector mode: the application supplies vectors via
    /// `set_vectors` and nothing is queued for auto-embedding.
    #[serde(default)]
    pub manual_vectors: bool,
    /// Fields with a secondary (equality) index.
    #[serde(default)]
    pub indexes: Vec<String>,
    /// The subset of `indexes` that also enforce uniqueness (null-valued
    /// and missing fields are exempt, SQL-style).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unique: Vec<String>,
}

impl CollectionMeta {
    pub(crate) fn is_unique(&self, field: &str) -> bool {
        self.unique.iter().any(|f| f == field)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CollectionInfo {
    pub name: String,
    pub purpose: Option<String>,
    pub embed: Vec<String>,
    /// True when the application supplies vectors itself (`set_vectors`).
    pub manual_vectors: bool,
    /// Fields with a secondary (equality) index.
    pub indexes: Vec<String>,
    /// The subset of `indexes` that also enforce uniqueness.
    pub unique: Vec<String>,
    pub count: u64,
}

/// One write in an atomic [`Db::batch`] — mixed collections welcome.
#[derive(Debug, Clone)]
pub enum BatchOp {
    Insert {
        collection: String,
        doc: Value,
    },
    /// Insert-or-replace by `_id` — see [`Db::upsert`].
    Upsert {
        collection: String,
        doc: Value,
    },
    Update {
        collection: String,
        id: String,
        doc: Value,
    },
    Delete {
        collection: String,
        id: String,
    },
}

/// Read a collection's meta inside a write transaction (default if absent).
pub(crate) fn collection_meta_in_txn(
    txn: &redb::WriteTransaction,
    collection: &str,
) -> Result<CollectionMeta> {
    let meta = txn.open_table(META)?;
    let json = meta
        .get(format!("{COLLECTION_PREFIX}{collection}").as_str())?
        .map(|v| v.value().to_string());
    Ok(json
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default())
}

/// Whether a collection has a meta record. It may still have no data table:
/// `set_purpose` / `create_index` / embed config all register a collection
/// before its first insert, and reads on such a collection are empty, not
/// `collection_not_found`.
fn is_configured(txn: &redb::ReadTransaction, collection: &str) -> Result<bool> {
    let meta = match txn.open_table(META) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    Ok(meta
        .get(format!("{COLLECTION_PREFIX}{collection}").as_str())?
        .is_some())
}

pub struct Db {
    pub(crate) core: std::sync::Arc<Core>,
    /// The embedding pipeline, present once an embedder is attached.
    /// Dropped before `core`'s Arc clone count matters — the worker owns
    /// its own Arc and is joined in EmbedRuntime::drop.
    pub(crate) embed: Option<crate::vector::EmbedRuntime>,
}

/// The storage handle shared between the Db and the embed worker thread.
pub(crate) struct Core {
    pub(crate) db: redb::Database,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db").finish_non_exhaustive()
    }
}

pub(crate) fn data_table(name: &str) -> String {
    format!("col:{name}")
}

pub(crate) fn validate_collection_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        return Err(TepinError::new(
            "invalid_collection_name",
            format!("invalid collection name {name:?}"),
            "collection names are 1-128 bytes with no control characters",
        ));
    }
    Ok(())
}

/// Write one document into an open table, enforcing id semantics:
/// explicit string `_id`s must be new (duplicate → error, never a silent
/// overwrite); generated ids are verified against the table and re-minted
/// on collision (bulk inserts can mint many ids per millisecond).
fn insert_doc(table: &mut redb::Table<&str, &[u8]>, mut doc: Value) -> Result<(String, Value)> {
    let obj = doc.as_object_mut().ok_or_else(|| {
        TepinError::new(
            "invalid_document",
            "documents must be JSON objects",
            "wrap your value in an object, e.g. {\"value\": ...}",
        )
    })?;
    let id = match obj.get("_id") {
        Some(Value::String(explicit)) => {
            // Ids are storage-key material (vector rows are `{id}\0{chunk}`)
            // — control characters would make the encoding ambiguous.
            if explicit.is_empty() || explicit.len() > 256 || explicit.chars().any(char::is_control)
            {
                return Err(TepinError::new(
                    "invalid_document",
                    format!("invalid _id {explicit:?}"),
                    "_id must be 1-256 bytes with no control characters; omit it to auto-generate one",
                ));
            }
            if table.get(explicit.as_str())?.is_some() {
                return Err(TepinError::new(
                    "duplicate_id",
                    format!("a document with _id {explicit:?} already exists"),
                    "use `tepin update` to replace it, or omit _id to auto-generate one",
                ));
            }
            explicit.clone()
        }
        Some(other) => {
            return Err(TepinError::new(
                "invalid_document",
                format!("_id must be a string, got {other}"),
                "omit _id to auto-generate one, or pass it as a string",
            ))
        }
        None => loop {
            let candidate = id::generate();
            if table.get(candidate.as_str())?.is_none() {
                obj.insert("_id".into(), Value::String(candidate.clone()));
                break candidate;
            }
        },
    };
    let bytes = serde_json::to_vec(&doc)?;
    table.insert(id.as_str(), bytes.as_slice())?;
    Ok((id, doc))
}

impl Db {
    /// Open a .tepin file, creating it (with its preamble) if absent.
    /// Opening never blocks on anything heavy — models load elsewhere, later.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let is_new = !path.exists() || std::fs::metadata(path)?.len() == 0;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;

        // Our custom StorageBackend bypasses redb's FileBackend and with it
        // redb's own file locking — so we hold the OS lock ourselves.
        // sqlite-style single-writer semantics: second opener gets an error.
        if let Err(e) = file.try_lock() {
            return Err(TepinError::new(
                "database_locked",
                format!("{} is open in another process", path.display()),
                "close the other process or retry shortly; tepindb allows one process at a time",
            )
            .with_source(std::io::Error::other(e.to_string())));
        }

        if is_new {
            use std::io::Write;
            file.write_all(&format::build_preamble())?;
            file.sync_data()?;
        } else {
            let mut head = vec![0u8; PREAMBLE_LEN as usize];
            let n = file.read(&mut head)?;
            head.truncate(n);
            format::parse_preamble(&head)?;
        }

        let db = redb::Builder::new().create_with_backend(PreambleBackend::new(file))?;
        Ok(Self {
            core: std::sync::Arc::new(Core { db }),
            embed: None,
        })
    }

    /// Open a fresh in-memory database: full engine, zero disk. Made for
    /// test suites and ephemeral scratch stores; everything vanishes on
    /// drop. No file, no preamble, no lock.
    pub fn open_in_memory() -> Result<Self> {
        let db = redb::Builder::new().create_with_backend(redb::backends::InMemoryBackend::new())?;
        Ok(Self {
            core: std::sync::Arc::new(Core { db }),
            embed: None,
        })
    }

    /// Open a .tepin file that must already exist — the read-path variant:
    /// a typo'd path is an error, never a silently created empty database.
    pub fn open_existing(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(TepinError::new(
                "file_not_found",
                format!("no database file at {}", path.display()),
                "check the path (an explicit file argument overrides TEPIN_DB); a new db is created by `tepin insert`",
            ));
        }
        Self::open(path)
    }

    /// Start building an open call with options — retry, and (eventually)
    /// anything else `open` itself shouldn't grow a parameter for.
    pub fn options() -> OpenOptions {
        OpenOptions::default()
    }

    /// Insert a JSON object; returns its id. Assigns a short sortable `_id`
    /// unless the document already carries one (a duplicate `_id` is an
    /// error, never a silent overwrite). Creates the collection lazily.
    pub fn insert(&self, collection: &str, doc: Value) -> Result<String> {
        let mut ids = self.insert_many(collection, vec![doc])?;
        Ok(ids.pop().expect("one doc in, one id out"))
    }

    /// Insert a batch atomically: either every document lands or none does.
    /// For multi-collection atomicity, see [`Db::batch`].
    pub fn insert_many(&self, collection: &str, docs: Vec<Value>) -> Result<Vec<String>> {
        let txn = self.core.db.begin_write()?;
        let ids = self.insert_in_txn(&txn, collection, docs)?;
        txn.commit()?;
        self.nudge_embed();
        Ok(ids)
    }

    fn insert_in_txn(
        &self,
        txn: &redb::WriteTransaction,
        collection: &str,
        docs: Vec<Value>,
    ) -> Result<Vec<String>> {
        validate_collection_name(collection)?;
        let mut ids = Vec::with_capacity(docs.len());
        let mut stored_docs = Vec::with_capacity(docs.len());
        {
            let table_name = data_table(collection);
            let def: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);
            let mut table = txn.open_table(def)?;
            for doc in docs {
                let (id, stored) = insert_doc(&mut table, doc)?;
                ids.push(id);
                stored_docs.push(stored);
            }
        }
        let meta_key = format!("{COLLECTION_PREFIX}{collection}");
        let mut meta = txn.open_table(META)?;
        let existing = meta.get(meta_key.as_str())?.map(|v| v.value().to_string());
        let cm: CollectionMeta = match existing {
            Some(json) => serde_json::from_str(&json).unwrap_or_default(),
            None => {
                let default = CollectionMeta::default();
                meta.insert(meta_key.as_str(), serde_json::to_string(&default)?.as_str())?;
                default
            }
        };
        drop(meta);
        if !cm.embed.is_empty() {
            if !cm.manual_vectors {
                crate::vector::queue_pending(txn, collection, &ids)?;
            }
            for (id, doc) in ids.iter().zip(&stored_docs) {
                let text = crate::vector::build_text(doc, &cm.embed);
                crate::fts::index_add(txn, collection, id, &text)?;
            }
        }
        for field in &cm.indexes {
            for (id, doc) in ids.iter().zip(&stored_docs) {
                crate::index::index_add(txn, collection, field, doc, id, cm.is_unique(field))?;
            }
        }
        Ok(ids)
    }

    /// Insert-or-replace by `_id`: a document whose `_id` already exists is
    /// replaced (update semantics — indexes swap, embeddings re-queue);
    /// anything else inserts, minting an id when the document has none.
    /// Returns the document's id either way.
    pub fn upsert(&self, collection: &str, doc: Value) -> Result<String> {
        let txn = self.core.db.begin_write()?;
        let id = self.upsert_in_txn(&txn, collection, doc)?;
        txn.commit()?;
        self.nudge_embed();
        Ok(id)
    }

    fn upsert_in_txn(
        &self,
        txn: &redb::WriteTransaction,
        collection: &str,
        doc: Value,
    ) -> Result<String> {
        validate_collection_name(collection)?;
        // Only a stored doc under the same string `_id` routes to replace;
        // every other shape (no _id, non-string _id, unknown id) takes the
        // insert path, which owns the id validation.
        let existing = match doc.get("_id") {
            Some(Value::String(id)) => {
                let table_name = data_table(collection);
                let def: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);
                let table = txn.open_table(def)?;
                let stored = table.get(id.as_str())?.is_some();
                stored.then(|| id.clone())
            }
            _ => None,
        };
        match existing {
            Some(id) => {
                self.update_in_txn(txn, collection, &id, doc)?;
                Ok(id)
            }
            None => Ok(self
                .insert_in_txn(txn, collection, vec![doc])?
                .pop()
                .expect("one doc in, one id out")),
        }
    }

    pub fn get(&self, collection: &str, id: &str) -> Result<Option<Value>> {
        let txn = self.core.db.begin_read()?;
        let table_name = data_table(collection);
        let def: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);
        let table = match txn.open_table(def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return if is_configured(&txn, collection)? {
                    Ok(None) // configured, nothing inserted yet
                } else {
                    Err(self.unknown_collection(collection))
                };
            }
            Err(e) => return Err(e.into()),
        };
        match table.get(id)? {
            Some(bytes) => Ok(Some(serde_json::from_slice(bytes.value())?)),
            None => Ok(None),
        }
    }

    /// Find documents matching a MongoDB-style filter. `{}` matches all.
    /// When a filter field has an equality condition and a secondary index
    /// (see [`Db::create_index`]), candidates come from the index instead
    /// of a full scan; every candidate is still verified against the full
    /// filter, so results are identical either way.
    pub fn find(&self, collection: &str, filter: &Value) -> Result<Vec<Value>> {
        let filter_obj = filter.as_object().ok_or_else(|| {
            TepinError::new(
                "invalid_filter",
                "filters must be JSON objects",
                "use {} to match everything, or {\"field\": \"value\"}",
            )
        })?;
        let txn = self.core.db.begin_read()?;
        let table_name = data_table(collection);
        let def: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);
        let table = match txn.open_table(def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return if is_configured(&txn, collection)? {
                    Ok(Vec::new()) // configured, nothing inserted yet
                } else {
                    Err(self.unknown_collection(collection))
                };
            }
            Err(e) => return Err(e.into()),
        };

        // Planner: first indexed field carrying a direct equality wins.
        let indexes = self.collection_indexes(collection)?;
        let indexed_eq = filter_obj.iter().find_map(|(field, cond)| {
            if !indexes.iter().any(|i| i == field) {
                return None;
            }
            match cond {
                Value::Object(ops) if ops.keys().any(|k| k.starts_with('$')) => {
                    ops.get("$eq").map(|v| (field.as_str(), v))
                }
                direct => Some((field.as_str(), direct)),
            }
        });

        let mut out = Vec::new();
        if let Some((field, value)) = indexed_eq {
            let mut ids = crate::index::candidates(&txn, collection, field, value)?;
            ids.sort(); // same id order a full scan would produce
            for id in ids {
                if let Some(bytes) = table.get(id.as_str())? {
                    let doc: Value = serde_json::from_slice(bytes.value())?;
                    if matches_filter(&doc, filter_obj)? {
                        out.push(doc);
                    }
                }
            }
        } else {
            for entry in table.iter()? {
                let (_, bytes) = entry?;
                let doc: Value = serde_json::from_slice(bytes.value())?;
                if matches_filter(&doc, filter_obj)? {
                    out.push(doc);
                }
            }
        }
        Ok(out)
    }

    fn collection_indexes(&self, collection: &str) -> Result<Vec<String>> {
        Ok(self
            .collections()?
            .into_iter()
            .find(|c| c.name == collection)
            .map(|c| c.indexes)
            .unwrap_or_default())
    }

    /// Create an equality index on a field (idempotent) and backfill it
    /// from every existing document. `find` uses it automatically.
    pub fn create_index(&self, collection: &str, field: &str) -> Result<()> {
        self.create_index_inner(collection, field, false)
    }

    /// Create an equality index that also enforces uniqueness: a second
    /// document with the same value is rejected (`unique_violation`).
    /// Null-valued and missing fields are exempt, SQL-style. Backfill
    /// verifies existing documents — duplicates fail the whole call.
    pub fn create_unique_index(&self, collection: &str, field: &str) -> Result<()> {
        self.create_index_inner(collection, field, true)
    }

    fn create_index_inner(&self, collection: &str, field: &str, unique: bool) -> Result<()> {
        validate_collection_name(collection)?;
        let txn = self.core.db.begin_write()?;
        {
            let mut cm = collection_meta_in_txn(&txn, collection)?;
            if !cm.indexes.iter().any(|i| i == field) {
                cm.indexes.push(field.to_string());
            }
            if unique && !cm.is_unique(field) {
                cm.unique.push(field.to_string());
            }
            let mut meta = txn.open_table(META)?;
            let key = format!("{COLLECTION_PREFIX}{collection}");
            meta.insert(key.as_str(), serde_json::to_string(&cm)?.as_str())?;
        }
        {
            let table_name = data_table(collection);
            let def: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);
            let docs: Vec<(String, Value)> = match txn.open_table(def) {
                Ok(t) => t
                    .iter()?
                    .filter_map(|e| {
                        e.map(|(k, v)| {
                            serde_json::from_slice(v.value())
                                .ok()
                                .map(|doc| (k.value().to_string(), doc))
                        })
                        .transpose()
                    })
                    .collect::<std::result::Result<_, _>>()?,
                Err(redb::TableError::TableDoesNotExist(_)) => Vec::new(),
                Err(e) => return Err(e.into()),
            };
            for (id, doc) in &docs {
                crate::index::index_add(&txn, collection, field, doc, id, unique)?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Drop a field's equality index (idempotent), unique or not. Data is
    /// untouched; `find` falls back to scanning.
    pub fn drop_index(&self, collection: &str, field: &str) -> Result<()> {
        let txn = self.core.db.begin_write()?;
        {
            let mut cm = collection_meta_in_txn(&txn, collection)?;
            cm.indexes.retain(|i| i != field);
            cm.unique.retain(|i| i != field);
            let mut meta = txn.open_table(META)?;
            let key = format!("{COLLECTION_PREFIX}{collection}");
            meta.insert(key.as_str(), serde_json::to_string(&cm)?.as_str())?;
        }
        crate::index::drop_index_table(&txn, collection, field)?;
        txn.commit()?;
        Ok(())
    }

    /// Replace a document by id. The stored `_id` always wins.
    pub fn update(&self, collection: &str, id: &str, doc: Value) -> Result<()> {
        let txn = self.core.db.begin_write()?;
        self.update_in_txn(&txn, collection, id, doc)?;
        txn.commit()?;
        self.nudge_embed();
        Ok(())
    }

    fn update_in_txn(
        &self,
        txn: &redb::WriteTransaction,
        collection: &str,
        id: &str,
        mut doc: Value,
    ) -> Result<()> {
        let obj = doc.as_object_mut().ok_or_else(|| {
            TepinError::new(
                "invalid_document",
                "documents must be JSON objects",
                "wrap your value in an object, e.g. {\"value\": ...}",
            )
        })?;
        obj.insert("_id".into(), Value::String(id.to_string()));
        let bytes = serde_json::to_vec(&doc)?;

        let old_bytes;
        {
            let table_name = data_table(collection);
            let def: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);
            let mut table = match txn.open_table(def) {
                Ok(t) => t,
                Err(redb::TableError::TableDoesNotExist(_)) => {
                    return Err(self.unknown_collection(collection))
                }
                Err(e) => return Err(e.into()),
            };
            old_bytes = match table.get(id)? {
                Some(v) => v.value().to_vec(),
                None => return Err(self.unknown_doc(collection, id)),
            };
            table.insert(id, bytes.as_slice())?;
        }
        let old_doc = serde_json::from_slice::<Value>(&old_bytes).ok();
        // The old vector and keyword entries are now stale — re-queue the
        // embedding and swap the index entries in the same transaction.
        let cm = collection_meta_in_txn(txn, collection)?;
        if !cm.embed.is_empty() {
            if !cm.manual_vectors {
                crate::vector::queue_pending(txn, collection, &[id])?;
            }
            if let Some(old) = &old_doc {
                let old_text = crate::vector::build_text(old, &cm.embed);
                crate::fts::index_remove(txn, collection, id, &old_text)?;
            }
            let new_text = crate::vector::build_text(&doc, &cm.embed);
            crate::fts::index_add(txn, collection, id, &new_text)?;
        }
        for field in &cm.indexes {
            if let Some(old) = &old_doc {
                crate::index::index_remove(txn, collection, field, old, id)?;
            }
            crate::index::index_add(txn, collection, field, &doc, id, cm.is_unique(field))?;
        }
        Ok(())
    }

    pub fn delete(&self, collection: &str, id: &str) -> Result<()> {
        let txn = self.core.db.begin_write()?;
        self.delete_in_txn(&txn, collection, id)?;
        txn.commit()?;
        Ok(())
    }

    fn delete_in_txn(
        &self,
        txn: &redb::WriteTransaction,
        collection: &str,
        id: &str,
    ) -> Result<()> {
        let old_bytes;
        {
            let table_name = data_table(collection);
            let def: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);
            let mut table = match txn.open_table(def) {
                Ok(t) => t,
                Err(redb::TableError::TableDoesNotExist(_)) => {
                    return Err(self.unknown_collection(collection))
                }
                Err(e) => return Err(e.into()),
            };
            old_bytes = match table.remove(id)? {
                Some(v) => v.value().to_vec(),
                None => return Err(self.unknown_doc(collection, id)),
            };
        }
        crate::vector::remove_vector_rows(txn, collection, id)?;
        let old_doc = serde_json::from_slice::<Value>(&old_bytes).ok();
        let cm = collection_meta_in_txn(txn, collection)?;
        if !cm.embed.is_empty() {
            if let Some(old) = &old_doc {
                let old_text = crate::vector::build_text(old, &cm.embed);
                crate::fts::index_remove(txn, collection, id, &old_text)?;
            }
        }
        for field in &cm.indexes {
            if let Some(old) = &old_doc {
                crate::index::index_remove(txn, collection, field, old, id)?;
            }
        }
        Ok(())
    }

    /// Apply a mixed sequence of writes across any collections in ONE
    /// atomic transaction: either every operation lands or none does.
    /// Returns the ids of inserted and upserted documents, in operation order.
    pub fn batch(&self, ops: Vec<BatchOp>) -> Result<Vec<String>> {
        let txn = self.core.db.begin_write()?;
        let mut inserted = Vec::new();
        for op in ops {
            match op {
                BatchOp::Insert { collection, doc } => {
                    inserted.extend(self.insert_in_txn(&txn, &collection, vec![doc])?);
                }
                BatchOp::Upsert { collection, doc } => {
                    inserted.push(self.upsert_in_txn(&txn, &collection, doc)?);
                }
                BatchOp::Update {
                    collection,
                    id,
                    doc,
                } => self.update_in_txn(&txn, &collection, &id, doc)?,
                BatchOp::Delete { collection, id } => self.delete_in_txn(&txn, &collection, &id)?,
            }
        }
        txn.commit()?;
        self.nudge_embed();
        Ok(inserted)
    }

    /// Set the free-text purpose of a collection (creates its meta if needed).
    pub fn set_purpose(&self, collection: &str, purpose: &str) -> Result<()> {
        self.update_meta(collection, |m| m.purpose = Some(purpose.to_string()))
    }

    pub fn collections(&self) -> Result<Vec<CollectionInfo>> {
        let txn = self.core.db.begin_read()?;
        let meta = match txn.open_table(META) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in meta.iter()? {
            let (key, val) = entry?;
            let Some(name) = key.value().strip_prefix(COLLECTION_PREFIX) else {
                continue;
            };
            let cm: CollectionMeta = serde_json::from_str(val.value()).unwrap_or_default();
            let table_name = data_table(name);
            let def: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);
            let count = match txn.open_table(def) {
                Ok(t) => t.len()?,
                Err(redb::TableError::TableDoesNotExist(_)) => 0,
                Err(e) => return Err(e.into()),
            };
            out.push(CollectionInfo {
                name: name.to_string(),
                purpose: cm.purpose,
                embed: cm.embed,
                manual_vectors: cm.manual_vectors,
                indexes: cm.indexes,
                unique: cm.unique,
                count,
            });
        }
        Ok(out)
    }

    fn update_meta(&self, collection: &str, f: impl FnOnce(&mut CollectionMeta)) -> Result<()> {
        let txn = self.core.db.begin_write()?;
        {
            let mut meta = txn.open_table(META)?;
            let key = format!("{COLLECTION_PREFIX}{collection}");
            let mut cm: CollectionMeta = match meta.get(key.as_str())? {
                Some(v) => serde_json::from_str(v.value()).unwrap_or_default(),
                None => CollectionMeta::default(),
            };
            f(&mut cm);
            let json = serde_json::to_string(&cm)?;
            meta.insert(key.as_str(), json.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    fn unknown_collection(&self, collection: &str) -> TepinError {
        let known = self
            .collections()
            .map(|cs| {
                cs.into_iter()
                    .map(|c| c.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        TepinError::new(
            "collection_not_found",
            format!("no collection named {collection:?}"),
            if known.is_empty() {
                "this database has no collections yet; `tepin insert` creates one".to_string()
            } else {
                format!("existing collections: {known}; run `tepin inspect` for details")
            },
        )
    }

    fn unknown_doc(&self, collection: &str, id: &str) -> TepinError {
        TepinError::new(
            "doc_not_found",
            format!("no document {id:?} in collection {collection:?}"),
            "run `tepin query` with a filter to find the id you meant",
        )
    }
}

/// The minimal MongoDB-style filter subset for v0:
/// equality per field, plus $eq/$ne/$gt/$gte/$lt/$lte/$in on a field.
fn matches_filter(doc: &Value, filter: &serde_json::Map<String, Value>) -> Result<bool> {
    for (field, cond) in filter {
        let actual = doc.get(field).unwrap_or(&Value::Null);
        let ok = match cond {
            Value::Object(ops) if ops.keys().any(|k| k.starts_with('$')) => {
                let mut all = true;
                for (op, expected) in ops {
                    all &= match op.as_str() {
                        "$eq" => values_equal(actual, expected),
                        "$ne" => !values_equal(actual, expected),
                        "$gt" => cmp_order(actual, expected) == Some(std::cmp::Ordering::Greater),
                        "$gte" => matches!(
                            cmp_order(actual, expected),
                            Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
                        ),
                        "$lt" => cmp_order(actual, expected) == Some(std::cmp::Ordering::Less),
                        "$lte" => matches!(
                            cmp_order(actual, expected),
                            Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
                        ),
                        "$in" => expected
                            .as_array()
                            .map(|arr| arr.iter().any(|e| values_equal(actual, e)))
                            .unwrap_or(false),
                        other => {
                            return Err(TepinError::new(
                                "invalid_filter",
                                format!("unsupported filter operator {other:?}"),
                                "v0 supports $eq, $ne, $gt, $gte, $lt, $lte, $in",
                            ))
                        }
                    };
                }
                all
            }
            expected => values_equal(actual, expected),
        };
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Options for opening a database. Built via [`Db::options`]:
///
/// ```no_run
/// # use tepin_core::Db;
/// # use std::time::Duration;
/// let db = Db::options()
///     .retry_for(Duration::from_secs(2))
///     .open("memory.tepin")?;
/// # Ok::<(), tepin_core::TepinError>(())
/// ```
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    retry_for: Option<std::time::Duration>,
}

impl OpenOptions {
    /// Keep retrying a `database_locked` open with backoff for up to this
    /// long — the cure for two processes racing to open at cold start.
    /// Any other error still fails immediately.
    pub fn retry_for(mut self, wait: std::time::Duration) -> Self {
        self.retry_for = Some(wait);
        self
    }

    /// Open a .tepin file with these options, creating it if absent.
    pub fn open(&self, path: impl AsRef<Path>) -> Result<Db> {
        let path = path.as_ref();
        let deadline = self.retry_for.map(|w| std::time::Instant::now() + w);
        let mut delay = std::time::Duration::from_millis(10);
        loop {
            match Db::open(path) {
                Err(e) if e.code == "database_locked" => {
                    let retry = deadline.is_some_and(|d| std::time::Instant::now() + delay <= d);
                    if !retry {
                        return Err(e);
                    }
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(std::time::Duration::from_millis(250));
                }
                other => return other,
            }
        }
    }
}

/// Mongo-style equality: numbers compare numerically (5 == 5.0),
/// everything else compares structurally.
pub(crate) fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => x == y,
            _ => a == b,
        },
        _ => a == b,
    }
}

fn cmp_order(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64()?.partial_cmp(&y.as_f64()?),
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn open_temp() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("test.tepin")).unwrap();
        (dir, db)
    }

    #[test]
    fn insert_get_find_update_delete_round_trip() {
        let (_dir, db) = open_temp();
        let id = db
            .insert("notes", json!({"title": "hello", "stars": 3}))
            .unwrap();

        let doc = db.get("notes", &id).unwrap().unwrap();
        assert_eq!(doc["title"], "hello");
        assert_eq!(doc["_id"], Value::String(id.clone()));

        let hits = db.find("notes", &json!({"stars": {"$gte": 3}})).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(db
            .find("notes", &json!({"stars": {"$lt": 3}}))
            .unwrap()
            .is_empty());

        db.update("notes", &id, json!({"title": "hello2"})).unwrap();
        assert_eq!(db.get("notes", &id).unwrap().unwrap()["title"], "hello2");

        db.delete("notes", &id).unwrap();
        assert_eq!(db.get("notes", &id).unwrap(), None);
    }

    #[test]
    fn file_reopens_and_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("persist.tepin");
        let id = {
            let db = Db::open(&path).unwrap();
            db.insert("things", json!({"k": "v"})).unwrap()
        };
        let db = Db::open(&path).unwrap();
        assert_eq!(db.get("things", &id).unwrap().unwrap()["k"], "v");
    }

    #[test]
    fn file_head_is_readable_documentation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("readable.tepin");
        Db::open(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let head = String::from_utf8_lossy(&bytes[..256]);
        assert!(head.starts_with("tepindb"));
    }

    #[test]
    fn collections_report_purpose_and_count() {
        let (_dir, db) = open_temp();
        db.insert("notes", json!({"a": 1})).unwrap();
        db.insert("notes", json!({"a": 2})).unwrap();
        db.set_purpose("notes", "scratch notes for tests").unwrap();

        let cols = db.collections().unwrap();
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].name, "notes");
        assert_eq!(cols[0].count, 2);
        assert_eq!(cols[0].purpose.as_deref(), Some("scratch notes for tests"));
    }

    #[test]
    fn errors_carry_code_and_hint() {
        let (_dir, db) = open_temp();
        let err = db.get("nope", "someid").unwrap_err();
        assert_eq!(err.code, "collection_not_found");
        assert!(!err.hint.is_empty());
    }
}
