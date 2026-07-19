//! The write→embed→search pipeline, driven through the public API with the
//! deterministic MockEmbedder: consistency (search sees a doc inserted a
//! moment ago), backfill, re-embedding on update, cleanup on delete,
//! crash-healing of the pending queue, and model provenance enforcement.

use std::sync::Arc;

use serde_json::json;
use tepin_core::embed::MockEmbedder;
use tepin_core::Db;

fn open_with_mock(path: &std::path::Path) -> Db {
    let mut db = Db::open(path).unwrap();
    db.attach_embedder(Arc::new(MockEmbedder::new(16))).unwrap();
    db
}

#[test]
fn insert_then_search_immediately_finds_the_doc() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with_mock(&dir.path().join("t.tepin"));
    db.set_embed_fields("notes", &["title"]).unwrap();

    let id = db.insert("notes", json!({"title": "alpha"})).unwrap();
    // No sleep: search must drain the queue before answering.
    let hits = db.search(Some("notes"), "alpha", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, id);
    assert_eq!(hits[0].collection, "notes");
    assert_eq!(hits[0].doc["title"], "alpha");
    // Mock is deterministic: identical text scores ~1.0 and wins outright.
    assert!(hits[0].score > 0.99);
    assert_eq!(db.pending_embeddings().unwrap(), 0);
}

#[test]
fn search_ranks_exact_text_first_and_respects_limit() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with_mock(&dir.path().join("t.tepin"));
    db.set_embed_fields("notes", &["title"]).unwrap();
    db.insert_many(
        "notes",
        vec![
            json!({"title": "the quick brown fox"}),
            json!({"title": "an unrelated topic"}),
            json!({"title": "another different thing"}),
        ],
    )
    .unwrap();

    let hits = db.search(Some("notes"), "the quick brown fox", 2).unwrap();
    assert_eq!(hits.len(), 2, "limit respected");
    assert_eq!(hits[0].doc["title"], "the quick brown fox");
    assert!(hits[0].score > hits[1].score, "scores sorted descending");
}

#[test]
fn backfill_embeds_docs_that_existed_before_config() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with_mock(&dir.path().join("t.tepin"));
    let docs: Vec<_> = (0..50)
        .map(|i| json!({"title": format!("doc number {i}")}))
        .collect();
    db.insert_many("notes", docs).unwrap();

    // Config arrives after the data: auto-backfill kicks in.
    db.set_embed_fields("notes", &["title"]).unwrap();
    db.flush_embeddings().unwrap();
    assert_eq!(db.pending_embeddings().unwrap(), 0);

    let hits = db.search(Some("notes"), "doc number 31", 3).unwrap();
    assert_eq!(hits[0].doc["title"], "doc number 31");
}

#[test]
fn update_reembeds_and_delete_removes_from_search() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with_mock(&dir.path().join("t.tepin"));
    db.set_embed_fields("notes", &["title"]).unwrap();

    let id = db
        .insert("notes", json!({"title": "original text"}))
        .unwrap();
    db.update("notes", &id, json!({"title": "completely new text"}))
        .unwrap();
    let hits = db.search(Some("notes"), "completely new text", 1).unwrap();
    assert_eq!(hits[0].id, id);
    assert!(hits[0].score > 0.99, "vector reflects the updated text");

    db.delete("notes", &id).unwrap();
    let hits = db.search(Some("notes"), "completely new text", 10).unwrap();
    assert!(hits.is_empty(), "deleted docs must leave no vector behind");
}

#[test]
fn db_wide_search_spans_collections_and_skips_unembedded() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with_mock(&dir.path().join("t.tepin"));
    db.set_embed_fields("notes", &["title"]).unwrap();
    db.set_embed_fields("tasks", &["summary"]).unwrap();

    db.insert("notes", json!({"title": "grocery list"}))
        .unwrap();
    db.insert("tasks", json!({"summary": "buy groceries"}))
        .unwrap();
    db.insert("logs", json!({"line": "grocery list"})).unwrap(); // not embedded

    let hits = db.search(None, "grocery list", 10).unwrap();
    let collections: Vec<_> = hits.iter().map(|h| h.collection.as_str()).collect();
    assert!(collections.contains(&"notes"));
    assert!(collections.contains(&"tasks"));
    assert!(
        !collections.contains(&"logs"),
        "unembedded collections are skipped"
    );
}

#[test]
fn multiple_embed_fields_are_joined() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with_mock(&dir.path().join("t.tepin"));
    db.set_embed_fields("notes", &["title", "body"]).unwrap();
    let id = db
        .insert("notes", json!({"title": "part one", "body": "part two"}))
        .unwrap();
    // Mock hashes the joined text, so only the exact join matches perfectly.
    let hits = db.search(Some("notes"), "part one\n\npart two", 1).unwrap();
    assert_eq!(hits[0].id, id);
    assert!(hits[0].score > 0.99);
}

