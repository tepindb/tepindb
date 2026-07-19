//! Built-in chunking end-to-end: long docs get one vector per chunk, search
//! finds the right chunk and returns its text as the snippet, and the
//! chunk rows follow the document through update and delete.

use std::sync::Arc;

use serde_json::json;
use tepin_core::embed::MockEmbedder;
use tepin_core::{chunk_text, Db};

fn open_embedded() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open(dir.path().join("t.tepin")).unwrap();
    db.attach_embedder(Arc::new(MockEmbedder::new(16))).unwrap();
    db.set_embed_fields("docs", &["body"]).unwrap();
    (dir, db)
}

fn long_text() -> String {
    // Distinct paragraphs so chunks have unique, findable content.
    (0..40)
        .map(|i| format!("Paragraph number {i} talks at length about topic-{i} and nothing else. It repeats itself enough to take up meaningful space in the chunk budget, sentence after sentence, so the text comfortably spans several chunks."))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[test]
fn search_returns_the_matching_chunk_as_snippet() {
    let (_dir, db) = open_embedded();
    let body = long_text();
    let chunks = chunk_text(&body);
    assert!(chunks.len() > 2, "test text must span several chunks");

    db.insert("docs", json!({"body": body})).unwrap();
    db.flush_embeddings().unwrap();

    // MockEmbedder is hash-based: only the exact chunk text embeds to the
    // query's vector, so the winning chunk is provably the right one.
    for (idx, chunk) in chunks.iter().enumerate().take(3) {
        let hits = db.search(Some("docs"), chunk, 5).unwrap();
        assert!(!hits.is_empty());
        let top = &hits[0];
        assert_eq!(
            top.chunk as usize, idx,
            "best chunk must be the queried one"
        );
        assert_eq!(top.chunks as usize, chunks.len());
        assert_eq!(&top.snippet, chunk, "snippet is the chunk text verbatim");
        assert!(top.score > 0.6, "exact chunk match must score high");
    }
}

#[test]
fn short_docs_are_single_chunk_with_full_snippet() {
    let (_dir, db) = open_embedded();
    db.insert("docs", json!({"body": "just a short note"}))
        .unwrap();
    let hits = db.search(Some("docs"), "just a short note", 3).unwrap();
    assert_eq!(hits[0].chunk, 0);
    assert_eq!(hits[0].chunks, 1);
    assert_eq!(hits[0].snippet, "just a short note");
}

#[test]
fn update_replaces_chunk_rows_and_delete_clears_them() {
    let (_dir, db) = open_embedded();
    let id = db.insert("docs", json!({"body": long_text()})).unwrap();
    db.flush_embeddings().unwrap();

    // Shrink the doc: stale chunk vectors must not survive the update.
    db.update("docs", &id, json!({"body": "tiny now"})).unwrap();
    db.flush_embeddings().unwrap();
    let hits = db.search(Some("docs"), "tiny now", 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].chunks, 1);
    // A query for old content must not find a ghost chunk with high score.
    let old_chunk = &chunk_text(&long_text())[2];
    let hits = db.search(Some("docs"), old_chunk, 5).unwrap();
    assert!(
        hits.iter().all(|h| h.score < 0.9),
        "stale chunk vectors must be gone after update"
    );

    db.delete("docs", &id).unwrap();
    assert_eq!(db.search(Some("docs"), "tiny now", 5).unwrap().len(), 0);
}

#[test]
fn control_character_ids_are_rejected() {
    let (_dir, db) = open_embedded();
    let err = db
        .insert("docs", json!({"_id": "bad\u{0}id", "body": "x"}))
        .unwrap_err();
    assert_eq!(err.code, "invalid_document");
    assert!(!err.hint.is_empty());
}
