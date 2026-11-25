# Continuum

**Cross-platform, vendor-neutral, unified logging for AI assistant conversations**

Continuum is a terminal-native system for archiving and searching conversations across multiple AI assistants (ChatGPT, Claude, Claude Code, Gemini, Grok, Goose, Codex). It stores everything as plain-text JSONL files organized by vendor and date, searchable with standard Unix tools (fd, rg, sk) and Nushell.

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
- Imports from multiple assistants (ChatGPT, Claude.ai, Grok) to unified storage
- Provides MCP server for searching YOUR conversation history (not web/docs)
- Uses agentic RAG (LLM decides when to search, not always-on)
- Stores as plain-text JSONL (greppable, future-proof, vendor-neutral)
- Built for terminal users (Rust + Nushell + modern CLI tools)

It fills a gap, but it's a small gap for a specific audience.

## What You Get

- **Plain-text archive**: JSONL files, no database, works with any tool
- **Cross-assistant search**: Claude Code can search your ChatGPT/Grok/Gemini history
- **Dual organization**: Per-vendor directories + per-date subdirectories
- **MCP integration**: Agentic RAG for selective, intelligent retrieval
- **Production importers**: Working tools for ChatGPT, Claude.ai, and Grok exports
- **Browser extensions**: One-click conversation export from ChatGPT, Grok, and Gemini
- **Activity reporting**: Daily AI activity extraction with skill tagging and cross-session context loading
- **Terminal-native**: Built for fd, rg, sk, Nushell workflows

## Architecture Overview

Continuum has three layers:

### 1. Capture Layer (this repo)
Wrapper binaries that intercept CLI assistant sessions and log them automatically:
- `continuum-claude` - Wraps Claude Code
- `continuum-codex` - Wraps Codex
- `continuum-goose` - Wraps Goose
- `continuum-gemini` - Wraps Gemini CLI

### 2. Import Layer ([dotfiles](https://github.com/willnapier/dotfiles))
Tools for importing conversations from web exports and browser extensions:
- `chatgpt-to-continuum` - Official OpenAI export + browser extension format
- `claude-to-continuum` - Claude.ai export
- `grok-to-continuum` - Official Grok export
- Browser extensions for ChatGPT, Grok, and Gemini (one-click DOM export)

### 3. Query Layer ([dotfiles](https://github.com/willnapier/dotfiles) + [continuum-mcp](https://github.com/willnapier/continuum))
- `continuum-activity` - Daily activity reports, session loading, skill filtering, backfill
- `continuum-mcp` - MCP server for cross-assistant semantic search
- Nushell functions - `continuum-search`, `continuum-timeline`, `continuum-stats`

## MCP Integration

Continuum provides intelligent conversation search to all your AI assistants via MCP.

Your AI assistants can **automatically search past conversations** to provide better, more informed responses:

- **Claude Code** can find relevant discussions from **ChatGPT, Grok, Goose, or Codex**
- **Goose** can reference previous work from **any assistant**
- **Unified memory** across all AI interactions
- **Selective search** - only queries when context matters (cost-effective!)

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

# Build and install wrappers under their continuum-* names only
cargo build --release
cp target/release/continuum ~/.local/bin/
cp target/release/continuum-claude ~/.local/bin/
cp target/release/continuum-codex ~/.local/bin/
cp target/release/continuum-goose ~/.local/bin/
cp target/release/continuum-gemini ~/.local/bin/

# DO NOT `ln -sf continuum-codex ~/.local/bin/codex` (or the same for claude/goose).
# That shadows the real CLI on PATH and used to re-spawn the wrapper forever.
# Wire capture via shell functions/aliases instead (nushell already has
# `def --wrapped codex` → continuum-codex. Install the real OpenAI CLI in
# Continuum's managed, off-PATH prefix with `continuum codex update`.

# Source Nushell functions
echo "source ~/continuum/continuum.nu" >> ~/.config/nushell/config.nu
```

Your conversations are now **automatically saved** to `~/Assistants/continuum-logs/` every time you use any assistant!

## Usage

### Configuration

Everything has a default; nothing is hard-wired to one machine or user.

| Setting | Default | Purpose |
|---------|---------|---------|
| `CONTINUUM_HOME` | `~/Assistants` | Root under which `continuum-logs/` and `continuum-usage/` live |
| `USAGEWATCH_STORE` | `$CONTINUUM_HOME/continuum-usage/v2` | Usage observation store (override for scratch runs) |
| `CONTINUUM_MACHINE_ID` | hostname | Machine identity used to namespace per-machine files |
| `CONTINUUM_ANTHROPIC_API_KEY` | unset | Metered Anthropic key for `usage-probe-anthropic-api` (containers, CI) |
| `CONTINUUM_ANTHROPIC_API_CONFIG` | `~/.config/continuum/anthropic-api.toml` | TOML with `api_key = "…"` **or** `key_file = "~/other/app/secrets.toml"` (whose top-level or `[ai]` `api_key` is used, so a key shared with another application is referenced, not copied), plus optional `assistant` / `account` labels |

`usage-probe-anthropic-api --check-key` resolves that configuration and reports where the key came from without calling the API.

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

### Usage monitoring (`usagewatch`)

Cross-provider usage, scarcity, perishability, and notification monitoring is provided by `usagewatch` and standalone `usage-probe-*` executables. The original Codex-only `continuum usage` command, wrapper banner, and scheduler have been retired. See `~/Assistants/shared/USAGEWATCH.md` for the current commands, schema, policy, and deployment model.

### Managed Codex CLI

Continuum installs the real OpenAI Codex CLI outside `PATH`, under `${XDG_DATA_HOME:-~/.local/share}/continuum/codex`. The capture wrapper and allocation probe share one resolver, so they cannot silently select different Codex versions.

```bash
# Install or update the managed CLI without writing to /usr
continuum codex update

