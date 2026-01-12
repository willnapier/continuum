# Continuum Quick Start

## What is Continuum?

Continuum lets you **search across all your AI assistant conversations** in one place, stored as plain-text JSONL files.

Instead of searching Codex logs separately from Claude Code logs separately from Goose logs, Continuum normalizes all conversations into a unified plain-text format that works with any Unix tool.

## 5-Minute Setup

### 1. Build the binaries

```bash
cd ~/Assistants/projects/continuum
cargo build --release
```

### 2. Add to PATH

```bash
# Create symlinks
ln -s ~/Assistants/projects/continuum/target/release/continuum ~/.local/bin/
ln -s ~/Assistants/projects/continuum/target/release/continuum-claude ~/.local/bin/
```

### 3. Source Nushell functions

```bash
# Add to ~/.config/nushell/config.nu
echo "source ~/dotfiles/nushell/continuum.nu" >> ~/.config/nushell/config.nu

# Or source manually for this session
source ~/dotfiles/nushell/continuum.nu
```

### 4. Import your existing sessions

```bash
# Import latest Codex session
continuum import --assistant codex

# Import latest Goose session
continuum import --assistant goose

# Or just start using continuum-claude wrapper (auto-imports as you chat)
echo "explain FTS5" | continuum-claude --print
```

## Daily Workflow

### Using Continuum with Each Assistant

**Claude Code**:
```bash
# Just use continuum-claude wrapper (conversations auto-saved)
continuum-claude --print "your prompt here"

# Or pipe input
echo "explain Rust lifetimes" | continuum-claude --print
```

**Codex**:
```bash
# Import after your Codex session
continuum import --assistant codex
```

**Goose**:
```bash
# Import after your Goose session
continuum import --assistant goose
```

### Searching Across All Assistants

```nushell
# Search for "FTS5" across all conversations
continuum-search "FTS5" --limit 10

# Results show which assistant said what
# ╭───┬─────────────┬────────────┬──────────────┬──────┬─────────╮
# │ # │  assistant  │    date    │   session    │ role │ content │
# ├───┼─────────────┼────────────┼──────────────┼──────┼─────────┤
# │ 0 │ codex       │ 2025-11-09 │ session-abc  │ user │ ...     │
# │ 1 │ claude-code │ 2025-11-09 │ session-xyz  │ asst │ ...     │
# ╰───┴─────────────┴────────────┴──────────────┴──────┴─────────╯
```

### Browsing Timeline

```nushell
# View today's conversations
continuum-timeline

# View specific date
continuum-timeline "2025-11-09"

# Filter by assistant
continuum-timeline --assistant "claude-code"

# Date range
continuum-timeline --from "2025-11-01" --to "2025-11-09"
```

### Getting Session Context

```nushell
# Get recent messages from a session
continuum-context "session-id" --limit 50

# Export session to markdown
continuum-export-md "session-id" output.md
```

### Statistics

```nushell
# Show statistics by assistant
continuum-stats

# ╭───┬─────────────┬──────────┬────────────────┬────────────╮
# │ # │  assistant  │ sessions │ total_messages │ total_size │
# ├───┼─────────────┼──────────┼────────────────┼────────────┤
# │ 0 │ claude-code │        7 │             25 │     2.8 kB │
# │ 1 │ codex       │        1 │           1746 │   993.7 kB │
# ╰───┴─────────────┴──────────┴────────────────┴────────────╯
```

## Understanding the Architecture

```
┌─────────────────────────────────────────────────────────┐
│          Your Assistant Conversations                    │
├─────────────────────────────────────────────────────────┤
│                                                          │
│   ~/.codex/sessions/     ~/.local/share/goose/   claude │
│   *.jsonl                sessions.db              (live) │
│                                                          │
└──────────┬────────────────────┬─────────────────┬───────┘
           │                    │                 │
           │ continuum import   │ continuum       │ continuum-claude
           │ (compress+write)   │ import          │ (wrapper)
           ▼                    ▼                 ▼
┌──────────────────────────────────────────────────────────┐
│        Plain-Text JSONL Files                            │
│        ~/continuum-logs/assistant/date/session/          │
├──────────────────────────────────────────────────────────┤
│                                                          │
│  session.json         messages.jsonl                    │
│  ┌────────────┐      ┌────────────────┐                │
│  │ metadata   │      │ {"id":1,...}   │                │
│  │ timestamps │      │ {"id":2,...}   │                │
│  │ count      │      │ {"id":3,...}   │                │
│  └────────────┘      └────────────────┘                │
│                                                          │
└────────┬─────────────────────────────────────────────────┘
         │
         │ ripgrep / Nushell / any Unix tool
         ▼
    Your unified
    search results
```

