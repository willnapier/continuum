# Continuum MCP server

Version 0.2.0 provides keyword search, timeline, full-session retrieval and
semantic search over the configured Continuum archive. The stdio transport
speaks MCP; it is not a conventional interactive CLI.

This crate was imported from the previously unversioned `~/continuum-mcp` source.
`SOURCE-IMPORT.json` records the original source hashes. It does not establish
which source produced the old installed executable. This workspace and its root
`Cargo.lock` now own builds; the old source directory remains untouched.

## Retrieval contract

`CONTINUUM_DIR` selects the archive root (default `~/continuum-logs`, with leading
`~` expanded). Each session is `assistant/YYYY-MM-DD/session/messages.jsonl` with
optional `session.json`. Assistant/session arguments must be single components;
absolute paths, traversal and symlinks are refused. Descendants are opened
relative to pinned directory handles with `NOFOLLOW` on Linux and macOS.

- `search_conversations`: case-insensitive regular expression, optional assistant
  and `YYYY-MM`/`YYYY-MM-DD` date filters; default 20 results.
- `get_timeline`: optional date/assistant, default 50 results in timestamp order.
- `get_conversation`: exact assistant/date/session; required messages and optional
  metadata. Corrupt messages or present-but-corrupt metadata are errors.
- `semantic_search_conversations`: query and default 10 results; invokes the
  sibling `semantic-query-continuum --json --root ARCHIVE`, then validates all
  returned archive paths again. `CONTINUUM_SEMANTIC_CONFIG` selects an explicit
  config for controlled deployments or fixtures.

Limits are 1–1000. Files are capped at 32 MiB. Invalid filters, inaccessible
archives, corrupt records and invalid expressions report MCP errors. A valid
search with no matches remains a successful JSON `[]` result. A corrupt unit
fails the whole request and names its archive-relative file; partial-result
policy and a content-free live-shape assessment remain rollout decisions. Unicode previews do
not split UTF-8 bytes. Query text is not written to server logs.

## Build and deploy

Use the workspace's locked dependencies and required host build guard:
`cargo test --locked -p continuum-mcp -p continuum-grok -p continuum-semantic-query`.
The semantic helper and runtime installation contract is in
[`../semantic-query/README.md`](../semantic-query/README.md). Install MCP and its
helper together and verify each binary's SHA-256 against the build receipt.
Source commit, lock hash, toolchain, tests and installed hash are distinct evidence.

The synthetic stdio and FAISS fixture receipts are linked by the owning work
order in `~/Assistants/shared/ASSISTANT-ORCHESTRATION-WORK-ORDER-2026-10-18.md`.
No qualification here authorizes the deferred neutral-orchestration switchover.
