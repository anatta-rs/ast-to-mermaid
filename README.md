# ast-to-mermaid

Tree-sitter → graph → Mermaid pipeline for code analysis.

Multi-tenant (Namespace / Repo / Branch), pluggable storage via [polystore](https://github.com/anatta-rs/polystore) traits, embedded SurrealDB by default, MCP server + CLI.

## Status

`v0.1.0` — **scaffold only**. Workspace structure + CI + tooling. Parser, renderer, resolver, store impls, MCP server, CLI commands all land in subsequent PRs.

## Workspace

```
crates/
  ast-to-mermaid-core/   # lib: parser + render + resolve + GraphStore impls
  ast-to-mermaid-cli/    # bin: standalone CLI
  ast-to-mermaid-mcp/    # bin: MCP server (stdio JSON-RPC)
```

The `core` crate is the only public lib; the two bin crates are thin wrappers over `core`.

## Backends

Three impls of the polystore traits will ship in `core`:

| Backend | GraphStore | KvStore | VectorStore | Use case |
|---|---|---|---|---|
| InMemory | ✓ | ✓ | ✓ (linear) | tests, smoke |
| SurrealDB embedded | ✓ | ✓ | ✓ (HNSW) | default standalone |
| External (DI) | — | — | — | downstream plugs (e.g. Anatta) |

## Quality gates

- `make check` → fmt + clippy (pedantic) + test
- `make coverage-gate` → enforces ≥ 95% line coverage
- CI runs all of the above + automated versioning via [release-plz](https://release-plz.dev)

## License

[Apache-2.0](./LICENSE)
