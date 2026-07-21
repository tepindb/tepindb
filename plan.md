# TepinDB

AI-first, single-file micro-database for CLI tools and agents. Rust core on redb, vector search built in (ONNX + bge-small), self-describing db file, shipped with MCP tools and `npx tepindb` tooling.

> Rich detail, decisions, and open questions live in Engram — run the pane or ask Claude "why".

## Milestones

1. **Core** — ✅ document store on redb, `.tepin` format (self-describing 4KB preamble), Mongo-subset filters, locking, validation suite.
2. **Embed** — ✅ ONNX + bge-small, async init, pinned-hash lazy download (from our own model release), write→embed→search pipeline, brute-force search, **hybrid BM25+vector fusion**, **built-in chunking** (one vector per chunk, best-chunk scoring, verbatim snippets).
3. **Rust driver** — ✅ `tepindb` crate: `open` / `open_auto` / custom embedders; example app + examples.
4. **Tooling** — ✅ `tepin mcp` server, CLI search, npx packaging (`npm/`: tepindb + tepindb-<platform> packages, published with provenance from release.yml). Published: crates 0.3.2, npm tepindb 0.3.2 (+ darwin/linux platform pkgs). Pending: npm win32-x64 (spam-flagged, awaiting resolution).
5. **Primitives tier (0.3.0)** — tepindb-minimal: BYO vectors (manual mode, raw KNN, readback), public keyword scores, cross-collection batch transactions, `open_in_memory`, secondary indexes (equality-first, redb index tables). tepindb-full stays the zero-config RAG.
6. **Dogfood** — ✅ replace sqlite in Engram with TepinDB, on the primitives tier (Engram milestone 6, 2026-07-20).
7. **Engram dossier (0.4.0)** — ✅ implemented (release pending). The first production integrator's requirements, all in one release: **serving** (multi-process reads via in-driver IPC, `docs/serving.md`, phase 1 full — no write-forwarding), **`tepin migrate`** (infrastructure + fixtures + CI + stability policy, `docs/format-stability.md`; no format break yet), embedder **unpin/reset** (`reset_embedder`), purposed-but-empty reads return `Ok(empty)`, native **upsert** (incl. batch + CLI + MCP), optional **unique** secondary indexes, open-with-retry/backoff (`Db::options().retry_for`).
8. **Drivers** — Go / TS / Python.

## Open questions

- Competitive positioning vs LanceDB / sqlite-vec (agent ergonomics is the bet).
