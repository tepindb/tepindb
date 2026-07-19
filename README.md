# TepinDB

[![CI](https://github.com/tepindb/tepindb/actions/workflows/ci.yml/badge.svg)](https://github.com/tepindb/tepindb/actions/workflows/ci.yml)
[![crates.io: tepindb](https://img.shields.io/crates/v/tepindb?label=tepindb)](https://crates.io/crates/tepindb)
[![crates.io: tepin-cli](https://img.shields.io/crates/v/tepin-cli?label=tepin-cli)](https://crates.io/crates/tepin-cli)
[![docs.rs](https://img.shields.io/docsrs/tepindb)](https://docs.rs/tepindb)

**AI-first, single-file micro-database for CLI tools and agents.**

One `.tepin` file holds your documents, your indexes, your vectors — and its
own documentation: run `head` on it and it tells you (or your LLM) exactly
what it is and how to work with it.

```bash
tepin insert memory.tepin notes '{"title": "hello tepin", "stars": 5}'
tepin query  memory.tepin notes '{"stars": {"$gte": 3}}'
tepin inspect memory.tepin           # markdown report of everything inside
tepin mcp    memory.tepin            # plug the db into an AI agent (MCP)
```

- **Single file, zero config** — everything about a database lives inside it.
- **Vector search built in** — ONNX + bge-small, `db.embed()`-simple, lazy
  model download (pinned SHA-256, from GitHub releases only).
- **Made for agents** — self-describing file, MCP tools, MongoDB-style
  filters, every error carries `{code, message, hint}`.
- **Rust core on [redb](https://github.com/cberner/redb)** — ACID,
  immediate-fsync durability by default.

## Examples

- `cargo run --example quickstart -p tepindb` — documents, filters, purposes; no model needed.
- `cargo run --example vector_search -p tepindb` — real semantic search (downloads bge-small once).
- [`examples/notes-app`](examples/notes-app) — a complete note-taking CLI built on the driver,
  with end-to-end tests that run in CI on every push.

Status: early. The format may change freely before 1.0 (`tepin migrate`
will always cover you). Plan lives in [plan.md](plan.md).

## License

MIT or Apache-2.0, at your option.