**Key insight**: Continuum doesn't replace your assistants' storage - it **normalizes** it to plain-text JSONL for universal search.

## File Structure

```
~/continuum-logs/
├── claude-code/
│   └── 2025-11-09/
│       └── session-abc123/
│           ├── session.json      # Metadata
│           └── messages.jsonl    # One message per line
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

Each `messages.jsonl` file contains:
```jsonl
{"id":1,"role":"user","content":"How do I...","timestamp":"2025-11-09T14:30:01Z"}
{"id":2,"role":"assistant","content":"You can...","timestamp":"2025-11-09T14:30:15Z"}
```

## Common Use Cases

### "What did any of my assistants say about error handling?"

```nushell
continuum-search "error handling" --limit 20
```

### "Show me yesterday's conversations"

```nushell
continuum-timeline "2025-11-08"
```

### "Get context from a specific session"

```nushell
# First find the session
continuum-search "topic you remember"

# Then get full context
continuum-context "session-id" --limit 100
```

### "Export a session to markdown"

```nushell
continuum-export-md "session-id" ~/Documents/conversation.md
```

## Using Standard Unix Tools

Since logs are plain text, you can use any tool:

```bash
# Search with ripgrep
rg "API design" ~/continuum-logs

# Count conversations per day
ls ~/continuum-logs/claude-code/2025-11-09/ | wc -l

# View latest conversation
cat ~/continuum-logs/claude-code/2025-11-09/*/messages.jsonl | tail -20

# Extract all user questions
rg '"role":"user"' ~/continuum-logs --json | lines | from json | where type == "match"

# Find when you discussed a topic
git -C ~/continuum-logs log -S "database design" --oneline
```

## Tips & Tricks

1. **Always import or use wrappers**: Data only gets into `~/continuum-logs/` when you import it or use `continuum-claude` wrapper

2. **Search is fast**: ripgrep is very fast even with thousands of messages

3. **Compression saves tokens**: Import applies noise filtering for ~20% token reduction

4. **Git integration**: Track conversation history with git:
   ```bash
   cd ~/continuum-logs && git init && git add .
   ```

5. **Custom queries**: Write your own Nushell functions in `~/dotfiles/nushell/continuum.nu`

6. **Works with semantic search**: Your existing semantic search tools work on these files

## Nushell Functions Reference

All query functions are in `~/dotfiles/nushell/continuum.nu`:

- `continuum-search "query"` - Full-text search with ripgrep
- `continuum-timeline [date]` - Browse conversations by date
- `continuum-stats` - Show statistics by assistant
- `continuum-context "session"` - Get recent messages
- `continuum-export-md "session" "file.md"` - Export to markdown

## Next Steps

- Read [USAGE.md](USAGE.md) for detailed function reference
- Read [ARCHITECTURE.md](ARCHITECTURE.md) for technical deep-dive
- Read [PLAIN-TEXT-ARCHITECTURE.md](PLAIN-TEXT-ARCHITECTURE.md) for design philosophy
- Explore Nushell functions in `~/dotfiles/nushell/continuum.nu`

## Troubleshooting

**"No conversations found"**: Did you import sessions first?
```bash
continuum import --assistant codex
continuum import --assistant goose
```

**"Nushell functions not found"**: Source the continuum.nu file:
```nushell
source ~/dotfiles/nushell/continuum.nu
```

**"continuum-claude command not found"**: Make sure you created the symlink:
```bash
ln -s ~/Assistants/projects/continuum/target/release/continuum-claude ~/.local/bin/
```

**"Goose database not found"**: Goose hasn't created its database yet - use Goose at least once first

**Check if logs exist**:
```bash
ls ~/continuum-logs
```

## Backup

```bash
# Simple rsync backup
rsync -av ~/continuum-logs/ /backup/continuum-logs/

# Compressed archive
tar -czf continuum-backup-$(date +%Y-%m-%d).tar.gz ~/continuum-logs

# Git-based backup
cd ~/continuum-logs
git init
git add .
git commit -m "Conversations from $(date +%Y-%m-%d)"
git remote add origin <your-backup-repo>
git push -u origin main
```

## What Works Now

✅ **Unified search across all assistants** (ripgrep + Nushell)
✅ **Plain-text storage** (JSONL files)
✅ **Automatic capture** (continuum-claude wrapper)
✅ **Manual import** (Codex, Goose)
✅ **Timeline browsing** (by date/range)
✅ **Statistics** (by assistant)
✅ **Export to markdown**
✅ **Universal tool compatibility** (rg, fd, awk, sed, jq)
✅ **Git integration** (diff, log, track changes)
✅ **Noise compression** (~20% token reduction)

The core functionality is complete: **plain-text conversation storage with universal search**.
