//! One dispatch surface for every driver: an op name and JSON args in,
//! JSON out. The CLI, the MCP server, the serve host, and the FFI-based
//! drivers (Go, TypeScript) all speak this same verb set with the same
//! argument names and the same result shapes — one surface to learn, one
//! to test, and any failing driver call can be replayed verbatim with the
//! CLI (`tepin <op> <file> …`).
//!
//! Setting `TEPIN_TRACE` (to anything but `0`) logs every dispatched op
//! to stderr as one JSON line — args, duration, outcome — from any
//! driver, in any language, with no code changes.

use std::path::Path;

use serde_json::{json, Value};

use crate::db::{BatchOp, Db};
use crate::error::{Result, TepinError};

/// Every op `dispatch` understands, in the order `tepin --help` lists the
/// matching commands. Drivers surface this in errors so a typo'd op name
/// answers with the whole menu.
pub const OPS: &[&str] = &[
    "inspect",
    "collections",
    "query",
    "get",
    "insert",
    "upsert",
    "update",
    "delete",
    "search",
    "keyword_search",
    "purpose",
    "embed_fields",
    "pending_embeddings",
    "batch",
    "create_index",
    "drop_index",
    "manual_vectors",
    "set_vectors",
    "search_by_vector",
    "get_vectors",
];

fn tracing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("TEPIN_TRACE")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
    })
}

/// Run one operation against an open handle. Works on local and served
/// (remote) handles alike — a served handle's write ops fail with the
/// same `database_locked` error the method calls produce.
pub fn dispatch(db: &Db, op: &str, args: &Value) -> Result<Value> {
    if !tracing() {
        return dispatch_inner(db, op, args);
    }
    let start = std::time::Instant::now();
    let out = dispatch_inner(db, op, args);
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    let outcome = match &out {
        Ok(_) => json!({"ok": true}),
        Err(e) => json!({"ok": false, "code": e.code}),
    };
    eprintln!(
        "{}",
        json!({"tepin_trace": {"op": op, "args": args, "ms": (ms * 100.0).round() / 100.0, "result": outcome}})
    );
    out
}

