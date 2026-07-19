# TepinDB

AI-first, single-file micro-database for CLI tools and agents. Rust core on redb, vector search built in (ONNX + bge-small), self-describing db file, shipped with MCP tools and `npx tepindb` tooling.

> Rich detail, decisions, and open questions live in Engram — run the pane or ask Claude "why".

## Milestones

1. **Core** — ✅ document store on redb, `.tepin` format (self-describing 4KB preamble), Mongo-subset filters, locking, validation suite.
2. **Embed** — ✅ ONNX + bge-small, async init, pinned-hash lazy download, write→embed→search pipeline, brute-force search, **hybrid BM25+vector fusion**.
3. **Rust driver** — ✅ `tepindb` crate: `open` / `open_auto` / custom embedders; example app + examples.
4. **Tooling** — ✅ `tepin mcp` server, CLI search, npx packaging (`npm/`: tepindb + tepin alias + @tepindb platform packages, published with provenance from release.yml). Remaining: claim npm/crates registrations, first release.
5. **Dogfood** — replace sqlite in Engram with TepinDB — **unblocked now that hybrid landed**.
6. **Drivers** — Go / TS / Python.

## Open questions

- Built-in chunking design (multiple vectors per doc) vs v0's loud truncation.
- Competitive positioning vs LanceDB / sqlite-vec (agent ergonomics is the bet).
