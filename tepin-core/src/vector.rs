//! The write→embed→search pipeline.
//!
//! Inserts into a collection with embed config land instantly: the doc is
//! written together with a row in the persistent pending queue, and a
//! background worker embeds it afterwards. `search()` drains the queue
//! before answering, so search-after-insert always sees everything. The
//! queue lives in the file — a crash leaves flagged docs that heal on the
//! next `attach_embedder`. Embedding never blocks CRUD.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::chunk::{chunk_text, MAX_CHUNKS};
use crate::db::{
    data_table, validate_collection_name, CollectionMeta, Core, Db, COLLECTION_PREFIX, META,
};
use crate::embed::{cosine, Embedder, Embedding};
use crate::error::{Result, TepinError};

/// Persistent embed queue: key = "collection\0id". Survives crashes.
const PENDING: TableDefinition<&str, &[u8]> = TableDefinition::new("__tepin_pending");
/// Meta key recording which model produced this file's vectors.
const EMBEDDER_KEY: &str = "embedder";
const BATCH: usize = 16;

fn vec_table(name: &str) -> String {
    format!("vec:{name}")
}

fn pending_key(collection: &str, id: &str) -> String {
    format!("{collection}\u{0}{id}")
}

/// One vector row per chunk: `{id}\0{chunk_idx}`. Doc ids can't contain
/// control characters (validated on insert), so the separator is
/// unambiguous. A key without a separator is a pre-chunking row read as
/// chunk 0 — old files keep working without migration.
fn chunk_key(id: &str, idx: usize) -> String {
    format!("{id}\u{0}{idx}")
}

