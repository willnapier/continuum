# Continuum Plain-Text Architecture

## Design Philosophy

**Core Principle**: Store everything as plain text/JSON files. Use Nushell's data pipelines for queries.

**Why This Works**:
- Nushell provides SQL-like query power on plain files
- Ripgrep provides fast full-text search
- No database lock-in or schema migrations
- Complete transparency and control
- Git-trackable, rsync-able, future-proof

## Directory Structure

```
~/continuum-logs/
├── codex/
│   └── 2025-11-09/
│       └── session-abc123/
│           ├── session.json      # Session metadata
│           └── messages.jsonl    # Messages (one JSON per line)
├── claude-code/
│   └── 2025-11-09/
│       └── session-xyz789/
│           ├── session.json
│           └── messages.jsonl
└── goose/
    └── 2025-11-09/
        └── session-123/
            ├── session.json
            └── messages.jsonl
```

**Organization**: `assistant/date/session-id/`

**File Formats**:
- `session.json`: Single JSON object with metadata
- `messages.jsonl`: JSONL (one message per line, each line is valid JSON)

## File Formats

### session.json
```json
{
  "id": "session-abc123",
  "assistant": "codex",
  "start_time": "2025-11-09T14:30:00Z",
  "end_time": "2025-11-09T15:45:00Z",
  "status": "closed",
  "source_path": "/path/to/original",
  "message_count": 42,
  "created_at": "2025-11-09T14:35:00Z"
}
```

### messages.jsonl
```jsonl
{"id":1,"role":"user","content":"How do I use FTS5?","timestamp":"2025-11-09T14:30:01Z"}
{"id":2,"role":"assistant","content":"FTS5 is SQLite's full-text search...","timestamp":"2025-11-09T14:30:15Z"}
{"id":3,"role":"user","content":"Can you show an example?","timestamp":"2025-11-09T14:31:00Z"}
```

**Why JSONL**:
- One message per line = easy to `tail`, `head`, `wc -l`
- Each line is valid JSON = easy to parse
- Append-only = simple to write
- Grep-able, awk-able, sed-able

## Component Architecture

### 1. Import Pipeline

```
Assistant logs → Adapter → Plain text export
```

**What adapters do**:
- Read native format (JSONL, SQLite, JSON stream)
- Apply noise filtering (remove pleasantries)
- Write to `~/continuum-logs/assistant/date/session/`

**No database writes**. Just file creation.

### 2. Query Methods

**A. Ripgrep (full-text search)**
```bash
rg "database design" ~/continuum-logs
```

**B. Nushell functions (structured queries)**
```nushell
# Search across all conversations
def continuum-search [query: string] {
    rg $query ~/continuum-logs --json
    | lines
    | each { from json }
    | where type == "match"
    | select data.path.text data.lines.text
}

# Timeline by date
def continuum-timeline [date: string] {
    ls $"~/continuum-logs/**/($date)/**/messages.jsonl"
    | each { |f|
        open $f.name
        | lines
        | each { from json }
        | insert session { $f.name | path dirname | path basename }
        | insert assistant { $f.name | path dirname | path dirname | path dirname | path basename }
    }
    | flatten
    | sort-by timestamp
}

# Stats by assistant
def continuum-stats [] {
    ls ~/continuum-logs/**/messages.jsonl
    | each { |f|
        {
            assistant: ($f.name | path dirname | path dirname | path dirname | path basename),
            messages: (open $f.name | lines | length)
        }
    }
    | group-by assistant
    | transpose assistant data
    | insert total { |row| $row.data | reduce -f 0 { |it, acc| $acc + $it.messages } }
}
```

**C. Semantic search (existing tool)**
```bash
semantic-query "database architecture patterns"
# Works on the same files
```

### 3. Simplified CLI

**continuum CLI becomes a thin wrapper**:
- `continuum import` → writes plain text files
- `continuum search` → calls `rg` + formats output
- `continuum timeline` → pure Nushell function (no CLI needed)
- `continuum stats` → pure Nushell function

