# Quality Control - Conversation Filtering

Continuum provides two mechanisms to prevent trivial or unwanted conversations from cluttering your logs.

## Overview

Every conversation with Claude Code, Codex, or Goose is **automatically captured** by default. Quality control lets you:

1. **Preemptively skip** conversations you know will be trivial (reminders, simple facts)
2. **Review after** each session to decide if it's worth keeping

## Mechanism 1: Preemptive Skip (nosave)

### Usage

Run `nosave` in your terminal **before** starting an assistant session:

```bash
nosave        # Set the marker
claude        # Start Claude Code - won't be saved
```

### What Happens

1. Creates marker file at `~/.continuum-nosave`
2. When you start any assistant (claude/codex/goose), it:
   - Detects the marker
   - Shows: `⚠ This conversation will NOT be saved to continuum logs`
   - Takes the marker for itself (it is renamed to
     `~/.continuum-nosave.reserved-<PID>` while the assistant runs, and removed
     when it exits)
   - Runs normally but doesn't save to `~/continuum-logs/`
   - If the assistant could not be started, or ended in a way that does not
     show it ran (a signal, with no session seen), puts the marker back, so
     the next start is still not saved (see "Wrap" in `ARCHITECTURE.md`)

### Use Cases

- Quick factual queries ("What's 2+2?")
- Simple reminders ("Remind me to call John")
- Test sessions
- Debugging/troubleshooting conversations

### Important Notes

- ✅ Works with **all three assistants** (Claude Code, Codex, Goose)
- ✅ Marker is **global** (filesystem-based, not terminal-specific)
- ✅ Consumed by **whichever assistant starts next**, regardless of terminal
- ✅ Single-use: Only affects the **first** assistant started after `nosave`
- ⚠️ If you run `nosave` then start multiple assistants, only the **first one** won't be saved
- ⚠️ Codex: a no-save `codex resume` must name the session (`codex resume <uuid>`);
  `codex resume --last` and the picker are refused, and the marker is kept
- ⚠️ Codex: while a no-save Codex session is open, any other Codex session that
  *begins* meanwhile is not saved either (Continuum cannot tell which rollout is
  the no-save one). Codex sessions already running are saved as usual. See
  "What keeps a no-save session out of Continuum" in `ARCHITECTURE.md`

### How the Marker Works

The `nosave` command creates a single file: `~/.continuum-nosave`

When **any assistant starts in any terminal**, it:
1. Checks for `~/.continuum-nosave`
2. If found: takes it (by renaming it) and skips saving; if the assistant then
   cannot be started, the marker is put back
3. If not found: Saves normally

**This means:**
- Running `nosave` in Terminal 1 affects assistants started in **any terminal**
- The marker is consumed by the first assistant to start (race condition if simultaneous)
- Each `nosave` only affects one session

### Example: Multiple Sessions

```bash
# Terminal 1
nosave        # Creates global marker at ~/.continuum-nosave

# Terminal 2 (different window)
claude        # ← Not saved (consumes marker)

# Terminal 3 (simultaneously)
codex         # ← Saved normally (marker already consumed by claude)
```

### Example: Multiple Skip Sessions

To skip multiple sessions, run `nosave` before each:

```bash
nosave && claude    # Session 1: not saved
nosave && codex     # Session 2: not saved
nosave && goose     # Session 3: not saved
```

## Mechanism 2: Post-Conversation Review

### What Happens

**After exiting an assistant session**, you're prompted:

```
─────────────────────────────────────────
Save this conversation? [Y/n/r]
```

- Press `Y` or `Enter`: Conversation saved to `~/continuum-logs/` (default)
- Press `n` or `no`: Conversation **deleted** from continuum logs
- Press `r` or `review`: **Review the full conversation** before deciding

### Review Mode

When you press `r`, the complete conversation is displayed with:

- Clear **USER** and **ASSISTANT** labels
- Message numbers for reference
- Content word-wrapped at 80 characters for readability
- Bordered boxes for visual clarity

**Example review output:**

```
═══════════════════════════════════════════
          CONVERSATION REVIEW
═══════════════════════════════════════════

┌─ USER (Message 1) ─────
│ Help me debug this Rust error...
└─────────────────────────────────────────

┌─ ASSISTANT (Message 2) ─────
│ Let me help you with that. The error
│ suggests that...
└─────────────────────────────────────────

═══════════════════════════════════════════
            END OF REVIEW
═══════════════════════════════════════════

Save this conversation? [Y/n]
```

After reviewing, you get a final save/discard prompt (without the review option).

### When the Prompt Appears

The prompt appears **when a specific assistant invocation exits**:

✅ **Triggers the prompt:**
- Typing `exit` or pressing Ctrl+D to close the assistant
- Closing the terminal window where the assistant is running
- The assistant crashing or terminating
- Ending that specific conversation session

❌ **Does NOT trigger the prompt:**
- Having multiple assistants running simultaneously
- Switching between terminal windows/panes
- Starting a new session while another is running
- Only exiting ONE assistant while others are still running

### Session Lifecycle Example

```bash
# Start Claude Code in Terminal 1
claude
> "Help me debug this Rust code..."
> ... conversation continues ...
> exit

# Prompt appears immediately:
📝 Importing session to continuum logs...
✓ Saved 15 messages to continuum logs

─────────────────────────────────────────
Save this conversation? [Y/n/r] y
✓ Conversation saved
```

### Multiple Sessions Example

```bash
# Terminal 1
claude
# ... work on project A ...
exit
# → Prompt: "Save this conversation? [Y/n/r]"

# Terminal 2 (simultaneously running)
codex
# ... work on project B ...
exit
# → Prompt: "Save this conversation? [Y/n/r]" (independent)
```

