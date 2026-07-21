//! Validation suite: exercises the public API the way real (and hostile)
//! callers will — data fidelity, filter semantics, id guarantees, batch
//! atomicity, persistence, file tampering, and locking.

use serde_json::{json, Value};
use tepin_core::Db;

fn open_temp() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.tepin")).unwrap();
    (dir, db)
}

// ---- data fidelity ----

#[test]
fn documents_round_trip_exactly() {
    let (_d, db) = open_temp();
    let doc = json!({
        "unicode": "héllo wörld — ☕ 日本語 🦀",
        "nested": {"deep": {"deeper": [1, 2, {"three": 3}]}},
        "float": 2.5066282746310007,
        "big_int": 9_007_199_254_740_993_i64,
        "negative": -42,
        "bool_t": true,
        "bool_f": false,
        "nothing": null,
        "empty_str": "",
        "empty_arr": [],
        "empty_obj": {}
    });
    let id = db.insert("fidelity", doc.clone()).unwrap();
    let mut got = db.get("fidelity", &id).unwrap().unwrap();
    got.as_object_mut().unwrap().remove("_id");
    assert_eq!(got, doc);
}

#[test]
fn large_documents_survive() {
    let (_d, db) = open_temp();
    let big = "x".repeat(1024 * 1024);
    let id = db.insert("big", json!({"payload": big.clone()})).unwrap();
    let got = db.get("big", &id).unwrap().unwrap();
    assert_eq!(got["payload"].as_str().unwrap().len(), big.len());
}

#[test]
fn unicode_collection_names_work() {
    let (_d, db) = open_temp();
    for name in ["café-☕", "col:tricky", "collection:trap", "日本語"] {
        let id = db.insert(name, json!({"ok": true})).unwrap();
        assert!(db.get(name, &id).unwrap().is_some(), "collection {name:?}");
    }
    assert_eq!(db.collections().unwrap().len(), 4);
}

// ---- collection name validation ----

#[test]
fn bad_collection_names_are_rejected() {
    let (_d, db) = open_temp();
    for bad in ["", "a\nb", "tab\tname", &"x".repeat(129)] {
        let err = db.insert(bad, json!({})).unwrap_err();
        assert_eq!(err.code, "invalid_collection_name", "name {bad:?}");
    }
}

// ---- id semantics ----

#[test]
fn explicit_ids_are_honored_and_duplicates_rejected() {
    let (_d, db) = open_temp();
    let id = db
        .insert("notes", json!({"_id": "my-key", "v": 1}))
        .unwrap();
    assert_eq!(id, "my-key");

    let err = db
        .insert("notes", json!({"_id": "my-key", "v": 2}))
        .unwrap_err();
    assert_eq!(err.code, "duplicate_id");
    // the original is untouched
    assert_eq!(db.get("notes", "my-key").unwrap().unwrap()["v"], 1);
    // same explicit id in a different collection is fine
    db.insert("other", json!({"_id": "my-key"})).unwrap();
}

#[test]
fn non_string_ids_are_rejected() {
    let (_d, db) = open_temp();
    let err = db.insert("notes", json!({"_id": 5})).unwrap_err();
    assert_eq!(err.code, "invalid_document");
}

#[test]
fn bulk_generated_ids_are_unique() {
    let (_d, db) = open_temp();
    let docs: Vec<Value> = (0..5000).map(|i| json!({"i": i})).collect();
    let ids = db.insert_many("bulk", docs).unwrap();
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), 5000, "generated ids must never collide");
    assert!(ids.iter().all(|id| id.len() == 12));
}

#[test]
fn generated_ids_sort_by_creation_time() {
    let (_d, db) = open_temp();
    let a = db.insert("t", json!({})).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(3));
    let b = db.insert("t", json!({})).unwrap();
    assert!(a < b);
}

// ---- batch atomicity ----

#[test]
fn insert_many_is_all_or_nothing() {
    let (_d, db) = open_temp();
    db.insert("atomic", json!({"_id": "taken"})).unwrap();

    let batch = vec![
        json!({"n": 1}),
        json!({"_id": "taken", "n": 2}), // duplicate → whole batch must abort
        json!({"n": 3}),
    ];
    let err = db.insert_many("atomic", batch).unwrap_err();
    assert_eq!(err.code, "duplicate_id");

    let all = db.find("atomic", &json!({})).unwrap();
    assert_eq!(all.len(), 1, "no doc from the failed batch may remain");
}

// ---- filter semantics ----

