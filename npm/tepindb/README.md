# tepindb

AI-first single-file database for CLI tools and agents. This package ships
the `tepin` CLI so you can use it with zero setup:

```sh
npx tepindb inspect my.tepin
npx tepindb query my.tepin notes '{"tag": "todo"}'
npx tepindb mcp my.tepin        # serve the database over MCP
```

A `.tepin` file is self-describing: run `inspect` on one and it tells you
what it contains and how it is organized.

This is the **slim** build — documents, filters, BM25 keyword search, and
the MCP server, with no ONNX runtime. For built-in semantic/vector search
install the full binary from
[GitHub releases](https://github.com/tepindb/tepindb/releases) or
`cargo install tepin-cli`.

The platform binary is installed via an `@tepindb/*` optionalDependency;
all binaries are built and published from the project's release workflow.
See the [repository](https://github.com/tepindb/tepindb) for docs, and
SECURITY.md there for the supply-chain story.
