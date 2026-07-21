//! Format migration: old .tepin file in, current-format file out, the
//! original untouched — the standing promise behind "the format may change
//! freely before 1.0".
//!
//! Shape: primary data (documents, collection meta, vectors, the embedder
//! pin, the pending queue) is copied through the version-aware readers and
//! re-written in canonical current form; derived state (keyword index,
//! secondary indexes) is rebuilt from the documents. The copy is verified
//! against the source before reporting success, and the destination is
//! always a new file — migration never touches the original.

use std::path::Path;

use redb::{ReadableDatabase, ReadableTable, TableDefinition};
use serde_json::Value;

use crate::db::{data_table, CollectionMeta, Db, COLLECTION_PREFIX, META};
use crate::error::{Result, TepinError};
use crate::format::{self, FORMAT_VERSION, PREAMBLE_LEN};
use crate::vector::{chunk_key, parse_chunk_key, vec_table, EMBEDDER_KEY, PENDING};

/// What a migration did, for reporting surfaces (CLI JSON output).
#[derive(Debug, Clone, serde::Serialize)]
pub struct MigrateReport {
    pub from_format: u32,
    pub to_format: u32,
    pub collections: usize,
    pub documents: u64,
    pub vector_rows: u64,
}

fn byte_def(name: &str) -> TableDefinition<'_, &'static str, &'static [u8]> {
    TableDefinition::new(name)
}