#[test]
fn filter_operators_behave() {
    let (_d, db) = open_temp();
    db.insert_many(
        "f",
        vec![
            json!({"n": 1, "s": "apple", "tag": "a"}),
            json!({"n": 2, "s": "banana", "tag": "b"}),
            json!({"n": 3, "s": "cherry", "tag": "a"}),
        ],
    )
    .unwrap();

    let count = |filter: Value| db.find("f", &filter).unwrap().len();

    assert_eq!(count(json!({})), 3);
    assert_eq!(count(json!({"n": 2})), 1);
    assert_eq!(count(json!({"n": {"$eq": 2}})), 1);
    assert_eq!(count(json!({"n": {"$ne": 2}})), 2);
    assert_eq!(count(json!({"n": {"$gt": 1}})), 2);
    assert_eq!(count(json!({"n": {"$gte": 1}})), 3);
    assert_eq!(count(json!({"n": {"$lt": 3}})), 2);
    assert_eq!(count(json!({"n": {"$lte": 3}})), 3);
    assert_eq!(count(json!({"n": {"$gt": 1, "$lt": 3}})), 1);
    assert_eq!(count(json!({"s": {"$gt": "apple"}})), 2, "string ordering");
    assert_eq!(count(json!({"n": {"$in": [1, 3]}})), 2);
    assert_eq!(count(json!({"tag": "a", "n": {"$gte": 2}})), 1, "AND");
    assert_eq!(count(json!({"missing_field": null})), 3, "missing == null");
    assert_eq!(count(json!({"missing_field": "x"})), 0);
}

#[test]
fn numbers_compare_numerically_like_mongo() {
    let (_d, db) = open_temp();
    db.insert("nums", json!({"n": 5})).unwrap();
    assert_eq!(db.find("nums", &json!({"n": 5.0})).unwrap().len(), 1);
    assert_eq!(
        db.find("nums", &json!({"n": {"$in": [5.0]}}))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        db.find("nums", &json!({"n": {"$ne": 5.0}})).unwrap().len(),
        0
    );
}

#[test]
fn bad_filters_error_with_hints() {
    let (_d, db) = open_temp();
    db.insert("f", json!({"a": 1})).unwrap();

    let err = db.find("f", &json!([1, 2])).unwrap_err();
    assert_eq!(err.code, "invalid_filter");

    let err = db.find("f", &json!({"a": {"$regex": "x"}})).unwrap_err();
    assert_eq!(err.code, "invalid_filter");
    assert!(err.hint.contains("$in"), "hint lists supported operators");
}

// ---- update/delete semantics ----

#[test]
fn update_pins_the_stored_id() {
    let (_d, db) = open_temp();
    let id = db.insert("u", json!({"v": 1})).unwrap();
    // an update trying to smuggle a different _id is overruled
    db.update("u", &id, json!({"_id": "other", "v": 2}))
        .unwrap();
    let got = db.get("u", &id).unwrap().unwrap();
    assert_eq!(got["_id"], Value::String(id.clone()));
    assert_eq!(got["v"], 2);
    assert!(db.get("u", "other").unwrap().is_none());
}

#[test]
fn missing_targets_error_cleanly() {
    let (_d, db) = open_temp();
    db.insert("x", json!({})).unwrap();

    assert_eq!(
        db.update("x", "ghost", json!({})).unwrap_err().code,
        "doc_not_found"
    );
    assert_eq!(db.delete("x", "ghost").unwrap_err().code, "doc_not_found");
    assert_eq!(
        db.get("nope", "id").unwrap_err().code,
        "collection_not_found"
    );
    assert_eq!(
        db.get("x", "ghost").unwrap(),
        None,
        "get is Option, not error"
    );
}

// ---- persistence & the preamble ----

#[test]
fn a_thousand_docs_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("many.tepin");
    {
        let db = Db::open(&path).unwrap();
        let docs: Vec<Value> = (0..1000)
            .map(|i| json!({"i": i, "even": i % 2 == 0}))
            .collect();
        db.insert_many("many", docs).unwrap();
    }
    let db = Db::open(&path).unwrap();
    assert_eq!(db.find("many", &json!({})).unwrap().len(), 1000);
    assert_eq!(db.find("many", &json!({"even": true})).unwrap().len(), 500);
    assert_eq!(db.collections().unwrap()[0].count, 1000);
}