fn dispatch_inner(db: &Db, op: &str, args: &Value) -> Result<Value> {
    let collection = || -> Result<&str> {
        args["collection"].as_str().ok_or_else(|| {
            TepinError::new(
                "invalid_filter",
                "missing required string argument 'collection'",
                "pass the collection name",
            )
        })
    };
    let id_arg = || -> Result<&str> {
        args["id"].as_str().ok_or_else(|| {
            TepinError::new(
                "doc_not_found",
                "missing required string argument 'id'",
                "pass the document's _id",
            )
        })
    };
    let doc_arg = || -> Result<Value> {
        args.get("doc")
            .cloned()
            .filter(|d| d.is_object())
            .ok_or_else(|| {
                TepinError::new(
                    "invalid_document",
                    "missing required object argument 'doc'",
                    "pass the document as a JSON object",
                )
            })
    };
    let query_arg = || -> Result<&str> {
        args["query"].as_str().ok_or_else(|| {
            TepinError::new(
                "invalid_filter",
                "missing required string argument 'query'",
                "pass a natural-language query string",
            )
        })
    };
    let limit = || args["limit"].as_u64().unwrap_or(5) as usize;
    let field = || -> Result<&str> {
        args["field"].as_str().ok_or_else(|| {
            TepinError::new(
                "invalid_filter",
                "missing required string argument 'field'",
                "pass the field name",
            )
        })
    };
    let fields = || -> Vec<String> {
        args["fields"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };

    match op {
        "inspect" => {
            let path = args["path"].as_str().map(Path::new);
            Ok(json!({"markdown": inspect_markdown(db, path)?}))
        }
        "collections" => Ok(json!({"collections": db.collections()?})),
        "query" => {
            let filter = args.get("filter").cloned().unwrap_or_else(|| json!({}));
            let docs = db.find(collection()?, &filter)?;
            Ok(json!({"count": docs.len(), "docs": docs}))
        }
        "get" => Ok(json!({"doc": db.get(collection()?, id_arg()?)?})),
        "insert" => {
            let col = collection()?;
            let id = db.insert(col, doc_arg()?)?;
            Ok(json!({"inserted": id, "collection": col}))
        }
        "upsert" => {
            let col = collection()?;
            let id = db.upsert(col, doc_arg()?)?;
            Ok(json!({"upserted": id, "collection": col}))
        }
        "update" => {
            let (col, id) = (collection()?, id_arg()?);
            db.update(col, id, doc_arg()?)?;
            Ok(json!({"updated": id, "collection": col}))
        }
        "delete" => {
            let (col, id) = (collection()?, id_arg()?);
            db.delete(col, id)?;
            Ok(json!({"deleted": id, "collection": col}))
        }
        "search" => {
            let hits = db.search(args["collection"].as_str(), query_arg()?, limit())?;
            Ok(json!({"count": hits.len(), "hits": hits}))
        }
        "keyword_search" => {
            let hits = db.keyword_search(args["collection"].as_str(), query_arg()?, limit())?;
            Ok(json!({"count": hits.len(), "hits": hits}))
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
            Ok(json!({"collection": col, "purpose": text}))
        }
        "embed_fields" => {
            let fields = fields();
            let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
            let col = collection()?;
            db.set_embed_fields(col, &refs)?;
            Ok(json!({
                "collection": col,
                "embed": fields,
                "pending_embeddings": db.pending_embeddings()?,
            }))
        }
        "pending_embeddings" => Ok(json!({"pending_embeddings": db.pending_embeddings()?})),
        "batch" => {
            let ops = parse_batch(args)?;
            let ids = db.batch(ops)?;
            Ok(json!({"ids": ids}))
        }
        "create_index" => {
            let (col, f) = (collection()?, field()?);
            if args["unique"].as_bool().unwrap_or(false) {
                db.create_unique_index(col, f)?;
            } else {
                db.create_index(col, f)?;
            }
            Ok(json!({"collection": col, "indexed": f}))
        }
        "drop_index" => {
            let (col, f) = (collection()?, field()?);
            db.drop_index(col, f)?;
            Ok(json!({"collection": col, "dropped": f}))
        }
        "manual_vectors" => {
            let fields = fields();
            let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
            let col = collection()?;
            db.set_manual_vectors(col, &refs)?;
            Ok(json!({"collection": col, "manual_vectors": fields}))
        }
        "set_vectors" => {
            let (col, id) = (collection()?, id_arg()?);
            let model_id = args["model_id"].as_str().ok_or_else(|| {
                TepinError::new(
                    "invalid_vector",
                    "missing required string argument 'model_id'",
                    "name the model the vectors came from; later writes must match it",
                )
            })?;
            let vectors: Vec<Vec<f32>> = parse_vectors(&args["vectors"]);
            db.set_vectors(col, id, model_id, &vectors)?;
            Ok(json!({"collection": col, "id": id, "vectors": vectors.len()}))
        }
        "search_by_vector" => {
            let vector: Vec<f32> = args["vector"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_f64())
                        .map(|v| v as f32)
                        .collect()
                })
                .unwrap_or_default();
            let hits = db.search_by_vector(args["collection"].as_str(), &vector, limit())?;
            Ok(json!({"count": hits.len(), "hits": hits}))
        }
        "get_vectors" => Ok(json!({"vectors": db.get_vectors(collection()?, id_arg()?)?})),
        other => Err(TepinError::new(
            "not_implemented",
            format!("unknown op {other:?}"),
            format!("known ops: {}", OPS.join(", ")),
        )),
    }
}

fn parse_vectors(v: &Value) -> Vec<Vec<f32>> {
    v.as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row.as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_f64())
                                .map(|x| x as f32)
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Wire shape of a batch: `{"ops": [{"op": "insert", "collection": …,
/// "doc": …}, {"op": "delete", "collection": …, "id": …}, …]}` — the
/// same argument names as the standalone ops.
fn parse_batch(args: &Value) -> Result<Vec<BatchOp>> {
    let bad = |msg: &str| {
        TepinError::new(
            "invalid_document",
            format!("invalid batch: {msg}"),
            "pass {\"ops\": [{\"op\": \"insert\"|\"upsert\"|\"update\"|\"delete\", \"collection\": …, \"doc\"/\"id\": …}]}",
        )
    };
    let ops = args["ops"]
        .as_array()
        .ok_or_else(|| bad("missing 'ops' array"))?;
    let mut out = Vec::with_capacity(ops.len());
    for entry in ops {
        let col = entry["collection"]
            .as_str()
            .ok_or_else(|| bad("an op is missing 'collection'"))?
            .to_string();
        let doc = || {
            entry
                .get("doc")
                .cloned()
                .ok_or_else(|| bad("an op is missing 'doc'"))
        };
        let id = || -> Result<String> {
            Ok(entry["id"]
                .as_str()
                .ok_or_else(|| bad("an op is missing 'id'"))?
                .to_string())
        };
        out.push(match entry["op"].as_str() {
            Some("insert") => BatchOp::Insert {
                collection: col,
                doc: doc()?,
            },
            Some("upsert") => BatchOp::Upsert {
                collection: col,
                doc: doc()?,
            },
            Some("update") => BatchOp::Update {
                collection: col,
                id: id()?,
                doc: doc()?,
            },
            Some("delete") => BatchOp::Delete {
                collection: col,
                id: id()?,
            },
            _ => return Err(bad("each op needs \"op\": insert|upsert|update|delete")),
        });
    }
    Ok(out)
}

