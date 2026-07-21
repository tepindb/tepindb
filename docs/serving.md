# Multi-process read access via in-driver serving

> Status: **shipped** (phase 1, 0.4.0) — `tepin-core/src/serve/`, behind the
> default-on `serve` feature. `ServeMode` on `Db::options()`; `tepin mcp`
> hosts; CLI read commands discover on `database_locked`. Write-forwarding
> (phase 2) remains unbuilt. Resolved open questions: CLI discovery is
> automatic (it only activates on an otherwise-fatal error); the sidecar
> lives in the runtime dir; pid-liveness checks are replaced by
> connect-plus-nonce validation (the connect attempt IS the liveness
> check); a remote handle surfaces itself via `Db::is_served()`. Beyond
> the phase-1 table, the primitives-tier reads (`search_by_vector`,
> `get_vectors`) are served too — Engram's read federation needs them.

## The problem

TepinDB is single-writer *and* single-process: `Db::open` takes an exclusive
OS lock on the file (`tepin-core/src/db.rs`, `file.try_lock()`) and holds it
for the handle's whole lifetime. A second process that opens the same file —
even just to read — gets `database_locked`. In practice this bites whenever a
long-lived writer is running: e.g. an Engram MCP server holds `graph.tepin`,
and `npx tepindb inspect graph.tepin` is refused.

```
$ npx tepindb inspect .engram/graph.tepin
{"error":{"code":"database_locked","message":".engram/graph.tepin is open in another process", ...}}
```

We want reader/writer parity — N readers concurrent with the one writer —
without giving up the single-file, embedded model.

## The core idea: the writer serves the reads

The naive fix (let a second process open the file read-only and bypass the
lock) is unsafe: redb commits by flipping a root pointer, so an uncoordinated
cross-process reader can follow a half-updated B-tree and read torn state.

Instead: **the lock-holder runs the reads.** redb already supports many
concurrent read transactions alongside the single write transaction *within a
process*, each reader on a consistent MVCC snapshot. So if the second process
sends its query to the writer and the writer executes it in-process, the read
gets snapshot isolation for free. The file lock stays exclusive; only
*queries and results* cross the process boundary, never raw pages.

Consequence: **read-parity needs in-process serving + discovery, not a WAL.**

This also lives entirely inside the driver. The only "rule" a host app follows
is "call `open()`" — which it already does. No app-level daemon, no protocol
for the app to learn.

Bonus: a host that has the full embedder attached can serve real semantic
`search` to a **slim** `npx` client that has no ONNX of its own.

## Architecture

```
 writer process (holds lock)                reader process (npx tepindb)
 ┌─────────────────────────┐                ┌──────────────────────────┐
 │ Db (Local)              │                │ Db::open() → try_lock ✗  │
 │  redb::Database (Arc)   │                │   → read sidecar         │
 │  ├ app write txns       │                │   → connect              │
 │  └ serve listener ──────┼── UDS / pipe ──┼─→ Db (Remote)            │
 │      begin_read() per   │   JSON frames  │     read ops → socket    │
 │      request (MVCC)     │                │     write ops → error    │
 └─────────────────────────┘                └──────────────────────────┘
        writes sidecar ─────────► runtime-dir/tepindb/<hash>.json
```

### Discovery sidecar

A small JSON file advertises the running server. Located in the OS runtime
dir (not next to the `.tepin` file — keeps the data directory pristine and
avoids putting sockets in synced/VCS folders), keyed by a hash of the
canonical absolute path:

```
${XDG_RUNTIME_DIR:-$TMPDIR}/tepindb/<sha256(canonical_abs_path)>.json
```

Both processes canonicalize the path the same way, so the reader can compute
the sidecar location from the db path alone. Contents:

```json
{
  "pid": 48213,
  "transport": "unix",              // "unix" | "windows-pipe"
  "endpoint": "/run/user/501/tepindb/48213.sock",
  "nonce": "b1df74b5…",             // guards against pid reuse
  "protocol_version": 1,
  "format_version": 3,
  "started_at_unix": 1784641000
}
```

On Windows there is no filesystem socket; `endpoint` is a named-pipe name and
`transport` is `windows-pipe`. The sidecar JSON is written after the listener
is bound, and removed on graceful shutdown.

### Transport

One dependency: `interprocess`, whose `LocalSocket` abstracts Unix domain
sockets and Windows named pipes behind one API. Endpoint permissions:

- UDS: `0600`, in a dir the invoking user owns.
- Named pipe: default DACL scoped to the current user.

### Wire protocol

Length-prefixed (u32 big-endian) JSON frames, one request → one response.

```jsonc
// request
{"id": 7, "op": "query", "args": {"collection": "notes", "filter": {"tag": "todo"}}}
// response (ok)
{"id": 7, "ok": {"docs": [ ... ]}}
// response (error) — the standard TepinDB error shape
{"id": 7, "error": {"code": "collection_not_found", "message": "...", "hint": "..."}}
```

`op`/`args`/result JSON reuse the existing MCP tool surface verbatim
(`tepin-cli/src/mcp.rs`), so there is no second schema to maintain:

| op | kind | phase |
|---|---|---|
| `inspect`, `query`, `get`, `search`, `keyword_search` | read | 1 |
| `insert`, `update`, `delete`, `purpose`, `embed_fields` | write | 2 |

The first frame is a `hello` carrying the client's `protocol_version`; the
server rejects incompatible versions rather than guessing.

## Server side

When a process opens in host mode and wins the lock, `open()` also:

