//! Semantic search with the real model. First run downloads bge-small
//! (34MB, SHA-256-verified) into the shared cache; afterwards it's instant.
//!
//!     cargo run --example vector_search

use serde_json::json;

fn main() -> tepindb::Result<()> {
    let dir = std::env::temp_dir().join("tepindb-quickstart");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("vector.tepin");
    let _ = std::fs::remove_file(&path);

    // open_auto attaches bge-small lazily: this call returns instantly,
    // the model loads in the background, only search() waits for it.
    let db = tepindb::open_auto(&path)?;

    db.set_embed_fields("memory", &["title", "body"])?;
    db.insert_many(
        "memory",
        vec![
            json!({"title": "Password reset flow",
                   "body": "Users click 'forgot password' in settings to get a reset email."}),
            json!({"title": "Standup notes",
                   "body": "The deploy is blocked on the flaky integration test."}),
            json!({"title": "Groceries",
                   "body": "Olive oil, tomatoes, basil, parmesan."}),
        ],
    )?;

    for query in [
        "how do users recover their account",
        "what should I cook tonight",
    ] {
        let hits = db.search(None, query, 1)?;
        println!(
            "{query:45} → {:?} (score {:.3})",
            hits[0].doc["title"], hits[0].score
        );
    }
    Ok(())
}