/// The `inspect` report: what this database contains and how to work with
/// it, as markdown. `path` is only used for display and on-disk size — an
/// in-memory or path-less handle inspects fine without it.
pub fn inspect_markdown(db: &Db, path: Option<&Path>) -> Result<String> {
    use std::fmt::Write;
    let cols = db.collections()?;
    let total: u64 = cols.iter().map(|c| c.count).sum();
    let shown_path = path
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<db>".into());
    let size = path
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .unwrap_or(0);

    let mut md = String::new();
    let _ = writeln!(md, "# TepinDB — {shown_path}\n");
    let _ = writeln!(
        md,
        "Format v{} · {} collection(s) · {} document(s) · {:.1} KiB\n",
        crate::format::FORMAT_VERSION,
        cols.len(),
        total,
        size as f64 / 1024.0
    );
    if cols.is_empty() {
        let _ = writeln!(
            md,
            "This database is empty. Create a collection by inserting:\n\n\
             ```\ntepin insert {shown_path} <collection> '{{\"any\": \"json\"}}'\n```"
        );
    } else {
        let _ = writeln!(md, "| collection | docs | embedded fields | purpose |");
        let _ = writeln!(md, "|---|---:|---|---|");
        for c in &cols {
            let _ = writeln!(
                md,
                "| {} | {} | {} | {} |",
                c.name,
                c.count,
                if c.embed.is_empty() {
                    "—".to_string()
                } else {
                    c.embed.join(", ")
                },
                c.purpose.as_deref().unwrap_or("—")
            );
        }
        let _ = writeln!(
            md,
            "\nQuery any collection with MongoDB-style filters:\n\n\
             ```\ntepin query {shown_path} <collection> '{{\"field\": \"value\"}}'\n```"
        );
    }
    Ok(md)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    #[test]
    fn round_trip_through_dispatch() {
        let db = db();
        let ins = dispatch(
            &db,
            "insert",
            &json!({"collection": "notes", "doc": {"title": "hi", "stars": 4}}),
        )
        .unwrap();
        let id = ins["inserted"].as_str().unwrap().to_string();

        let got = dispatch(&db, "get", &json!({"collection": "notes", "id": id})).unwrap();
        assert_eq!(got["doc"]["title"], "hi");

        let q = dispatch(
            &db,
            "query",
            &json!({"collection": "notes", "filter": {"stars": {"$gte": 3}}}),
        )
        .unwrap();
        assert_eq!(q["count"], 1);

        dispatch(
            &db,
            "update",
            &json!({"collection": "notes", "id": id, "doc": {"title": "hi2"}}),
        )
        .unwrap();
        dispatch(&db, "delete", &json!({"collection": "notes", "id": id})).unwrap();
        let gone = dispatch(&db, "get", &json!({"collection": "notes", "id": id})).unwrap();
        assert!(gone["doc"].is_null());
    }

    #[test]
    fn batch_and_indexes_and_inspect() {
        let db = db();
        let ids = dispatch(
            &db,
            "batch",
            &json!({"ops": [
                {"op": "insert", "collection": "a", "doc": {"k": 1}},
                {"op": "upsert", "collection": "a", "doc": {"_id": "x", "k": 2}},
            ]}),
        )
        .unwrap();
        assert_eq!(ids["ids"].as_array().unwrap().len(), 2);

        dispatch(
            &db,
            "create_index",
            &json!({"collection": "a", "field": "k", "unique": false}),
        )
        .unwrap();
        dispatch(
            &db,
            "purpose",
            &json!({"collection": "a", "text": "test docs"}),
        )
        .unwrap();

        let md = dispatch(&db, "inspect", &json!({})).unwrap();
        assert!(md["markdown"].as_str().unwrap().contains("test docs"));
    }

    #[test]
    fn unknown_op_lists_the_menu() {
        let err = dispatch(&db(), "quyre", &json!({})).unwrap_err();
        assert_eq!(err.code, "not_implemented");
        assert!(err.hint.contains("query"));
    }

    #[test]
    fn missing_args_answer_with_rich_errors() {
        let err = dispatch(&db(), "insert", &json!({"collection": "c"})).unwrap_err();
        assert_eq!(err.code, "invalid_document");
        assert!(!err.hint.is_empty());
    }
}
