# Continuum MCP & RAG System

**Status**: ✅ Production - Deployed 2025-11-22
**Architecture**: Cross-platform, vendor-neutral, unified logging system with Agentic RAG

## The Vision

For 2+ years, the goal has been: **AI assistants that can search and learn from all past conversations across all platforms.**

This system makes that vision real:
- Claude Code asks about Rust → searches ChatGPT, Goose, and Codex conversations
- Goose references previous work → finds relevant discussions from any assistant
- Unified memory across all AI interactions

## What is RAG (Retrieval-Augmented Generation)?

**The Pattern**: Retrieval-Augmented Generation enhances LLM responses by retrieving relevant information from external sources before generating answers.

**Continuum uses Agentic RAG**, a more sophisticated variant:

### Traditional RAG vs Agentic RAG

**Traditional RAG** (always-on):
- Every query triggers automatic retrieval
- Predetermined when retrieval happens
- Higher cost (searches even when unnecessary)
- Simpler implementation

**Agentic RAG** (what Continuum implements):
- LLM decides when to retrieve via tool calls
- Selective, intelligent context gathering
- Cost-effective (only searches when beneficial)
- More flexible and powerful

### How Continuum's Agentic RAG Works

1. User sends prompt → "What did we discuss about Rust projects?"
2. LLM recognizes need for historical context
3. LLM calls `search_conversations` or `semantic_search_conversations` MCP tool
4. Tool searches `~/continuum-logs/` across all assistants
5. LLM uses retrieved context to inform response

**For simple queries** ("What's 2+2?"), the LLM skips retrieval entirely - instant answer, zero search cost.

**For contextual queries**, the LLM retrieves exactly what it needs when it needs it.

### Why Agentic RAG is Superior

- ✅ **Cost-effective** - Only searches when the LLM judges it beneficial
- ✅ **Faster** - Simple questions get instant answers without search overhead
- ✅ **Smarter** - LLM intelligence determines when context matters
- ✅ **Flexible** - Can retrieve from multiple tools and sources
- ✅ **Powerful** - Full conversation history available on demand

## Architecture

### Continuum MCP Server

**Repository**: `~/continuum-mcp/`
**Binary**: `~/.local/bin/continuum-mcp`
**Protocol**: Model Context Protocol (MCP)
**Language**: Rust (rmcp 0.8)

**What It Does**: Exposes conversation search tools to MCP-compatible AI assistants

**Available Tools**:

1. **`search_conversations`** - Fast keyword search using ripgrep
   - Filters: assistant, date, query
   - Max results configurable
   - Returns matching messages with previews

2. **`semantic_search_conversations`** - AI-powered semantic search
   - Uses OpenAI embeddings (requires `OPENAI_API_KEY`)
   - Finds conceptually related conversations
   - PREFERRED for exploring topics

3. **`get_timeline`** - Browse conversations chronologically
   - Filter by date and/or assistant
   - Defaults to today's date
   - Returns ordered message list

4. **`get_conversation`** - Retrieve full conversation by ID
   - Get complete session with metadata
   - Useful for deep dives

**Data Source**: Plain-text JSONL files at `~/continuum-logs/`

### Configuration - All Assistants Enabled

#### Claude Desktop (Claude Code)
**Config**: `~/Library/Application Support/Claude/claude_desktop_config.json`
```json
{
  "mcpServers": {
    "continuum": {
      "command": "~/.local/bin/continuum-mcp"
    }
  }
}
```

#### Goose
**Config**: `~/.config/goose/config.yaml`
```yaml
extensions:
  continuum:
    name: Continuum MCP
    cmd: ~/.local/bin/continuum-mcp
    args: []
    enabled: true
    type: stdio
    timeout: 300
```

#### Codex (OpenAI)
**Configured via CLI**:
```bash
codex mcp add continuum -- ~/.local/bin/continuum-mcp
codex mcp list  # Verify: Status=enabled
```

**Activation**: Restart assistant applications for changes to take effect

## Import Tools - Unifying All AI Conversations

### ChatGPT Importer ✅

**Tool**: `chatgpt-to-continuum`
**Repository**: `~/dotfiles/rust-projects/chatgpt-to-continuum/`
**Binary**: `~/.local/bin/chatgpt-to-continuum`
**Status**: Production-ready

