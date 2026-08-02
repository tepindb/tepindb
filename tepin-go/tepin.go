// Package tepin is the Go driver for TepinDB — the AI-first single-file
// database. One .tepin file holds documents, indexes, vectors, and its
// own documentation.
//
// The driver speaks the same verbs as the tepin CLI and the MCP server:
// one surface to learn. Every error is a *tepin.Error carrying a stable
// Code, a human Message, a Hint telling you what to do next — and, when
// the op has a CLI twin, CLI: the exact terminal command that reproduces
// the failing call. Set TEPIN_TRACE=1 to stream every op as a JSON line
// on stderr.
//
// No cgo: the driver loads the prebuilt libtepin library (the same Rust
// engine behind the CLI) at runtime. Resolution order: the TEPIN_LIB
// environment variable, the user cache, then a one-time download from
// the project's GitHub release, verified against a SHA-256 pinned in
// this package.
//
//	db, err := tepin.Open("memory.tepin")
//	id, err := db.Insert("notes", tepin.M{"title": "hello", "stars": 5})
//	docs, err := db.Query("notes", tepin.M{"stars": tepin.M{"$gte": 3}})
package tepin

import (
	"encoding/json"
	"fmt"
	"time"
)

// M is shorthand for documents and MongoDB-style filters:
// tepin.M{"stars": tepin.M{"$gte": 3}}.
type M = map[string]any

// Doc is a stored document: arbitrary JSON with a string "_id".
type Doc = map[string]any

// Error is the standard TepinDB error contract.
type Error struct {
	// Stable machine-readable code (docs/errors.md).
	Code    string `json:"code"`
	Message string `json:"message"`
	// What to do next.
	Hint string `json:"hint"`
	// The `tepin …` CLI command reproducing this op, when it has one —
	// paste it into a terminal to debug the same database directly.
	CLI string `json:"-"`
}

func (e *Error) Error() string {
	s := fmt.Sprintf("%s: %s (hint: %s)", e.Code, e.Message, e.Hint)
	if e.CLI != "" {
		s += fmt.Sprintf(" (repro: %s)", e.CLI)
	}
	return s
}

// CollectionInfo describes one collection, as reported by Collections.
type CollectionInfo struct {
	Name    string   `json:"name"`
	Purpose *string  `json:"purpose"`
	Embed   []string `json:"embed"`
	// True when the application supplies vectors itself (set_vectors).
	ManualVectors bool `json:"manual_vectors"`
	// Fields with a secondary (equality) index.
	Indexes []string `json:"indexes"`
	// The subset of Indexes that also enforce uniqueness.
	Unique []string `json:"unique"`
	Count  uint64   `json:"count"`
}

// SearchHit is one semantic search result: score, provenance, the
// best-matching chunk's text, and the full document.
type SearchHit struct {
	Collection string  `json:"collection"`
	ID         string  `json:"id"`
	Score      float64 `json:"score"`
	Chunk      uint32  `json:"chunk"`
	Chunks     uint32  `json:"chunks"`
	Snippet    string  `json:"snippet"`
	Truncated  bool    `json:"truncated"`
	Doc        Doc     `json:"doc"`
}

// KeywordHit is one raw BM25 hit (primitives tier) — fetch the document
// with Get if needed.
type KeywordHit struct {
	Collection string  `json:"collection"`
	ID         string  `json:"id"`
	Score      float64 `json:"score"`
}

// BatchWrite is one op inside an atomic Batch.
type BatchWrite struct {
	// "insert", "upsert", "update", or "delete".
	Op         string `json:"op"`
	Collection string `json:"collection"`
	Doc        any    `json:"doc,omitempty"`
	ID         string `json:"id,omitempty"`
}

// ServeMode relates an open to other processes on the same file
// (docs/serving.md in the TepinDB repo).
type ServeMode string

const (
	// ServeOff is the pure-embedded default: exclusive access.
	ServeOff ServeMode = "off"
	// ServeHost serves reads to other processes while this handle holds
	// the file — `npx tepindb inspect` works against your live app.
	ServeHost ServeMode = "host"
	// ServeDiscover reads through an existing host when locked out.
	ServeDiscover ServeMode = "discover"
	// ServeHostOrDiscover does whichever applies.
	ServeHostOrDiscover ServeMode = "host_or_discover"
)

type openOptions struct {
	existing bool
	embedder string
	serve    ServeMode
	retry    time.Duration
}

// Option configures Open / OpenMemory.
type Option func(*openOptions)

// WithExisting makes Open error (file_not_found) instead of creating the
// file — read-path semantics: a typo'd path is never a silent empty db.
func WithExisting() Option { return func(o *openOptions) { o.existing = true } }

// WithEmbedder wires up the default model (bge-small) for Search — lazy,
// SHA-256-pinned download into the shared cache on first embed. Without
// it the database is a pure document store (Query and KeywordSearch
// still work).
func WithEmbedder() Option { return func(o *openOptions) { o.embedder = "auto" } }