**Most functionality moves to Nushell functions** in your `config.nu`.

## Migration Strategy

### Phase 1: New Import Path
1. Modify adapters to write plain text files
2. Remove all SQLite storage code
3. Keep wrapper (`continuum-claude`) unchanged

### Phase 2: Nushell Functions
1. Create `~/dotfiles/nushell/continuum.nu`
2. Add functions: search, timeline, stats, context
3. Source from `config.nu`

### Phase 3: Export Existing Data
1. Read current SQLite database
2. Export to new plain-text format
3. Verify with Nushell queries

### Phase 4: Cleanup
1. Remove database schema code
2. Remove storage.rs
3. Simplify CLI to just import + thin search wrapper

## What We're Removing

**From the codebase**:
- `continuum-core/src/storage.rs` (entire file)
- SQLite dependency
- Schema migrations
- Database initialization
- FTS5 triggers
- All SQL queries

**What remains**:
- Adapters (read native formats)
- Compression (noise filtering)
- CLI (thin wrappers for import)
- Wrapper (`continuum-claude`)

**Lines of code reduction**: ~800 lines removed, ~200 lines of Nushell functions added.

## Advantages Gained

1. **Complete Transparency**: `cat` any file to see content
2. **Git Integration**: `git diff` shows actual changes
3. **Tool Universality**: Works with any Unix tool
4. **No Schema Migrations**: JSON format is self-describing
5. **Simple Backup**: `rsync ~/continuum-logs backup/`
6. **Future Proof**: Plain text survives everything
7. **Semantic Search Ready**: Already indexed by your existing tools
8. **Simpler Codebase**: Less to maintain, easier to understand

## Advantages Lost

1. **FTS5 Ranking**: But you have semantic search for ranking
2. **10ms Query Speed**: But ripgrep is already fast enough
3. **ACID Transactions**: But you don't need them for read-heavy logs
4. **Built-in Aggregations**: But Nushell provides these

## Example Queries

### Full-Text Search (ripgrep)
```bash
rg "FTS5" ~/continuum-logs
rg "database.*design" ~/continuum-logs
rg -i "error" ~/continuum-logs --json | from json
```

### Structured Queries (Nushell)
```nushell
# Messages from last 7 days
continuum-timeline (date now | format date "%Y-%m-%d")
| where timestamp > ((date now) - 7day)

# Only questions (user messages with "?")
continuum-search "database"
| where role == "user"
| where content =~ "?"

# Session statistics
continuum-stats
| where total > 100
| sort-by total --reverse

# Export to markdown
continuum-timeline "2025-11-09"
| each { |msg|
    $"### ($msg.role) at ($msg.timestamp)\n\n($msg.content)\n\n"
}
| str join
| save today-log.md
```

### Semantic Search (existing)
```bash
semantic-query "database architecture patterns" ~/continuum-logs
```

## Implementation Plan

**Step 1**: Create new export format
- Modify adapters to write JSONL
- Test with one session from each assistant

**Step 2**: Build Nushell functions
- `continuum-search`
- `continuum-timeline`
- `continuum-stats`
- Add to dotfiles

**Step 3**: Export existing database
- Read all sessions/messages from SQLite
- Write to new format
- Verify data integrity

**Step 4**: Remove database code
- Delete storage.rs
- Remove SQLite dependency
- Update CLI to use new format

**Step 5**: Update documentation
- USAGE.md with Nushell examples
- QUICK-START.md with new workflow
- ARCHITECTURE.md with new design

## Estimated Effort

- **Code removal**: ~800 lines
- **New code**: ~200 lines (Nushell functions)
- **Migration**: ~100 lines (export script)
- **Testing**: Verify with real data
- **Documentation**: Update all guides

**Total**: ~4-6 hours of focused work

**Result**: Simpler, more transparent, more aligned with Unix philosophy and your existing tools.