1. binds a `LocalSocket` listener on a fresh endpoint,
2. writes the sidecar,
3. spawns a listener thread that, per connection, reads frames and dispatches
   each read op via `core.db.begin_read()` → serialize → reply.

Because every served read is its own redb read transaction inside the writer,
reads are concurrent with each other and with the app's write transaction, all
snapshot-isolated. No new locking. Writes (phase 2) funnel through the
existing `Db` write methods, which redb already serializes; ordering relative
to the host app's own writes is nondeterministic (documented).

Shutdown (graceful `Drop`): stop accepting, drain in-flight, remove sidecar +
socket, release the file lock.

## Client side (the transparent bit)

`Db` gains an internal backend so the public API is unchanged:

```rust
enum Backend {
    Local(Arc<Core>),      // holds the lock, may host
    Remote(RemoteClient),  // talks to a host over the socket
}
```

`open()` flow on lock failure:

1. `try_lock` fails.
2. If discover mode is off → return `database_locked` (today's behavior).
3. Read the sidecar. Validate: pid alive (`kill(pid,0)` / `OpenProcess`),
   nonce matches, `protocol_version` compatible.
4. Valid → connect, `hello`, return `Db { Backend::Remote }`. Read methods go
   over the socket; write methods return `database_locked` in phase 1 (hint:
   "another process holds the write lock; this handle is read-only").
5. Stale (dead pid / bad nonce) → delete the stale sidecar, then return
   `database_locked` (or, if `create`-ish semantics apply, take the lock).

## Enablement & the embedded-purity tension

An always-listening socket on every write-open would violate the "single file,
zero config, no surprise network surface" principle for library embedders.
Resolution — serving is a mode, off by default in the library:

```rust
pub enum ServeMode { Off, Host, Discover, HostAndDiscover }
Db::open(path)                       // ServeMode::Off — unchanged, pure
Db::open_with(path, ServeMode::Host) // advertises + serves
```

- `tepin-core` `Db::open` default: **`Off`**.
- Our **CLI / `tepin mcp` server**: open as **`Host`** (advertises).
- Our **CLI read commands**: on `database_locked`, retry as **`Discover`**.

The Engram win falls out for free: Engram runs *our* `tepin mcp` server, so if
that server hosts by default, `npx tepindb inspect` (discover on lock failure)
connects to it with **zero changes to Engram**.

## Scope: reads first

Phase 1 serves reads only; a `Remote` handle's writes error clearly. Phase 2
may forward writes to the host — a bigger step (authority, ordering, the host
becoming a real server) evaluated separately. Serving is not a substitute for
the lock: exactly one process ever mutates.

## Security model

The socket exposes db contents to local processes that can open it. UDS `0600`
/ per-user pipe DACL means "same user only" — the same trust boundary as the
file itself (anyone who can open the socket could already open the file). No
new cross-user exposure. No auth token in phase 1; revisit if we ever bind
beyond a per-user endpoint (we don't plan to).

## Failure & lifecycle matrix

| situation | behavior |
|---|---|
| host exits gracefully | sidecar + socket removed; next opener takes the lock |
| host crashes | stale sidecar remains; reader detects dead pid, cleans it, falls back |
| pid reused by unrelated process | nonce mismatch on connect → treat as stale |
| sidecar present, connect refused | treat as stale, clean, fall back |
| protocol version mismatch | reader does not connect; returns `database_locked` with a version hint |
| host db is a newer `format_version` | reader refuses (same rule as opening a too-new file) |

## Cross-platform notes

- Liveness: `libc::kill(pid, 0)` on unix; `OpenProcess`/`GetExitCodeProcess`
  on Windows.
- Runtime dir: `XDG_RUNTIME_DIR` → `TMPDIR` → `std::env::temp_dir()`.
- Two transport code paths behind `interprocess`; one protocol above them.

## Implementation phases

- **Phase 0 — Engram PoC.** `tepin mcp` writes the sidecar; CLI `inspect` /
  `query` / `get` detect `database_locked`, read the sidecar, connect, and
  serve the read. Reads only, unix only. Proves the loop end-to-end.
- **Phase 1 — general.** `Backend { Local, Remote }` in `Db`; `ServeMode`; all
  read ops; sidecar lifecycle + takeover; Windows named pipes; version
  handshake; two-process integration tests.
- **Phase 2 — writes (maybe).** Forward mutations to the host; define ordering
  and authority; or decide serving stays read-only forever.

## Open questions

1. Default `ServeMode` for the CLI read path — discover automatically, or only
   under a flag / `TEPIN_SERVE`? (Leaning: automatic; it only activates on an
   otherwise-fatal `database_locked`.)
2. Do we ever want write-forwarding, or is "writes require the lock, full
   stop" the honest long-term answer?
3. Sidecar in the runtime dir (proposed) vs beside the file — the latter is
   simpler to discover but pollutes the data dir and risks socket-in-VCS.
4. Should a `Remote` handle surface that it's remote (e.g. `inspect` noting
   "served by pid N"), or stay fully transparent?

## Testing

- Two-process integration: spawn a `Host`, connect a client, assert reads match
  a direct open; assert reads are consistent while the host commits in a loop.
- Stale sidecar: write a sidecar with a dead pid → reader cleans it and returns
  `database_locked` (or takes the lock).
- Concurrency: many clients reading while the host writes; no torn reads, no
  deadlock.
- Slim-client bonus: a no-ONNX client gets real `search` results from a
  full-embedder host.
