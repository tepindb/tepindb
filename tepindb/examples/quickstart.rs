//! The 60-second tour: documents, filters, purposes — no model, no network.
//!
//!     cargo run --example quickstart

use serde_json::json;

fn main() -> tepindb::Result<()> {
    let dir = std::env::temp_dir().join("tepindb-quickstart");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("quickstart.tepin");
    let _ = std::fs::remove_file(&path);

    // One file. Run `head` on it afterwards — it explains itself.
    let db = tepindb::open(&path)?;

    // Collections appear on first insert; docs get short sortable ids.
    let id = db.insert("notes", json!({"title": "hello tepin", "stars": 5}))?;
    db.insert_many(
        "notes",
        vec![
            json!({"title": "second note", "stars": 2}),
            json!({"title": "third note", "stars": 4, "tag": "work"}),
        ],
    )?;
    println!("inserted, first id = {id}");

    // Mongo-style filters, same syntax as the CLI and MCP tools.
    let good = db.find("notes", &json!({"stars": {"$gte": 4}}))?;
    println!("stars >= 4 → {} docs", good.len());

    let tagged = db.find("notes", &json!({"tag": "work"}))?;
    println!("tag == work → {:?}", tagged[0]["title"]);

    // Documents update by id; the stored _id always wins.
    db.update(
        "notes",
        &id,
        json!({"title": "hello tepin (edited)", "stars": 5}),
    )?;
    println!("updated → {:?}", db.get("notes", &id)?.unwrap()["title"]);

    // Purpose metadata tells the next reader (human or LLM) what this is.
    db.set_purpose("notes", "quickstart demo notes")?;
    for c in db.collections()? {
        println!("collection {} ({} docs): {:?}", c.name, c.count, c.purpose);
    }

    // Rich errors: stable code + hint, everywhere.
    let err = db.get("nope", "id").unwrap_err();
    println!("error demo → code={} hint={}", err.code, err.hint);

    println!(
        "\ndb file: {} — try `head {}`",
        path.display(),
        path.display()
    );
    Ok(())
}
