# Continuum Architecture

## Core Philosophy

Continuum follows the **Unix philosophy**: plain text, composability, transparency, and tool universality.

**Problem Solved**: AI assistants store conversations in different formats, making cross-assistant search and analysis difficult.

**Solution**: Normalize conversations into plain-text JSONL files, making them searchable with any Unix tool (ripgrep, fd, awk, sed) and Nushell's structured data pipelines.

## Plain-Text Architecture

### Storage Format

All conversations are stored as plain-text JSONL files in `~/continuum-logs/`:

```
~/continuum-logs/
├── claude-code/
│   └── YYYY-MM-DD/
│       └── session-id/
│           ├── session.json      # Metadata
│           └── messages.jsonl    # One message per line
├── codex/
│   └── YYYY-MM-DD/
│       └── session-id/
│           ├── session.json
│           └── messages.jsonl
└── goose/
    └── YYYY-MM-DD/
        └── session-id/
            ├── session.json
            └── messages.jsonl
```

### File Formats

**session.json** - Session metadata:
```json
{
  "id": "session-abc123",
  "assistant": "claude-code",
  "start_time": "2025-11-09T14:30:00Z",
  "end_time": "2025-11-09T15:45:00Z",
  "status": "closed",
  "message_count": 42,
  "created_at": "2025-11-09T14:35:00Z"
}
```

**messages.jsonl** - Messages (one JSON object per line):
```jsonl
{"id":1,"role":"user","content":"How do I...","timestamp":"2025-11-09T14:30:01Z"}
{"id":2,"role":"assistant","content":"You can...","timestamp":"2025-11-09T14:30:15Z"}
{"id":3,"role":"user","content":"Can you show...","timestamp":"2025-11-09T14:31:00Z"}
```

## Data Flow

```
┌──────────────────────────────────────────────────────────────┐
│              Assistant Native Storage                         │
├──────────────────────────────────────────────────────────────┤
│                                                               │
│  ~/.codex/sessions/        ~/.local/share/goose/   claude    │
│  YYYY/MM/DD/*.jsonl       sessions.db              --print   │
│                                                               │
└────────┬───────────────────────┬──────────────────┬──────────┘
         │                       │                  │
         │ CodexAdapter          │ GooseAdapter     │ continuum-claude
         │                       │                  │ wrapper
         ▼                       ▼                  ▼
┌──────────────────────────────────────────────────────────────┐
│         Plain-Text JSONL Files                               │
│         ~/continuum-logs/assistant/date/session/             │
├──────────────────────────────────────────────────────────────┤
│                                                               │
│  session.json         messages.jsonl                         │
│  ┌────────────┐      ┌────────────────┐                     │
│  │ metadata   │      │ {"id":1,...}   │                     │
│  │ timestamps │      │ {"id":2,...}   │                     │
│  │ count      │      │ {"id":3,...}   │                     │
│  └────────────┘      └────────────────┘                     │
│                                                               │
└────────┬──────────────────────────────────────────────────────┘
         │
         │ ripgrep / Nushell / any Unix tool
         ▼
┌──────────────────────────────────────────────────────────────┐
│                    Query Interface                            │
│                                                               │
│  $ continuum-search "FTS5"                                   │
│  $ continuum-timeline 2025-11-09                             │
│  $ rg "pattern" ~/continuum-logs                             │
│                                                               │
└──────────────────────────────────────────────────────────────┘
```

## Component Architecture

### continuum-core (Library)

**Adapters** (`src/adapters/`)
- `LogAdapter` trait - Interface all adapters implement
- `CodexAdapter` - Reads JSONL files from Codex
- `GooseAdapter` - Reads from Goose's SQLite database
- Future: Additional adapters for other assistants

**PlainTextWriter** (`src/plaintext.rs`)
- Writes sessions and messages to JSONL files
- Manages directory structure
- Handles timestamp formats (ISO8601, SQLite)
- Append-only message writing
- Session metadata updates

**Compression** (`src/compression.rs`)
- `NoiseFilter` - Removes pleasantries, boilerplate
- `MessageCompressor` - Batch processing, token estimation
- Regex-based (no LLM required)
- ~20% token reduction

**Types** (`src/types.rs`)
- Shared structures for Codex log entries
- Common data types across adapters

### continuum-cli (Binary)

Minimal CLI focused on import only:

**Commands**:
- `import` - Import sessions from Codex/Goose to plain-text JSONL
- `stats` - Show helpful message about Nushell functions

**Removed** (now handled by Nushell):
- ~~`search`~~ → `continuum-search` (Nushell function)
- ~~`timeline`~~ → `continuum-timeline` (Nushell function)
- ~~`context`~~ → `continuum-context` (Nushell function)
- ~~`summarize`~~ → Can be added as Nushell function if needed
- ~~`bookmark`~~ → Can be added as plain-text files if needed

### continuum-claude (Wrapper Binary)

**Purpose**: Capture Claude Code conversations automatically.

**How it works**:
1. Captures stdin (user prompt)
2. Spawns `claude --output-format stream-json`
3. Parses JSON events in real-time
4. Writes to `~/continuum-logs/claude-code/` using PlainTextWriter
5. Forwards output to user (transparent wrapper)

