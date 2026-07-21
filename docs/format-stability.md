# Format stability policy

The `.tepin` container carries a `format_version` in its preamble
(`tepin-core/src/format.rs`, currently **v0**). This document is the
contract integrators can plan releases against.

## The promise

**Every format version ever published stays migratable to the current
format, forever.** `tepin migrate <file>` reads any published version and
writes a fresh current-format file; the original is never modified — not
even by a byte (the source is snapshotted before reading). This promise is
enforced in CI: `tepin-core/tests/fixtures/` holds one fixture database
per published format version, and the harness in
`tepin-core/tests/migrate.rs` migrates and validates every one of them on
every run.

Pre-1.0, the format may change freely **between** releases — but never
without the migration path landing in the same release.

## What bumps `format_version`

A bump is required for any change that makes a file unreadable (or
silently misreadable) by builds on the other side of the change:

- preamble layout or size changes, payload offset changes
- storage-key encoding changes (document keys, vector chunk keys,
  index/fts key schemes) without a read-side compat shim
- value-encoding changes (vector byte layout, meta record shapes that old
  readers would misparse rather than ignore)
- removing or repurposing a table

## What does NOT bump it

- **Additive meta fields** that old readers ignore and new readers default
  (e.g. the `unique` list on collection meta, added in 0.4.0)
- **New tables** unknown to old readers
- **Read-side compat shims**: when new code keeps reading the old shape
  (e.g. pre-chunking vector rows under a bare doc id read as chunk 0), the
  version stays put and `tepin migrate` normalizes the old shape on copy

One caution follows from the additive rule: an **older** build writing to
a file touched by a newer build may drop additive fields it doesn't know
(meta records are rewritten whole). Mixing build versions against one live
file is unsupported pre-1.0 — upgrade all writers together.

## The support window

There is no window: all published versions, forever. The cost of keeping a
version readable is one committed fixture plus its reader, which is why
the reader dispatch lives in one place (`tepin-core/src/migrate.rs`).

## Process for a format break

1. Land the new format and its reader/writer.
2. In the same change: commit a fixture built by the last release of the
   old format (`cargo test -p tepin-core --test migrate -- --ignored
   regenerate` on that release), extend the version dispatch in
   `migrate.rs`, and bump `FORMAT_VERSION`.
3. The fixture harness proves old → current before the release ships.
