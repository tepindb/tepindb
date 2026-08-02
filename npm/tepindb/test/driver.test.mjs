// End-to-end driver test against a locally built addon:
//   cargo build -p tepin-node
//   TEPIN_NODE_ADDON=../../target/debug/libtepin_node.dylib npm test
// CI builds the addon and sets TEPIN_NODE_ADDON (see ci.yml).

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, test } from "node:test";

import { open, openMemory, TepinError, version } from "tepindb";

if (!process.env.TEPIN_NODE_ADDON) {
  console.error("TEPIN_NODE_ADDON must point at a built tepin-node library");
  process.exit(1);
}

let dir;
before(() => {
  dir = mkdtempSync(join(tmpdir(), "tepindb-driver-"));
});
after(() => {
  rmSync(dir, { recursive: true, force: true });
});

test("version reports the op surface", () => {
  const v = version();
  assert.ok(Number.isInteger(v.format_version));
  assert.ok(v.ops.includes("query"));
});

test("crud round trip on a real file", async () => {
  const db = await open(join(dir, "crud.tepin"));
  try {
    const id = await db.insert("notes", { title: "hello", stars: 4 });
    assert.equal((await db.get("notes", id)).title, "hello");

    const hits = await db.query("notes", { stars: { $gte: 3 } });
    assert.equal(hits.length, 1);
    assert.equal(hits[0]._id, id);

    await db.update("notes", id, { title: "hello2" });
    assert.equal((await db.get("notes", id)).title, "hello2");

    await db.purpose("notes", "driver test notes");
    assert.ok((await db.inspect()).includes("driver test notes"));

    const cols = await db.collections();
    assert.equal(cols[0].name, "notes");
    assert.equal(cols[0].count, 1);

    await db.delete("notes", id);
    assert.equal(await db.get("notes", id), null);
  } finally {
    db.close();
  }
});

test("file persists across open/close, and locks release", async () => {
  const path = join(dir, "persist.tepin");
  let db = await open(path);
  const id = await db.insert("kv", { k: "v" });
  db.close();

  db = await open(path, { existing: true });
  assert.equal((await db.get("kv", id)).k, "v");
  db.close();
});

test("batch writes are atomic and return ids", async () => {
  const db = await openMemory();
  try {
    const ids = await db.batch([
      { op: "insert", collection: "a", doc: { n: 1 } },
      { op: "upsert", collection: "a", doc: { _id: "x", n: 2 } },
    ]);
    assert.deepEqual(ids.length, 2);
    assert.equal(ids[1], "x");
  } finally {
    db.close();
  }
});

test("keyword search works without any model", async () => {
  const db = await openMemory();
  try {
    // The BM25 index follows the embed-fields config (no model needed).
    await db.embedFields("docs", ["body"]);
    await db.insert("docs", { body: "the quick brown fox" });
    await db.insert("docs", { body: "sleepy grey cat" });
    const hits = await db.keywordSearch("brown fox", { limit: 2 });
    assert.ok(hits.length >= 1);
    // Keyword hits are the raw primitives tier: {collection, id, score}.
    assert.equal(
      (await db.get(hits[0].collection, hits[0].id)).body,
      "the quick brown fox",
    );
  } finally {
    db.close();
  }
});

test("errors carry code, hint, and a CLI repro", async () => {
  const path = join(dir, "err.tepin");
  const db = await open(path);
  try {
    await assert.rejects(
      () => db.get("nope", "someid"),
      (e) => {
        assert.ok(e instanceof TepinError);
        assert.equal(e.code, "collection_not_found");
        assert.ok(e.hint.length > 0);
        assert.ok(e.cli.startsWith(`tepin get ${path}`));
        return true;
      },
    );
  } finally {
    db.close();
  }
});

test("generic call() speaks the shared op surface", async () => {
  const db = await openMemory();
  try {
    await db.call("insert", { collection: "c", doc: { a: 1 } });
    const out = await db.call("query", { collection: "c" });
    assert.equal(out.count, 1);

    await assert.rejects(
      () => db.call("quyre"),
      (e) => e.code === "not_implemented" && e.hint.includes("query"),
    );
  } finally {
    db.close();
  }
});

test("open existing on a missing path is a clean error", async () => {
  await assert.rejects(
    () => open(join(dir, "missing.tepin"), { existing: true }),
    (e) => e instanceof TepinError && e.code === "file_not_found",
  );
});

test("double close is safe, use-after-close is a clean error", async () => {
  const db = await openMemory();
  db.close();
  db.close();
  assert.throws(
    () => db.callSync("query", { collection: "c" }),
    (e) => e.code === "invalid_handle",
  );
});