**Export Process**:
1. Visit https://chatgpt.com → Settings → Data controls → Export data
2. Receive email with download link (expires in 24 hours)
3. Download and extract `conversations.json`

**Import Usage**:
```bash
chatgpt-to-continuum ~/Downloads/conversations.json

# Output: ~/continuum-logs/chatgpt/YYYY-MM-DD/session-id/
#   ├── messages.jsonl
#   └── session.json
```

**Technical Implementation**:
- Parses ChatGPT's tree-based conversation structure
- Walks node mapping to extract chronological message order
- Handles conversation branching (regenerated responses)
- Converts to continuum's flat JSONL format
- Preserves all metadata (timestamps, roles, IDs)
- **Rich Content Handling**: Processes audio, images, and video as placeholder text
  - Text content: Imported verbatim
  - Rich media objects: Converted to `[content_type]` placeholders (e.g., `[real_time_user_audio_video_asset_pointer]`)
  - Uses `serde_json::Value` for flexible content parsing

**ChatGPT Export Format**:
```json
{
  "title": "Conversation title",
  "create_time": 1234567890.0,
  "mapping": {
    "node-id": {
      "message": {
        "author": {"role": "user|assistant|system"},
        "content": {"parts": ["message text"]},
        "create_time": 1234567890.0
      },
      "parent": "parent-node-id",
      "children": ["child-node-id"]
    }
  }
}
```

### Claude.ai Importer ✅

**Tool**: `claude-to-continuum`
**Repository**: `~/dotfiles/rust-projects/claude-to-continuum/`
**Binary**: `~/.local/bin/claude-to-continuum`
**Status**: Production-ready

**Export Process**:
1. Visit https://claude.ai → Settings → Privacy & Data → Export data
2. Receive email with download link (expires in 24 hours)
3. Download and extract ZIP file containing `conversations.json`

**Import Usage**:
```bash
claude-to-continuum ~/claude-export/conversations.json

# Output: ~/continuum-logs/claude/YYYY-MM-DD/uuid/
#   ├── messages.jsonl
#   └── session.json
```

**Technical Implementation**:
- Parses Claude.ai's flat conversation structure (simpler than ChatGPT's tree)
- Converts sender roles: "human" → "user", "assistant" → "assistant"
- Skips empty messages automatically
- Preserves conversation names and UUIDs
- Converts to continuum's JSONL format
- Date-based organization by conversation creation time

**Claude.ai Export Format**:
```json
{
  "uuid": "eec4b395-13bb-41ce-a102-0426612abae9",
  "name": "Conversation Title",
  "created_at": "2025-11-03T10:58:33.335146Z",
  "updated_at": "2025-11-03T10:58:48.359385Z",
  "chat_messages": [
    {
      "uuid": "7d561028-e32e-4b1d-9514-50e1c094ccdc",
      "text": "Message content",
      "sender": "human",
      "created_at": "2025-11-03T10:58:34.641406Z"
    }
  ]
}
```

### Grok Importer (Planned)

**Export Method**: Official export via account settings at Grok.com
**Format**: JSON in ZIP file
**Status**: Feasible but requires format inspection
**Third-party alternatives**: Multiple browser extensions available

**Priority**: Medium (smaller user base, official export exists)

### Gemini Importer (Deferred)

**Export Method**: None (no official export)
**Alternatives**:
- Browser extensions (fragile, breaks with UI changes)
- Gemini CLI experimental `/export` command
- Individual response exports to Google Docs

**Status**: Low priority - no reliable export method
**Decision**: Skip until Google provides official export

## Storage Format - Plain Text JSONL

All conversations (native and imported) use identical format with **dual organization**:
- **Per-vendor**: Separate directories for each assistant (chatgpt/, claude/, claude-code/, goose/, codex/)
- **Per-date**: Within each vendor, organized by YYYY-MM-DD subdirectories

This dual structure enables both vendor-specific queries and unified cross-platform timeline searches.

### Directory Structure
```
~/continuum-logs/
  claude-code/        # Native continuum logging
    2025-11-22/
      session-abc123/
        messages.jsonl
        session.json

  claude/            # Imported from Claude.ai
    2025-11-03/
      uuid-abc123/
        messages.jsonl
        session.json

  chatgpt/           # Imported from ChatGPT
    2025-11-15/
      conv-xyz789/
        messages.jsonl
        session.json

  codex/             # Native continuum logging
  goose/             # Native continuum logging
```

