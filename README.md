# Continuum

**Cross-platform, vendor-neutral, unified logging for AI assistant conversations**

Continuum is a terminal-native system for archiving and searching conversations across multiple AI assistants (ChatGPT, Claude, Claude Code, Goose, Codex). It stores everything as plain-text JSONL files organized by vendor and date, searchable with standard Unix tools (fd, rg, sk) and Nushell.

## Who This Is For

This is a **niche tool** for people who:
- Use multiple AI assistants (web and CLI)
- Want vendor-neutral conversation archives
- Prefer terminal tools over web interfaces
- Value plain-text over databases
- Want cross-assistant conversation search via MCP

If you just use one AI assistant or prefer GUI tools, this probably isn't for you.

## What Makes This Different

Most conversation export/import tools either:
- Extract from one platform (browser extensions)
- Import into proprietary systems (Obsidian plugins)
- Focus on web crawling/documents (other MCP+RAG projects)

Continuum is the only complete system (that I could find) that:
- ✅ Imports from multiple assistants (ChatGPT, Claude.ai) to unified storage
- ✅ Provides MCP server for searching YOUR conversation history (not web/docs)
- ✅ Uses agentic RAG (LLM decides when to search, not always-on)
- ✅ Stores as plain-text JSONL (greppable, future-proof, vendor-neutral)
- ✅ Built for terminal users (Rust + Nushell + modern CLI tools)

It fills a gap, but it's a small gap for a specific audience.

## What You Get

- **Plain-text archive**: JSONL files, no database, works with any tool
- **Cross-assistant search**: Claude Code can search your ChatGPT history
- **Dual organization**: Per-vendor directories + per-date subdirectories
- **MCP integration**: Agentic RAG for selective, intelligent retrieval
- **Production importers**: Working tools for ChatGPT and Claude.ai exports
- **Terminal-native**: Built for fd, rg, sk, Nushell workflows

## 🌟 NEW: MCP Integration & Cross-Assistant Memory (2025-11-22)

**Continuum now provides intelligent conversation search to ALL your AI assistants via MCP!**

### What This Means

Your AI assistants can now **automatically search past conversations** to provide better, more informed responses:

- ✅ **Claude Code** can find relevant discussions from **ChatGPT, Goose, or Codex**
- ✅ **Goose** can reference previous work from **any assistant**
- ✅ **Unified memory** across all AI interactions
- ✅ **Selective search** - only queries when context matters (cost-effective!)

### Already Configured For

- **Claude Desktop (Claude Code)** - Ready to use
- **Goose** - Ready to use
- **Codex (OpenAI)** - Ready to use

### Import Your Existing Conversations

```bash
# Import your ChatGPT history
chatgpt-to-continuum ~/Downloads/conversations.json

# Import your Claude.ai history
claude-to-continuum ~/claude-export/conversations.json

# Now searchable alongside native continuum logs!
# Combined: 2,598 conversations from 2023-2025
```

### How It Works

Instead of always-on indexing (expensive!), the LLM **decides when to search**:

```
You: "What did we discuss about Rust last week?"
LLM: *searches continuum logs* → finds relevant conversations → informed answer

You: "What's 2+2?"
LLM: *no search needed* → instant answer
```

**See [MCP-RAG-SYSTEM.md](MCP-RAG-SYSTEM.md) for complete documentation of the RAG architecture, import tools, and rationale.**

## Quick Start

```bash
# Clone the repository
git clone https://github.com/willnapier/continuum
cd continuum

# Build and install
cargo build --release
cp target/release/continuum ~/.local/bin/
cp target/release/continuum-claude ~/.local/bin/
cp target/release/continuum-codex ~/.local/bin/
cp target/release/continuum-goose ~/.local/bin/

# Install wrappers (replaces your assistant binaries)
ln -sf ~/continuum/target/release/continuum-claude ~/.local/bin/claude
ln -sf ~/continuum/target/release/continuum-codex ~/.local/bin/codex
ln -sf ~/continuum/target/release/continuum-goose ~/.local/bin/goose

# Source Nushell functions
echo "source ~/continuum/continuum.nu" >> ~/.config/nushell/config.nu
```

Your conversations are now **automatically saved** to `~/continuum-logs/` every time you use any assistant!

## Usage

### Automatic Capture (All Assistants)

**Every conversation is automatically captured:**

```bash
# Just use your assistants normally
claude          # Conversations automatically saved
codex           # Conversations automatically saved
goose           # Conversations automatically saved
```

When you exit any session, you'll be prompted to keep or discard it.

### Quality Control

**Skip trivial conversations:**

```bash
# Preemptive skip
nosave          # Set marker
claude          # This conversation won't be saved
```

**Post-conversation review** (happens automatically after each session):
```
Save this conversation? [Y/n/r]
  Y - Save to continuum logs
  n - Discard permanently
  r - Review full conversation before deciding
```

See **[QUALITY-CONTROL.md](QUALITY-CONTROL.md)** for complete documentation.

### Search Conversations

```nushell
# Full-text search
continuum-search "async programming"

# Browse by date
continuum-timeline 2025-11-09

# Get statistics
continuum-stats
```

### Manual Import (for backlog/historical sessions)

```bash
# Import backlog of existing sessions
continuum import --assistant claude-code    # Import latest Claude Code session
continuum import --assistant codex          # Import latest Codex session
continuum import --assistant goose          # Import latest Goose session

# Import specific session
continuum import --assistant claude-code --session /path/to/session.jsonl

# Import from web exports (ChatGPT, Claude.ai, Grok)
chatgpt-to-continuum ~/Downloads/conversations.json
claude-to-continuum ~/Downloads/conversations.json
grok-to-continuum ~/Downloads/conversations.json
```

