//! In-driver serving, end to end: a Host handle and Discover handles on
//! the same file. The second open loses the file lock, finds the host
//! through the sidecar, and reads through it — snapshot-isolated, while
//! the host keeps writing.

#![cfg(feature = "serve")]

use serde_json::json;
use std::sync::Arc;
use tepin_core::embed::MockEmbedder;
use tepin_core::{Db, ServeMode};

fn host_open(path: &std::path::Path) -> Db {
    Db::options()
        .serve(ServeMode::Host)
        .open(path)
        .expect("host open")
}

fn discover_open(path: &std::path::Path) -> Db {
    Db::options()
        .serve(ServeMode::Discover)
        .open_existing(path)
        .expect("discover open")
}

#[test]
fn reads_are_served_while_the_writer_holds_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("served.tepin");
    let host = host_open(&path);
    host.create_index("nodes", "status").unwrap();
    host.insert(
        "nodes",
        json!({"_id": "n1", "status": "open", "title": "first"}),
    )
    .unwrap();
    host.insert(
        "nodes",
        json!({"_id": "n2", "status": "done", "title": "second"}),
    )
    .unwrap();

    // A plain open still refuses — discovery is opt-in.
    let err = Db::open(&path).unwrap_err();
    assert_eq!(err.code, "database_locked");

    let reader = discover_open(&path);
    assert!(reader.is_served());
    assert!(!host.is_served());

    // Reads match a direct view exactly.
    assert_eq!(
        reader.get("nodes", "n1").unwrap().unwrap()["title"],
        "first"
    );
    assert_eq!(reader.get("nodes", "ghost").unwrap(), None);
    assert_eq!(
        reader
            .find("nodes", &json!({"status": "open"}))
            .unwrap()
            .len(),
        1
    );
    let cols = reader.collections().unwrap();
    assert_eq!(cols.len(), 1);
    assert_eq!(cols[0].count, 2);
    assert_eq!(cols[0].indexes, vec!["status"]);

    // Errors keep their shape across the wire.
    let err = reader.find("ghost", &json!({})).unwrap_err();
    assert_eq!(err.code, "collection_not_found");
    assert!(!err.hint.is_empty());

    // Served handles are read-only; writes say so clearly.
    for err in [
        reader.insert("nodes", json!({"x": 1})).unwrap_err(),
        reader.upsert("nodes", json!({"_id": "n1"})).unwrap_err(),
        reader.update("nodes", "n1", json!({"x": 1})).unwrap_err(),
        reader.delete("nodes", "n1").unwrap_err(),
        reader.set_purpose("nodes", "nope").unwrap_err(),
    ] {
        assert_eq!(err.code, "database_locked");
    }

    // The host keeps writing while the reader is connected, and new
    // commits become visible to subsequent served reads.
    host.insert("nodes", json!({"_id": "n3", "status": "open"}))
        .unwrap();
    assert_eq!(reader.find("nodes", &json!({})).unwrap().len(), 3);

    // Dropping everything releases the file for a normal open.
    drop(reader);
    drop(host);
    let db = Db::open(&path).unwrap();
    assert_eq!(db.find("nodes", &json!({})).unwrap().len(), 3);
}

#[test]
fn primitives_reads_are_served_too() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prim.tepin");
    let host = host_open(&path);
    host.set_manual_vectors("nodes", &["title"]).unwrap();
    host.insert("nodes", json!({"_id": "a", "title": "auth decision"}))
        .unwrap();
    host.insert("nodes", json!({"_id": "b", "title": "storage decision"}))
        .unwrap();
    host.set_vectors("nodes", "a", "m", &[vec![1.0, 0.0]])
        .unwrap();
    host.set_vectors("nodes", "b", "m", &[vec![0.0, 1.0]])
        .unwrap();

    let reader = discover_open(&path);
    assert_eq!(
        reader.get_vectors("nodes", "a").unwrap(),
        vec![vec![1.0, 0.0]]
    );
    let hits = reader
        .search_by_vector(Some("nodes"), &[0.9, 0.1], 10)
        .unwrap();
    assert_eq!(hits[0].id, "a");
    let kw = reader.keyword_search(Some("nodes"), "storage", 10).unwrap();
    assert_eq!(kw[0].id, "b");
}

