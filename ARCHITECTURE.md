# Continuum Architecture

Continuum keeps a vendor-neutral record of assistant work and watches how much
assistant capacity is available. It does three jobs:

| Job | What it does | Main parts |
|---|---|---|
| **Record** | Captures every assistant conversation into one plain-text store, then makes it searchable and loadable | `continuum import`, per-turn hooks, importers, `ctce` / `ctsme`, `continuum-activity`, `continuum-mcp` |
| **Wrap** | Starts the harnesses Will runs by hand, so each session is recorded and the recording rules hold | `continuum-claude`, `continuum-codex`, `continuum-gemini`; the managed Codex install |
| **Observe** | Reports usage and limits across providers | `usagewatch` and its `usage-probe-*` probes |

Continuum does not decide anything about the work itself. It does not choose a
harness, model, effort level or route, run tasks, or judge whether a task
succeeded. Those jobs belong to the dispatcher (see
[Boundaries](#boundaries-with-neighbouring-parts)).

---

## 1. Record

### The store

Every conversation lives as plain text under `~/Assistants/continuum-logs/`, so it
syncs between hosts with the rest of `~/Assistants`:

```
~/Assistants/continuum-logs/
└── <source>/                 # claude-code, codex, grok-cli, gemini-cli, claude, chatgpt, grok, …
    └── YYYY-MM-DD/
        └── <session-id>/
            ├── session.json    # id, assistant, start/end time, status, message count
            └── messages.jsonl  # one message per line: id, role, content, timestamp
```

Live harness captures (`claude-code`, `codex`, `grok-cli`, `gemini-cli`),
web-chat imports (`claude`, `chatgpt`, `grok`, `gemini`) and direct API chats
(`fireworks`) are stored side by side, one directory per source.

### How sessions get in

| Source | Capture path |
|---|---|
| Claude Code | Per-turn Stop hook `continuum import -a claude-code --hook`; the 5-minute `continuum-sync-claude` timer; `continuum-claude` at exit |
| Codex | Per-turn Stop hook `continuum import -a codex --hook` (Codex runs it only once the hook is trusted on that machine); `continuum-auto-import` timer on nimbini; `continuum-codex` at exit |
| Grok Build (`grok` CLI) | Grok `Stop` and `SessionEnd` hooks run `continuum-grok`, which scans Grok's own transcript and imports it |
| Gemini CLI | `continuum-gemini` at exit |
| Web chats (claude.ai, ChatGPT, Grok, Gemini) | Exported by hand; `ai-export-watcher` sees the download and runs `chatgpt-to-continuum`, which reads all four formats. Only conversations Will chooses to export get in. A vendor's full data export goes through `claude-to-continuum` or `grok-to-continuum`, run by hand. |
| DeepSeek on Fireworks (`dsv4`) | The Nushell `fireworks-chat` function writes each exchange as it happens |

Claude Code and Codex have **one importer each** (`continuum_core::import`), and
the hooks, the timers and the wrappers all call it. It replaces a session's file
(temp file, then rename) instead of appending, skips content it already holds,
and takes a per-session lock. Re-importing is therefore always safe and leaves
one copy. Because the hooks fire after every completed turn, a session survives
a crash, a killed terminal or a power cut, minus the turn in progress.

### What is never recorded

The importer refuses, so every capture path inherits the refusal:

- sessions listed in the clinical-session registry
  (`~/.local/share/continuum/claude-clinical-sessions`); if that check can't be
  made, the import fails rather than proceeding;
- sessions launched as no-save (`~/.continuum-nosave`). The harness's hooks
  enter the session in a per-machine no-save registry
  (`~/.local/share/continuum/nosave-sessions/`), which every importer checks,
  including the timers that never see how the session was launched. Known gap:
  a fork of a no-save session, made in an ordinary launch, gets a new id that
  isn't registered.

The hooks also refuse any session started by `cc-clinical` (`CC_CLINICAL_PID`
in the harness's environment), and `cc-clinical` moves its transcripts to a
separate guarded store on exit.

### Retrieving and deriving

| Tool | What it gives |
|---|---|
| `ctce` | Interactive text search over the store |
| `ctsme` | Semantic search, using the index built by `semantic-indexer-continuum` |
| `continuum-search`, `continuum-timeline`, `continuum-stats` | Nushell functions (`~/dotfiles/nushell/continuum.nu`) for ad-hoc queries |
| `continuum-mcp` | An MCP server that lets an assistant search past conversations (wired into Codex) |
| `continuum-activity` | Daily activity for DayPages and the DevLog; `load` seeds a new session with a past one; `recall` points to past sessions that mention a named path, command or project |
| `continuum-dream` | Nightly consolidation of each host's Claude Code memory from recent sessions |

These tools read the store. The one that rewrites it is
`continuum-activity clean`, which removes duplicates.

---

## 2. Wrap

Typing `claude`, `codex` or `gemini` runs a Continuum wrapper. In Nushell,
`config.nu` routes each name to its wrapper; in Bash, `alias claude=continuum-claude`
does the same for Claude Code. Each wrapper:

1. finds the real harness binary and starts it as a child process, passing the
   arguments through;
2. after the harness exits, imports the session through the same importer as
   the hooks, applying the same refusals;
3. adds only what recording needs. In `--print` mode, `continuum-claude` adds
   `--output-format stream-json --verbose` so it can capture the output.

`continuum-claude` also queues a DayPage line (`daypage-append`) when a session
with exchanges ended without writing one.

**Continuum owns the real Codex CLI.** It installs Codex off `PATH`, under
`~/.local/share/continuum/codex/`. `continuum codex update` updates that copy,
and `continuum codex which` reports which binary is selected. One resolver in
`continuum-core` serves both the wrapper and the Codex usage probe, and it
refuses to start itself recursively.

Since the per-turn hooks arrived (October 2026), capture no longer depends on the
wrappers: an unwrapped Claude Code or Codex session is still recorded. What the
wrappers still add is the import at exit, the DayPage line, the managed Codex
install, and a single place where every hand-started session begins.

### The rule for wrappers

A wrapper starts the harness, records the session, and refuses to record when
the rules say so. It does not choose a model, effort level, route or harness,
and it does not change Will's arguments beyond what recording needs.

Every hand-started session passes through a wrapper, so it is the obvious place
to add launch-time choices, and that is how Continuum would turn into the
component everything runs through. If a launch-time choice is ever wanted, the
wrapper reads it from the dispatcher's policy; it never holds a policy of its
own.

---

## 3. Observe

`usagewatch` (crates `continuum-usage` and `continuum-usage-core`) reports usage
and limits across subscriptions and API accounts. It tracks two separate things:

- **scarcity**: is a limit close?
- **perishability**: is there unused allowance that will be lost at the next reset?

Each provider is an executable named `usage-probe-<name>` on `PATH` that prints
one JSON object. The core holds no vendor code and no credentials. Smoke probes
(`*-smoke`) send one small real request each hour, to prove a request actually
goes through. A timer (`usagewatch-monitor`) refreshes on a cadence and sends
desktop notifications.

usagewatch is advice only. It never changes a tier, model, effort level or
route. The dispatcher may read its stored observations as advice; it doesn't
trigger a refresh.

---

## Boundaries with neighbouring parts

| Part | Owns | Relationship to Continuum |
|---|---|---|
| Harnesses (Claude Code, Codex, Grok Build, Gemini CLI) | Doing the work; their own native transcripts | Continuum copies their transcripts into the store |
| Dispatcher (`agent-dispatch`) and bridge (`agent-bridge`), in `~/dotfiles/rust-projects/` | Choosing what runs where (harness, model, effort), running it, checking the result, escalating, its own ledger | Reads usagewatch observations as advice; writes nothing to Continuum |
| Guards (clinical footing, `phi-guard`, the clinical-data hooks) | The data boundary across every tool | Continuum enforces its import refusals but doesn't set the boundary policy |
| DayPages and assistant memory | Will's diary; per-host assistant memory | Fed from the store by `continuum-activity` and `continuum-dream` |

In short: **Continuum records what happened and observes what's available. The
dispatcher decides what to run and checks whether it worked.** A proposal that
gives Continuum a decision belongs in the dispatcher instead.

---

## Crates in this repository

| Crate | Binary | Job |
|---|---|---|
| `continuum-core` | library | Record: the importers, `PlainTextWriter`, the clinical and no-save refusals, the Codex resolver, the noise filter |
| `continuum-cli` | `continuum` | Record and wrap: `import` (including `--hook`), `stats`, `codex which` / `codex update` |
| `continuum-claude` | `continuum-claude` | Wrap |
| `continuum-codex` | `continuum-codex` | Wrap |
| `continuum-gemini` | `continuum-gemini` | Wrap |
| `continuum-goose` | `continuum-goose` | Wrap (legacy) |
| `continuum-grok` | `continuum-grok` | Record: hook-driven import of Grok Build sessions |
| `continuum-usage` | `usagewatch` | Observe |
| `continuum-usage-core` | library | Observe: probe contract, observation store, policy |
| `usage-probe-*` | one per probe | Observe: `claude`, `codex`, `grok`, `anthropic-api`, and the `*-smoke` probes |

Related tools outside this repository:

- **In `~/dotfiles/rust-projects/`:** `continuum-sync-claude`,
  `continuum-auto-import`, `continuum-activity`, `continuum-dream`,
  `ai-export-watcher`, `claude-to-continuum`, `chatgpt-to-continuum`,
  `grok-to-continuum`.
- **In `~/dotfiles/scripts/`:** the semantic indexer
  (`semantic-indexer-continuum`).
- **Separately:** `continuum-mcp`, built from `~/continuum-mcp`, which has no
  commits. A versioned copy is on this repository's unmerged branch
  `fix/retrieval-boundaries-2026-10-20`.

---

## Design decisions that still hold

- **Plain text, not a database.** Any tool (`rg`, `fd`, `jq`, Nushell) can read
  the store, it backs up and syncs as ordinary files, and there are no schema
  migrations. The cost is no built-in ranking and slower search, which ripgrep
  and the semantic index cover.
- **JSONL.** One message per line streams well and works with line-based tools.
- **One importer per assistant, replacing instead of appending.** Before October
  2026, separate importers appended, and live sessions piled up many copies of
  the same messages. One idempotent importer ended that.
- **Capture at each turn, not at exit.** A session that ends badly is still on
  record.
- **Refusals live in the importer,** so a new capture path can't skip them.
- **Wrappers record; they never decide.**

## Where to look next

- `~/Assistants/shared/CONTINUUM-STATUS.md`: current state, per-source
  details and dated receipts.
- `~/Assistants/shared/USAGEWATCH.md`: usagewatch policy and probes.
- `~/dotfiles/rust-projects/agent-dispatch/README.md`: the dispatcher.
- `PLAIN-TEXT-ARCHITECTURE.md`: the original case for plain text.