// WithServe sets how this handle relates to other processes on the same
// file. ServeHost makes a running app's database live-inspectable.
func WithServe(mode ServeMode) Option { return func(o *openOptions) { o.serve = mode } }

// WithRetry keeps retrying a locked open with backoff for up to d —
// the cure for two processes racing to open at cold start.
func WithRetry(d time.Duration) Option { return func(o *openOptions) { o.retry = d } }

// DB is an open database handle. Methods are safe for concurrent use.
// Close releases the file lock.
type DB struct {
	handle uintptr
	path   string
	served bool
	closed bool
}

// Open opens (or creates) a database file.
func Open(path string, opts ...Option) (*DB, error) {
	o := openOptions{serve: ServeOff}
	for _, opt := range opts {
		opt(&o)
	}
	return open(map[string]any{
		"path":     path,
		"existing": o.existing,
		"embedder": orOff(o.embedder),
		"serve":    string(o.serve),
		"retry_ms": o.retry.Milliseconds(),
	}, path)
}

// OpenMemory opens a fresh in-memory database — full engine, zero disk,
// gone on Close.
func OpenMemory(opts ...Option) (*DB, error) {
	o := openOptions{serve: ServeOff}
	for _, opt := range opts {
		opt(&o)
	}
	return open(map[string]any{
		"in_memory": true,
		"embedder":  orOff(o.embedder),
	}, "")
}

func orOff(s string) string {
	if s == "" {
		return "off"
	}
	return s
}

func open(options map[string]any, path string) (*DB, error) {
	lib, err := load()
	if err != nil {
		return nil, err
	}
	optJSON, err := json.Marshal(options)
	if err != nil {
		return nil, wrapJSONErr(err)
	}
	ok, terr := lib.open(string(optJSON))
	if terr != nil {
		return nil, terr
	}
	var opened struct {
		Handle uintptr `json:"handle"`
		Served bool    `json:"served"`
	}
	if err := json.Unmarshal(ok, &opened); err != nil {
		return nil, wrapJSONErr(err)
	}
	return &DB{handle: opened.Handle, path: path, served: opened.Served}, nil
}

// Path returns the path this handle was opened with ("" for in-memory).
func (db *DB) Path() string { return db.path }

// Served reports whether another process holds the write lock and this
// handle reads through its in-driver server (docs/serving.md).
func (db *DB) Served() bool { return db.served }

// Call runs any op by name — the generic escape hatch. Same op names,
// argument names, and result shapes as the CLI and the MCP server.
func (db *DB) Call(op string, args any) (json.RawMessage, error) {
	if db.closed {
		return nil, &Error{
			Code:    "invalid_handle",
			Message: "this database handle is closed",
			Hint:    "Open the database again; Close ends a handle for good",
		}
	}
	argJSON, err := json.Marshal(args)
	if err != nil {
		return nil, wrapJSONErr(err)
	}
	lib, err := load()
	if err != nil {
		return nil, err
	}
	ok, terr := lib.call(db.handle, op, string(argJSON))
	if terr != nil {
		terr.CLI = cliRepro(db.path, op, args)
		return nil, terr
	}
	return ok, nil
}

func (db *DB) callInto(op string, args any, out any) error {
	ok, err := db.Call(op, args)
	if err != nil {
		return err
	}
	if out == nil {
		return nil
	}
	if err := json.Unmarshal(ok, out); err != nil {
		return wrapJSONErr(err)
	}
	return nil
}

// Inspect returns a markdown report of everything inside — start here.
func (db *DB) Inspect() (string, error) {
	var out struct {
		Markdown string `json:"markdown"`
	}
	args := M{}
	if db.path != "" {
		args["path"] = db.path
	}
	if err := db.callInto("inspect", args, &out); err != nil {
		return "", err
	}
	return out.Markdown, nil
}

// Collections lists collection metadata: names, counts, purposes,
// embedded fields.
func (db *DB) Collections() ([]CollectionInfo, error) {
	var out struct {
		Collections []CollectionInfo `json:"collections"`
	}
	if err := db.callInto("collections", M{}, &out); err != nil {
		return nil, err
	}
	return out.Collections, nil
}

// Query finds documents with a MongoDB-style filter; nil matches all.
func (db *DB) Query(collection string, filter any) ([]Doc, error) {
	if filter == nil {
		filter = M{}
	}
	var out struct {
		Docs []Doc `json:"docs"`
	}
	err := db.callInto("query", M{"collection": collection, "filter": filter}, &out)
	if err != nil {
		return nil, err
	}
	return out.Docs, nil
}

// Get fetches one document by _id; nil (and no error) when absent.
func (db *DB) Get(collection, id string) (Doc, error) {
	var out struct {
		Doc Doc `json:"doc"`
	}
	if err := db.callInto("get", M{"collection": collection, "id": id}, &out); err != nil {
		return nil, err
	}
	return out.Doc, nil
}