fn parse_chunk_key(key: &str) -> (&str, u32) {
    match key.rsplit_once('\u{0}') {
        Some((id, idx)) => match idx.parse() {
            Ok(i) => (id, i),
            Err(_) => (key, 0),
        },
        None => (key, 0),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct EmbedderInfo {
    model_id: String,
    dim: usize,
}

/// One search result. Long documents are chunked at write time (one vector
/// per chunk); a hit is the document's best-matching chunk. `snippet` is
/// that chunk's text verbatim — the relevant excerpt, not the whole blob.
/// `truncated` surfaces loud truncation from write time (rare with
/// chunking: an over-dense single chunk, or a doc past the chunk cap).
#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub collection: String,
    pub id: String,
    pub score: f32,
    /// Index of the best-matching chunk (0 for unchunked short docs).
    pub chunk: u32,
    /// Total chunks this document's embed text splits into.
    pub chunks: u32,
    /// The best-matching chunk's text.
    pub snippet: String,
    pub truncated: bool,
    pub doc: Value,
}

/// One raw vector-search hit (the primitives tier): the document's
/// best-matching chunk by cosine, no keyword fusion, no doc fetch.
#[derive(Debug, Clone, Serialize)]
pub struct VectorHit {
    pub collection: String,
    pub id: String,
    pub chunk: u32,
    pub score: f32,
    pub truncated: bool,
}

/// One raw BM25 keyword hit (the primitives tier). Scores are comparable
/// within one collection; across collections they are raw BM25 sums.
#[derive(Debug, Clone, Serialize)]
pub struct KeywordHit {
    pub collection: String,
    pub id: String,
    pub score: f32,
}

#[derive(Debug, Clone)]
struct StoredError {
    code: &'static str,
    message: String,
    hint: String,
}

impl StoredError {
    fn of(e: &TepinError) -> Self {
        Self {
            code: e.code,
            message: e.message.clone(),
            hint: e.hint.clone(),
        }
    }
    fn rebuild(&self) -> TepinError {
        TepinError::new(self.code, self.message.clone(), self.hint.clone())
    }
}

struct WorkerState {
    wake: bool,
    shutdown: bool,
    processed: u64,
    last_error: Option<StoredError>,
}

struct Shared {
    state: Mutex<WorkerState>,
    cond: Condvar,
}

pub(crate) struct EmbedRuntime {
    pub(crate) embedder: Arc<dyn Embedder>,
    shared: Arc<Shared>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl EmbedRuntime {
    pub(crate) fn nudge(&self) {
        self.shared.state.lock().unwrap().wake = true;
        self.shared.cond.notify_all();
    }
}

impl Drop for EmbedRuntime {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().shutdown = true;
        self.shared.cond.notify_all();
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

impl Db {
    /// Attach an embedder and start the background pipeline. The application
    /// chooses which model loads and when — the core never downloads or
    /// loads anything on its own. Any pending work left by a previous
    /// process (crash, slim build) starts healing immediately.
    pub fn attach_embedder(&mut self, embedder: Arc<dyn Embedder>) -> Result<()> {
        if self.embed.is_some() {
            return Err(TepinError::new(
                "embedder_already_attached",
                "an embedder is already attached to this database handle",
                "attach exactly one embedder per open database",
            ));
        }
        // Vectors from different models must never mix.
        if let Some(info) = self.stored_embedder_info()? {
            if info.model_id != embedder.model_id() || info.dim != embedder.dim() {
                return Err(TepinError::new(
                    "embedder_mismatch",
                    format!(
                        "this file's vectors were produced by {} (dim {}), but the attached embedder is {} (dim {})",
                        info.model_id,
                        info.dim,
                        embedder.model_id(),
                        embedder.dim()
                    ),
                    "attach the original model, or re-embed the whole database with the new one",
                ));
            }
        }

        let shared = Arc::new(Shared {
            state: Mutex::new(WorkerState {
                wake: true, // heal leftovers right away
                shutdown: false,
                processed: 0,
                last_error: None,
            }),
            cond: Condvar::new(),
        });
        let worker = std::thread::Builder::new()
            .name("tepin-embed-worker".into())
            .spawn({
                let core = Arc::clone(&self.core);
                let embedder = Arc::clone(&embedder);
                let shared = Arc::clone(&shared);
                move || worker_loop(&core, embedder.as_ref(), &shared)
            })
            .expect("spawn embed worker");

        self.embed = Some(EmbedRuntime {
            embedder,
            shared,
            worker: Some(worker),
        });
        Ok(())
    }

    pub fn embedder_attached(&self) -> bool {
        self.embed.is_some()
    }

    /// Declare which fields of a collection get embedded automatically.
    /// Every existing document is re-queued (auto-backfill); watch progress
    /// via `pending_embeddings()`.
    pub fn set_embed_fields(&self, collection: &str, fields: &[&str]) -> Result<()> {
        self.configure_embed(collection, fields, false)
    }

    /// Manual vector mode (the primitives tier): the same fields drive the
    /// keyword index, but the application supplies vectors itself via
    /// [`Db::set_vectors`] — nothing is ever queued for auto-embedding and
    /// no model is needed.
    pub fn set_manual_vectors(&self, collection: &str, fields: &[&str]) -> Result<()> {
        self.configure_embed(collection, fields, true)
    }

    fn configure_embed(&self, collection: &str, fields: &[&str], manual: bool) -> Result<()> {
        validate_collection_name(collection)?;
        let txn = self.core.db.begin_write()?;
        {
            let mut meta = txn.open_table(META)?;
            let key = format!("{COLLECTION_PREFIX}{collection}");
            let mut cm: CollectionMeta = match meta.get(key.as_str())? {
                Some(v) => serde_json::from_str(v.value()).unwrap_or_default(),
                None => CollectionMeta::default(),
            };
            cm.embed = fields.iter().map(|f| f.to_string()).collect();
            cm.manual_vectors = manual && !fields.is_empty();
            let json = serde_json::to_string(&cm)?;
            meta.insert(key.as_str(), json.as_str())?;
        }
        {
            // Backfill: queue every existing doc for embedding (auto mode)
            // and rebuild the keyword index in place; manual mode and
            // turning embedding off both clear the queue.
            let table_name = data_table(collection);
            let def: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);
            let docs: Vec<(String, Vec<u8>)> = match txn.open_table(def) {
                Ok(table) => table
                    .iter()?
                    .map(|e| e.map(|(k, v)| (k.value().to_string(), v.value().to_vec())))
                    .collect::<std::result::Result<_, _>>()?,
                Err(redb::TableError::TableDoesNotExist(_)) => Vec::new(),
                Err(e) => return Err(e.into()),
            };
            {
                let mut pending = txn.open_table(PENDING)?;
                for (id, _) in &docs {
                    let key = pending_key(collection, id);
                    if fields.is_empty() || manual {
                        pending.remove(key.as_str())?;
                    } else {
                        pending.insert(key.as_str(), [].as_slice())?;
                    }
                }
            }
            crate::fts::index_clear(&txn, collection)?;
            if !fields.is_empty() {
                let field_names: Vec<String> = fields.iter().map(|f| f.to_string()).collect();
                for (id, bytes) in &docs {
                    if let Ok(doc) = serde_json::from_slice::<Value>(bytes) {
                        let text = build_text(&doc, &field_names);
                        crate::fts::index_add(&txn, collection, id, &text)?;
                    }
                }
            }
        }
        txn.commit()?;
        self.nudge_embed();
        Ok(())
    }

    /// How many documents still await embedding (the backfill/progress gauge).
    pub fn pending_embeddings(&self) -> Result<u64> {
        let txn = self.core.db.begin_read()?;
        match txn.open_table(PENDING) {
            Ok(t) => Ok(t.len()?),
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    /// Block until the embed queue is empty. Called by `search()`; also the
    /// "close() flushes" story. Surfaces the worker's error if it is stuck.
    pub fn flush_embeddings(&self) -> Result<()> {
        let Some(rt) = &self.embed else {
            return Ok(());
        };
        let mut last = (u64::MAX, u64::MAX);
        let mut stalled = 0u32;
        loop {
            let pending = self.pending_embeddings()?;
            if pending == 0 {
                return Ok(());
            }
            let (processed, error) = {
                let s = rt.shared.state.lock().unwrap();
                (s.processed, s.last_error.clone())
            };
            if (pending, processed) == last {
                stalled += 1;
                if stalled > 20 {
                    if let Some(e) = error {
                        return Err(e.rebuild());
                    }
                }
            } else {
                stalled = 0;
                last = (pending, processed);
            }
            rt.nudge();
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    /// Brute-force vector search. `collection: None` searches every embedded
    /// collection — "search everything I know". Drains pending embeddings
    /// first, so a doc inserted a moment ago is already findable.
    pub fn search(
        &self,
        collection: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let rt = self.embed.as_ref().ok_or_else(|| {
            TepinError::new(
                "embedder_not_attached",
                "search needs an embedder, and none is attached",
                "attach one (e.g. tepindb's default model) before calling search",
            )
        })?;
        self.flush_embeddings()?;
        let query_embedding = rt.embedder.embed(query)?;
        let dim = rt.embedder.dim();

        let txn = self.core.db.begin_read()?;
        let all = self.collections()?;
        let targets = embedded_targets(&all, collection)?;

        // Score every chunk, keep each document's best chunk (max-sim).
        let mut hits: Vec<(String, String, f32, bool, u32)> = Vec::new();
        for col in &targets {
            for (id, score, chunk_idx, truncated) in
                best_chunk_hits(&txn, col, &query_embedding.vector, dim)?
            {
                hits.push((col.clone(), id, score, truncated, chunk_idx));
            }
        }

        // Hybrid fusion: score = 0.7·cosine + 0.3·(BM25 / query max).
        // Keyword weight is normalized per query across every target
        // collection, so an exact keyword hit adds up to 0.3 and a query
        // with no term overlap degrades cleanly to pure vector ranking.
        let query_terms = crate::fts::tokenize(query);
        let mut keyword: HashMap<String, HashMap<String, f32>> = HashMap::new();
        let mut max_bm = 0.0f32;
        if !query_terms.is_empty() {
            for col in &targets {
                let scores = crate::fts::bm25_scores(&txn, col, &query_terms)?;
                for &v in scores.values() {
                    max_bm = max_bm.max(v);
                }
                keyword.insert(col.clone(), scores);
            }
        }
        if max_bm > 0.0 {
            for hit in &mut hits {
                let bm = keyword
                    .get(&hit.0)
                    .and_then(|m| m.get(&hit.1))
                    .copied()
                    .unwrap_or(0.0);
                hit.2 = 0.7 * hit.2 + 0.3 * (bm / max_bm);
            }
        }
        hits.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(limit);

        // Fetch winning docs; the deterministic chunker re-derives the
        // best chunk's text as the snippet.
        let embed_fields: HashMap<&str, &[String]> = all
            .iter()
            .map(|c| (c.name.as_str(), c.embed.as_slice()))
            .collect();
        let mut results = Vec::with_capacity(hits.len());
        for (col, id, score, truncated, chunk_idx) in hits {
            let table_name = data_table(&col);
            let def: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);
            let table = match txn.open_table(def) {
                Ok(t) => t,
                Err(redb::TableError::TableDoesNotExist(_)) => continue,
                Err(e) => return Err(e.into()),
            };
            if let Some(bytes) = table.get(id.as_str())? {
                let doc: Value = serde_json::from_slice(bytes.value())?;
                let fields = embed_fields.get(col.as_str()).copied().unwrap_or(&[]);
                let chunks = chunk_text(&build_text(&doc, fields));
                // A mid-backfill doc can briefly disagree with its stored
                // chunk index; fall back rather than fail the search.
                let snippet = chunks
                    .get(chunk_idx as usize)
                    .or_else(|| chunks.first())
                    .cloned()
                    .unwrap_or_default();
                results.push(SearchHit {
                    collection: col,
                    id,
                    score,
                    chunk: chunk_idx,
                    chunks: chunks.len() as u32,
                    snippet,
                    truncated,
                    doc,
                });
            }
        }
        Ok(results)
    }

    /// Supply a document's vectors yourself — the primitives tier. The
    /// collection must be in manual mode ([`Db::set_manual_vectors`]); one
    /// vector stores as chunk 0, several as chunks 0..n (your chunking,
    /// your composition). The first write records `model_id` + dimension;
    /// later writes must match — vectors from different models never mix.
    pub fn set_vectors(
        &self,
        collection: &str,
        id: &str,
        model_id: &str,
        vectors: &[Vec<f32>],
    ) -> Result<()> {
        let dim = match vectors.first() {
            Some(v) if !v.is_empty() => v.len(),
            _ => {
                return Err(TepinError::new(
                    "invalid_vector",
                    "set_vectors needs at least one non-empty vector",
                    "pass one vector per chunk, all with the same dimension",
                ))
            }
        };
        if vectors.iter().any(|v| v.len() != dim) {
            return Err(TepinError::new(
                "invalid_vector",
                "vectors in one call must share a dimension",
                "pass one vector per chunk, all with the same dimension",
            ));
        }

        let txn = self.core.db.begin_write()?;
        {
            let cm = crate::db::collection_meta_in_txn(&txn, collection)?;
            if !cm.manual_vectors {
                return Err(TepinError::new(
                    "manual_vectors_disabled",
                    format!("collection {collection:?} is not in manual vector mode"),
                    "call set_manual_vectors(collection, fields) first; auto-embedded collections own their vectors",
                ));
            }
            let table_name = data_table(collection);
            let def: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);
            let exists = match txn.open_table(def) {
                Ok(t) => t.get(id)?.is_some(),
                Err(redb::TableError::TableDoesNotExist(_)) => false,
                Err(e) => return Err(e.into()),
            };
            if !exists {
                return Err(TepinError::new(
                    "doc_not_found",
                    format!("no document {id:?} in collection {collection:?}"),
                    "insert the document before attaching vectors to it",
                ));
            }

            // Model guard, same rule as attach_embedder.
            let mut meta = txn.open_table(META)?;
            let stored: Option<EmbedderInfo> = meta
                .get(EMBEDDER_KEY)?
                .and_then(|v| serde_json::from_str(v.value()).ok());
            match stored {
                Some(info) => {
                    if info.model_id != model_id || info.dim != dim {
                        return Err(TepinError::new(
                            "embedder_mismatch",
                            format!(
                                "this file's vectors are {} (dim {}), but set_vectors got {} (dim {})",
                                info.model_id, info.dim, model_id, dim
                            ),
                            "keep one model per file, or re-vector the whole database with the new one",
                        ));
                    }
                }
                None => {
                    let info = serde_json::to_string(&EmbedderInfo {
                        model_id: model_id.to_string(),
                        dim,
                    })?;
                    meta.insert(EMBEDDER_KEY, info.as_str())?;
                }
            }
            drop(meta);

            let vec_name = vec_table(collection);
            let vdef: TableDefinition<&str, &[u8]> = TableDefinition::new(&vec_name);
            let mut table = txn.open_table(vdef)?;
            remove_doc_vectors(&mut table, id)?;
            for (idx, v) in vectors.iter().enumerate() {
                table.insert(
                    chunk_key(id, idx).as_str(),
                    encode_vector(false, v).as_slice(),
                )?;
            }
            drop(table);
            let mut pending = txn.open_table(PENDING)?;
            pending.remove(pending_key(collection, id).as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Read a document's stored vectors back, ordered by chunk index.
    /// Empty when the document has no vectors (yet).
    pub fn get_vectors(&self, collection: &str, id: &str) -> Result<Vec<Vec<f32>>> {
        let Some(info) = self.stored_embedder_info()? else {
            return Ok(Vec::new());
        };
        let txn = self.core.db.begin_read()?;
        let vec_name = vec_table(collection);
        let def: TableDefinition<&str, &[u8]> = TableDefinition::new(&vec_name);
        let table = match txn.open_table(def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut chunks: Vec<(u32, Vec<f32>)> = Vec::new();
        if let Some(bytes) = table.get(id)? {
            if let Some((_, v)) = decode_vector(bytes.value(), info.dim) {
                chunks.push((0, v));
            }
        }
        let start = format!("{id}\u{0}");
        let end = format!("{id}\u{1}");
        for entry in table.range(start.as_str()..end.as_str())? {
            let (key, bytes) = entry?;
            let (_, idx) = parse_chunk_key(key.value());
            if let Some((_, v)) = decode_vector(bytes.value(), info.dim) {
                chunks.push((idx, v));
            }
        }
        chunks.sort_by_key(|(idx, _)| *idx);
        Ok(chunks.into_iter().map(|(_, v)| v).collect())
    }

    /// Raw KNN by a caller-supplied query vector — no embedder, no keyword
    /// fusion, no doc fetch; per-document best chunk, best first. Does not
    /// drain the auto-embed queue (manual collections have none).
    pub fn search_by_vector(
        &self,
        collection: Option<&str>,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<VectorHit>> {
        let Some(info) = self.stored_embedder_info()? else {
            return Ok(Vec::new());
        };
        if query.len() != info.dim {
            return Err(TepinError::new(
                "embedder_mismatch",
                format!(
                    "query vector has dim {}, this file's vectors have dim {}",
                    query.len(),
                    info.dim
                ),
                "produce the query vector with the same model that produced the stored vectors",
            ));
        }
        let txn = self.core.db.begin_read()?;
        let all = self.collections()?;
        let targets = embedded_targets(&all, collection)?;
        let mut hits = Vec::new();
        for col in &targets {
            for (id, score, chunk, truncated) in best_chunk_hits(&txn, col, query, info.dim)? {
                hits.push(VectorHit {
                    collection: col.clone(),
                    id,
                    chunk,
                    score,
                    truncated,
                });
            }
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);
        Ok(hits)
    }

    /// Raw BM25 keyword scores for a text query — the keyword half of
    /// hybrid search, exposed for custom fusion. Best first.
    pub fn keyword_search(
        &self,
        collection: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<KeywordHit>> {
        let txn = self.core.db.begin_read()?;
        let all = self.collections()?;
        let targets = embedded_targets(&all, collection)?;
        let terms = crate::fts::tokenize(query);
        let mut hits = Vec::new();
        if !terms.is_empty() {
            for col in &targets {
                for (id, score) in crate::fts::bm25_scores(&txn, col, &terms)? {
                    hits.push(KeywordHit {
                        collection: col.clone(),
                        id,
                        score,
                    });
                }
            }
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);
        Ok(hits)
    }

    /// Clear this file's embedder pin and every stored vector — the
    /// model-swap escape hatch, no full-file rebuild needed. Documents and
    /// the keyword index are untouched. Auto-embed collections are fully
    /// re-queued, so the next attached embedder rebuilds their vectors;
    /// manual collections need fresh `set_vectors` calls. Refused while an
    /// embedder is attached: its worker could race the reset and re-pin
    /// the old model — reset first, then attach the new one.
    pub fn reset_embedder(&self) -> Result<()> {
        if self.embed.is_some() {
            return Err(TepinError::new(
                "embedder_already_attached",
                "reset_embedder needs a handle without an embedder attached",
                "open the file, call reset_embedder, then attach the new model",
            ));
        }
        let all = self.collections()?;
        let txn = self.core.db.begin_write()?;
        {
            let mut meta = txn.open_table(META)?;
            meta.remove(EMBEDDER_KEY)?;
        }
        for col in &all {
            let name = vec_table(&col.name);
            let def: TableDefinition<&str, &[u8]> = TableDefinition::new(&name);
            txn.delete_table(def)?;
        }
        txn.delete_table(PENDING)?;
        for col in &all {
            if col.embed.is_empty() || col.manual_vectors {
                continue;
            }
            let name = data_table(&col.name);
            let def: TableDefinition<&str, &[u8]> = TableDefinition::new(&name);
            let ids: Vec<String> = match txn.open_table(def) {
                Ok(t) => t
                    .iter()?
                    .map(|e| e.map(|(k, _)| k.value().to_string()))
                    .collect::<std::result::Result<_, _>>()?,
                Err(redb::TableError::TableDoesNotExist(_)) => Vec::new(),
                Err(e) => return Err(e.into()),
            };
            queue_pending(&txn, &col.name, &ids)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub(crate) fn nudge_embed(&self) {
        if let Some(rt) = &self.embed {
            rt.nudge();
        }
    }

    fn stored_embedder_info(&self) -> Result<Option<EmbedderInfo>> {
        let txn = self.core.db.begin_read()?;
        let meta = match txn.open_table(META) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(meta
            .get(EMBEDDER_KEY)?
            .and_then(|v| serde_json::from_str(v.value()).ok()))
    }
}

/// Queue docs for embedding inside an open write transaction (insert/update
/// paths call this so doc + queue-row commit atomically).
pub(crate) fn queue_pending<S: AsRef<str>>(
    txn: &redb::WriteTransaction,
    collection: &str,
    ids: &[S],
) -> Result<()> {
    let mut pending = txn.open_table(PENDING)?;
    for id in ids {
        pending.insert(pending_key(collection, id.as_ref()).as_str(), [].as_slice())?;
    }
    Ok(())
}

/// Resolve which collections a search runs over: one named embedded
/// collection, or every embedded collection.
fn embedded_targets(
    all: &[crate::db::CollectionInfo],
    collection: Option<&str>,
) -> Result<Vec<String>> {
    match collection {
        Some(name) => {
            let info = all.iter().find(|c| c.name == name).ok_or_else(|| {
                TepinError::new(
                    "collection_not_found",
                    format!("no collection named {name:?}"),
                    "run `tepin inspect` to list collections",
                )
            })?;
            if info.embed.is_empty() {
                return Err(TepinError::new(
                    "collection_not_embedded",
                    format!("collection {name:?} has no embed fields configured"),
                    "declare them first, e.g. `tepin embed-fields <file> <collection> <field>...`",
                ));
            }
            Ok(vec![name.to_string()])
        }
        None => Ok(all
            .iter()
            .filter(|c| !c.embed.is_empty())
            .map(|c| c.name.clone())
            .collect()),
    }
}

/// Scan one collection's vector rows and keep each document's best chunk
/// (max-sim): (id, score, chunk, truncated).
fn best_chunk_hits(
    txn: &redb::ReadTransaction,
    col: &str,
    query: &[f32],
    dim: usize,
) -> Result<Vec<(String, f32, u32, bool)>> {
    let table_name = vec_table(col);
    let def: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);
    let table = match txn.open_table(def) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut best: HashMap<String, (f32, u32, bool)> = HashMap::new();
    for entry in table.iter()? {
        let (key, bytes) = entry?;
        let (id, chunk_idx) = parse_chunk_key(key.value());
        if let Some((truncated, vector)) = decode_vector(bytes.value(), dim) {
            let score = cosine(query, &vector);
            match best.get_mut(id) {
                Some(b) if b.0 >= score => {}
                Some(b) => *b = (score, chunk_idx, truncated),
                None => {
                    best.insert(id.to_string(), (score, chunk_idx, truncated));
                }
            }
        }
    }
    Ok(best
        .into_iter()
        .map(|(id, (score, chunk, truncated))| (id, score, chunk, truncated))
        .collect())
}

/// Remove a doc's vector and queue rows (the delete path). Covers both
/// chunked rows (`id\0{n}`) and the pre-chunking plain-key row.
pub(crate) fn remove_vector_rows(
    txn: &redb::WriteTransaction,
    collection: &str,
    id: &str,
) -> Result<()> {
    let table_name = vec_table(collection);
    let def: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);
    match txn.open_table(def) {
        Ok(mut t) => remove_doc_vectors(&mut t, id)?,
        Err(redb::TableError::TableDoesNotExist(_)) => {}
        Err(e) => return Err(e.into()),
    }
    let mut pending = txn.open_table(PENDING)?;
    pending.remove(pending_key(collection, id).as_str())?;
    Ok(())
}

/// Remove every vector row belonging to one document.
fn remove_doc_vectors(table: &mut redb::Table<&str, &[u8]>, id: &str) -> Result<()> {
    table.remove(id)?;
    let start = format!("{id}\u{0}");
    let end = format!("{id}\u{1}");
    let keys: Vec<String> = table
        .range(start.as_str()..end.as_str())?
        .map(|e| e.map(|(k, _)| k.value().to_string()))
        .collect::<std::result::Result<_, _>>()?;
    for key in keys {
        table.remove(key.as_str())?;
    }
    Ok(())
}

fn encode_vector(truncated: bool, vector: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + vector.len() * 4);
    out.push(u8::from(truncated));
    for v in vector {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn decode_vector(bytes: &[u8], dim: usize) -> Option<(bool, Vec<f32>)> {
    if bytes.len() != 1 + dim * 4 {
        return None;
    }
    let truncated = bytes[0] != 0;
    let vector = bytes[1..]
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    Some((truncated, vector))
}

struct PendingItem {
    key: String,
    collection: String,
    id: String,
    /// The doc bytes as read; store phase re-checks them so an update that
    /// races the embed re-queues instead of getting a stale vector.
    doc_bytes: Option<Vec<u8>>,
    text: String,
}

fn worker_loop(core: &Core, embedder: &dyn Embedder, shared: &Shared) {
    loop {
        {
            let mut s = shared.state.lock().unwrap();
            while !s.wake && !s.shutdown {
                s = shared.cond.wait(s).unwrap();
            }
            if s.shutdown {
                return;
            }
            s.wake = false;
        }
        // Drain until empty or until an error tells us to wait for a nudge.
        loop {
            if shared.state.lock().unwrap().shutdown {
                return;
            }
            let batch = match read_batch(core) {
                Ok(b) => b,
                Err(e) => {
                    record_error(shared, &e);
                    break;
                }
            };
            if batch.is_empty() {
                break;
            }
            // Embed outside any transaction — inference must never hold
            // the write lock. Long docs are chunked; one vector per chunk.
            let mut embeddings: Vec<Option<Vec<Embedding>>> = Vec::with_capacity(batch.len());
            let mut embed_failed = false;
            'items: for item in &batch {
                if item.doc_bytes.is_none() || item.text.is_empty() {
                    embeddings.push(None);
                    continue;
                }
                let mut chunks = chunk_text(&item.text);
                let capped = chunks.len() > MAX_CHUNKS;
                chunks.truncate(MAX_CHUNKS);
                let mut vecs = Vec::with_capacity(chunks.len());
                for chunk in &chunks {
                    match embedder.embed(chunk) {
                        Ok(e) => vecs.push(e),
                        Err(e) => {
                            record_error(shared, &e);
                            embed_failed = true;
                            break 'items;
                        }
                    }
                }
                if capped {
                    // Text past the chunk cap was not embedded — say so.
                    if let Some(last) = vecs.last_mut() {
                        last.truncated = true;
                    }
                }
                embeddings.push(Some(vecs));
            }
            if embed_failed {
                // Leave the queue as is; retry on the next nudge instead of
                // hot-looping against a broken model.
                break;
            }
            match store_batch(core, embedder, &batch, &embeddings) {
                Ok(stored) => {
                    let mut s = shared.state.lock().unwrap();
                    s.processed += stored;
                    s.last_error = None;
                }
                Err(e) => {
                    record_error(shared, &e);
                    break;
                }
            }
            shared.cond.notify_all();
        }
        shared.cond.notify_all();
    }
}

fn record_error(shared: &Shared, e: &TepinError) {
    shared.state.lock().unwrap().last_error = Some(StoredError::of(e));
    shared.cond.notify_all();
}

fn read_batch(core: &Core) -> Result<Vec<PendingItem>> {
    let txn = core.db.begin_read()?;
    let pending = match txn.open_table(PENDING) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut fields_cache: HashMap<String, Vec<String>> = HashMap::new();
    let mut items = Vec::new();
    for entry in pending.iter()? {
        if items.len() >= BATCH {
            break;
        }
        let (key, _) = entry?;
        let key = key.value().to_string();
        let Some((collection, id)) = key.split_once('\u{0}') else {
            continue;
        };
        let (collection, id) = (collection.to_string(), id.to_string());

        let fields = match fields_cache.get(&collection) {
            Some(f) => f.clone(),
            None => {
                let meta = txn.open_table(META)?;
                let mkey = format!("{COLLECTION_PREFIX}{collection}");
                let f = meta
                    .get(mkey.as_str())?
                    .and_then(|v| serde_json::from_str::<CollectionMeta>(v.value()).ok())
                    .map(|cm| cm.embed)
                    .unwrap_or_default();
                fields_cache.insert(collection.clone(), f.clone());
                f
            }
        };

        let table_name = data_table(&collection);
        let def: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);
        let doc_bytes = match txn.open_table(def) {
            Ok(t) => t.get(id.as_str())?.map(|v| v.value().to_vec()),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(e) => return Err(e.into()),
        };
        let text = doc_bytes
            .as_deref()
            .and_then(|b| serde_json::from_slice::<Value>(b).ok())
            .map(|doc| build_text(&doc, &fields))
            .unwrap_or_default();

        items.push(PendingItem {
            key,
            collection,
            id,
            doc_bytes,
            text,
        });
    }
    Ok(items)
}

fn store_batch(
    core: &Core,
    embedder: &dyn Embedder,
    batch: &[PendingItem],
    embeddings: &[Option<Vec<Embedding>>],
) -> Result<u64> {
    let txn = core.db.begin_write()?;
    let mut stored = 0u64;
    {
        // Record vector provenance with the first vectors ever written.
        let mut meta = txn.open_table(META)?;
        if meta.get(EMBEDDER_KEY)?.is_none() {
            let info = serde_json::to_string(&EmbedderInfo {
                model_id: embedder.model_id().to_string(),
                dim: embedder.dim(),
            })?;
            meta.insert(EMBEDDER_KEY, info.as_str())?;
        }
        drop(meta);

        let mut pending = txn.open_table(PENDING)?;
        for (item, embedding) in batch.iter().zip(embeddings) {
            // Re-check the doc: if it changed while we were embedding, keep
            // the queue row so the new version gets embedded next round.
            let table_name = data_table(&item.collection);
            let def: TableDefinition<&str, &[u8]> = TableDefinition::new(&table_name);
            let current = match txn.open_table(def) {
                Ok(t) => t.get(item.id.as_str())?.map(|v| v.value().to_vec()),
                Err(redb::TableError::TableDoesNotExist(_)) => None,
                Err(e) => return Err(e.into()),
            };
            if current != item.doc_bytes {
                continue;
            }

            let vec_name = vec_table(&item.collection);
            let vdef: TableDefinition<&str, &[u8]> = TableDefinition::new(&vec_name);
            let mut vectors = txn.open_table(vdef)?;
            // Replace, never merge: drop every existing row for this doc
            // (a re-embed may produce fewer chunks; deletes produce none).
            remove_doc_vectors(&mut vectors, &item.id)?;
            if let Some(chunk_vecs) = embedding {
                for (idx, e) in chunk_vecs.iter().enumerate() {
                    vectors.insert(
                        chunk_key(&item.id, idx).as_str(),
                        encode_vector(e.truncated, &e.vector).as_slice(),
                    )?;
                }
            }
            drop(vectors);
            pending.remove(item.key.as_str())?;
            stored += 1;
        }
    }
    txn.commit()?;
    Ok(stored)
}

/// The embedded (and keyword-indexed) text: configured fields in order,
/// joined by blank lines. Strings verbatim; other values as JSON. Missing
/// fields are skipped.
pub(crate) fn build_text(doc: &Value, fields: &[String]) -> String {
    let mut parts = Vec::new();
    for field in fields {
        match doc.get(field) {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) => parts.push(s.clone()),
            Some(other) => parts.push(other.to_string()),
        }
    }
    parts.join("\n\n")
}
