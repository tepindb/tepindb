//! The MCP server: `tepin mcp <file>` serves this database's operations to
//! AI agents over stdio (newline-delimited JSON-RPC 2.0, per the MCP spec).
//! Deliberately hand-rolled — the protocol surface we need is small, and
//! the slim binary stays slim.
//!
//! The tool surface mirrors the CLI exactly (one surface to learn): every
//! tool answers JSON text; every failure is `{"error": {code, message,
//! hint}}` with `isError: true`, so an agent can self-correct from the
//! hint alone. The embedder attaches lazily on the first `search` call —
//! serving a database never downloads or loads a model by itself.

use std::io::{BufRead, Write};
use std::path::Path;

use serde_json::{json, Value};
use tepin_core::{Db, TepinError};

pub fn serve(file: &Path) -> Result<(), TepinError> {
    // The MCP server is the long-lived lock holder in practice (e.g. an
    // agent session on a live database) — host reads for everyone else.
    let mut db = Db::options()
        .serve(tepin_core::ServeMode::Host)
        .open(file)?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = msg.get("id").filter(|v| !v.is_null()).cloned();
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));

        let outcome: Option<Result<Value, (i64, String)>> = match method.as_str() {
            "initialize" => Some(Ok(json!({
                "protocolVersion": params
                    .get("protocolVersion")
                    .cloned()
                    .unwrap_or_else(|| json!("2025-06-18")),
                "capabilities": {"tools": {}},
                "serverInfo": {
                    "name": "tepindb",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "instructions": "TepinDB: an AI-first single-file database. Call `inspect` first — it tells you what this database contains and how it is organized.",
            }))),
            "ping" => Some(Ok(json!({}))),
            "tools/list" => Some(Ok(json!({"tools": tool_defs()}))),
            "tools/call" => Some(Ok(handle_call(&mut db, file, &params))),
            m if m.starts_with("notifications/") => None,
            _ => id
                .is_some()
                .then(|| Err((-32601, format!("method {method:?} not found")))),
        };

        if let (Some(id), Some(outcome)) = (id, outcome) {
            let response = match outcome {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err((code, message)) => json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": code, "message": message},
                }),
            };
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// Every tool-level failure comes back as content with isError, carrying
/// the standard {code, message, hint} shape — agents read the hint.
fn handle_call(db: &mut Db, file: &Path, params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or_default();
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match dispatch(db, file, name, &args) {
        Ok(text) => json!({"content": [{"type": "text", "text": text}], "isError": false}),
        Err(e) => json!({
            "content": [{"type": "text", "text": e.to_json().to_string()}],
            "isError": true,
        }),
    }
}