#[test]
fn preamble_stays_intact_after_heavy_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("heavy.tepin");
    let db = Db::open(&path).unwrap();
    for batch in 0..10 {
        let docs: Vec<Value> = (0..100).map(|i| json!({"batch": batch, "i": i})).collect();
        db.insert_many("h", docs).unwrap();
    }
    drop(db);
    let bytes = std::fs::read(&path).unwrap();
    let meta = tepin_core::format::parse_preamble(&bytes).unwrap();
    assert_eq!(meta.payload_offset, tepin_core::format::PREAMBLE_LEN);
    assert!(String::from_utf8_lossy(&bytes[..200]).contains("tepindb"));
}

#[test]
fn purpose_before_first_insert_is_fine() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.tepin");
    let db = Db::open(&path).unwrap();
    db.set_purpose("planned", "docs that will exist later")
        .unwrap();
    let cols = db.collections().unwrap();
    assert_eq!(cols.len(), 1);
    assert_eq!(cols[0].count, 0);
    assert_eq!(
        cols[0].purpose.as_deref(),
        Some("docs that will exist later")
    );
}

// ---- hostile files ----

#[test]
fn tampered_magic_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("evil.tepin");
    Db::open(&path).unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[0] = b'X';
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(Db::open(&path).unwrap_err().code, "not_a_tepin_file");
}

#[test]
fn truncated_file_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trunc.tepin");
    std::fs::write(&path, b"tepindb v0 but the file ends here").unwrap();
    assert_eq!(Db::open(&path).unwrap_err().code, "invalid_preamble");
}

#[test]
fn future_format_version_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("future.tepin");
    Db::open(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let text = String::from_utf8_lossy(&bytes[..4096]).into_owned();
    let bumped = text.replace("\"format_version\":0", "\"format_version\":9");
    assert_ne!(text, bumped, "replacement must hit");
    let mut new_bytes = bumped.into_bytes();
    new_bytes.extend_from_slice(&bytes[4096..]);
    std::fs::write(&path, &new_bytes).unwrap();
    assert_eq!(Db::open(&path).unwrap_err().code, "format_too_new");
}

#[test]
fn corrupt_payload_fails_gracefully_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt.tepin");
    {
        let db = Db::open(&path).unwrap();
        db.insert("c", json!({"v": 1})).unwrap();
    }
    let mut bytes = std::fs::read(&path).unwrap();
    for b in bytes[4096..4096 + 512].iter_mut() {
        *b = 0xAA;
    }
    std::fs::write(&path, &bytes).unwrap();
    let err = Db::open(&path).unwrap_err();
    assert_eq!(err.code, "storage_error");
    assert!(!err.hint.is_empty());
}

#[test]
fn sqlite_files_are_politely_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("actually.sqlite");
    let mut fake = b"SQLite format 3\x00".to_vec();
    fake.resize(8192, 0);
    std::fs::write(&path, &fake).unwrap();
    assert_eq!(Db::open(&path).unwrap_err().code, "not_a_tepin_file");
}

// ---- locking ----

#[test]
fn second_open_of_a_locked_file_errors_gracefully() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("locked.tepin");
    let db1 = Db::open(&path).unwrap();
    db1.insert("l", json!({"v": 1})).unwrap();

    let err = Db::open(&path).unwrap_err();
    assert_eq!(err.code, "database_locked");
    assert!(err.hint.contains("one process"));

    // the first handle is unaffected
    db1.insert("l", json!({"v": 2})).unwrap();
    assert_eq!(db1.find("l", &json!({})).unwrap().len(), 2);
}

#[test]
fn open_with_retry_waits_out_a_cold_start_race() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("raced.tepin");
    let db1 = Db::open(&path).unwrap();

    // A too-short retry budget still ends in database_locked.
    let err = Db::options()
        .retry_for(std::time::Duration::from_millis(30))
        .open(&path)
        .unwrap_err();
    assert_eq!(err.code, "database_locked");

    // Releasing the lock mid-retry lets the second opener through.
    let holder = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(150));
        drop(db1);
    });
    let db2 = Db::options()
        .retry_for(std::time::Duration::from_secs(5))
        .open(&path)
        .unwrap();
    db2.insert("l", json!({"v": 1})).unwrap();
    holder.join().unwrap();

    // Errors other than the lock never trigger the retry loop.
    let missing = dir.path().join("nope").join("deep.tepin");
    let t0 = std::time::Instant::now();
    assert!(Db::options()
        .retry_for(std::time::Duration::from_secs(5))
        .open(&missing)
        .is_err());
    assert!(t0.elapsed() < std::time::Duration::from_secs(1));
}