#[test]
fn served_semantic_search_uses_the_host_model() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("semantic.tepin");
    let mut host = host_open(&path);
    host.attach_embedder(Arc::new(MockEmbedder::new(16)))
        .unwrap();
    host.set_embed_fields("notes", &["title"]).unwrap();
    host.insert("notes", json!({"_id": "n1", "title": "the moon landing"}))
        .unwrap();

    // The reader has NO model of its own — search runs in the host,
    // which also drains its embed queue first (search-after-insert).
    let reader = discover_open(&path);
    let hits = reader.search(Some("notes"), "the moon landing", 3).unwrap();
    assert_eq!(hits[0].id, "n1");
    assert!(!hits[0].snippet.is_empty());

    // Freshly inserted on the host → immediately findable through serving.
    host.insert(
        "notes",
        json!({"_id": "n2", "title": "completely different topic"}),
    )
    .unwrap();
    let hits = reader
        .search(Some("notes"), "completely different topic", 3)
        .unwrap();
    assert_eq!(hits[0].id, "n2");
}

#[test]
fn served_reads_stay_consistent_under_a_write_storm() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("storm.tepin");
    let host = host_open(&path);
    host.insert("events", json!({"seq": 0})).unwrap();
    let reader = discover_open(&path);

    let writer = std::thread::spawn(move || {
        for seq in 1..=50 {
            host.insert("events", json!({"seq": seq})).unwrap();
        }
        host // keep the host alive until the reads are done
    });

    // Every served read is a consistent snapshot: counts only grow.
    let mut last = 0;
    for _ in 0..100 {
        let count = reader.find("events", &json!({})).unwrap().len();
        assert!(count >= last, "count went backwards: {count} < {last}");
        last = count;
    }
    let host = writer.join().unwrap();
    assert_eq!(reader.find("events", &json!({})).unwrap().len(), 51);
    drop(host);
}

#[test]
fn stale_sidecar_is_cleaned_and_reported_as_locked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stale.tepin");

    // Find our sidecar in the runtime dir by its embedded canonical path
    // (parallel tests advertise their own sidecars concurrently).
    let host = host_open(&path);
    let canonical = std::fs::canonicalize(&path).unwrap();
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("TMPDIR").map(std::path::PathBuf::from))
        .unwrap_or_else(std::env::temp_dir)
        .join("tepindb");
    let sidecar_file = std::fs::read_dir(&base)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.extension().and_then(|x| x.to_str()) == Some("json")
                && std::fs::read(p)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                    .is_some_and(|v| v["path"] == json!(canonical.to_string_lossy()))
        })
        .expect("hosting must advertise a sidecar");
    let mut sidecar: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&sidecar_file).unwrap()).unwrap();
    drop(host); // graceful shutdown removes the sidecar
    assert!(!sidecar_file.exists());

    // Simulate a crashed host: lock held by a NON-hosting process, plus a
    // leftover sidecar pointing at a dead endpoint.
    let _plain = Db::open(&path).unwrap();
    sidecar["endpoint"] = json!(format!("{}-gone", sidecar["endpoint"].as_str().unwrap()));
    std::fs::write(&sidecar_file, serde_json::to_vec(&sidecar).unwrap()).unwrap();

    let err = Db::options()
        .serve(ServeMode::Discover)
        .open_existing(&path)
        .unwrap_err();
    assert_eq!(err.code, "database_locked");
    assert!(!sidecar_file.exists(), "stale sidecar must be cleaned up");
}

#[test]
fn serve_off_advertises_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("silent.tepin");
    let db = Db::open(&path).unwrap();
    db.insert("n", json!({"x": 1})).unwrap();
    // No sidecar → discovery finds nothing and reports the plain lock.
    let err = Db::options()
        .serve(ServeMode::Discover)
        .open_existing(&path)
        .unwrap_err();
    assert_eq!(err.code, "database_locked");
}
