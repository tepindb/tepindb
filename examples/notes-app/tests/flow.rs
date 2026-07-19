//! End-to-end tests of the example app: every driver operation exercised
//! through a real compiled binary against a real .tepin file, using the
//! mock embedder so CI stays light (no model download).

use assert_cmd::Command;
use serde_json::Value;

struct App {
    _dir: tempfile::TempDir,
    db: std::path::PathBuf,
}

impl App {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("notes.tepin");
        Self { _dir: dir, db }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("notes").unwrap();
        cmd.env("NOTES_DB", &self.db)
            .env("NOTES_EMBEDDER", "mock")
            .args(args);
        cmd
    }

    fn ok_json(&self, args: &[&str]) -> Value {
        let out = self.cmd(args).output().unwrap();
        assert!(
            out.status.success(),
            "args {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let first_line = String::from_utf8_lossy(&out.stdout);
        serde_json::from_str(first_line.lines().next().unwrap_or("null")).unwrap()
    }
}

#[test]
fn full_note_lifecycle() {
    let app = App::new();

    let id = app.ok_json(&["add", "buy milk and eggs", "errands"])["added"]
        .as_str()
        .unwrap()
        .to_string();
    app.ok_json(&["add", "write the quarterly report", "work"]);
    app.ok_json(&["add", "fix the flaky integration test", "work"]);

    // list shows all three
    let out = app.cmd(&["list"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 3);

    // filters work through the app
    let out = app.cmd(&["find", r#"{"tag": "work"}"#]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 2);

    // semantic search (mock: exact text ranks first)
    let out = app
        .cmd(&["search", "buy milk and eggs", "1"])
        .output()
        .unwrap();
    let hit: Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).lines().next().unwrap()).unwrap();
    assert_eq!(hit["id"].as_str().unwrap(), id);
    assert!(hit["score"].as_f64().unwrap() > 0.99);

    // done flips status and the doc is re-embedded without breaking search
    app.ok_json(&["done", &id]);
    let out = app
        .cmd(&["find", r#"{"status": "done"}"#])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 1);

    // rm removes it from docs and from search
    app.ok_json(&["rm", &id]);
    let out = app.cmd(&["list"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 2);
    let out = app
        .cmd(&["search", "buy milk and eggs", "3"])
        .output()
        .unwrap();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let hit: Value = serde_json::from_str(line).unwrap();
        assert_ne!(
            hit["id"].as_str().unwrap(),
            id,
            "removed note must not surface"
        );
    }

    // info reports the schema story: purpose, embed fields, empty queue
    let info = app.ok_json(&["info"]);
    assert_eq!(info["collection"], "notes");
    assert_eq!(info["embed"][0], "text");
    assert!(info["purpose"].as_str().unwrap().contains("semantic"));
}

#[test]
fn state_persists_across_invocations() {
    let app = App::new();
    app.ok_json(&["add", "note from the first process"]);
    // A completely new process finds it — single file, no daemon, no config.
    let out = app
        .cmd(&["search", "note from the first process", "1"])
        .output()
        .unwrap();
    let hit: Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).lines().next().unwrap()).unwrap();
    assert!(hit["score"].as_f64().unwrap() > 0.99);
}

#[test]
fn errors_reach_the_user_with_hints() {
    let app = App::new();
    app.ok_json(&["add", "one note"]);

    let out = app.cmd(&["done", "nonexistent-id"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["code"], "doc_not_found");
    assert!(!err["error"]["hint"].as_str().unwrap().is_empty());

    let out = app.cmd(&["find", "{broken json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["code"], "invalid_json");
}

#[test]
fn search_without_an_embedder_explains_itself() {
    let app = App::new();
    let mut cmd = Command::cargo_bin("notes").unwrap();
    cmd.env("NOTES_DB", &app.db).env_remove("NOTES_EMBEDDER");
    cmd.args(["add", "a note"]).assert().success();

    let mut cmd = Command::cargo_bin("notes").unwrap();
    cmd.env("NOTES_DB", &app.db).env_remove("NOTES_EMBEDDER");
    let out = cmd.args(["search", "anything"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["code"], "embedder_not_attached");
}
