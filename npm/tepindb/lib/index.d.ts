/**
 * tepindb — AI-first single-file database.
 *
 * The driver speaks the same verbs as the `tepin` CLI and the MCP server:
 * one surface to learn. Every error is a TepinError carrying a stable
 * `code`, a human `message`, a `hint` telling you what to do next — and,
 * when the op has a CLI twin, `cli`: the exact terminal command that
 * reproduces the failing call. Set TEPIN_TRACE=1 to stream every op as a
 * JSON line on stderr.
 */

/** A stored document: arbitrary JSON object with a string `_id`. */
export interface Doc {
  _id: string;
  [field: string]: unknown;
}

/**
 * MongoDB-style filter: field equality, or per-field operators
 * `$eq | $ne | $gt | $gte | $lt | $lte | $in`. `{}` matches everything.
 */
export type Filter = Record<string, unknown>;

export interface CollectionInfo {
  name: string;
  purpose: string | null;
  embed: string[];
  /** True when the application supplies vectors itself (set_vectors). */
  manual_vectors: boolean;
  indexes: string[];
  unique: string[];
  count: number;
}

export interface SearchHit {
  collection: string;
  id: string;
  score: number;
  /** Index of the best-matching chunk (0 for unchunked short docs). */
  chunk: number;
  /** Total chunks this document's embed text splits into. */
  chunks: number;
  /** The best-matching chunk's text. */
  snippet: string;
  truncated: boolean;
  doc: Doc;
}

/** Raw BM25 hit (primitives tier) — fetch the doc with get() if needed. */
export interface KeywordHit {
  collection: string;
  id: string;
  score: number;
}

export interface BatchWrite {
  op: "insert" | "upsert" | "update" | "delete";
  collection: string;
  doc?: Record<string, unknown>;
  id?: string;
}

export interface OpenOptions {
  /** true = error when the file does not exist, instead of creating it. */
  existing?: boolean;
  /**
   * "auto" wires up bge-small for search() — lazy, SHA-256-pinned
   * download on first embed. Default "off": pure document store.
   */
  embedder?: "off" | "auto";
  /**
   * "host": serve reads to other processes while you hold the file —
   * `npx tepindb inspect` works against your live app. "discover": read
   * through an existing host when locked out. See docs/serving.md.
   */
  serve?: "off" | "host" | "discover" | "host_or_discover";
  /** Keep retrying a locked open with backoff for this many ms. */
  retryMs?: number;
}

export class TepinError extends Error {
  /** Stable machine-readable code (docs/errors.md). */
  code: string;
  /** What to do next. */
  hint: string;
  /** The `tepin …` CLI command reproducing this op, when it has one. */
  cli?: string;
}

export class Db {
  /** Path this handle was opened with (undefined for in-memory). */
  readonly path: string | undefined;
  /** True when reads go through another process's in-driver server. */
  readonly served: boolean;

  /** Markdown report of everything inside — start here. */
  inspect(): Promise<string>;
  collections(): Promise<CollectionInfo[]>;
  query(collection: string, filter?: Filter): Promise<Doc[]>;
  get(collection: string, id: string): Promise<Doc | null>;
  insert(collection: string, doc: Record<string, unknown>): Promise<string>;
  upsert(collection: string, doc: Record<string, unknown>): Promise<string>;
  update(
    collection: string,
    id: string,
    doc: Record<string, unknown>,
  ): Promise<void>;
  delete(collection: string, id: string): Promise<void>;
  /** Atomic multi-op write; all or nothing. Returns the written ids. */
  batch(ops: BatchWrite[]): Promise<string[]>;
  search(
    query: string,
    opts?: { collection?: string; limit?: number },
  ): Promise<SearchHit[]>;
  keywordSearch(
    query: string,
    opts?: { collection?: string; limit?: number },
  ): Promise<KeywordHit[]>;
  purpose(collection: string, text: string): Promise<void>;
  embedFields(
    collection: string,
    fields: string[],
  ): Promise<{ pending_embeddings: number }>;
  createIndex(
    collection: string,
    field: string,
    opts?: { unique?: boolean },
  ): Promise<void>;

  /**
   * Run any op by name — the generic escape hatch. Same op names,
   * argument names, and result shapes as the CLI and MCP server.
   */
  call<T = unknown>(op: string, args?: Record<string, unknown>): Promise<T>;
  callSync<T = unknown>(op: string, args?: Record<string, unknown>): T;

  /** Close the handle and release the file lock. */
  close(): void;
}

/** Open (or create) a database file. */
export function open(path: string, options?: OpenOptions): Promise<Db>;

/** A fresh in-memory database — full engine, zero disk, gone on close. */
export function openMemory(
  options?: Pick<OpenOptions, "embedder">,
): Promise<Db>;

/** Driver, format, and op-surface info from the loaded native addon. */
export function version(): {
  version: string;
  format_version: number;
  embedding: boolean;
  ops: string[];
};
