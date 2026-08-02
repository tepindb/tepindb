# tepin-go — the Go driver for TepinDB

**AI-first, single-file micro-database.** One `.tepin` file holds your
documents, your indexes, your vectors — and its own documentation.

```go
import tepin "github.com/tepindb/tepindb/tepin-go"

db, err := tepin.Open("memory.tepin")
id, err := db.Insert("notes", tepin.M{"title": "hello tepin", "stars": 5})
docs, err := db.Query("notes", tepin.M{"stars": tepin.M{"$gte": 3}})
```

No cgo. The driver loads the prebuilt `libtepin` engine (the same Rust
core behind the `tepin` CLI) at runtime — a tagged driver release
downloads it once from the project's GitHub release, verified against a
SHA-256 pinned in the driver source. `TEPIN_LIB` always overrides:

```sh
# development tree (no pinned release):
cargo build -p tepin-ffi
TEPIN_LIB=$PWD/target/debug/libtepin_ffi.dylib go test ./...
```

## Why this driver is easy to debug

- **One surface everywhere.** Driver methods, the `tepin` CLI, and the
  MCP server speak the same verbs with the same argument names and the
  same result shapes. `db.Call(op, args)` is the generic escape hatch.
- **Errors that teach.** Every failure is a `*tepin.Error` with a stable
  `Code`, a `Hint` telling you what to do next — and `CLI`, the exact
  `tepin …` terminal command that reproduces the failing op against the
  same file.
- **Trace anything.** `TEPIN_TRACE=1` streams every op (args, duration,
  outcome) as JSON lines on stderr — same switch in every language.
- **Live inspection.** Open with `tepin.WithServe(tepin.ServeHost)` and
  your running app's database answers `npx tepindb inspect app.tepin`
  from another terminal — snapshot-isolated, zero config
  ([docs/serving.md](../docs/serving.md)).

## Semantic search

```go
db, _ := tepin.Open("memory.tepin", tepin.WithEmbedder())
db.EmbedFields("notes", "title", "body")
db.Insert("notes", tepin.M{"title": "reset flow", "body": "how we reset passwords"})
hits, _ := db.Search("how do I reset my password", nil)
```

`WithEmbedder` wires up bge-small — lazily downloaded (SHA-256-pinned)
on first embed; opening stays instant. Without it the database is a pure
document store: `Query` and `KeywordSearch` (BM25) always work.
