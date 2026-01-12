# Continuum Usage Guide

**Continuum** is a plain-text assistant conversation manager. It captures and stores conversations from multiple AI assistants as JSONL files, making them searchable with standard Unix tools and Nushell.

## Quick Start

### 1. Automatic Capture (Claude Code)

Claude Code conversations are automatically captured using the `continuum-claude` wrapper:

```bash
# Use the wrapper instead of claude directly
echo "What is 2 + 2?" | continuum-claude --print

# Or add to your shell config (already done in ~/dotfiles)
alias claude='continuum-claude'
```

**Location**: Conversations saved to `~/continuum-logs/claude-code/YYYY-MM-DD/session-id/`

### 2. Import from Other Assistants

Import conversations from Codex or Goose:

```bash
# Import latest Codex session
continuum import --assistant codex

# Import latest Goose session
continuum import --assistant goose

# Import specific session
continuum import --assistant codex --session ~/.codex/sessions/my-session.jsonl
```

### 3. Search Conversations

Use Nushell functions for powerful queries:

```nushell
# Full-text search across all conversations
continuum-search "database design" --limit 10

# View timeline for today
continuum-timeline

# View specific date
continuum-timeline "2025-11-09"

# Date range
continuum-timeline --from "2025-11-01" --to "2025-11-09"

# Filter by assistant
continuum-timeline --assistant "claude-code"

# Get statistics
continuum-stats

# Get context from a session
continuum-context "session-id"

# Export session to markdown
continuum-export-md "session-id" output.md
```

### 4. Use Standard Unix Tools

Since logs are plain text, you can use any Unix tool:

```bash
# Search with ripgrep
rg "API design" ~/continuum-logs

# Count conversations per day
ls ~/continuum-logs/claude-code/2025-11-09/ | wc -l

# View latest conversation
cat ~/continuum-logs/claude-code/2025-11-09/*/messages.jsonl | tail -20

# Extract all user questions
rg '"role":"user"' ~/continuum-logs --json | lines | from json | where type == "match"

# Find sessions about specific topics
fd -t d . ~/continuum-logs | xargs -I {} sh -c 'rg -q "machine learning" {} && echo {}'
```

## File Structure

```
~/continuum-logs/
├── claude-code/
│   └── 2025-11-09/
│       └── session-abc123/
│           ├── session.json      # Metadata
│           └── messages.jsonl    # Messages (one per line)
├── codex/
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

### session.json Format

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

### messages.jsonl Format

```jsonl
{"id":1,"role":"user","content":"How do I use FTS5?","timestamp":"2025-11-09T14:30:01Z"}
{"id":2,"role":"assistant","content":"FTS5 is SQLite's full-text search...","timestamp":"2025-11-09T14:30:15Z"}
{"id":3,"role":"user","content":"Can you show an example?","timestamp":"2025-11-09T14:31:00Z"}
```

## Nushell Function Reference

### continuum-search

Full-text search using ripgrep.

```nushell
continuum-search "query" --limit 20
```

**Returns**: Table with columns: `assistant`, `date`, `session`, `role`, `content`, `file`, `line`

### continuum-timeline

Browse conversations by date.

```nushell
# Today's conversations
continuum-timeline

# Specific date
continuum-timeline "2025-11-09"

# Date range
continuum-timeline --from "2025-11-01" --to "2025-11-09"

# Filter by assistant
continuum-timeline --assistant "claude-code"
```

**Returns**: Table of messages with `assistant`, `session`, `role`, `content`, `timestamp`

### continuum-stats

Show statistics by assistant.

```nushell
continuum-stats
```

**Returns**: Table with columns: `assistant`, `sessions`, `total_messages`, `total_size`

### continuum-context

Get recent messages from a session.

```nushell
continuum-context "session-id" --limit 50
```

**Returns**: Last N messages from the session

### continuum-export-md

Export session to markdown.

```nushell
continuum-export-md "session-id" output.md
```

Creates a markdown file with all messages formatted nicely.

## Advanced Usage

### Combining Nushell and Unix Tools

```nushell
# Find all sessions mentioning "API" and count messages
continuum-search "API"
| get session
| uniq
| each { |s|
    open ~/continuum-logs/**/($s)/messages.jsonl
    | lines
    | length
}
| math sum

# Get all assistant responses from today
continuum-timeline (date now | format date "%Y-%m-%d")
| where role == "assistant"
| select content

# Find your most active conversation day
ls ~/continuum-logs/**/session.json
| each { open $in.name }
| get start_time
| each { split row "T" | first }
| group-by
| transpose date sessions
| insert count { |row| $row.sessions | length }
| sort-by count --reverse
| first
```

### Git Integration

Track your conversation history with git:

```bash
cd ~/continuum-logs
git init
git add .
git commit -m "Conversations from $(date +%Y-%m-%d)"

# View conversation diffs
git diff HEAD~1

# Find when you discussed a topic
git log -S "database design" --oneline
```

### Backup

```bash
# Simple rsync backup
rsync -av ~/continuum-logs/ /backup/continuum-logs/

# Compressed archive
tar -czf continuum-backup-$(date +%Y-%m-%d).tar.gz ~/continuum-logs
```

## Troubleshooting

### "No conversations found"

Check that continuum-claude wrapper is being used:

```bash
which claude  # Should point to continuum-claude
ls ~/continuum-logs/claude-code/  # Should have date directories
```

### "Nushell functions not found"

Ensure continuum.nu is sourced:

```nushell
# Check if sourced
cat ~/.config/nushell/config.nu | rg continuum

# Source manually
source ~/dotfiles/nushell/continuum.nu
```

### Import not working

Verify source data exists:

```bash
# Codex
ls ~/.codex/sessions/

# Goose
ls ~/.local/share/goose/sessions/sessions.db
```

## Tips

1. **Use tab completion**: Nushell provides excellent tab completion for session IDs
2. **Alias common queries**: Add your own custom queries to continuum.nu
3. **Combine with jq**: For more complex JSON processing
4. **Use semantic search**: Your existing semantic search tools work on these files
5. **Create custom views**: Write Nushell scripts for your common analysis patterns

## Next Steps

- Explore the Nushell functions in `~/dotfiles/nushell/continuum.nu`
- Set up git tracking for conversation history
- Create custom Nushell queries for your workflow
- Integrate with your existing semantic search system