fn dispatch(db: &mut Db, file: &Path, name: &str, args: &Value) -> Result<String, TepinError> {
    let collection = || -> Result<&str, TepinError> {
        args["collection"].as_str().ok_or_else(|| {
            TepinError::new(
                "invalid_filter",
                "missing required string argument 'collection'",
                "pass the collection name; `inspect` lists all collections",
            )
        })
    };
    let id_arg = || -> Result<&str, TepinError> {
        args["id"].as_str().ok_or_else(|| {
            TepinError::new(
                "doc_not_found",
                "missing required string argument 'id'",
                "pass the document's _id; find one via `query`",
            )
        })
    };

    match name {
        "inspect" => crate::inspect_markdown(db, file),
        "query" => {
            let filter = args.get("filter").cloned().unwrap_or_else(|| json!({}));
            let docs = db.find(collection()?, &filter)?;
            Ok(json!({"count": docs.len(), "docs": docs}).to_string())
        }
        "get" => {
            let doc = db.get(collection()?, id_arg()?)?;
            Ok(json!({"doc": doc}).to_string())
        }
        "insert" => {
            let doc = args.get("doc").cloned().ok_or_else(|| {
                TepinError::new(
                    "invalid_document",
                    "missing required argument 'doc'",
                    "pass the document as a JSON object",
                )
            })?;
            let col = collection()?;
            let id = db.insert(col, doc)?;
            Ok(json!({"inserted": id, "collection": col}).to_string())
        }
        "upsert" => {
            let doc = args.get("doc").cloned().ok_or_else(|| {
                TepinError::new(
                    "invalid_document",
                    "missing required argument 'doc'",
                    "pass the document as a JSON object; give it an _id to replace an existing one",
                )
            })?;
            let col = collection()?;
            let id = db.upsert(col, doc)?;
            Ok(json!({"upserted": id, "collection": col}).to_string())
        }
        "update" => {
            let doc = args.get("doc").cloned().ok_or_else(|| {
                TepinError::new(
                    "invalid_document",
                    "missing required argument 'doc'",
                    "pass the full replacement document as a JSON object",
                )
            })?;
            let (col, id) = (collection()?, id_arg()?);
            db.update(col, id, doc)?;
            Ok(json!({"updated": id, "collection": col}).to_string())
        }
        "delete" => {
            let (col, id) = (collection()?, id_arg()?);
            db.delete(col, id)?;
            Ok(json!({"deleted": id, "collection": col}).to_string())
        }
        "search" => {
            let query = args["query"].as_str().ok_or_else(|| {
                TepinError::new(
                    "invalid_filter",
                    "missing required string argument 'query'",
                    "pass a natural-language query string",
                )
            })?;
            ensure_embedder(db)?;
            let limit = args["limit"].as_u64().unwrap_or(5) as usize;
            let hits = db.search(args["collection"].as_str(), query, limit)?;
            Ok(json!({"hits": hits}).to_string())
        }
        "purpose" => {
            let text = args["text"].as_str().ok_or_else(|| {
                TepinError::new(
                    "invalid_document",
                    "missing required string argument 'text'",
                    "pass the collection's purpose as free text",
                )
            })?;
            let col = collection()?;
            db.set_purpose(col, text)?;
            Ok(json!({"collection": col, "purpose": text}).to_string())
        }
        "embed_fields" => {
            let fields: Vec<String> = args["fields"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
            let col = collection()?;
            db.set_embed_fields(col, &refs)?;
            Ok(json!({
                "collection": col,
                "embed": fields,
                "pending_embeddings": db.pending_embeddings()?,
            })
            .to_string())
        }
        other => Err(TepinError::new(
            "not_implemented",
            format!("unknown tool {other:?}"),
            "call tools/list for the available tools",
        )),
    }
}

/// Attach the default model on first search — never earlier, so serving a
/// database costs nothing until semantic search is actually used.
#[cfg(feature = "embed")]
fn ensure_embedder(db: &mut Db) -> Result<(), TepinError> {
    if db.embedder_attached() {
        return Ok(());
    }
    let cache = tepin_embed::fetch::default_cache_dir()?;
    let lazy = tepin_embed::OnnxEmbedder::spawn_lazy(&tepin_embed::fetch::BGE_SMALL, cache);
    db.attach_embedder(std::sync::Arc::new(lazy))
}

#[cfg(not(feature = "embed"))]
fn ensure_embedder(_db: &mut Db) -> Result<(), TepinError> {
    Err(TepinError::new(
        "not_implemented",
        "this build has no embedding support (slim binary)",
        "use a full build (`cargo install tepin-cli`) for semantic search; `query` works everywhere",
    ))
}

fn tool_defs() -> Value {
    let collection = json!({"type": "string", "description": "Collection name"});
    let id = json!({"type": "string", "description": "Document _id"});
    json!([
        {
            "name": "inspect",
            "description": "Markdown report of everything in this database: collections, purposes, embed fields, document counts. Start here.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "query",
            "description": "Find documents with a MongoDB-style JSON filter. Supported operators: $eq, $ne, $gt, $gte, $lt, $lte, $in. Empty filter matches all.",
            "inputSchema": {"type": "object", "properties": {
                "collection": collection,
                "filter": {"type": "object", "description": "e.g. {\"status\": \"open\", \"stars\": {\"$gte\": 3}}"}
            }, "required": ["collection"]}
        },
        {
            "name": "get",
            "description": "Fetch one document by its _id.",
            "inputSchema": {"type": "object", "properties": {"collection": collection, "id": id},
                            "required": ["collection", "id"]}
        },
        {
            "name": "insert",
            "description": "Insert a JSON document. Creates the collection on first use; assigns a short sortable _id unless the doc carries one (duplicates are rejected, never overwritten).",
            "inputSchema": {"type": "object", "properties": {
                "collection": collection,
                "doc": {"type": "object", "description": "The document"}
            }, "required": ["collection", "doc"]}
        },
        {
            "name": "upsert",
            "description": "Insert-or-replace by _id: replaces the existing document with the same _id, inserts otherwise (minting an _id if the doc has none).",
            "inputSchema": {"type": "object", "properties": {
                "collection": collection,
                "doc": {"type": "object", "description": "The document; include _id to target an existing one"}
            }, "required": ["collection", "doc"]}
        },
        {
            "name": "update",
            "description": "Replace a document by _id (the stored _id always wins). Re-embeds automatically if the collection has embed fields.",
            "inputSchema": {"type": "object", "properties": {
                "collection": collection, "id": id,
                "doc": {"type": "object", "description": "The full replacement document"}
            }, "required": ["collection", "id", "doc"]}
        },
        {
            "name": "delete",
            "description": "Delete a document by _id (also removes its search vector).",
            "inputSchema": {"type": "object", "properties": {"collection": collection, "id": id},
                            "required": ["collection", "id"]}
        },
        {
            "name": "search",
            "description": "Semantic vector search in natural language. Searches every embedded collection unless one is named. Results carry a relevance score and the full document.",
            "inputSchema": {"type": "object", "properties": {
                "query": {"type": "string", "description": "Natural-language query"},
                "collection": {"type": "string", "description": "Optional: restrict to one collection"},
                "limit": {"type": "integer", "description": "Max results (default 5)"}
            }, "required": ["query"]}
        },
        {
            "name": "purpose",
            "description": "Set a collection's free-text purpose — shown by inspect so future readers know what it is for.",
            "inputSchema": {"type": "object", "properties": {
                "collection": collection,
                "text": {"type": "string", "description": "What this collection is for"}
            }, "required": ["collection", "text"]}
        },
        {
            "name": "embed_fields",
            "description": "Declare which fields of a collection get embedded for semantic search. Existing docs are backfilled automatically.",
            "inputSchema": {"type": "object", "properties": {
                "collection": collection,
                "fields": {"type": "array", "items": {"type": "string"},
                           "description": "Field names to embed, e.g. [\"title\", \"body\"]"}
            }, "required": ["collection", "fields"]}
        }
    ])
}