### messages.jsonl Format
One JSON object per line:
```json
{"id":1,"role":"user","content":"Hello","timestamp":"2025-11-22T10:00:00Z"}
{"id":2,"role":"assistant","content":"Hi there!","timestamp":"2025-11-22T10:00:05Z"}
```

### session.json Format
```json
{
  "id": "session-abc123",
  "assistant": "chatgpt",
  "start_time": "2025-11-22T10:00:00Z",
  "end_time": "2025-11-22T10:30:00Z",
  "status": "imported",
  "message_count": 24,
  "created_at": "2025-11-22T10:00:00Z"
}
```

## Usage Examples

### From Any Assistant

**Explicit search**:
```
"Search my past conversations about Rust projects"
"What have I discussed with assistants about MCP?"
"Look up our previous work on continuum"
```

**Proactive assistance** (LLM decides to search):
```
"How should I configure my Rust project?"
→ LLM may search for previous Rust configuration discussions

"Continue working on that feature"
→ LLM may search recent conversations for context
```

### Force Search When Needed
```
"Can you check my conversation history for that?"
"Search for what we discussed about X last week"
```

## Benefits - The Complete Picture

### Cross-Assistant Memory
- **Before**: Each assistant had isolated memory
- **After**: All assistants share unified conversation history
- **Impact**: Goose can learn from Claude Code sessions, ChatGPT history informs Codex responses

### Cost Efficiency
- **Selective search**: Only queries when context is relevant
- **Smart decisions**: LLM judges when search adds value
- **Semantic search**: Uses OpenAI API only for conceptual queries (optional)

### Plain-Text Philosophy
- **Greppable**: Use ripgrep for fast keyword search
- **Transparent**: Human-readable JSONL format
- **Git-trackable**: Plain text enables version control
- **No lock-in**: Import/export without proprietary formats

### Unix Composability
- **Separate programs**: continuum-mcp, chatgpt-to-continuum, claude-to-continuum, forge-mcp
- **Clear interfaces**: MCP protocol for tool exposure
- **Standard formats**: JSONL for data storage
- **Reusable components**: Search logic works anywhere

### Terminal Tool Integration
The plain-text JSONL format enables direct searching with modern Rust-based tools:

**File Discovery** (fd):
```bash
fd -e jsonl . ~/continuum-logs/  # Find all message files
fd "2025-11-" ~/continuum-logs/  # Find today's conversations
```

**Content Search** (rg):
```bash
rg "Rust projects" ~/continuum-logs/           # Keyword search
rg -C 3 "MCP" ~/continuum-logs/chatgpt/        # Search with context
rg --json "machine learning" | from json       # Structured output for Nushell
```

**Interactive Selection** (sk):
```bash
fd -e jsonl | sk | xargs cat                   # Fuzzy find and view
rg -l "database" | sk | xargs bat              # Search, select, display
```

**Nushell Commands**:
```nushell
ls ~/continuum-logs/**/2025-11-22/ | get name  # All conversations from today
open messages.jsonl | from json | where role == "user"  # Extract user messages
glob ~/continuum-logs/**/*.jsonl | each { |f| open $f | from json }  # Load all
```

## Technical Details

### MCP Server Implementation

**Core Library** (`~/continuum-mcp/src/lib.rs`):
- `find_sessions()` - Locate session directories with filters
- `read_messages()` - Parse JSONL message files
- `search_conversations()` - Keyword search via ripgrep
- `semantic_search_conversations()` - AI semantic search via `semantic-query`
- `get_timeline()` - Chronological message browsing

**Server** (`~/continuum-mcp/src/main.rs`):
- Exposes 4 tools via MCP protocol
- Handles parameter parsing and validation
- Returns results as JSON
- Logs to stderr (stdout reserved for MCP)

**Dependencies**:
- `rmcp` 0.8 - MCP server framework
- `serde`/`serde_json` - JSON handling
- `anyhow` - Error handling
- `chrono` - Timestamp parsing
- `walkdir` - Directory traversal

### Import Tool Implementation

