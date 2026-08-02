# The Go and TypeScript drivers

> Status: **shipped in-tree** — `tepin-ffi/` (the C ABI), `tepin-node/`
> (the Node addon), `tepin-go/` (the Go module), and the driver library
> inside `npm/tepindb/lib/`. CI runs both drivers end-to-end against a
> freshly built engine on every tier-1 target (ci.yml `drivers` job).

## One surface, five doors

Everything speaks the same op set. `tepin_core::ops::dispatch` is the
single dispatch surface — op names, argument names, result shapes, and
the `{code, message, hint}` error contract are identical whether an op
arrives through:

| door | transport |
|---|---|
| Rust driver (`tepindb` crate) | direct method calls |
| `tepin` CLI | argv → JSON on stdout |
| MCP server (`tepin mcp`) | JSON-RPC over stdio |
| in-driver serving (`docs/serving.md`) | JSON frames over a local socket |
| **Go / TypeScript drivers** | **in-process FFI through `libtepin`** |

Consequences that make the drivers cheap to learn and debug:

- A driver call that fails can be **replayed with the CLI**: driver
  errors carry `cli` (TS) / `CLI` (Go) — the exact `tepin …` command
  that runs the same op against the same file.
- `db.call(op, args)` / `db.Call(op, args)` is the generic escape hatch;
  an op name typo answers with the full op menu in the hint.
- `TEPIN_TRACE=1` streams every dispatched op (args, duration, outcome)
  as JSON lines on stderr — same switch in every language, implemented
  once in the dispatch layer.

## Architecture: FFI over one C ABI

The drivers are thin shells over the Rust engine loaded **in-process**.
No cgo, no subprocess, no sockets on the hot path.

```
 tepin-go (pure Go, purego dlopen)      npm/tepindb/lib (JS wrapper)
        │                                       │
        ▼                                       ▼
 libtepin  (tepin-ffi: C ABI)           tepin.node  (tepin-node: napi)
        └───────────────┬───────────────────────┘
                        ▼
          tepin_ffi::driver  (handle registry, open options)
                        ▼
          tepin_core::ops::dispatch  (the one op surface)
```

### The C ABI (`tepin-ffi`)

Five functions, JSON strings in, JSON envelope strings out — `{"ok": …}`
or `{"error": {code, message, hint}}`:

```c
char* tepin_version(void);
char* tepin_open(const char* options_json);   // {"ok":{"handle":1,"served":false}}
char* tepin_call(uint64_t handle, const char* op, const char* args_json);
char* tepin_close(uint64_t handle);
void  tepin_free(char* ptr);                  // frees any returned string
```

Design properties:

- **JSON-first keeps the ABI frozen.** New ops and options never change
  the five signatures, so old drivers load new engines (and answer
  `not_implemented` with the op menu when they guess wrong).
- Handles are process-local `u64`s in a registry — no raw pointers cross
  the boundary, a stale handle is a clean `invalid_handle` error, and
  the engine's thread-safety is a compile-time assertion.
- Panics never unwind across the ABI; they come back as
  `{"error": {"code": "panic", …}}`.
- Open options (one JSON object): `path` / `in_memory`, `existing`,
  `serve` (`off|host|discover|host_or_discover`), `embedder`
  (`off|auto`), `retry_ms`.

Because the engine is in-process, a driver `open` with `serve: "host"`
makes the app's live database inspectable from outside —
`npx tepindb inspect app.tepin` answers, snapshot-isolated, while the
app keeps writing (`docs/serving.md`). That composes: `discover` lets a
driver read through some other process's host, and a served handle's
`search` uses the **host's** model.

### TypeScript (`npm/tepindb`)

`import { open } from "tepindb"` — the same package as the CLI; the
napi addon `tepin.node` ships inside the existing `tepindb-<platform>`
optionalDependencies next to the binary. Promise-based (ops run on the
libuv worker pool, never blocking the event loop), `.d.ts` shipped,
`TEPIN_NODE_ADDON` overrides the addon path for dev builds.

npm ships the **slim** addon (no ONNX), matching the slim CLI policy:
semantic search on a slim addon works through a serving host with a
model, or point `TEPIN_NODE_ADDON` at the full `tepin-node-<platform>.node`
from the GitHub release. Everything else — documents, filters, batch,
indexes, BM25 `keywordSearch` — is fully local.

### Go (`tepin-go/`)

`go get github.com/tepindb/tepindb/tepin-go` — pure Go, **no cgo**: the
driver dlopens `libtepin` via `purego`. Resolution order:

1. `TEPIN_LIB` (explicit path — dev builds: `cargo build -p tepin-ffi`);
2. the user cache (`os.UserCacheDir()/tepindb/lib/<version>/`);
3. one-time download of `libtepin-<os>-<arch>.<ext>` from the GitHub
   release, verified against a **SHA-256 pinned in the driver source**
   (`tepin-go/pins.go`) — the same supply-chain model as the embedding
   model download: GitHub releases only, pinned digest, no fallback.

The GitHub-release `libtepin` is the full build, so Go gets semantic
search out of the box.

## Release flow additions (release.yml)

- The `binaries` job also builds and attaches, per target:
  `libtepin-<platform>.<ext>` (full C ABI library, + `.sha256`) and
  `tepin-node-<platform>.node` (full addon, + `.sha256`); the slim addon
  rides the existing npm artifacts into the platform packages.
- The `go-driver` job runs after `binaries`: it regenerates
  `tepin-go/pins.go` from the uploaded libraries' digests, commits it to
  `main`, and pushes the module tag `tepin-go/vX.Y.Z` — the tag Go's
  module proxy resolves. (Pins must be *committed*, not stamped into a
  checkout: Go fetches module source from git.)

## Testing

- `tepin-ffi` unit tests exercise the ABI through the extern fns,
  including NULL/garbage input, stale handles, and lock release.
- `tepin-go`: `TEPIN_LIB=…/libtepin_ffi.dylib go test ./...` — CRUD,
  persistence, batch atomicity, BM25, error contract incl. CLI repro,
  use-after-close, and a concurrent writers+readers stress test.
- `npm/tepindb`: `TEPIN_NODE_ADDON=…/libtepin_node.dylib node --test` —
  the same matrix from JS.
- CI (`drivers` job) runs all of the above on macOS/Linux/Windows
  against a slim engine built from the same commit.