### Use Any Unix Tool

```bash
# Search with ripgrep
rg "machine learning" ~/continuum-logs

# Count today's conversations
ls ~/continuum-logs/claude-code/$(date +%Y-%m-%d)/ | wc -l

# Find when you discussed a topic
git -C ~/continuum-logs log -S "API design" --oneline
```

## File Structure

```
~/continuum-logs/
├── claude-code/2025-11-09/session-abc123/
│   ├── session.json      # Metadata
│   └── messages.jsonl    # One message per line
├── codex/2025-11-09/session-xyz789/
└── goose/2025-11-09/session-123/
```

Each `messages.jsonl` file contains:

```jsonl
{"id":1,"role":"user","content":"How do I...","timestamp":"2025-11-09T14:30:01Z"}
{"id":2,"role":"assistant","content":"You can...","timestamp":"2025-11-09T14:30:15Z"}
```

## Nushell Functions

- `continuum-search <query>` - Full-text search with ripgrep
- `continuum-timeline [date]` - Browse conversations by date
- `continuum-stats` - Show statistics by assistant
- `continuum-context <session>` - Get recent messages
- `continuum-export-md <session> <file>` - Export to markdown

## Documentation

- **[PLATFORM-COMPATIBILITY.md](PLATFORM-COMPATIBILITY.md)** - Cross-platform support (Linux, macOS, BSD)
- **[QUALITY-CONTROL.md](QUALITY-CONTROL.md)** - Quality control system (nosave, post-conversation review)
- **[MCP-RAG-SYSTEM.md](MCP-RAG-SYSTEM.md)** - MCP integration and cross-assistant memory
- **[USAGE.md](USAGE.md)** - Complete usage guide with examples
- **[PLAIN-TEXT-ARCHITECTURE.md](PLAIN-TEXT-ARCHITECTURE.md)** - Design philosophy and architecture

## Philosophy

Continuum follows the Unix philosophy:

1. **Plain text is universal**: Works with every tool ever created
2. **Composability**: Pipe data between programs freely
3. **Transparency**: No hidden state, no black boxes
4. **Future-proof**: Plain text will outlast any database

## Platform Support

✅ **Fully cross-platform** - Works identically on:
- **Linux** (all distributions - Arch, Ubuntu, Fedora, etc.)
- **macOS** (Intel & Apple Silicon)
- **BSD** (FreeBSD, OpenBSD, NetBSD)

**Pure Rust implementation** with zero platform-specific dependencies.

See **[PLATFORM-COMPATIBILITY.md](PLATFORM-COMPATIBILITY.md)** for complete platform documentation.

## Requirements

- Rust 1.70+ (for building)
- Nushell (for structured queries)
- ripgrep (for fast search)
- fd (optional, for file finding)

**Works with any window manager/compositor** (tested with Niri, GNOME, KDE, i3/Sway, etc.)

## Advanced Usage

### Git Integration

```bash
cd ~/continuum-logs
git init
git add .
git commit -m "Initial conversations"
```

Now every conversation is version controlled!

### Semantic Search

Your existing semantic search tools work directly on the JSONL files:

```bash
semantic-query "database design patterns" ~/continuum-logs
```

### Custom Queries

Add your own Nushell functions to `~/dotfiles/nushell/continuum.nu`:

```nushell
# Find all questions you asked today
def my-questions [] {
    continuum-timeline (date now | format date "%Y-%m-%d")
    | where role == "user"
    | select content
}
```

## Realistic Expectations

**This is not**:
- A breakthrough innovation (conversation logging exists)
- Going to replace your assistant's built-in history
- For non-technical users
- A mass-market tool

**This is**:
- A complete, working solution to a specific problem
- Well-architected Rust code
- A good reference implementation of agentic RAG
- Useful if you match the target audience
- Worth putting out there for the few people who need exactly this

**Expected audience**: Small but engaged. If 100 people find this useful, that's success.

## Project Status

- **Automatic Capture**: ✅ Claude Code, Codex, Goose (all three)
- **Quality Control**: ✅ Preemptive (nosave) + Post-conversation review
- **MCP Server**: ✅ Production (continuum-mcp)
- **Web Importers**: ✅ ChatGPT, Claude.ai, Grok (handles rich content)
- **Noise Filtering**: ✅ Automatic compression (~20% reduction)
- **Documentation**: ✅ Complete
- **Testing**: ⚠️  Works for me, YMMV

**Known limitations**:
- No GUI (by design)
- Requires technical setup (wrapping assistant binaries)
- MCP configuration varies by assistant
- Import tools need official exports (no scraping)
- Claude Code ~30 day retention for full sessions

## License

MIT

## Contributing

Contributions welcome! This project values:
- Simplicity over features
- Plain text over databases
- Terminal tools over GUIs
- Unix philosophy

## Related Repositories

| Repository | Description |
|------------|-------------|
| [dotfiles](https://github.com/willnapier/dotfiles) | System configuration with AI export watchers and converter tools |
| [nushell-knowledge-tools](https://github.com/willnapier/nushell-knowledge-tools) | Universal CLI functions for knowledge management |
| [helix-knowledge-integration](https://github.com/willnapier/helix-knowledge-integration) | Helix editor integration for the knowledge tools |

## See Also

- [Nushell](https://www.nushell.sh/) - Modern shell with structured data
- [ripgrep](https://github.com/BurntSushi/ripgrep) - Fast text search
- [fd](https://github.com/sharkdp/fd) - Fast file finder
- [Model Context Protocol](https://modelcontextprotocol.io/) - MCP specification

---

**Made with plain text and Unix philosophy**
*A niche tool for terminal users who use multiple AI assistants*
