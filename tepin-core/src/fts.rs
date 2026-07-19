//! The keyword half of hybrid search: a BM25 inverted index per embedded
//! collection, maintained synchronously inside the same write transaction
//! as the document (tokenization is cheap — unlike embedding, it needs no
//! queue). The same `embed` fields config drives both signals, so one
//! declaration buys a document both meanings and words.
//!
//! Layout, one redb table per collection (`fts:{name}`, &str → u64):
//!   "t\0{term}\0{id}" → term frequency in that doc
//!   "d\0{id}"         → doc length in tokens
//!   "s\0docs"         → number of indexed docs
//!   "s\0len"          → total tokens across docs

use std::collections::HashMap;

use redb::{ReadableTable, TableDefinition};

use crate::error::Result;

const K1: f32 = 1.2;
const B: f32 = 0.75;

fn fts_table(name: &str) -> String {
    format!("fts:{name}")
}

fn def(table_name: &str) -> TableDefinition<'_, &'static str, u64> {
    TableDefinition::new(table_name)
}

/// Lowercased unicode-alphanumeric words, length ≥ 2. "TEP-1234" → ["tep",
/// "1234"] — queries split the same way, so code-ish tokens match exactly.
pub(crate) fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 2)
        .map(str::to_string)
        .collect()
}

fn term_counts(text: &str) -> HashMap<String, u64> {
    let mut counts = HashMap::new();
    for token in tokenize(text) {
        *counts.entry(token).or_insert(0) += 1;
    }
    counts
}

/// Add one doc's terms to the index (insert/update paths, inside the txn).
pub(crate) fn index_add(
    txn: &redb::WriteTransaction,
    collection: &str,
    id: &str,
    text: &str,
) -> Result<()> {
    let table_name = fts_table(collection);
    let mut table = txn.open_table(def(&table_name))?;
    let counts = term_counts(text);
    let doc_len: u64 = counts.values().sum();
    for (term, count) in &counts {
        table.insert(format!("t\u{0}{term}\u{0}{id}").as_str(), count)?;
    }
    table.insert(format!("d\u{0}{id}").as_str(), doc_len)?;
    bump(&mut table, "s\u{0}docs", 1)?;
    bump(&mut table, "s\u{0}len", doc_len as i64)?;
    Ok(())
}

/// Remove one doc's terms (update/delete paths, inside the txn). The old
/// doc text is re-tokenized — the index never needs a separate term log.
pub(crate) fn index_remove(
    txn: &redb::WriteTransaction,
    collection: &str,
    id: &str,
    old_text: &str,
) -> Result<()> {
    let table_name = fts_table(collection);
    let mut table = match txn.open_table(def(&table_name)) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let counts = term_counts(old_text);
    for term in counts.keys() {
        table.remove(format!("t\u{0}{term}\u{0}{id}").as_str())?;
    }
    let doc_len = table
        .remove(format!("d\u{0}{id}").as_str())?
        .map(|v| v.value())
        .unwrap_or(0);
    if doc_len > 0 || !counts.is_empty() {
        bump(&mut table, "s\u{0}docs", -1)?;
        bump(&mut table, "s\u{0}len", -(doc_len as i64))?;
    }
    Ok(())
}

/// Drop a collection's whole index (embed config rebuild).
pub(crate) fn index_clear(txn: &redb::WriteTransaction, collection: &str) -> Result<()> {
    let table_name = fts_table(collection);
    txn.delete_table(def(&table_name))?;
    Ok(())
}

fn bump(table: &mut redb::Table<&'static str, u64>, key: &str, delta: i64) -> Result<()> {
    let current = table.get(key)?.map(|v| v.value()).unwrap_or(0);
    let next = (current as i64 + delta).max(0) as u64;
    table.insert(key, next)?;
    Ok(())
}

/// Raw BM25 scores for a query against one collection: doc id → score.
/// Only docs sharing at least one term appear.
pub(crate) fn bm25_scores(
    txn: &redb::ReadTransaction,
    collection: &str,
    query_terms: &[String],
) -> Result<HashMap<String, f32>> {
    let table_name = fts_table(collection);
    let table = match txn.open_table(def(&table_name)) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(HashMap::new()),
        Err(e) => return Err(e.into()),
    };
    let docs = table.get("s\u{0}docs")?.map(|v| v.value()).unwrap_or(0);
    if docs == 0 {
        return Ok(HashMap::new());
    }
    let total_len = table.get("s\u{0}len")?.map(|v| v.value()).unwrap_or(0);
    let avg_len = (total_len as f32 / docs as f32).max(1.0);

    let mut scores: HashMap<String, f32> = HashMap::new();
    let mut len_cache: HashMap<String, f32> = HashMap::new();
    for term in query_terms {
        let start = format!("t\u{0}{term}\u{0}");
        let end = format!("t\u{0}{term}\u{1}");
        let matches: Vec<(String, u64)> = table
            .range(start.as_str()..end.as_str())?
            .map(|e| e.map(|(k, v)| (k.value().to_string(), v.value())))
            .collect::<std::result::Result<_, _>>()?;
        if matches.is_empty() {
            continue;
        }
        let df = matches.len() as f32;
        let idf = (1.0 + (docs as f32 - df + 0.5) / (df + 0.5)).ln();
        for (key, tf) in matches {
            let Some(id) = key.rsplit('\u{0}').next() else {
                continue;
            };
            let doc_len = match len_cache.get(id) {
                Some(&l) => l,
                None => {
                    let l = table
                        .get(format!("d\u{0}{id}").as_str())?
                        .map(|v| v.value() as f32)
                        .unwrap_or(avg_len);
                    len_cache.insert(id.to_string(), l);
                    l
                }
            };
            let tf = tf as f32;
            let contribution =
                idf * (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * doc_len / avg_len));
            *scores.entry(id.to_string()).or_insert(0.0) += contribution;
        }
    }
    Ok(scores)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_lowercases_splits_and_keeps_numbers() {
        assert_eq!(
            tokenize("Error TEP-1234: Couldn't reset!"),
            vec!["error", "tep", "1234", "couldn", "reset"]
        );
        assert_eq!(
            tokenize("a I x"),
            Vec::<String>::new(),
            "1-char tokens dropped"
        );
        assert_eq!(tokenize("Café Über"), vec!["café", "über"], "unicode kept");
    }

    #[test]
    fn term_counts_count() {
        let c = term_counts("zebra zebra zebra keeper");
        assert_eq!(c["zebra"], 3);
        assert_eq!(c["keeper"], 1);
    }
}
