//! Protocol-level test of `tepin mcp`: a scripted MCP session over real
//! stdio pipes against the compiled binary — initialize handshake,
//! tools/list, tool calls, and the error contract. No model is touched
//! (search is not called), so this runs everywhere.

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

/// Send newline-delimited JSON-RPC messages, close stdin, collect responses.
fn session(db_path: &std::path::Path, messages: &[Value]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tepin"))
        .arg("mcp")
        .arg(db_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        for m in messages {
            writeln!(stdin, "{m}").unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "server exited nonzero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).expect("every output line is JSON"))
        .collect()
}

fn call(id: u64, tool: &str, arguments: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
           "params": {"name": tool, "arguments": arguments}})
}

/// Extract the text payload of a tools/call result as parsed JSON.
fn text_json(response: &Value) -> Value {
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    serde_json::from_str(text).unwrap_or_else(|_| json!(text))
}

#[test]
fn full_mcp_session() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("mcp.tepin");

    let responses = session(
        &db,
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                   "params": {"protocolVersion": "2025-06-18",
                              "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
            call(
                3,
                "insert",
                json!({"collection": "notes",
                                     "doc": {"title": "hello mcp", "stars": 5}}),
            ),
            call(
                4,
                "query",
                json!({"collection": "notes",
                                    "filter": {"stars": {"$gte": 3}}}),
            ),
            call(
                5,
                "purpose",
                json!({"collection": "notes", "text": "mcp test notes"}),
            ),
            call(
                6,
                "embed_fields",
                json!({"collection": "notes", "fields": ["title"]}),
            ),
            call(7, "inspect", json!({})),
            call(8, "query", json!({"collection": "ghost"})),
            call(9, "nonexistent_tool", json!({})),
            json!({"jsonrpc": "2.0", "id": 10, "method": "bogus/method"}),
            json!({"jsonrpc": "2.0", "id": 11, "method": "ping"}),
        ],
    );

    // One response per id'd request; the notification produced none.
    assert_eq!(responses.len(), 11);

    // initialize: echoes the protocol version, names the server, instructs.
    assert_eq!(responses[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(responses[0]["result"]["serverInfo"]["name"], "tepindb");
    assert!(responses[0]["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("inspect"));

    // tools/list: all nine tools, each with a schema.
    let tools = responses[1]["result"]["tools"].as_array().unwrap();
    let names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in [
        "inspect",
        "query",
        "get",
        "insert",
        "update",
        "delete",
        "search",
        "purpose",
        "embed_fields",
    ] {
        assert!(names.contains(&expected), "missing tool {expected}");
    }
    assert!(tools.iter().all(|t| t["inputSchema"]["type"] == "object"));

    // insert → query round trip through the protocol.
    let inserted = text_json(&responses[2]);
    let id = inserted["inserted"].as_str().unwrap();
    assert_eq!(responses[2]["result"]["isError"], false);
    let queried = text_json(&responses[3]);
    assert_eq!(queried["count"], 1);
    assert_eq!(queried["docs"][0]["_id"].as_str().unwrap(), id);

    // embed_fields reports the backfill queue.
    let embed = text_json(&responses[5]);
    assert_eq!(embed["pending_embeddings"], 1);

    // inspect is markdown and reflects purpose + embed fields.
    let md = responses[6]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(md.starts_with("# TepinDB"));
    assert!(md.contains("mcp test notes"));
    assert!(md.contains("title"));

    // Tool-level failure: isError with our {code, message, hint} shape.
    assert_eq!(responses[7]["result"]["isError"], true);
    let err = text_json(&responses[7]);
    assert_eq!(err["error"]["code"], "collection_not_found");
    assert!(!err["error"]["hint"].as_str().unwrap().is_empty());

    // Unknown tool: also a loud, hinted failure.
    assert_eq!(responses[8]["result"]["isError"], true);
    assert_eq!(text_json(&responses[8])["error"]["code"], "not_implemented");

    // Unknown method: JSON-RPC error, not a crash.
    assert_eq!(responses[9]["error"]["code"], -32601);

    // ping pongs.
    assert_eq!(responses[10]["result"], json!({}));
}

#[test]
fn mcp_state_persists_for_later_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("persist.tepin");

    session(
        &db,
        &[call(
            1,
            "insert",
            json!({"collection": "kv", "doc": {"k": "v"}}),
        )],
    );
    // A brand-new server process over the same file sees the data.
    let responses = session(&db, &[call(1, "query", json!({"collection": "kv"}))]);
    assert_eq!(text_json(&responses[0])["count"], 1);
}