// Insert stores a document and returns its _id (minted unless the doc
// carries one; a duplicate _id is an error, never a silent overwrite).
// Creates the collection on first use.
func (db *DB) Insert(collection string, doc any) (string, error) {
	var out struct {
		Inserted string `json:"inserted"`
	}
	if err := db.callInto("insert", M{"collection": collection, "doc": doc}, &out); err != nil {
		return "", err
	}
	return out.Inserted, nil
}

// Upsert inserts-or-replaces by _id and returns the _id.
func (db *DB) Upsert(collection string, doc any) (string, error) {
	var out struct {
		Upserted string `json:"upserted"`
	}
	if err := db.callInto("upsert", M{"collection": collection, "doc": doc}, &out); err != nil {
		return "", err
	}
	return out.Upserted, nil
}

// Update replaces a document by _id.
func (db *DB) Update(collection, id string, doc any) error {
	return db.callInto("update", M{"collection": collection, "id": id, "doc": doc}, nil)
}

// Delete removes a document by _id (and its search vectors).
func (db *DB) Delete(collection, id string) error {
	return db.callInto("delete", M{"collection": collection, "id": id}, nil)
}

// Batch runs several writes in one atomic transaction — all or nothing.
// Returns the written ids, in order.
func (db *DB) Batch(ops []BatchWrite) ([]string, error) {
	var out struct {
		IDs []string `json:"ids"`
	}
	if err := db.callInto("batch", M{"ops": ops}, &out); err != nil {
		return nil, err
	}
	return out.IDs, nil
}

// SearchOpts narrows Search / KeywordSearch. Zero value = every embedded
// collection, 5 results.
type SearchOpts struct {
	Collection string
	Limit      int
}

func (o *SearchOpts) args(query string) M {
	args := M{"query": query}
	if o != nil && o.Collection != "" {
		args["collection"] = o.Collection
	}
	if o != nil && o.Limit > 0 {
		args["limit"] = o.Limit
	}
	return args
}

// Search runs semantic vector search in natural language. Needs an
// embedder (WithEmbedder) — or a serving host that has one.
func (db *DB) Search(query string, opts *SearchOpts) ([]SearchHit, error) {
	var out struct {
		Hits []SearchHit `json:"hits"`
	}
	if err := db.callInto("search", opts.args(query), &out); err != nil {
		return nil, err
	}
	return out.Hits, nil
}

// KeywordSearch runs raw BM25 keyword search — no model needed. The
// index follows the EmbedFields config.
func (db *DB) KeywordSearch(query string, opts *SearchOpts) ([]KeywordHit, error) {
	var out struct {
		Hits []KeywordHit `json:"hits"`
	}
	if err := db.callInto("keyword_search", opts.args(query), &out); err != nil {
		return nil, err
	}
	return out.Hits, nil
}

// Purpose sets a collection's free-text purpose (shown by Inspect, so
// future readers — human or AI — know what it is for).
func (db *DB) Purpose(collection, text string) error {
	return db.callInto("purpose", M{"collection": collection, "text": text}, nil)
}

// EmbedFields declares which fields get embedded for semantic search;
// existing documents are backfilled automatically.
func (db *DB) EmbedFields(collection string, fields ...string) error {
	return db.callInto("embed_fields", M{"collection": collection, "fields": fields}, nil)
}

// CreateIndex adds a secondary equality index on a field.
func (db *DB) CreateIndex(collection, field string) error {
	return db.callInto("create_index", M{"collection": collection, "field": field}, nil)
}

// CreateUniqueIndex adds a unique secondary index on a field.
func (db *DB) CreateUniqueIndex(collection, field string) error {
	return db.callInto("create_index", M{"collection": collection, "field": field, "unique": true}, nil)
}

// Close releases the handle and its file lock. Further calls error with
// invalid_handle. Closing twice is safe.
func (db *DB) Close() error {
	if db.closed {
		return nil
	}
	lib, err := load()
	if err != nil {
		return err
	}
	db.closed = true
	if _, terr := lib.close(db.handle); terr != nil {
		return terr
	}
	return nil
}

// Version reports the loaded engine: library version, file format
// version, whether embedding is compiled in, and the full op surface.
func Version() (map[string]any, error) {
	lib, err := load()
	if err != nil {
		return nil, err
	}
	raw, terr := lib.version()
	if terr != nil {
		return nil, terr
	}
	var out map[string]any
	if err := json.Unmarshal(raw, &out); err != nil {
		return nil, wrapJSONErr(err)
	}
	return out, nil
}

func wrapJSONErr(err error) *Error {
	return &Error{
		Code:    "invalid_json",
		Message: fmt.Sprintf("could not encode/decode JSON: %v", err),
		Hint:    "documents and filters must marshal to JSON objects",
	}
}
