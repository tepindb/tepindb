//! notes — the TepinDB example application.
//!
//! A tiny but real note-taking CLI that exercises every driver operation:
//! documents, filters, updates, deletes, purposes, embed config, and
//! semantic search. Deliberately dependency-light (hand-rolled arg
//! parsing) so the tepindb calls stay in plain sight.
//!
//! Configuration is two env vars:
//!   NOTES_DB       — the .tepin file (default ./notes.tepin)
//!   NOTES_EMBEDDER — auto | mock | none (default none)
//!     auto: the real bge-small model, lazy-downloaded (tepindb::open_auto)
//!     mock: deterministic test embedder — used by this app's test suite
//!     none: pure document store; `notes search` explains what to do
//!
//! Usage:
//!   notes add "text of the note" [tag]
//!   notes list
//!   notes find '{"tag": "work"}'
//!   notes search "anything, in natural language" [k]
//!   notes done <id>
//!   notes rm <id>
//!   notes info

use std::process::ExitCode;
use std::sync::Arc;

use serde_json::json;
use tepindb::{Db, Result};

fn open_db() -> Result<Db> {
    let path = std::env::var("NOTES_DB").unwrap_or_else(|_| "notes.tepin".into());
    let mut db = tepindb::open(&path)?;
    match std::env::var("NOTES_EMBEDDER").as_deref() {
        Ok("auto") => tepindb::attach_default_embedder(&mut db)?,
        Ok("mock") => db.attach_embedder(Arc::new(tepindb::MockEmbedder::new(64)))?,
        _ => {}
    }
    if db.embedder_attached() {
        db.set_embed_fields("notes", &["text"])?;
    }
    db.set_purpose(
        "notes",
        "personal notes; text is embedded for semantic search",
    )?;
    Ok(db)
}

fn run(args: &[String]) -> Result<()> {
    let db = open_db()?;
    match args {
        [cmd, text] if cmd == "add" => {
            let id = db.insert("notes", json!({"text": text, "status": "open"}))?;
            println!("{}", json!({"added": id}));
        }
        [cmd, text, tag] if cmd == "add" => {
            let id = db.insert("notes", json!({"text": text, "tag": tag, "status": "open"}))?;
            println!("{}", json!({"added": id, "tag": tag}));
        }
        [cmd] if cmd == "list" => {
            let docs = db.find("notes", &json!({}))?;
            for d in &docs {
                println!("{d}");
            }
            eprintln!("{} note(s)", docs.len());
        }
        [cmd, filter] if cmd == "find" => {
            let filter = serde_json::from_str(filter)?;
            for d in db.find("notes", &filter)? {
                println!("{d}");
            }
        }
        [cmd, query, rest @ ..] if cmd == "search" => {
            let k = rest.first().and_then(|s| s.parse().ok()).unwrap_or(5usize);
            for hit in db.search(Some("notes"), query, k)? {
                println!(
                    "{}",
                    json!({"score": hit.score, "id": hit.id, "text": hit.doc["text"]})
                );
            }
        }
        [cmd, id] if cmd == "done" => {
            let mut doc = db.get("notes", id)?.ok_or_else(|| {
                tepindb::TepinError::new(
                    "doc_not_found",
                    format!("no note {id:?}"),
                    "run `notes list` to see ids",
                )
            })?;
            doc["status"] = json!("done");
            db.update("notes", id, doc)?;
            println!("{}", json!({"done": id}));
        }
        [cmd, id] if cmd == "rm" => {
            db.delete("notes", id)?;
            println!("{}", json!({"removed": id}));
        }
        [cmd] if cmd == "info" => {
            for c in db.collections()? {
                println!(
                    "{}",
                    json!({"collection": c.name, "docs": c.count, "purpose": c.purpose,
                           "embed": c.embed})
                );
            }
            println!(
                "{}",
                json!({"pending_embeddings": db.pending_embeddings()?})
            );
        }
        _ => {
            eprintln!(
                "usage: notes add <text> [tag] | list | find <json> | search <query> [k] | done <id> | rm <id> | info"
            );
            std::process::exit(2);
        }
    }
    // Flush before exit so a short-lived process never strands the queue
    // (it would heal next run anyway — this keeps search warm for the
    // NEXT invocation too).
    db.flush_embeddings()?;
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", e.to_json());
            ExitCode::FAILURE
        }
    }
}
