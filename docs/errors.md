# Error code registry

Every TepinDB error — Rust API, CLI, MCP — has the same shape:

```json
{"error": {"code": "...", "message": "...", "hint": "..."}}
```

`code` is stable and machine-matchable. `hint` always says what to do next.
New codes are added here in the same change that introduces them; codes are
never repurposed.

| code | meaning |
|---|---|
| `file_not_found` | A read command was pointed at a path with no database file (reads never create files). |
| `not_a_tepin_file` | The file does not start with the tepindb magic. |
| `invalid_preamble` | The preamble is truncated, corrupt, or missing its metadata block. |
| `format_too_new` | The file was written by a newer format version than this build reads. |
| `database_locked` | Another process has this database open (one process at a time). |
| `storage_error` | The redb storage engine failed (corruption, i/o). |
| `io_error` | Filesystem-level failure (permissions, missing path, disk). |
| `invalid_json` | A document or filter argument was not valid JSON. |
| `invalid_document` | A document was valid JSON but not an object, or its `_id` was not a string. |
| `invalid_filter` | A filter was not an object, or used an unsupported operator. |
| `invalid_collection_name` | Collection names are 1–128 bytes with no control characters. |
| `duplicate_id` | An insert carried an explicit `_id` that already exists (inserts never overwrite). |
| `collection_not_found` | The named collection does not exist in this database. |
| `doc_not_found` | No document with that id exists in the collection. |
| `not_implemented` | The operation exists in the surface but is not wired up in this build. |
| `model_download_failed` | The embedding model could not be fetched into the shared cache. |
| `checksum_mismatch` | A downloaded or cached model file failed its pinned SHA-256 check. |
| `model_load_failed` | The model files exist but onnxruntime/tokenizer could not load them. |
| `embedding_failed` | Model inference failed for a specific input. |
| `embedder_already_attached` | A database handle takes exactly one embedder; this one already has it. |
| `embedder_not_attached` | `search` needs an embedder (or use `search_by_vector` / `keyword_search`, which don't). |
| `embedder_mismatch` | The vectors in this file came from a different model or dimension than the one supplied. |
| `collection_not_embedded` | The collection has no embed fields configured, so it cannot be searched. |
| `invalid_vector` | `set_vectors` got no vectors, an empty vector, or mixed dimensions in one call. |
| `manual_vectors_disabled` | `set_vectors` works only on collections in manual vector mode (`set_manual_vectors`). |