# Show the selected binary, source, and canonical managed target
continuum codex which
```

Resolver precedence is: `CONTINUUM_CODEX_BIN` override, managed install, legacy user install, identity-filtered `PATH`, then system paths as a warned compatibility fallback. The wrapper sets a depth guard before launching the real CLI, turning a copied or misnamed wrapper into a clear error instead of recursive spawning. Codex's built-in updater targets the ambient npm installation rather than Continuum's off-PATH copy, so the deployed Codex configuration sets `check_for_update_on_startup = false`; use `continuum codex update` instead.

On Linux, `continuum-claude`, `continuum-codex`, `continuum-gemini`, `continuum-goose` and the usage probes that start a harness run it through the mask launcher, `ai-masked <harness> <arguments>`, which hides the protected folders from the harness. Without `ai-masked` they start nothing and say so; they never start the harness directly instead. Install it with `rust-redeploy --only ai-masked`. On macOS the harness is still started directly for now. See "Wrap" in [ARCHITECTURE.md](ARCHITECTURE.md) and `~/Assistants/shared/AI-FOOTING-MASK.md`.

A no-save request (`nosave`, the marker `~/.continuum-nosave`) is used up only by a session that started (on macOS too). A wrapper reserves the marker, and gives it back when the launcher cannot be run, refuses (exit 113) or cannot run the harness (126, 127), and when a signal ends its child without the wrapper having seen a session, because a signal may have ended the launcher before the harness started. In those cases the wrapper does no import or deletion either. A harness that itself ends in one of those ways after a real session the wrapper did not see also gets its request back, so the next session is not saved either: the mistake is made towards not saving. The table is under "Wrap" in [ARCHITECTURE.md](ARCHITECTURE.md).

A no-save Codex session started through `continuum-codex` stays out of Continuum when its hooks do not run, for a new session and for `codex resume <uuid>`. A no-save `codex resume` must name the session's id right after `resume`, followed only by prompt words (`codex resume <uuid> [prompt]`), and is marked no-save before Codex starts; any other form is refused and the request kept. While a no-save Codex start is open, no Codex rollout begun after it is stored, by the timer or anything else, and when it closes those rollouts are marked no-save and any stored copy removed. So any other Codex session that writes a rollout and begins while a no-save Codex session is open is not saved either; forum rounds are not affected, as they run `codex exec --ephemeral`, which writes no rollout. If the wrapper was killed, that lasts until a close succeeds after its Codex ended (normally the next five-minute timer import, later if imports fail). Still relying on the hooks: a session chosen inside a running no-save Codex, a rollout a Codex app-server creates only after Codex has ended, and rollouts outside `CODEX_HOME` and `~/.codex`. See "What keeps a no-save session out of Continuum" in [ARCHITECTURE.md](ARCHITECTURE.md).

Every close is logged in `~/.local/share/continuum/codex-nosave-closes.jsonl` (per machine, outside the synced archive): one JSON line with the time, the closer (`wrapper` or `import`), the start record's file name, each rollout marked no-save by file stem, each stored copy removed by path, and the step that failed, if one did; names and paths only. An import that closes a start also prints one line with the counts (`= Closed no-save Codex start …: marked N Codex session(s) no-save, removed M stored copy(ies)`), which the timer keeps in `~/.local/share/continuum/auto-import.log`. A log that cannot be written stops neither the marking nor the removal: the wrapper says so on its terminal, and an import exits non-zero, so the timer raises its alert. So the log is best effort: a wrapper whose terminal is gone reports a failed write to nobody, and a closer killed between removing the start and writing its line leaves a close with no line. Appends are made under a lock on the log; a line cut short is ended by the next entry, and a reader skips a line that is not JSON. `continuum codex nosave-starts [--older-than-hours 6]` reports start records on record longer than that, by count and age only, and touches none (exit 0 none, 1 some, 3 could not check); `system-health-check` runs it on nimbini as a finding. To undo one removal: delete the session's marker in `~/.local/share/continuum/nosave-sessions/` (its rollout stem, and its thread id if that is there too), then `continuum import -a codex -s <rollout file>`.

### Import Web Conversations

```bash
# Import from official platform exports
chatgpt-to-continuum ~/Downloads/conversations.json
claude-to-continuum ~/Downloads/conversations.json
grok-to-continuum ~/Downloads/grok-export.json