**Each terminal/session is completely independent.**

### What Gets Saved

When you press `Y` or `Enter`:
- Conversation saved to `~/continuum-logs/{assistant}/{date}/{session-id}/`
- Messages compressed (noise filtered)
- Searchable via MCP
- Accessible to all assistants via cross-assistant memory

When you press `n`:
- Session directory **deleted** from `~/continuum-logs/`
- Cannot be recovered
- Not searchable via MCP

## Combining Both Mechanisms

You can use both approaches together:

```bash
# Known trivial conversation
nosave
claude "What's 2+2?"
# → No prompt, not saved

# Unknown complexity conversation
claude "Help me with this project"
# ... conversation happens ...
# → Not sure if it's worth keeping
exit
# Prompt: "Save this conversation? [Y/n/r]"
# Press 'r' to review the conversation
# After reviewing: Press 'n' to discard
# ✗ Conversation discarded
```

### Review Mode Use Cases

Use review mode (`r`) when:
- Uncertain about conversation value after completion
- Want to verify what was discussed before committing to logs
- Need to check if any sensitive information was shared
- Conversation was long and you want a quick summary
- Multiple topics were covered and you're unsure if it's worth keeping

## Technical Details

### Marker File

- **Location**: `~/.continuum-nosave`
- **Created by**: `nosave` command
- **Consumed by**: First assistant that starts after marker is created
- **Taken**: when detected, by renaming it to `~/.continuum-nosave.reserved-<PID>` (single-use)
- **Removed**: when that assistant exits; **put back** if it could not be started, or ended without showing that it ran

### Session Directory Structure

```
~/continuum-logs/
├── claude-code/{date}/{session-id}/
├── codex/{date}/{session-id}/
└── goose/{date}/{session-id}/
```

When you decline to save (press `n`), the entire `{session-id}/` directory is deleted.

### Process Flow

```
┌─────────────────┐
│  Start Session  │
└────────┬────────┘
         │
         ▼
    ┌────────────────┐      Yes     ┌──────────────────┐
    │ Marker exists? │──────────────▶│ Skip saving      │
    └────────┬───────┘              └──────────────────┘
             │ No
             ▼
    ┌─────────────────┐
    │ Run assistant   │
    └────────┬────────┘
             │
             ▼
    ┌─────────────────┐
    │ Session ends    │
    └────────┬────────┘
             │
             ▼
    ┌─────────────────┐
    │ Import session  │
    └────────┬────────┘
             │
             ▼
    ┌─────────────────┐
    │ Prompt user     │
    └────────┬────────┘
             │
        ┌────┴────┐
        │         │
      Press Y   Press n
        │         │
        ▼         ▼
    ┌─────┐   ┌─────────┐
    │Save │   │ Delete  │
    └─────┘   └─────────┘
```

## Best Practices

### Use `nosave` for:
- Quick factual queries
- Testing commands
- Simple reminders
- Debugging sessions
- Conversations you **know** are trivial before starting

### Use post-conversation review for:
- Conversations where value is unclear upfront
- When you realize mid-conversation it's not worth keeping
- Regular workflow (every session gets reviewed)

### Don't worry about:
- Using quality control "too much" - it's designed for frequent use
- Missing the prompt - it blocks until you respond
- Breaking anything - declining to save just deletes the log, assistant still worked normally

## Troubleshooting

### Prompt doesn't appear
- Make sure you properly exited the assistant (Ctrl+D or `exit`)
- Check if `nosave` was run beforehand (marker consumed)
- Verify the wrapper is installed: `which claude` should point to `continuum-claude`

### Session saved but I pressed 'n'
- The prompt appears AFTER import completes
- Once you press 'n', the session directory is deleted
- Verify with: `ls ~/continuum-logs/{assistant}/{date}/`

### Multiple prompts appearing
- If you run multiple sessions, each gets its own prompt
- This is expected behavior - each session is independent

### Marker not working
- Check marker file: `ls ~/.continuum-nosave`
- Verify `nosave` command exists: `which nosave`
- Remember: marker is single-use (consumed by first assistant started)

## FAQ

**Q: Can I disable the post-conversation prompt entirely?**
A: Not currently. The prompt ensures you consciously decide what to keep. Use `nosave` for sessions you know should be skipped.

**Q: What if I accidentally press 'n'?**
A: The conversation is permanently deleted from continuum logs. However, the original session still exists in the assistant's native storage (e.g., `~/.claude/projects/` for Claude Code) for ~30 days.

**Q: Does this affect the assistant's native logging?**
A: No. Claude Code still writes to `~/.claude/projects/`, Codex to `~/.codex/sessions/`, Goose to its SQLite database. Quality control only affects continuum's unified logs.

**Q: Can I review conversations before deciding?**
A: Yes! Press `r` when prompted to see the full conversation with all messages displayed in a readable format. After reviewing, you'll get a final save/discard prompt.

**Q: What happens if I just close the terminal without responding?**
A: The terminal closes, the prompt is lost, and the session remains saved (default behavior is to save).

**Q: Does noise filtering happen before or after the prompt?**
A: Before. The message count shown includes noise filtering (removed pleasantries, boilerplate, etc.). What gets saved is already compressed.

## See Also

- [README.md](README.md) - Project overview and quick start
- [USAGE.md](USAGE.md) - Complete usage guide
- [PLAIN-TEXT-ARCHITECTURE.md](PLAIN-TEXT-ARCHITECTURE.md) - Design philosophy
