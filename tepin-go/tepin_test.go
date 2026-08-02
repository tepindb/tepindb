// End-to-end driver tests against a locally built engine:
//
//	cargo build -p tepin-ffi
//	TEPIN_LIB=../target/debug/libtepin_ffi.dylib go test ./...
//
// CI builds the library and sets TEPIN_LIB (see ci.yml).
package tepin

import (
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestMain(m *testing.M) {
	if os.Getenv("TEPIN_LIB") == "" {
		println("TEPIN_LIB must point at a built libtepin (cargo build -p tepin-ffi)")
		os.Exit(1)
	}
	os.Exit(m.Run())
}

func mustOpen(t *testing.T, opts ...Option) *DB {
	t.Helper()
	db, err := Open(filepath.Join(t.TempDir(), "t.tepin"), opts...)
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	t.Cleanup(func() { _ = db.Close() })
	return db
}

func TestVersionReportsOpSurface(t *testing.T) {
	v, err := Version()
	if err != nil {
		t.Fatalf("version: %v", err)
	}
	ops, ok := v["ops"].([]any)
	if !ok || len(ops) == 0 {
		t.Fatalf("expected ops list, got %v", v)
	}
}

func TestCrudRoundTrip(t *testing.T) {
	db := mustOpen(t)

	id, err := db.Insert("notes", M{"title": "hello", "stars": 4})
	if err != nil {
		t.Fatalf("insert: %v", err)
	}

	doc, err := db.Get("notes", id)
	if err != nil || doc["title"] != "hello" {
		t.Fatalf("get: %v / %v", doc, err)
	}

	docs, err := db.Query("notes", M{"stars": M{"$gte": 3}})
	if err != nil || len(docs) != 1 {
		t.Fatalf("query: %v / %v", docs, err)
	}
	if _, err := db.Query("notes", nil); err != nil {
		t.Fatalf("nil filter should match all: %v", err)
	}

	if err := db.Update("notes", id, M{"title": "hello2"}); err != nil {
		t.Fatalf("update: %v", err)
	}
	if err := db.Purpose("notes", "go driver test notes"); err != nil {
		t.Fatalf("purpose: %v", err)
	}

	md, err := db.Inspect()
	if err != nil || !strings.Contains(md, "go driver test notes") {
		t.Fatalf("inspect: %v / %v", md, err)
	}

	cols, err := db.Collections()
	if err != nil || len(cols) != 1 || cols[0].Name != "notes" || cols[0].Count != 1 {
		t.Fatalf("collections: %+v / %v", cols, err)
	}

	if err := db.Delete("notes", id); err != nil {
		t.Fatalf("delete: %v", err)
	}
	if doc, err := db.Get("notes", id); err != nil || doc != nil {
		t.Fatalf("get after delete: %v / %v", doc, err)
	}
}

func TestPersistenceAcrossOpens(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "persist.tepin")

	db, err := Open(path)
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	id, err := db.Insert("kv", M{"k": "v"})
	if err != nil {
		t.Fatalf("insert: %v", err)
	}
	if err := db.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}

	db, err = Open(path, WithExisting())
	if err != nil {
		t.Fatalf("reopen: %v", err)
	}
	defer db.Close()
	doc, err := db.Get("kv", id)
	if err != nil || doc["k"] != "v" {
		t.Fatalf("get after reopen: %v / %v", doc, err)
	}
}

func TestBatchIsAtomic(t *testing.T) {
	db, err := OpenMemory()
	if err != nil {
		t.Fatalf("open memory: %v", err)
	}
	defer db.Close()

	ids, err := db.Batch([]BatchWrite{
		{Op: "insert", Collection: "a", Doc: M{"n": 1}},
		{Op: "upsert", Collection: "a", Doc: M{"_id": "x", "n": 2}},
	})
	if err != nil || len(ids) != 2 || ids[1] != "x" {
		t.Fatalf("batch: %v / %v", ids, err)
	}
}

