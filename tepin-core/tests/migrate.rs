//! The format-migration promise, proven: old file in, current-format file
//! out, original untouched — plus the fixture harness that keeps every
//! published format version migratable forever.

use serde_json::json;
use tepin_core::{migrate_file, Db};

#[test]
fn migrate_copies_everything_and_never_touches_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.tepin");
    let dst = dir.path().join("dst.tepin");

    {
        let db = Db::open(&src).unwrap();
        db.set_purpose("nodes", "graph nodes").unwrap();
        db.set_manual_vectors("nodes", &["title"]).unwrap();
        db.create_index("edges", "from").unwrap();
        db.create_unique_index("nodes", "slug").unwrap();
        db.insert(
            "nodes",
            json!({"_id": "n1", "title": "first", "slug": "one"}),
        )
        .unwrap();
        db.insert(
            "nodes",
            json!({"_id": "n2", "title": "second", "slug": "two"}),
        )
        .unwrap();
        db.set_vectors("nodes", "n1", "m", &[vec![1.0, 0.0], vec![0.5, 0.5]])
            .unwrap();
        db.insert("edges", json!({"from": "n1", "to": "n2"}))
            .unwrap();
        // An auto-embed collection with an unhealed queue (slim writer).
        db.set_embed_fields("notes", &["body"]).unwrap();
        db.insert("notes", json!({"body": "queued for embedding"}))
            .unwrap();
    }
    let src_bytes_before = std::fs::read(&src).unwrap();

    let report = migrate_file(&src, &dst).unwrap();
    assert_eq!(report.from_format, 0);
    assert_eq!(report.to_format, tepin_core::format::FORMAT_VERSION);
    assert_eq!(report.collections, 3);
    assert_eq!(report.documents, 4);
    assert_eq!(report.vector_rows, 2);

    // Never destructive: the source file is byte-identical.
    assert_eq!(std::fs::read(&src).unwrap(), src_bytes_before);

    // The copy is a fully working database.
    let db = Db::open(&dst).unwrap();
    let cols = db.collections().unwrap();
    assert_eq!(cols.len(), 3);
    let nodes = cols.iter().find(|c| c.name == "nodes").unwrap();
    assert_eq!(nodes.count, 2);
    assert_eq!(nodes.purpose.as_deref(), Some("graph nodes"));
    assert!(nodes.manual_vectors);
    assert_eq!(nodes.unique, vec!["slug"]);
    assert_eq!(db.get("nodes", "n1").unwrap().unwrap()["title"], "first");
    assert_eq!(
        db.get_vectors("nodes", "n1").unwrap(),
        vec![vec![1.0, 0.0], vec![0.5, 0.5]]
    );
    // Derived state was rebuilt: index answers, keyword search answers.
    assert_eq!(db.find("edges", &json!({"from": "n1"})).unwrap().len(), 1);
    assert_eq!(
        db.keyword_search(Some("nodes"), "second", 5).unwrap()[0].id,
        "n2"
    );
    // The unhealed embed queue rode along.
    assert_eq!(db.pending_embeddings().unwrap(), 1);
    // Constraints copied as meta stay live for new writes.
    let err = db
        .insert("nodes", json!({"slug": "one", "title": "dup"}))
        .unwrap_err();
    assert_eq!(err.code, "unique_violation");
}

#[test]
fn migrate_refuses_bad_paths() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.tepin");
    let dst = dir.path().join("dst.tepin");

    let err = migrate_file(&src, &dst).unwrap_err();
    assert_eq!(err.code, "file_not_found");

    Db::open(&src).unwrap();
    std::fs::write(&dst, b"already here").unwrap();
    let err = migrate_file(&src, &dst).unwrap_err();
    assert_eq!(err.code, "destination_exists");
    assert_eq!(std::fs::read(&dst).unwrap(), b"already here");
}

/// Every committed fixture — one per published format version — must
/// migrate cleanly to the current format and open as a working database.
/// When a format break lands, a fixture built by the LAST release of the
/// old format gets committed here, and this harness covers it forever.
#[test]
fn every_fixture_migrates_to_the_current_format() {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let dir = tempfile::tempdir().unwrap();
    for entry in std::fs::read_dir(&fixtures).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("tepin") {
            continue;
        }
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        let out = dir.path().join(format!("{name}.migrated.tepin"));
        let report = migrate_file(&path, &out)
            .unwrap_or_else(|e| panic!("fixture {name} failed to migrate: {e}"));
        assert_eq!(report.to_format, tepin_core::format::FORMAT_VERSION);

        let db = Db::open(&out).unwrap();
        let cols = db.collections().unwrap();
        let counts: serde_json::Map<String, serde_json::Value> = cols
            .iter()
            .map(|c| (c.name.clone(), json!(c.count)))
            .collect();
        // A sibling .expected.json pins what the fixture must contain.
        let expected_path = fixtures.join(format!("{name}.expected.json"));
        if expected_path.exists() {
            let expected: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&expected_path).unwrap()).unwrap();
            assert_eq!(
                json!(counts),
                expected["collections"],
                "fixture {name}: collection counts diverge from {}",
                expected_path.display()
            );
        }
    }
}

/// Regenerates the current-format fixture. Run by hand when the fixture
/// set needs a new member (e.g. right before a format break):
/// `cargo test -p tepin-core --test migrate -- --ignored regenerate`
#[test]
#[ignore = "writes into tests/fixtures — run by hand"]
fn regenerate_current_format_fixture() {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let version = tepin_core::format::FORMAT_VERSION;
    let path = fixtures.join(format!("v{version}.tepin"));
    let _ = std::fs::remove_file(&path);
    let db = Db::open(&path).unwrap();
    db.set_purpose("nodes", "fixture nodes").unwrap();
    db.set_manual_vectors("nodes", &["title"]).unwrap();
    db.create_unique_index("nodes", "slug").unwrap();
    db.insert("nodes", json!({"_id": "n1", "title": "alpha", "slug": "a"}))
        .unwrap();
    db.insert("nodes", json!({"_id": "n2", "title": "beta", "slug": "b"}))
        .unwrap();
    db.set_vectors("nodes", "n1", "fixture-model", &[vec![1.0, 0.0]])
        .unwrap();
    db.set_vectors("nodes", "n2", "fixture-model", &[vec![0.0, 1.0]])
        .unwrap();
    db.create_index("edges", "from").unwrap();
    db.insert("edges", json!({"_id": "e1", "from": "n1", "to": "n2"}))
        .unwrap();
    std::fs::write(
        fixtures.join(format!("v{version}.expected.json")),
        serde_json::to_string_pretty(&json!({
            "collections": {"nodes": 2, "edges": 1}
        }))
        .unwrap(),
    )
    .unwrap();
}