**ChatGPT Importer** (`~/dotfiles/rust-projects/chatgpt-to-continuum/`):
- Tree walking algorithm for conversation reconstruction
- Handles branching (regenerated responses)
- Timestamp conversion (Unix epoch → RFC3339)
- Progress reporting (every 100 conversations)
- Error resilience (continues on individual failures)

**Output**:
```
Found 247 conversations
Processed 100 conversations...
Processed 200 conversations...

Import complete!
  Success: 245
  Errors:  2
  Output:  ~/continuum-logs/chatgpt
```

## Future Enhancements

### Additional Importers
- [x] Claude.ai importer (✅ Complete - 2025-11-22)
- [x] ChatGPT importer (✅ Complete - handles rich content)
- [ ] Grok importer (medium priority)
- [ ] Gemini importer (low priority - awaiting official export)

### MCP Server Features
- [ ] Full-text indexing for faster search
- [ ] Advanced filtering (by model, by topic)
- [ ] Conversation summarization
- [ ] Export conversations to various formats

### Semantic Search Improvements
- [ ] Local embedding models (eliminate OpenAI dependency)
- [ ] Vector database integration (faster semantic search)
- [ ] Hybrid search (combine keyword + semantic)

### Web Interface (Optional)
- [ ] Browse conversations via web UI
- [ ] Visual timeline view
- [ ] Advanced search interface
- [ ] Conversation analytics

## Rationale - Why This Approach?

### Why MCP Instead of Direct Integration?
- **Universal compatibility**: Works with any MCP-compatible assistant
- **Clean separation**: Search logic separate from assistant code
- **Easy updates**: Improve search without touching assistant configs
- **Composability**: Multiple MCP servers can coexist (forge-mcp, continuum-mcp)

### Why Plain Text Instead of Database?
- **Transparency**: Human-readable, inspectable, debuggable
- **Git-friendly**: Version control for conversation history
- **No dependencies**: No database setup, migrations, or corruption
- **Tool compatibility**: Works with grep, ripgrep, Nushell, semantic-query

### Why Selective Search Instead of Automatic?
- **Cost**: Semantic search uses API credits
- **Speed**: Simple questions get instant answers
- **Intelligence**: LLM better judges relevance than always-on indexing
- **Flexibility**: User can force search when needed

### Why Separate Importers Instead of Unified Tool?
- **Unix philosophy**: Do one thing well
- **Maintenance**: Easier to fix one exporter when format changes
- **Extensibility**: Add new assistants without touching existing code
- **Testing**: Validate each importer independently

## Success Metrics

**The system is successful when**:
- ✅ CLI assistants can search conversations from web assistants
- ✅ Web assistant history enriches CLI assistant responses
- ✅ Zero manual intervention for search (LLM decides)
- ✅ Conversations searchable via keyword and semantic queries
- ✅ Unified memory across all AI interactions

**Current Status**: ✅ All metrics achieved (2025-11-22)

## Appendix: Configuration Files

### continuum-mcp Cargo.toml
```toml
[package]
name = "continuum-mcp"
version = "0.1.0"
edition = "2021"

[dependencies]
rmcp = { version = "0.8", features = ["server", "transport-io"] }
tokio = { version = "1", features = ["full"] }
anyhow = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
schemars = { version = "1.0", features = ["derive"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
chrono = { version = "0.4", features = ["serde"] }
walkdir = "2"
```

### chatgpt-to-continuum Cargo.toml
```toml
[package]
name = "chatgpt-to-continuum"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
chrono = "0.4"
clap = { version = "4", features = ["derive"] }
```

### claude-to-continuum Cargo.toml
```toml
[package]
name = "claude-to-continuum"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
chrono = "0.4"
clap = { version = "4", features = ["derive"] }
```

## Import Statistics (2025-11-22)

**ChatGPT Import**:
- Total conversations: 2,340
- Date range: 2023-04-03 to 2025-11-22
- Date directories: 681
- Errors: 0
- Storage size: ~124 MB

**Claude.ai Import**:
- Total conversations: 258
- Date range: 2025-11-03 to 2025-11-22
- Date directories: 124
- Errors: 0
- Storage size: ~39 MB

**Combined Total**: 2,598 conversations across all platforms

---

**Last Updated**: 2025-11-22
**Author**: Built collaboratively with Claude Code
**Related**: See PLAIN-TEXT-ARCHITECTURE.md, README.md, USAGE.md
