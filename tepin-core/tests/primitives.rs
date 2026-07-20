//! The primitives tier (tepindb-minimal), exercised the way Engram will
//! use it: in-memory stores, BYO vectors with a caller-owned model,
//! cross-collection atomic batches, and secondary indexes.

use serde_json::json;
use tepin_core::{BatchOp, Db};

#[test]
fn in_memory_store_works_without_a_file() {
    let db = Db::open_in_memory().unwrap();
    let id = db.insert("nodes", json!({"title": "hello"})).unwrap();
    assert_eq!(db.get("nodes", &id).unwrap().unwrap()["title"], "hello");
    assert_eq!(db.find("nodes", &json!({})).unwrap().len(), 1);
}

#[test]
fn batch_is_atomic_across_collections() {
    let db = Db::open_in_memory().unwrap();
    let node = db.insert("nodes", json!({"title": "n1"})).unwrap();

    // A mixed batch touching three collections lands as one unit.
    let inserted = db
        .batch(vec![
            BatchOp::Insert {
                collection: "edges".into(),
                doc: json!({"from": node, "type": "because"}),
            },
            BatchOp::Update {
                collection: "nodes".into(),
                id: node.clone(),
                doc: json!({"title": "n1 archived", "status": "archived"}),
            },
            BatchOp::Insert {
                collection: "audit".into(),
                doc: json!({"action": "archived", "entity_id": node}),
            },
        ])
        .unwrap();
    assert_eq!(inserted.len(), 2);
    assert_eq!(db.find("edges", &json!({})).unwrap().len(), 1);
    assert_eq!(db.find("audit", &json!({})).unwrap().len(), 1);
    assert_eq!(
        db.get("nodes", &node).unwrap().unwrap()["status"],
        "archived"
    );

    // A failing op (duplicate explicit _id) rolls back EVERY op in the batch.
    let err = db
        .batch(vec![
            BatchOp::Insert {
                collection: "audit".into(),
                doc: json!({"action": "should not survive"}),
            },
            BatchOp::Insert {
                collection: "nodes".into(),
                doc: json!({"_id": node, "title": "dup"}),
            },
        ])
        .unwrap_err();
    assert_eq!(err.code, "duplicate_id");
    assert_eq!(
        db.find("audit", &json!({})).unwrap().len(),
        1,
        "first op of the failed batch must be rolled back"
    );
}

#[test]
fn manual_vectors_end_to_end() {
    let db = Db::open_in_memory().unwrap();
    db.set_manual_vectors("nodes", &["title", "body"]).unwrap();

    let a = db
        .insert(
            "nodes",
            json!({"title": "auth decision", "body": "jwt over sessions"}),
        )
        .unwrap();
    let b = db
        .insert(
            "nodes",
            json!({"title": "storage decision", "body": "redb file"}),
        )
        .unwrap();
    // Manual mode: nothing queues, ever.
    assert_eq!(db.pending_embeddings().unwrap(), 0);

    // The application owns embedding: store its vectors under its model id.
    db.set_vectors("nodes", &a, "engram-bge", &[vec![1.0, 0.0, 0.0]])
        .unwrap();
    db.set_vectors("nodes", &b, "engram-bge", &[vec![0.0, 1.0, 0.0]])
        .unwrap();

    // Readback (Engram's suspect scan does pairwise cosine on these).
    assert_eq!(
        db.get_vectors("nodes", &a).unwrap(),
        vec![vec![1.0, 0.0, 0.0]]
    );

    // Raw KNN by caller-supplied query vector.
    let hits = db
        .search_by_vector(Some("nodes"), &[0.9, 0.1, 0.0], 10)
        .unwrap();
    assert_eq!(hits[0].id, a);
    assert!(hits[0].score > hits[1].score);

    // Raw keyword scores for custom fusion.
    let kw = db
        .keyword_search(Some("nodes"), "redb file storage", 10)
        .unwrap();
    assert_eq!(kw[0].id, b);

    // Model guard: different model id or dimension is refused.
    let err = db
        .set_vectors("nodes", &a, "other-model", &[vec![1.0, 0.0, 0.0]])
        .unwrap_err();
    assert_eq!(err.code, "embedder_mismatch");
    let err = db
        .search_by_vector(Some("nodes"), &[1.0, 0.0], 10)
        .unwrap_err();
    assert_eq!(err.code, "embedder_mismatch");

    // Vectors follow the document out on delete.
    db.delete("nodes", &a).unwrap();
    assert!(db.get_vectors("nodes", &a).unwrap().is_empty());
    assert!(db
        .search_by_vector(Some("nodes"), &[1.0, 0.0, 0.0], 10)
        .unwrap()
        .iter()
        .all(|h| h.id != a));
}

