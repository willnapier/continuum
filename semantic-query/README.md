# Semantic query

The Rust `semantic-query` and `semantic-query-continuum` binaries replace the
Dotfiles shell/Python query entry points. The Python file here is only the FAISS
native-library binding. The existing indexer, configurations and indexes remain
in place; querying never rebuilds an index. Only structurally compatible indexes
are usable. The legacy incremental indexer can append a vector without extending
its path list; count disagreement is now refused explicitly. A synthetic legacy
indexer parity check does not certify live indexes, so rollout remains held
until their shape and the whole-request failure policy are accepted.

## Runtime and build

Create a separate virtual environment on each host at
`~/.local/share/semantic-search-runtime/venv`, outside every synced tree, then
install `runtime-requirements.txt` with `pip install --only-binary=:all:`.
The current pins support the tested Linux and macOS interpreters. Do not copy a
venv between hosts. `SEMANTIC_SEARCH_PYTHON` overrides its interpreter for fixtures.

Build from this workspace using the host build guard and `cargo build --locked
--release -p continuum-mcp -p continuum-semantic-query`. Install all three binaries
into the same directory: MCP resolves its sibling `semantic-query-continuum`.
After the Dotfiles mapping retirement has been deployed, install by fresh-inode
rename; never copy through an old script symlink. Subsequent builds use the
external deployment registry. Mac signing rules still apply to signed tools
such as `continuum-grok`; see `~/Assistants/shared/RUST-REDEPLOY.md`.

## Interface

Both commands accept `--text QUERY` or `--file PATH`, `--limit N`, `--config PATH`,
and `--json`. Defaults retain the existing config paths:

- `semantic-query`: `~/.local/share/semantic-search/config.yaml`
- `semantic-query-continuum`: `~/.local/share/semantic-search-continuum/config.yaml`

Human output retains score/filename lines for Nushell selectors. JSON is an array
of `{path, similarity, title, snippet}` objects. Errors exit nonzero; only valid
searches with no qualifying matches return `[]`.

MCP always supplies `--root ARCHIVE`. Every metadata path must then name an
existing `assistant/YYYY-MM-DD/session/messages.jsonl` or `session.json` under that root. Directory
handles and no-follow opens refuse symlink traversal. Selected records are parsed
strictly; malformed files produce errors rather than missing results. Without
`--root`, the generic notes workflow retains its configured filesystem scope.

`OPENAI_API_KEY` is required for query embeddings. `OPENAI_BASE_URL`, if supplied,
selects the explicit embedding endpoint; otherwise the endpoint is OpenAI `/v1`.
There is no model/account fallback. Fixture qualification uses only a loopback
mock endpoint and synthetic vectors, never real notes or embeddings. The key is
removed before starting the FAISS binding. Python runs in isolated mode (`-I`)
from `/`, ignoring the caller’s current directory and `PYTHON*` import settings. Config, metadata and result files
have size limits; HTTP and trusted-helper execution have time/output limits.

The index and configuration are trusted local inputs. This is not sandboxing of
hostile native FAISS files or an execution-containment guarantee. Existing paths
captured before exclusion remain retrievable; there is no retroactive erasure.
