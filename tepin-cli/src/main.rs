//! The `tepin` CLI. Every command answers in JSON (inspect answers in
//! markdown); every error is `{"error": {code, message, hint}}` on stderr.
//! The CLI and the MCP server expose the same operations with identical
//! behavior — one surface to learn.

mod mcp;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use tepin_core::{Db, TepinError};

#[derive(Parser)]
#[command(
    name = "tepin",
    version,
    about = "TepinDB — AI-first single-file database for CLI tools",
    after_help = "The database file can also come from the TEPIN_DB environment variable."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct FileArg {
    /// Path to the .tepin database file (or set TEPIN_DB)
    #[arg(value_name = "FILE")]
    file: PathBuf,
}

#[derive(Subcommand)]
enum Command {
    /// Markdown report: collections, purposes, stats — start here
    Inspect {
        #[command(flatten)]
        file: FileArg,
    },
    /// Find documents with a MongoDB-style JSON filter
    Query {
        #[command(flatten)]
        file: FileArg,
        collection: String,
        /// e.g. '{"status": "open", "stars": {"$gte": 3}}'
        #[arg(default_value = "{}")]
        filter: String,
    },
    /// Semantic vector search across embedded collections
    Search {
        #[command(flatten)]
        file: FileArg,
        query: String,
        /// Restrict to one collection (default: everything embedded)
        #[arg(long)]
        collection: Option<String>,
        /// Maximum number of results
        #[arg(long, default_value_t = 5)]
        limit: usize,
    },
    /// Insert a JSON document (creates the collection on first use)
    Insert {
        #[command(flatten)]
        file: FileArg,
        collection: String,
        /// The document, e.g. '{"title": "hello"}'
        doc: String,
    },
    /// Insert-or-replace by _id: replaces an existing document with the
    /// same _id, inserts otherwise
    Upsert {
        #[command(flatten)]
        file: FileArg,
        collection: String,
        /// The document, e.g. '{"_id": "n1", "title": "hello"}'
        doc: String,
    },
    /// Fetch one document by id
    Get {
        #[command(flatten)]
        file: FileArg,
        collection: String,
        id: String,
    },
    /// Replace a document by id
    Update {
        #[command(flatten)]
        file: FileArg,
        collection: String,
        id: String,
        doc: String,
    },
    /// Delete a document by id
    Delete {
        #[command(flatten)]
        file: FileArg,
        collection: String,
        id: String,
    },
    /// Set the free-text purpose of a collection (shown by inspect)
    Purpose {
        #[command(flatten)]
        file: FileArg,
        collection: String,
        text: String,
    },
    /// Declare which fields get embedded for vector search (backfills
    /// existing docs into the embed queue)
    EmbedFields {
        #[command(flatten)]
        file: FileArg,
        collection: String,
        fields: Vec<String>,
    },
    /// Serve MCP tools over stdio — plug this database into an AI agent
    Mcp {
        #[command(flatten)]
        file: FileArg,
    },
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // An explicit file argument always wins; only when parsing fails and
        // TEPIN_DB is set do we retry with the env path spliced in after the
        // subcommand. Deterministic — no path-shaped guessing.
        Err(parse_err) => match retry_with_env_file() {
            Some(cli) => cli,
            None => parse_err.exit(),
        },
    };
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", serde_json::to_string_pretty(&e.to_json()).unwrap());
            ExitCode::FAILURE
        }
    }
}

fn retry_with_env_file() -> Option<Cli> {
    let db = std::env::var_os("TEPIN_DB")?;
    let mut argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if argv.len() < 2 {
        return None;
    }
    argv.insert(2, db);
    Cli::try_parse_from(argv).ok()
}