**Usage**: Transparently replaces `claude` command.

### Nushell Functions

**Location**: `~/dotfiles/nushell/continuum.nu`

**Functions**:
- `continuum-search` - Full-text search using ripgrep
- `continuum-timeline` - Date/range-based browsing
- `continuum-stats` - Statistics by assistant
- `continuum-context` - Get session messages
- `continuum-export-md` - Export to markdown

## Key Design Decisions

### 1. Why plain-text instead of a database?

**Advantages**:
- ✅ Universal tool compatibility (rg, fd, awk, sed, jq)
- ✅ Git integration (diff, log, track changes)
- ✅ Complete transparency (`cat` to view)
- ✅ Future-proof (plain text outlasts any tool)
- ✅ No schema migrations
- ✅ Simple backup (rsync, tar)
- ✅ Works with existing semantic search tools

**Trade-offs Accepted**:
- No FTS5 ranking (but semantic search available)
- Slightly slower search (but ripgrep is very fast)
- No ACID transactions (but logs are append-only)

### 2. Why JSONL instead of JSON arrays?

**Rationale**:
- Append-only writing (safer, no need to rewrite entire file)
- Line-by-line processing (`head`, `tail`, `wc -l`)
- Streaming-friendly
- Each line is valid JSON (tools can parse incrementally)

### 3. Why Nushell for queries instead of CLI commands?

**Rationale**:
- Nushell provides SQL-like query power on plain files
- User already uses Nushell
- Composability with other Nushell functions
- No need to maintain CLI query commands
- Flexibility to create custom queries

### 4. Why keep noise compression?

**Rationale**:
- ~20% token reduction proven effective
- Fast and deterministic (regex-based)
- Works offline
- Useful for context preparation
- Applied during import (one-time cost)

## Current Capabilities

### Automatic Capture
- **Claude Code**: Real-time via `continuum-claude` wrapper
- **Codex**: Manual import with noise filtering
- **Goose**: Manual import with noise filtering

### Search & Query
- **Full-text**: `continuum-search` (ripgrep-based)
- **Timeline**: `continuum-timeline` (date/range browsing)
- **Statistics**: `continuum-stats` (aggregations)
- **Direct tools**: `rg`, `fd`, `awk`, `sed` all work

### Export
- **Markdown**: `continuum-export-md`
- **Custom**: Write your own Nushell functions

## Performance Characteristics

- **Search**: Depends on ripgrep (~milliseconds for thousands of messages)
- **Compression**: O(n) regex matching, ~5-10ms for 100 messages
- **Import**: I/O bound, depends on source format
- **File size**: ~0.5-1 KB per message (JSONL format)

## Security Considerations

1. **File permissions**: `~/continuum-logs/` inherits user home directory permissions
2. **No encryption**: Files are plaintext (consider encrypting directory if needed)
3. **API keys**: Not stored in logs (continuum-claude doesn't capture keys)
4. **Git tracking**: Be careful not to commit sensitive conversations

## Future Enhancements

**Optional additions** (system is complete as-is):
1. Semantic search integration examples
2. Systemd timer for auto-import
3. Visualization tools (conversation graphs)
4. Additional export formats (HTML, PDF)
5. Nushell functions for summarization
6. Git-based sync between machines

## File Structure

```
continuum/
├── continuum-core/          # Library crate
│   ├── src/
│   │   ├── adapters/
│   │   │   ├── mod.rs       # LogAdapter trait
│   │   │   ├── codex.rs     # Codex adapter
│   │   │   └── goose.rs     # Goose adapter
│   │   ├── compression.rs   # NoiseFilter, MessageCompressor
│   │   ├── plaintext.rs     # PlainTextWriter
│   │   ├── types.rs         # Shared types
│   │   └── lib.rs           # Re-exports
│   └── Cargo.toml
├── continuum-cli/           # Main CLI binary
│   ├── src/
│   │   └── main.rs          # Import command
│   └── Cargo.toml
├── continuum-claude/        # Claude Code wrapper
│   ├── src/
│   │   └── main.rs          # Wrapper implementation
│   └── Cargo.toml
├── Cargo.toml               # Workspace manifest
├── README.md                # Project overview
├── USAGE.md                 # User guide
├── ARCHITECTURE.md          # This file
└── PLAIN-TEXT-ARCHITECTURE.md  # Design philosophy
```

## Comparison to Database Approach

### Old Architecture (Removed)
- SQLite database at `~/.config/continuum/continuum.db`
- FTS5 full-text search
- Multiple CLI commands
- ~800 lines of database code
- Schema migrations needed

### New Architecture (Current)
- Plain-text JSONL files at `~/continuum-logs/`
- ripgrep + Nushell queries
- Minimal CLI (import only)
- ~200 lines of plain-text writer code
- No migrations ever needed

## Conclusion

Continuum is a **plain-text conversation storage system** that normalizes conversations from different assistants into a universal format. It doesn't replace assistant-native storage - it **complements** it with normalized exports.

The architecture is intentionally simple:
- Adapters handle source diversity
- PlainTextWriter provides consistent storage
- Nushell provides query capabilities
- Unix tools provide unlimited flexibility
- Wrapper handles live capture

This makes it transparent, maintainable, and future-proof.