#[test]
fn pending_queue_survives_crash_and_heals_on_attach() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crash.tepin");
    let id;
    {
        // A slim process (no embedder) writes into an embedded collection —
        // exactly what a crash mid-pipeline also leaves behind.
        let db = Db::open(&path).unwrap();
        db.set_embed_fields("notes", &["title"]).unwrap();
        id = db
            .insert("notes", json!({"title": "written while slim"}))
            .unwrap();
        assert!(db.pending_embeddings().unwrap() > 0);

        let err = db.search(Some("notes"), "anything", 1).unwrap_err();
        assert_eq!(err.code, "embedder_not_attached");
    }
    // Reopen with an embedder: leftovers heal without being asked.
    let db = open_with_mock(&path);
    db.flush_embeddings().unwrap();
    assert_eq!(db.pending_embeddings().unwrap(), 0);
    let hits = db.search(Some("notes"), "written while slim", 1).unwrap();
    assert_eq!(hits[0].id, id);
}

#[test]
fn model_provenance_is_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prov.tepin");
    {
        let db = open_with_mock(&path);
        db.set_embed_fields("notes", &["title"]).unwrap();
        db.insert("notes", json!({"title": "creates vectors"}))
            .unwrap();
        db.flush_embeddings().unwrap();
    }
    // Same model id, different dim → refuse.
    let mut db = Db::open(&path).unwrap();
    let err = db
        .attach_embedder(Arc::new(MockEmbedder::new(32)))
        .unwrap_err();
    assert_eq!(err.code, "embedder_mismatch");
    assert!(err.message.contains("mock"));
    drop(db); // shadowing alone would keep the file lock held

    // The right embedder still attaches fine.
    let mut db = Db::open(&path).unwrap();
    db.attach_embedder(Arc::new(MockEmbedder::new(16))).unwrap();
}

#[test]
fn search_errors_are_specific() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with_mock(&dir.path().join("t.tepin"));
    db.insert("plain", json!({"v": 1})).unwrap();

    let err = db.search(Some("ghost"), "q", 1).unwrap_err();
    assert_eq!(err.code, "collection_not_found");

    let err = db.search(Some("plain"), "q", 1).unwrap_err();
    assert_eq!(err.code, "collection_not_embedded");

    // Double attach is refused.
    let mut db2 = Db::open(dir.path().join("t2.tepin")).unwrap();
    db2.attach_embedder(Arc::new(MockEmbedder::new(16)))
        .unwrap();
    let err = db2
        .attach_embedder(Arc::new(MockEmbedder::new(16)))
        .unwrap_err();
    assert_eq!(err.code, "embedder_already_attached");
}

#[test]
fn disabling_embedding_clears_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.tepin");
    let db = Db::open(&path).unwrap(); // slim: queue only grows
    db.set_embed_fields("notes", &["title"]).unwrap();
    db.insert("notes", json!({"title": "queued"})).unwrap();
    assert!(db.pending_embeddings().unwrap() > 0);

    db.set_embed_fields("notes", &[]).unwrap();
    assert_eq!(db.pending_embeddings().unwrap(), 0);
}

#[test]
fn crud_never_blocks_on_a_slow_embedder() {
    use tepin_core::embed::{Embedder, Embedding};

    struct SlowEmbedder;
    impl Embedder for SlowEmbedder {
        fn model_id(&self) -> &str {
            "mock" // matches MockEmbedder's id family; fresh file anyway
        }
        fn dim(&self) -> usize {
            4
        }
        fn embed(&self, _: &str) -> tepin_core::Result<Embedding> {
            std::thread::sleep(std::time::Duration::from_millis(120));
            Ok(Embedding {
                vector: vec![0.5; 4],
                truncated: false,
            })
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open(dir.path().join("t.tepin")).unwrap();
    db.attach_embedder(Arc::new(SlowEmbedder)).unwrap();
    db.set_embed_fields("notes", &["title"]).unwrap();

    let t0 = std::time::Instant::now();
    for i in 0..5 {
        db.insert("notes", json!({"title": format!("doc {i}")}))
            .unwrap();
        db.find("notes", &json!({})).unwrap();
    }
    assert!(
        t0.elapsed() < std::time::Duration::from_millis(300),
        "5 inserts + finds took {:?} — CRUD must not wait for embedding",
        t0.elapsed()
    );
    db.flush_embeddings().unwrap();
    assert_eq!(db.pending_embeddings().unwrap(), 0);
}
