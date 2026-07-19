//! The flagship flow end-to-end with the REAL model: open_auto → declare
//! embed fields → insert → semantic search. Ignored by default; run with:
//!   cargo test -p tepindb -- --ignored
#![cfg(feature = "embedding")]

use serde_json::json;
use std::time::{Duration, Instant};

#[test]
#[ignore = "loads the real 34MB bge-small model"]
fn semantic_search_end_to_end() {
    let dir = tempfile::tempdir().unwrap();

    let t0 = Instant::now();
    let db = tepindb::open_auto(dir.path().join("memory.tepin")).unwrap();
    assert!(
        t0.elapsed() < Duration::from_millis(150),
        "open_auto must not block on the model, took {:?}",
        t0.elapsed()
    );

    db.set_embed_fields("notes", &["title", "body"]).unwrap();
    db.insert_many(
        "notes",
        vec![
            json!({"title": "Password reset flow",
                   "body": "Users click 'forgot password' in account settings to get a reset email."}),
            json!({"title": "Kitchen inventory",
                   "body": "We are out of olive oil and the sauce reduction needs low heat."}),
            json!({"title": "Deployment runbook",
                   "body": "Ship the binary via GitHub releases with checksums and an SBOM."}),
        ],
    )
    .unwrap();

    // Also prove db-wide search across a second collection.
    db.set_embed_fields("tasks", &["summary"]).unwrap();
    db.insert(
        "tasks",
        json!({"summary": "rotate the leaked credentials and force re-login"}),
    )
    .unwrap();

    let hits = db
        .search(Some("notes"), "how do users reset a password", 3)
        .unwrap();
    println!(
        "top hit: {:?} (score {:.3}), time to first search: {:?}",
        hits[0].doc["title"],
        hits[0].score,
        t0.elapsed()
    );
    assert_eq!(hits[0].doc["title"], "Password reset flow");

    let hits = db
        .search(None, "security incident with stolen passwords", 4)
        .unwrap();
    let top_collections: Vec<_> = hits.iter().take(2).map(|h| h.collection.as_str()).collect();
    assert!(
        top_collections.contains(&"tasks"),
        "db-wide search should surface the credential-rotation task near the top, got {hits:?}"
    );

    // Search-after-insert consistency with the real model too.
    db.insert(
        "notes",
        json!({"title": "Quarterly report", "body": "Revenue grew twelve percent."}),
    )
    .unwrap();
    let hits = db
        .search(Some("notes"), "how did revenue do this quarter", 1)
        .unwrap();
    assert_eq!(hits[0].doc["title"], "Quarterly report");
}
