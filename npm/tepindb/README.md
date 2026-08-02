# tepindb

AI-first single-file database for CLI tools and agents. This package ships
the `tepin` CLI **and the TypeScript/JavaScript driver** — one install,
both doors to the same database:

```sh
npx tepindb inspect my.tepin
npx tepindb query my.tepin notes '{"tag": "todo"}'
npx tepindb mcp my.tepin        # serve the database over MCP
```

```js
import { open } from "tepindb";

const db = await open("my.tepin", { serve: "host" }); // live-inspectable
const id = await db.insert("notes", { title: "hello", stars: 5 });
const docs = await db.query("notes", { stars: { $gte: 3 } });
db.close();
```

The driver speaks the same verbs as the CLI and the MCP server — one
surface to learn. Every error carries `{code, message, hint}` **and
`cli`**: the exact `tepin …` terminal command that reproduces the failing
call. Set `TEPIN_TRACE=1` to stream every op as JSON lines on stderr.
While your app runs with `serve: "host"`, `npx tepindb inspect my.tepin`
from another terminal reads it live, snapshot-isolated.

A `.tepin` file is self-describing: run `inspect` on one and it tells you
what it contains and how it is organized.

This is the **slim** build — documents, filters, BM25 keyword search, and
the MCP server, with no ONNX runtime. For built-in semantic/vector search
install the full binary from
[GitHub releases](https://github.com/tepindb/tepindb/releases) or
`cargo install tepin-cli`.

The platform binary is installed via a `tepindb-<platform>` optionalDependency;
all binaries are built and published from the project's release workflow.
See the [repository](https://github.com/tepindb/tepindb) for docs, and
SECURITY.md there for the supply-chain story.
