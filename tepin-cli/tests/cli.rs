//! End-to-end tests of the `tepin` binary: the exact surface an LLM or a
//! shell script sees — argument shapes, JSON output, markdown inspect, the
//! error contract on stderr, and the TEPIN_DB env fallback.

use assert_cmd::Command;
use serde_json::Value;

fn tepin() -> Command {
    let mut cmd = Command::cargo_bin("tepin").unwrap();
    cmd.env_remove("TEPIN_DB");
    cmd
}

fn json_stdout(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout is valid JSON")
}

#[test]
fn insert_query_get_delete_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cli.tepin");

    let out = tepin()
        .args(["insert"])
        .arg(&db)
        .args(["notes", r#"{"title": "hello", "stars": 5}"#])
        .output()
        .unwrap();
    assert!(out.status.success());
    let id = json_stdout(&out)["inserted"].as_str().unwrap().to_string();

    let out = tepin()
        .args(["query"])
        .arg(&db)
        .args(["notes", r#"{"stars": {"$gte": 3}}"#])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v = json_stdout(&out);
    assert_eq!(v["count"], 1);
    assert_eq!(v["docs"][0]["title"], "hello");

    let out = tepin()
        .args(["get"])
        .arg(&db)
        .args(["notes", &id])
        .output()
        .unwrap();
    assert_eq!(json_stdout(&out)["title"], "hello");

    let out = tepin()
        .args(["delete"])
        .arg(&db)
        .args(["notes", &id])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(json_stdout(&out)["deleted"], Value::String(id));
}

#[test]
fn tepin_db_env_var_replaces_the_path_arg() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("env.tepin");

    let out = tepin()
        .env("TEPIN_DB", &db)
        .args(["insert", "notes", r#"{"v": 1}"#])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = tepin()
        .env("TEPIN_DB", &db)
        .args(["query", "notes"])
        .output()
        .unwrap();
    assert_eq!(json_stdout(&out)["count"], 1);
}

#[test]
fn errors_are_json_on_stderr_with_exit_1() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("err.tepin");
    tepin()
        .args(["insert"])
        .arg(&db)
        .args(["notes", r#"{"v": 1}"#])
        .assert()
        .success();

    let out = tepin()
        .args(["query"])
        .arg(&db)
        .args(["ghost_collection"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err: Value = serde_json::from_slice(&out.stderr).expect("stderr is valid JSON");
    assert_eq!(err["error"]["code"], "collection_not_found");
    assert!(err["error"]["hint"].as_str().unwrap().contains("notes"));
}

#[test]
fn invalid_json_argument_is_a_clean_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("badjson.tepin");
    let out = tepin()
        .args(["insert"])
        .arg(&db)
        .args(["notes", "{not json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["code"], "invalid_json");
}

#[test]
fn inspect_renders_markdown() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("md.tepin");
    tepin()
        .args(["insert"])
        .arg(&db)
        .args(["notes", r#"{"v": 1}"#])
        .assert()
        .success();
    tepin()
        .args(["purpose"])
        .arg(&db)
        .args(["notes", "test notes"])
        .assert()
        .success();

    let out = tepin().args(["inspect"]).arg(&db).output().unwrap();
    let md = String::from_utf8_lossy(&out.stdout);
    assert!(md.starts_with("# TepinDB"));
    assert!(md.contains("| notes | 1 |"));
    assert!(md.contains("test notes"));
}

#[test]
fn read_commands_never_create_files() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("typo.tepin");
    for args in [
        vec!["inspect"],
        vec!["query", "notes"],
        vec!["get", "notes", "someid"],
    ] {
        let mut cmd = tepin();
        cmd.arg(args[0]).arg(&missing).args(&args[1..]);
        let out = cmd.output().unwrap();
        assert_eq!(out.status.code(), Some(1), "args {args:?}");
        let err: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(err["error"]["code"], "file_not_found", "args {args:?}");
        assert!(!missing.exists(), "read command must not create the file");
    }
}

#[test]
fn search_on_a_missing_file_is_a_clean_error() {
    // search is a read command: it must not create the file, and the error
    // arrives before any model is touched.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ni.tepin");
    let out = tepin()
        .args(["search"])
        .arg(&db)
        .args(["what is love"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["code"], "file_not_found");
    assert!(!db.exists());
}

#[test]
fn upsert_inserts_then_replaces() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("up.tepin");

    let out = tepin()
        .args(["upsert"])
        .arg(&db)
        .args(["notes", r#"{"_id": "n1", "status": "open"}"#])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(json_stdout(&out)["upserted"], "n1");

    let out = tepin()
        .args(["upsert"])
        .arg(&db)
        .args(["notes", r#"{"_id": "n1", "status": "closed"}"#])
        .output()
        .unwrap();
    assert!(out.status.success());

    let out = tepin()
        .args(["query"])
        .arg(&db)
        .args(["notes"])
        .output()
        .unwrap();
    let body = json_stdout(&out);
    assert_eq!(body["count"], 1);
    assert_eq!(body["docs"][0]["status"], "closed");
}
