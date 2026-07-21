//! Secondary (equality) indexes: one redb table per indexed field
//! (`idx:{collection}:{field}`), maintained inside the same write
//! transaction as the document, so an index can never disagree with the
//! data it points at.
//!
//! The index is hash-based — key = `{fnv1a(value)}\0{doc_id}` — which makes
//! it equality-only by design. `find` re-verifies every candidate against
//! the full filter, so a hash collision costs one extra read, never a
//! wrong result. Numbers hash their f64 bits, so `5` and `5.0` land on the
//! same key (Mongo-style numeric equality); a missing field indexes as
//! null, matching the filter semantics.

use redb::{ReadableTable, TableDefinition};
use serde_json::Value;

use crate::error::{Result, TepinError};

fn idx_table(collection: &str, field: &str) -> String {
    format!("idx:{collection}:{field}")
}

fn def(name: &str) -> TableDefinition<'_, &'static str, &'static [u8]> {
    TableDefinition::new(name)
}

/// FNV-1a 64 over a type-tagged canonical encoding of the value.
fn value_hash(value: &Value) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let eat = |h: &mut u64, bytes: &[u8]| {
        for &b in bytes {
            *h = (*h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    match value {
        Value::Null => eat(&mut h, b"z"),
        Value::Bool(b) => {
            eat(&mut h, b"b");
            eat(&mut h, &[u8::from(*b)]);
        }
        Value::Number(n) => {
            eat(&mut h, b"n");
            // f64 bits: 5 and 5.0 must collide (numeric equality).
            eat(
                &mut h,
                &n.as_f64().unwrap_or(f64::MAX).to_bits().to_be_bytes(),
            );
        }
        Value::String(s) => {
            eat(&mut h, b"s");
            eat(&mut h, s.as_bytes());
        }
        // Arrays/objects: serde_json's default map is sorted, so this
        // string form is canonical.
        other => {
            eat(&mut h, b"j");
            eat(&mut h, other.to_string().as_bytes());
        }
    }
    format!("{h:016x}")
}

fn entry_key(value: &Value, id: &str) -> String {
    format!("{}\u{0}{id}", value_hash(value))
}

pub(crate) fn index_add(
    txn: &redb::WriteTransaction,
    collection: &str,
    field: &str,
    doc: &Value,
    id: &str,
    unique: bool,
) -> Result<()> {
    let value = doc.get(field).unwrap_or(&Value::Null);
    // Null (and missing) values are exempt from uniqueness, SQL-style.
    if unique && !value.is_null() {
        if let Some(holder) = unique_holder(txn, collection, field, value, id)? {
            return Err(TepinError::new(
                "unique_violation",
                format!(
                    "collection {collection:?} already has a document with {field} = {value} (id {holder:?})"
                ),
                "update the existing document instead, or drop the unique index if duplicates are intended",
            ));
        }
    }
    let name = idx_table(collection, field);
    let mut table = txn.open_table(def(&name))?;
    table.insert(entry_key(value, id).as_str(), [].as_slice())?;
    Ok(())
}

/// The id of another document already holding `value` under a unique index,
/// if any. Index keys are hashes, so every same-hash entry is re-verified
/// against its stored document — a collision must never fake a violation.
fn unique_holder(
    txn: &redb::WriteTransaction,
    collection: &str,
    field: &str,
    value: &Value,
    id: &str,
) -> Result<Option<String>> {
    let name = idx_table(collection, field);
    let table = txn.open_table(def(&name))?;
    let start = format!("{}\u{0}", value_hash(value));
    let end = format!("{}\u{1}", value_hash(value));
    let mut candidates = Vec::new();
    for entry in table.range(start.as_str()..end.as_str())? {
        let (key, _) = entry?;
        if let Some((_, other)) = key.value().split_once('\u{0}') {
            if other != id {
                candidates.push(other.to_string());
            }
        }
    }
    drop(table);
    if candidates.is_empty() {
        return Ok(None);
    }
    let data = txn.open_table(def(&crate::db::data_table(collection)))?;
    for other in candidates {
        if let Some(bytes) = data.get(other.as_str())? {
            let doc: Value = serde_json::from_slice(bytes.value())?;
            let stored = doc.get(field).unwrap_or(&Value::Null);
            if crate::db::values_equal(stored, value) {
                return Ok(Some(other));
            }
        }
    }
    Ok(None)
}

pub(crate) fn index_remove(
    txn: &redb::WriteTransaction,
    collection: &str,
    field: &str,
    doc: &Value,
    id: &str,
) -> Result<()> {
    let value = doc.get(field).unwrap_or(&Value::Null);
    let name = idx_table(collection, field);
    let mut table = match txn.open_table(def(&name)) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    table.remove(entry_key(value, id).as_str())?;
    Ok(())
}

pub(crate) fn drop_index_table(
    txn: &redb::WriteTransaction,
    collection: &str,
    field: &str,
) -> Result<()> {
    txn.delete_table(def(&idx_table(collection, field)))?;
    Ok(())
}

/// Candidate doc ids for `field == value` — a superset (hash collisions
/// possible); the caller re-verifies with the full filter.
pub(crate) fn candidates(
    txn: &redb::ReadTransaction,
    collection: &str,
    field: &str,
    value: &Value,
) -> Result<Vec<String>> {
    let name = idx_table(collection, field);
    let table = match txn.open_table(def(&name)) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let start = format!("{}\u{0}", value_hash(value));
    let end = format!("{}\u{1}", value_hash(value));
    let mut ids = Vec::new();
    for entry in table.range(start.as_str()..end.as_str())? {
        let (key, _) = entry?;
        if let Some((_, id)) = key.value().split_once('\u{0}') {
            ids.push(id.to_string());
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numeric_equality_collides_on_purpose() {
        assert_eq!(value_hash(&json!(5)), value_hash(&json!(5.0)));
        assert_ne!(value_hash(&json!(5)), value_hash(&json!(6)));
    }

    #[test]
    fn types_do_not_collide_by_tag() {
        assert_ne!(value_hash(&json!("5")), value_hash(&json!(5)));
        assert_ne!(value_hash(&json!(null)), value_hash(&json!(false)));
        assert_ne!(value_hash(&json!(true)), value_hash(&json!("true")));
    }
}
