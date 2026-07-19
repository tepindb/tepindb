//! Hybrid (BM25 + vector) search. The MockEmbedder produces meaningless
//! vectors, so any correct ranking on a *paraphrased* query here is proof
//! the keyword signal works: with mock vectors, only BM25 can connect
//! "beta gamma" to the doc containing those words.

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
fn keywords_rescue_what_vectors_miss() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with_mock(&dir.path().join("t.tepin"));
    db.set_embed_fields("notes", &["text"]).unwrap();
    let target = db
        .insert("notes", json!({"text": "alpha beta gamma delta"}))
        .unwrap();
    db.insert("notes", json!({"text": "completely unrelated words"}))
        .unwrap();
    db.insert("notes", json!({"text": "another different document"}))
        .unwrap();

    // Not the exact stored text → mock cosine is noise; BM25 must decide.
    let hits = db.search(Some("notes"), "beta gamma", 3).unwrap();
    assert_eq!(
        hits[0].id, target,
        "keyword overlap must rank the doc first"
    );
    assert!(
        hits[0].score > hits[1].score,
        "keyword bonus must beat vector noise: {:?}",
        hits.iter().map(|h| h.score).collect::<Vec<_>>()
    );
}

#[test]
fn code_like_tokens_match_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with_mock(&dir.path().join("t.tepin"));
    db.set_embed_fields("bugs", &["title"]).unwrap();
    let target = db
        .insert(
            "bugs",
            json!({"title": "Error TEP-1234 crashes on startup"}),
        )
        .unwrap();
    db.insert("bugs", json!({"title": "Error TEP-9999 hangs on shutdown"}))
        .unwrap();

    let hits = db.search(Some("bugs"), "TEP-1234", 2).unwrap();
    assert_eq!(hits[0].id, target, "the exact ticket id must win");
}

#[test]
fn rarer_terms_weigh_more_via_idf() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with_mock(&dir.path().join("t.tepin"));
    db.set_embed_fields("notes", &["text"]).unwrap();
    // "common" appears everywhere; "zebra" once.
    let rare = db.insert("notes", json!({"text": "common zebra"})).unwrap();
    for i in 0..5 {
        db.insert("notes", json!({"text": format!("common filler {i}")}))
            .unwrap();
    }
    let hits = db.search(Some("notes"), "common zebra", 3).unwrap();
    assert_eq!(
        hits[0].id, rare,
        "the doc with the rare term must rank first"
    );
}

#[test]
fn update_swaps_keyword_entries() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with_mock(&dir.path().join("t.tepin"));
    db.set_embed_fields("notes", &["text"]).unwrap();
    // Heavy term frequency, so a stale index entry would dominate BM25.
    let old_heavy = db
        .insert("notes", json!({"text": "zebra zebra zebra zebra"}))
        .unwrap();
    let keeper = db
        .insert("notes", json!({"text": "zebra keeper notebook"}))
        .unwrap();

    db.update("notes", &old_heavy, json!({"text": "entirely new topic"}))
        .unwrap();

    let hits = db.search(Some("notes"), "zebra", 2).unwrap();
    assert_eq!(
        hits[0].id, keeper,
        "stale entries for the updated doc must not outrank the real match"
    );

    // And the updated doc is findable by its NEW terms.
    let hits = db.search(Some("notes"), "entirely new topic", 1).unwrap();
    assert_eq!(hits[0].id, old_heavy);
}

#[test]
fn backfill_indexes_preexisting_docs() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with_mock(&dir.path().join("t.tepin"));
    db.insert("notes", json!({"text": "quokka sighting at dawn"}))
        .unwrap();
    db.insert("notes", json!({"text": "meeting minutes tuesday"}))
        .unwrap();

    // Config after data: both vector queue AND keyword index backfill.
    db.set_embed_fields("notes", &["text"]).unwrap();
    let hits = db.search(Some("notes"), "quokka dawn", 2).unwrap();
    assert!(hits[0].doc["text"].as_str().unwrap().contains("quokka"));
}

#[test]
fn keyword_index_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("persist.tepin");
    let target;
    {
        let db = open_with_mock(&path);
        db.set_embed_fields("notes", &["text"]).unwrap();
        target = db
            .insert("notes", json!({"text": "persistent walrus fact"}))
            .unwrap();
        db.insert("notes", json!({"text": "other content"}))
            .unwrap();
        db.flush_embeddings().unwrap();
    }
    let db = open_with_mock(&path);
    let hits = db.search(Some("notes"), "walrus fact", 2).unwrap();
    assert_eq!(hits[0].id, target);
}

#[test]
fn db_wide_keyword_normalization_spans_collections() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with_mock(&dir.path().join("t.tepin"));
    db.set_embed_fields("notes", &["text"]).unwrap();
    db.set_embed_fields("tasks", &["text"]).unwrap();
    db.insert("notes", json!({"text": "walrus appears in notes"}))
        .unwrap();
    let task = db
        .insert("tasks", json!({"text": "walrus walrus walrus everywhere"}))
        .unwrap();

    let hits = db.search(None, "walrus", 2).unwrap();
    assert_eq!(hits.len(), 2, "both collections contribute");
    assert_eq!(
        hits[0].id, task,
        "higher term frequency should win across collections"
    );
}

#[test]
fn no_keyword_overlap_degrades_to_pure_vector() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with_mock(&dir.path().join("t.tepin"));
    db.set_embed_fields("notes", &["text"]).unwrap();
    let id = db
        .insert("notes", json!({"text": "exact stored text"}))
        .unwrap();

    // The exact stored text: mock cosine 1.0 AND full keyword bonus → 1.0.
    let hits = db.search(Some("notes"), "exact stored text", 1).unwrap();
    assert_eq!(hits[0].id, id);
    assert!(hits[0].score > 0.99);

    // Zero term overlap: pure vector semantics, cosine untouched by fusion.
    let hits = db.search(Some("notes"), "xyzzy plugh", 1).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].score < 0.99,
        "unrelated query must not score like a match"
    );
}

#[test]
fn disabling_embedding_drops_the_keyword_index_too() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with_mock(&dir.path().join("t.tepin"));
    db.set_embed_fields("notes", &["text"]).unwrap();
    db.insert("notes", json!({"text": "walrus content"}))
        .unwrap();
    db.flush_embeddings().unwrap();

    db.set_embed_fields("notes", &[]).unwrap();
    // Re-enable with no docs changed: backfill rebuilds everything cleanly.
    db.set_embed_fields("notes", &["text"]).unwrap();
    let hits = db.search(Some("notes"), "walrus", 1).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].score > 0.5,
        "rebuilt index must contribute keywords"
    );
}
