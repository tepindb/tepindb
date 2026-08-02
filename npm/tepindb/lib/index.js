// The tepindb TypeScript/JavaScript driver: the same verbs as the CLI and
// MCP server (one surface to learn), Promise-based, with rich errors.
// Every TepinError carries {code, message, hint} — and, when the failing
// op has a CLI twin, `cli`: the exact `tepin …` command that reproduces
// it in a terminal, so a stuck human (or AI) can debug outside the app.
// Set TEPIN_TRACE=1 to stream every op as a JSON line on stderr.

const { loadAddon } = require("./native");

/** The standard TepinDB error: code + message + hint (+ cli repro). */
class TepinError extends Error {
  constructor({ code, message, hint }, cli) {
    super(message);
    this.name = "TepinError";
    this.code = code;
    this.hint = hint;
    if (cli) this.cli = cli;
  }
}

// Ops with a CLI twin: op name -> [subcommand, argument order].
const CLI_SHAPES = {
  inspect: [],
  query: ["collection", "filter"],
  get: ["collection", "id"],
  insert: ["collection", "doc"],
  upsert: ["collection", "doc"],
  update: ["collection", "id", "doc"],
  delete: ["collection", "id"],
  purpose: ["collection", "text"],
  search: ["query"],
};

function shellQuote(s) {
  return `'${String(s).replace(/'/g, `'\\''`)}'`;
}

/** The `tepin …` command equivalent to an op, or null when it has none. */
function cliRepro(path, op, args) {
  const shape = CLI_SHAPES[op];
  if (!shape || !path) return null;
  const parts = ["tepin", op.replace(/_/g, "-"), path];
  for (const key of shape) {
    const v = args[key];
    if (v === undefined || v === null) continue;
    parts.push(
      typeof v === "string" ? shellQuote(v) : shellQuote(JSON.stringify(v)),
    );
  }
  if (op === "search") {
    if (args.collection) parts.push("--collection", args.collection);
    if (args.limit) parts.push("--limit", String(args.limit));
  }
  return parts.join(" ");
}

function unwrap(envelopeJson, path, op, args) {
  const v = JSON.parse(envelopeJson);
  if (v.error) throw new TepinError(v.error, cliRepro(path, op, args ?? {}));
  return v.ok;
}

/**
 * An open database. Obtain via open() / openMemory(); release with
 * close() — the file lock frees on close (or process exit).
 */
class Db {
  #addon;
  #handle;
  #path;
  #served;

  constructor(addon, handle, path, served) {
    this.#addon = addon;
    this.#handle = handle;
    this.#path = path;
    this.#served = served;
  }

  /** Path this handle was opened with (undefined for in-memory). */
  get path() {
    return this.#path;
  }

  /**
   * True when another process holds the write lock and this handle reads
   * through its in-driver server (docs/serving.md).
   */
  get served() {
    return this.#served;
  }

  #openHandle() {
    if (this.#handle === null) {
      throw new TepinError({
        code: "invalid_handle",
        message: "this database handle is closed",
        hint: "open() the database again; close() ends a handle for good",
      });
    }
    return this.#handle;
  }

  /**
   * Run any op by name — the generic escape hatch. Same op names,
   * argument names, and result shapes as the CLI and MCP server.
   */
  async call(op, args = {}) {
    const out = await this.#addon.call(
      this.#openHandle(),
      op,
      JSON.stringify(args),
    );
    return unwrap(out, this.#path, op, args);
  }

  /** Synchronous variant of call(), for scripts and REPLs. */
  callSync(op, args = {}) {
    return unwrap(
      this.#addon.callSync(this.#openHandle(), op, JSON.stringify(args)),
      this.#path,
      op,
      args,
    );
  }

  /** Markdown report of everything inside — start here. */
  async inspect() {
    return (await this.call("inspect", { path: this.#path })).markdown;
  }

  /** Collection metadata: names, counts, purposes, embedded fields. */
  async collections() {
    return (await this.call("collections")).collections;
  }

  /** Find documents with a MongoDB-style filter ({} matches all). */
  async query(collection, filter = {}) {
    return (await this.call("query", { collection, filter })).docs;
  }

  /** Fetch one document by _id (null when absent). */
  async get(collection, id) {
    return (await this.call("get", { collection, id })).doc;
  }

  /** Insert a document; returns its _id. Creates the collection lazily. */
  async insert(collection, doc) {
    return (await this.call("insert", { collection, doc })).inserted;
  }

  /** Insert-or-replace by _id; returns the _id. */
  async upsert(collection, doc) {
    return (await this.call("upsert", { collection, doc })).upserted;
  }

  /** Replace a document by _id. */
  async update(collection, id, doc) {
    await this.call("update", { collection, id, doc });
  }

  /** Delete a document by _id (also removes its search vectors). */
  async delete(collection, id) {
    await this.call("delete", { collection, id });
  }

  /**
   * Atomic multi-op write: [{op: "insert"|"upsert"|"update"|"delete",
   * collection, doc?/id?}, …]. All or nothing; returns the written ids.
   */
  async batch(ops) {
    return (await this.call("batch", { ops })).ids;
  }

  /** Semantic search in natural language across embedded collections. */
  async search(query, { collection, limit } = {}) {
    return (await this.call("search", { query, collection, limit })).hits;
  }

  /** BM25 keyword search — works without any embedding model. */
  async keywordSearch(query, { collection, limit } = {}) {
    return (await this.call("keyword_search", { query, collection, limit }))
      .hits;
  }

  /** Set a collection's free-text purpose (shown by inspect). */
  async purpose(collection, text) {
    await this.call("purpose", { collection, text });
  }

  /** Declare which fields get embedded for semantic search. */
  async embedFields(collection, fields) {
    return await this.call("embed_fields", { collection, fields });
  }

  /** Secondary equality index; {unique: true} enforces uniqueness. */
  async createIndex(collection, field, { unique } = {}) {
    await this.call("create_index", { collection, field, unique });
  }

  /** Close the handle and release the file lock. Safe to call once. */
  close() {
    if (this.#handle === null) return;
    const out = this.#addon.closeSync(this.#handle);
    this.#handle = null;
    unwrap(out, this.#path, "close");
  }
}

function openWith(addon, options, path) {
  const ok = unwrap(addon.openSync(JSON.stringify(options)), path, "open");
  return new Db(addon, ok.handle, path, ok.served);
}

/**
 * Open (or create) a database file. Options:
 *  - existing:  true = error instead of create (read-path semantics)
 *  - embedder:  "auto" wires up bge-small for search() — lazy,
 *               SHA-256-pinned download on first embed; default "off"
 *  - serve:     "host" serves reads to other processes while you hold the
 *               file — `npx tepindb inspect` works against your live app;
 *               "discover" reads through an existing host when locked out;
 *               "host_or_discover" does whichever applies (docs/serving.md)
 *  - retryMs:   keep retrying a locked open with backoff for this long
 */
async function open(path, { existing, embedder, serve, retryMs } = {}) {
  const addon = loadAddon();
  return openWith(
    addon,
    { path, existing, embedder, serve, retry_ms: retryMs },
    path,
  );
}

/** A fresh in-memory database — full engine, zero disk, gone on close. */
async function openMemory({ embedder } = {}) {
  return openWith(loadAddon(), { in_memory: true, embedder }, undefined);
}

/** Driver, format, and op-surface info from the loaded addon. */
function version() {
  return JSON.parse(loadAddon().version()).ok;
}

module.exports = { open, openMemory, version, Db, TepinError };