fn run(cli: Cli) -> Result<(), TepinError> {
    match cli.command {
        Command::Inspect { file } => {
            let db = Db::open_existing(&file.file)?;
            print!("{}", inspect_markdown(&db, &file.file)?);
        }
        Command::Query {
            file,
            collection,
            filter,
        } => {
            let db = Db::open_existing(&file.file)?;
            let filter: Value = serde_json::from_str(&filter)?;
            let docs = db.find(&collection, &filter)?;
            emit(&json!({"count": docs.len(), "docs": docs}));
        }
        Command::Insert {
            file,
            collection,
            doc,
        } => {
            let db = Db::open(&file.file)?;
            let doc: Value = serde_json::from_str(&doc)?;
            let id = db.insert(&collection, doc)?;
            emit(&json!({"inserted": id, "collection": collection}));
        }
        Command::Upsert {
            file,
            collection,
            doc,
        } => {
            let db = Db::open(&file.file)?;
            let doc: Value = serde_json::from_str(&doc)?;
            let id = db.upsert(&collection, doc)?;
            emit(&json!({"upserted": id, "collection": collection}));
        }
        Command::Get {
            file,
            collection,
            id,
        } => {
            let db = Db::open_existing(&file.file)?;
            match db.get(&collection, &id)? {
                Some(doc) => emit(&doc),
                None => emit(&json!({"doc": null, "id": id})),
            }
        }
        Command::Update {
            file,
            collection,
            id,
            doc,
        } => {
            let db = Db::open(&file.file)?;
            let doc: Value = serde_json::from_str(&doc)?;
            db.update(&collection, &id, doc)?;
            emit(&json!({"updated": id, "collection": collection}));
        }
        Command::Delete {
            file,
            collection,
            id,
        } => {
            let db = Db::open(&file.file)?;
            db.delete(&collection, &id)?;
            emit(&json!({"deleted": id, "collection": collection}));
        }
        Command::Purpose {
            file,
            collection,
            text,
        } => {
            let db = Db::open(&file.file)?;
            db.set_purpose(&collection, &text)?;
            emit(&json!({"collection": collection, "purpose": text}));
        }
        Command::EmbedFields {
            file,
            collection,
            fields,
        } => {
            let db = Db::open(&file.file)?;
            let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
            db.set_embed_fields(&collection, &refs)?;
            emit(&json!({
                "collection": collection,
                "embed": fields,
                "pending_embeddings": db.pending_embeddings()?,
            }));
        }
        #[cfg(feature = "embed")]
        Command::Search {
            file,
            query,
            collection,
            limit,
        } => {
            let mut db = Db::open_existing(&file.file)?;
            let cache = tepin_embed::fetch::default_cache_dir()?;
            let lazy =
                tepin_embed::OnnxEmbedder::spawn_lazy(&tepin_embed::fetch::BGE_SMALL, cache);
            db.attach_embedder(std::sync::Arc::new(lazy))?;
            let hits = db.search(collection.as_deref(), &query, limit)?;
            emit(&json!({"count": hits.len(), "hits": hits}));
        }
        #[cfg(not(feature = "embed"))]
        Command::Search { .. } => {
            return Err(TepinError::new(
                "not_implemented",
                "this build has no embedding support (slim binary)",
                "use a full build (`cargo install tepin-cli`) for semantic search; `tepin query` works everywhere",
            ))
        }
        Command::Mcp { file } => {
            mcp::serve(&file.file)?;
        }
    }
    Ok(())
}

fn emit(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap());
}

pub(crate) fn inspect_markdown(db: &Db, path: &std::path::Path) -> Result<String, TepinError> {
    use std::fmt::Write;
    let cols = db.collections()?;
    let total: u64 = cols.iter().map(|c| c.count).sum();
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    let mut md = String::new();
    let _ = writeln!(md, "# TepinDB — {}\n", path.display());
    let _ = writeln!(
        md,
        "Format v{} · {} collection(s) · {} document(s) · {:.1} KiB\n",
        tepin_core::format::FORMAT_VERSION,
        cols.len(),
        total,
        size as f64 / 1024.0
    );
    if cols.is_empty() {
        let _ = writeln!(
            md,
            "This database is empty. Create a collection by inserting:\n\n\
             ```\ntepin insert {} <collection> '{{\"any\": \"json\"}}'\n```",
            path.display()
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
             ```\ntepin query {} <collection> '{{\"field\": \"value\"}}'\n```",
            path.display()
        );
    }
    Ok(md)
}