# Import from browser extension exports (same tool handles both formats)
chatgpt-to-continuum ~/Downloads/ChatGPT-MyConversation.json
```

### Activity Reporting

`continuum-activity` extracts structured activity reports from all logged sessions:

```bash
# Today's activity (markdown)
continuum-activity

# Specific date
continuum-activity 2026-02-07

# JSON output
continuum-activity --json

# Full user messages, not truncated
continuum-activity --verbose

# CC sessions only (skip Continuum archive)
continuum-activity --cc-only
```

### Cross-Session Context Loading

Load previous conversations as context for new AI sessions:

```bash
# Search and interactively pick sessions
continuum-activity load --search "Friston"

# Load most recent session
continuum-activity load --last

# Filter by assistant
continuum-activity load --last --assistant claude-code

# Filter by skill (e.g., sessions using the senior-dev persona)
continuum-activity load --skill senior-dev

# Pipe into another assistant for cross-pollination
continuum-activity load --search "API design" | claude -p "Continue this discussion."
```

### Session Maintenance

```bash
# Deduplicate messages across all sessions
continuum-activity clean --dry-run
continuum-activity clean

# Backfill skill metadata into existing sessions
continuum-activity backfill --dry-run
continuum-activity backfill
```

### Search Conversations

```nushell
# Full-text search
continuum-search "async programming"

# Browse by date
continuum-timeline 2025-11-09

# Get statistics
continuum-stats
```

### Use Any Unix Tool

```bash
# Search with ripgrep
rg "machine learning" ~/Assistants/continuum-logs

# Find when you discussed a topic
rg -l "API design" ~/Assistants/continuum-logs --type jsonl
```

## File Structure

```
~/Assistants/continuum-logs/
├── claude-code/2025-11-09/session-abc123/
│   ├── session.json      # Metadata (incl. skills, title)
│   └── messages.jsonl    # One message per line
├── chatgpt/2025-11-09/session-xyz789/
├── grok/2025-11-09/session-123/
├── gemini-cli/2025-11-09/session-456/
└── codex/2025-11-09/session-789/
```

Each `session.json` contains:

```json
{
  "id": "abc123",
  "assistant": "claude-code",
  "start_time": "2025-11-09T14:30:01Z",
  "end_time": "2025-11-09T15:45:00Z",
  "message_count": 42,
  "title": "API Design Discussion",
  "skills": ["senior-dev"]
}
```

Each `messages.jsonl` file contains:

```jsonl
{"id":1,"role":"user","content":"How do I...","timestamp":"2025-11-09T14:30:01Z"}
{"id":2,"role":"assistant","content":"You can...","timestamp":"2025-11-09T14:30:15Z"}
```

## Skills System

Sessions can be tagged with **skills** — named personas or modes used during the conversation (e.g., `senior-dev`, `philosophy-tutor`, `clinical-notes`).

Skills are populated automatically:
- **CC sessions**: Extracted from Skill tool_use blocks in JSONL logs
- **Imported sessions**: Matched against skill aliases (title/project name mapping)
- **Retroactively**: Via `continuum-activity backfill`

Skill aliases are configured in `~/.config/continuum/skill-aliases.json`:

```json
{
  "Geoff": "philosophy-tutor",
  "Diana": "diana",
  "Senior Dev": "senior-dev",
  "Seneca": "life-optimiser"
}
```

Filter sessions by skill:

```bash
continuum-activity load --skill senior-dev
continuum-activity load --skill philosophy-tutor --search "pragmatism"
```

## Browser Extensions

Three browser extensions for one-click conversation export (compatible with the importers):

| Extension | Platform | Output |
|-----------|----------|--------|
| **ChatGPT Exporter** | chatgpt.com | JSON (Exporter format) |
| **Grok Exporter** | grok.com | JSON (Exporter format) |
| **Gemini Exporter** | gemini.google.com | JSON (Exporter format) |

Each extension adds an export button to the conversation page. The exported JSON includes metadata, messages, title, and project/folder context. Import with `chatgpt-to-continuum` (handles all Exporter-format JSON).

Extensions are in the [dotfiles repo](https://github.com/willnapier/dotfiles) under `browser-extensions/`.

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

Fully cross-platform - Works identically on:
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

- **Automatic Capture**: Claude Code, Codex, Goose, Gemini CLI
- **Quality Control**: Preemptive (nosave) + Post-conversation review
- **MCP Server**: Production (continuum-mcp)
- **Web Importers**: ChatGPT, Claude.ai, Grok (handles rich content)
- **Browser Extensions**: ChatGPT, Grok, Gemini (DOM-based export)
- **Activity Reporting**: Daily reports, session loading, skill filtering
- **Session Maintenance**: Deduplication, skill backfill
- **Documentation**: Complete
- **Testing**: Works for me, YMMV

**Known limitations**:
- No GUI (by design)
- Requires technical setup (wrapping assistant binaries)
- MCP configuration varies by assistant
- Import tools need official exports or browser extensions (no scraping)
- Claude Code retains full sessions for about 30 days

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
| [dotfiles](https://github.com/willnapier/dotfiles) | Importers, browser extensions, continuum-activity, and system configuration |
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