#[test]
fn set_vectors_requires_manual_mode_and_an_existing_doc() {
    let db = Db::open_in_memory().unwrap();
    db.set_embed_fields("auto", &["title"]).unwrap();
    let id = db.insert("auto", json!({"title": "x"})).unwrap();
    let err = db.set_vectors("auto", &id, "m", &[vec![1.0]]).unwrap_err();
    assert_eq!(err.code, "manual_vectors_disabled");

    db.set_manual_vectors("manual", &["title"]).unwrap();
    let err = db
        .set_vectors("manual", "ghost", "m", &[vec![1.0]])
        .unwrap_err();
    assert_eq!(err.code, "doc_not_found");
}

#[test]
fn secondary_index_matches_full_scan_exactly() {
    let db = Db::open_in_memory().unwrap();
    for i in 0..50 {
        db.insert(
            "edges",
            json!({
                "from": format!("n{}", i % 7),
                "kind": if i % 2 == 0 { "because" } else { "answers" },
                "weight": i,
            }),
        )
        .unwrap();
    }
    // Backfill on an existing collection.
    db.create_index("edges", "from").unwrap();
    db.create_index("edges", "kind").unwrap();

    for filter in [
        json!({"from": "n3"}),
        json!({"kind": "because"}),
        json!({"from": "n3", "kind": "answers"}),
        json!({"from": {"$eq": "n5"}, "weight": {"$gte": 10}}),
        json!({"from": "no-such-node"}),
    ] {
        let via_index = db.find("edges", &filter).unwrap();
        db.drop_index("edges", "from").unwrap();
        db.drop_index("edges", "kind").unwrap();
        let via_scan = db.find("edges", &filter).unwrap();
        assert_eq!(
            via_index, via_scan,
            "filter {filter} must not change results"
        );
        db.create_index("edges", "from").unwrap();
        db.create_index("edges", "kind").unwrap();
    }
}

#[test]
fn index_follows_updates_deletes_and_numeric_equality() {
    let db = Db::open_in_memory().unwrap();
    db.create_index("docs", "status").unwrap();
    let id = db
        .insert("docs", json!({"status": "open", "n": 5}))
        .unwrap();
    db.create_index("docs", "n").unwrap();

    assert_eq!(
        db.find("docs", &json!({"status": "open"})).unwrap().len(),
        1
    );
    // Mongo-style numeric equality through the index: 5 == 5.0.
    assert_eq!(db.find("docs", &json!({"n": 5.0})).unwrap().len(), 1);

    db.update("docs", &id, json!({"status": "resolved", "n": 5}))
        .unwrap();
    assert!(db
        .find("docs", &json!({"status": "open"}))
        .unwrap()
        .is_empty());
    assert_eq!(
        db.find("docs", &json!({"status": "resolved"}))
            .unwrap()
            .len(),
        1
    );

    db.delete("docs", &id).unwrap();
    assert!(db
        .find("docs", &json!({"status": "resolved"}))
        .unwrap()
        .is_empty());

    // Missing field indexes as null and matches a null filter.
    db.insert("docs", json!({"n": 1})).unwrap();
    assert_eq!(db.find("docs", &json!({"status": null})).unwrap().len(), 1);
}