func TestKeywordSearchWithoutModel(t *testing.T) {
	db, err := OpenMemory()
	if err != nil {
		t.Fatalf("open memory: %v", err)
	}
	defer db.Close()

	// The BM25 index follows the embed-fields config (no model needed).
	if err := db.EmbedFields("docs", "body"); err != nil {
		t.Fatalf("embed fields: %v", err)
	}
	if _, err := db.Insert("docs", M{"body": "the quick brown fox"}); err != nil {
		t.Fatalf("insert: %v", err)
	}
	if _, err := db.Insert("docs", M{"body": "sleepy grey cat"}); err != nil {
		t.Fatalf("insert: %v", err)
	}

	hits, err := db.KeywordSearch("brown fox", &SearchOpts{Limit: 2})
	if err != nil || len(hits) == 0 {
		t.Fatalf("keyword search: %v / %v", hits, err)
	}
	doc, err := db.Get(hits[0].Collection, hits[0].ID)
	if err != nil || doc["body"] != "the quick brown fox" {
		t.Fatalf("hit doc: %v / %v", doc, err)
	}
}

func TestErrorsCarryCodeHintAndCLIRepro(t *testing.T) {
	db := mustOpen(t)

	_, err := db.Get("nope", "someid")
	var terr *Error
	if !errors.As(err, &terr) {
		t.Fatalf("expected *tepin.Error, got %T: %v", err, err)
	}
	if terr.Code != "collection_not_found" || terr.Hint == "" {
		t.Fatalf("unexpected error: %+v", terr)
	}
	if !strings.HasPrefix(terr.CLI, "tepin get ") {
		t.Fatalf("expected CLI repro, got %q", terr.CLI)
	}
}

func TestGenericCallSpeaksTheSharedSurface(t *testing.T) {
	db := mustOpen(t)

	if _, err := db.Call("insert", M{"collection": "c", "doc": M{"a": 1}}); err != nil {
		t.Fatalf("call insert: %v", err)
	}
	out, err := db.Call("query", M{"collection": "c"})
	if err != nil || !strings.Contains(string(out), `"count":1`) {
		t.Fatalf("call query: %s / %v", out, err)
	}

	_, err = db.Call("quyre", M{})
	var terr *Error
	if !errors.As(err, &terr) || terr.Code != "not_implemented" || !strings.Contains(terr.Hint, "query") {
		t.Fatalf("typo'd op should answer with the menu: %v", err)
	}
}

func TestOpenExistingOnMissingPathIsClean(t *testing.T) {
	_, err := Open(filepath.Join(t.TempDir(), "missing.tepin"), WithExisting())
	var terr *Error
	if !errors.As(err, &terr) || terr.Code != "file_not_found" {
		t.Fatalf("expected file_not_found, got %v", err)
	}
}

func TestUseAfterCloseIsClean(t *testing.T) {
	db, err := OpenMemory()
	if err != nil {
		t.Fatalf("open memory: %v", err)
	}
	if err := db.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}
	if err := db.Close(); err != nil {
		t.Fatalf("double close should be a no-op: %v", err)
	}
	_, err = db.Call("query", M{"collection": "c"})
	var terr *Error
	if !errors.As(err, &terr) || terr.Code != "invalid_handle" {
		t.Fatalf("expected invalid_handle, got %v", err)
	}
}

func TestConcurrentWritersAndReaders(t *testing.T) {
	db := mustOpen(t)
	done := make(chan error, 8)
	for w := 0; w < 4; w++ {
		go func() {
			var err error
			for i := 0; i < 25; i++ {
				if _, e := db.Insert("stress", M{"n": i}); e != nil {
					err = e
					break
				}
			}
			done <- err
		}()
		go func() {
			var err error
			for i := 0; i < 25; i++ {
				if _, e := db.Query("stress", nil); e != nil {
					if terr := new(Error); errors.As(e, &terr) && terr.Code == "collection_not_found" {
						continue // first insert may not have landed yet
					}
					err = e
					break
				}
			}
			done <- err
		}()
	}
	for i := 0; i < 8; i++ {
		if err := <-done; err != nil {
			t.Fatalf("concurrent op failed: %v", err)
		}
	}
	docs, err := db.Query("stress", nil)
	if err != nil || len(docs) != 100 {
		t.Fatalf("expected 100 docs, got %d / %v", len(docs), err)
	}
}