/// Migrate `src` into a brand-new file at `dst`. `src` is opened but never
/// modified; `dst` must not exist yet. Works for every published format
/// version — including the current one, where it doubles as a canonicalizing
/// copy (legacy vector keys are normalized, derived indexes rebuilt).
pub fn migrate_file(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> Result<MigrateReport> {
    let (src, dst) = (src.as_ref(), dst.as_ref());
    if !src.exists() {
        return Err(TepinError::new(
            "file_not_found",
            format!("no database file at {}", src.display()),
            "check the source path; migrate reads an existing .tepin file",
        ));
    }
    if dst.exists() {
        return Err(TepinError::new(
            "destination_exists",
            format!("{} already exists", dst.display()),
            "migrate never overwrites; pass a fresh output path (the original is left untouched either way)",
        ));
    }

    // The preamble names the source's format version (and politely rejects
    // files newer than this build — same rule as open).
    let head = {
        use std::io::Read;
        let mut file = std::fs::File::open(src)?;
        let mut buf = vec![0u8; PREAMBLE_LEN as usize];
        let n = file.read(&mut buf)?;
        buf.truncate(n);
        buf
    };
    let from_format = format::parse_preamble(&head)?.format_version;

    // Version dispatch. Every format version ever published stays readable
    // here forever. v0 — the only published format — reads through the live
    // engine, whose compat shims (e.g. pre-chunking vector keys) normalize
    // during the copy below.
    match from_format {
        0 => {}
        newer => unreachable!("parse_preamble rejects format v{newer}"),
    }

    // The original is never even opened for writing: take the same OS lock
    // every writer takes (so no process mutates it mid-copy), snapshot its
    // bytes, and migrate from the snapshot. Opening src through redb would
    // write engine bookkeeping into it — snapshotting keeps "untouched"
    // literal, down to the byte.
    let src_file = std::fs::File::open(src)?;
    if let Err(e) = src_file.try_lock() {
        return Err(TepinError::new(
            "database_locked",
            format!("{} is open in another process", src.display()),
            "close the other process before migrating; tepindb allows one process at a time",
        )
        .with_source(std::io::Error::other(e.to_string())));
    }
    let snapshot = {
        let mut os = dst.as_os_str().to_owned();
        os.push(".migrate-src");
        std::path::PathBuf::from(os)
    };
    std::fs::copy(src, &snapshot)?;

    let result = (|| {
        let src_db = Db::open(&snapshot)?;
        let dst_db = Db::open(dst)?;
        let report = copy_all(&src_db, &dst_db, from_format)?;
        verify(&src_db, &dst_db)?;
        Ok(report)
    })();
    let _ = std::fs::remove_file(&snapshot);
    if result.is_err() {
        // dst didn't exist before this call — a failed migration leaves
        // nothing behind.
        let _ = std::fs::remove_file(dst);
    }
    result
}

/// One read snapshot of src, one atomic write transaction into dst.
fn copy_all(src: &Db, dst: &Db, from_format: u32) -> Result<MigrateReport> {
    let src_txn = src.core.db.begin_read()?;
    let dst_txn = dst.core.db.begin_write()?;
    let mut documents = 0u64;
    let mut vector_rows = 0u64;

    // Collection meta, the embedder pin, and the pending queue — copied.
    // Meta rows round-trip through CollectionMeta, so they land in
    // canonical current shape.
    let mut collections: Vec<(String, CollectionMeta)> = Vec::new();
    match src_txn.open_table(META) {
        Ok(src_meta) => {
            let mut dst_meta = dst_txn.open_table(META)?;
            for entry in src_meta.iter()? {
                let (key, val) = entry?;
                if let Some(name) = key.value().strip_prefix(COLLECTION_PREFIX) {
                    let cm: CollectionMeta = serde_json::from_str(val.value()).unwrap_or_default();
                    dst_meta.insert(key.value(), serde_json::to_string(&cm)?.as_str())?;
                    collections.push((name.to_string(), cm));
                } else if key.value() == EMBEDDER_KEY {
                    dst_meta.insert(key.value(), val.value())?;
                }
            }
        }
        Err(redb::TableError::TableDoesNotExist(_)) => {}
        Err(e) => return Err(e.into()),
    }
    match src_txn.open_table(PENDING) {
        Ok(src_pending) => {
            let mut dst_pending = dst_txn.open_table(PENDING)?;
            for entry in src_pending.iter()? {
                let (key, val) = entry?;
                dst_pending.insert(key.value(), val.value())?;
            }
        }
        Err(redb::TableError::TableDoesNotExist(_)) => {}
        Err(e) => return Err(e.into()),
    }

    for (name, cm) in &collections {
        // Documents: raw byte copy — a migration must not reformat JSON.
        let docs: Vec<(String, Vec<u8>)> = match src_txn.open_table(byte_def(&data_table(name))) {
            Ok(t) => t
                .iter()?
                .map(|e| e.map(|(k, v)| (k.value().to_string(), v.value().to_vec())))
                .collect::<std::result::Result<_, _>>()?,
            Err(redb::TableError::TableDoesNotExist(_)) => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        if !docs.is_empty() {
            let mut dst_docs = dst_txn.open_table(byte_def(&data_table(name)))?;
            for (id, bytes) in &docs {
                dst_docs.insert(id.as_str(), bytes.as_slice())?;
            }
            documents += docs.len() as u64;
        }

        // Derived state: rebuilt from the documents, not copied.
        for (id, bytes) in &docs {
            let Ok(doc) = serde_json::from_slice::<Value>(bytes) else {
                continue;
            };
            if !cm.embed.is_empty() {
                let text = crate::vector::build_text(&doc, &cm.embed);
                crate::fts::index_add(&dst_txn, name, id, &text)?;
            }
            // Uniqueness is not re-enforced here: existing data wins over
            // a constraint it may predate — migration never drops rows.
            for field in &cm.indexes {
                crate::index::index_add(&dst_txn, name, field, &doc, id, false)?;
            }
        }

        // Vectors: rows re-keyed through the chunk-key parser, which is
        // where pre-chunking plain keys become canonical `{id}\0{chunk}`.
        match src_txn.open_table(byte_def(&vec_table(name))) {
            Ok(src_vecs) => {
                let mut dst_vecs = dst_txn.open_table(byte_def(&vec_table(name)))?;
                for entry in src_vecs.iter()? {
                    let (key, bytes) = entry?;
                    let (id, chunk) = parse_chunk_key(key.value());
                    dst_vecs.insert(chunk_key(id, chunk as usize).as_str(), bytes.value())?;
                    vector_rows += 1;
                }
            }
            Err(redb::TableError::TableDoesNotExist(_)) => {}
            Err(e) => return Err(e.into()),
        }
    }

    dst_txn.commit()?;
    Ok(MigrateReport {
        from_format,
        to_format: FORMAT_VERSION,
        collections: collections.len(),
        documents,
        vector_rows,
    })
}

/// Re-read both sides and require every source document byte-for-byte in
/// the destination before calling the migration a success.
fn verify(src: &Db, dst: &Db) -> Result<()> {
    let mismatch = |what: String| {
        TepinError::new(
            "migration_failed",
            format!("verification failed: {what}"),
            "the output file is incomplete — delete it and re-run; the original is untouched",
        )
    };
    let src_txn = src.core.db.begin_read()?;
    let dst_txn = dst.core.db.begin_read()?;
    for col in src.collections()? {
        let src_docs = match src_txn.open_table(byte_def(&data_table(&col.name))) {
            Ok(t) => Some(t),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(e) => return Err(e.into()),
        };
        let Some(src_docs) = src_docs else { continue };
        let dst_docs = match dst_txn.open_table(byte_def(&data_table(&col.name))) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(mismatch(format!("collection {:?} missing", col.name)))
            }
            Err(e) => return Err(e.into()),
        };
        for entry in src_docs.iter()? {
            let (key, val) = entry?;
            match dst_docs.get(key.value())? {
                Some(copied) if copied.value() == val.value() => {}
                _ => {
                    return Err(mismatch(format!(
                        "document {:?} in {:?} differs",
                        key.value(),
                        col.name
                    )))
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A pre-chunking file stored vectors under the bare doc id. Write one
    /// raw legacy row and prove migration re-keys it canonically.
    #[test]
    fn legacy_plain_vector_keys_are_normalized() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("legacy.tepin");
        let dst = dir.path().join("current.tepin");
        {
            let db = Db::open(&src).unwrap();
            db.set_manual_vectors("notes", &["title"]).unwrap();
            db.insert("notes", json!({"_id": "n1", "title": "old"}))
                .unwrap();
            db.set_vectors("notes", "n1", "m", &[vec![1.0, 0.0]])
                .unwrap();
            // Rewrite the row the way pre-chunking builds keyed it.
            let txn = db.core.db.begin_write().unwrap();
            {
                let mut vecs = txn.open_table(byte_def(&vec_table("notes"))).unwrap();
                let bytes = {
                    let removed = vecs.remove(chunk_key("n1", 0).as_str()).unwrap().unwrap();
                    removed.value().to_vec()
                };
                vecs.insert("n1", bytes.as_slice()).unwrap();
            }
            txn.commit().unwrap();
        }

        let report = migrate_file(&src, &dst).unwrap();
        assert_eq!(report.vector_rows, 1);

        let db = Db::open(&dst).unwrap();
        let txn = db.core.db.begin_read().unwrap();
        let vecs = txn.open_table(byte_def(&vec_table("notes"))).unwrap();
        assert!(vecs.get("n1").unwrap().is_none(), "legacy key must be gone");
        assert!(vecs.get(chunk_key("n1", 0).as_str()).unwrap().is_some());
        drop(vecs);
        drop(txn);
        assert_eq!(db.get_vectors("notes", "n1").unwrap(), vec![vec![1.0, 0.0]]);
    }
}
