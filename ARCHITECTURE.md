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

**On Linux the harness starts through the mask launcher.** `continuum-claude`,
`continuum-codex`, `continuum-gemini` and `continuum-goose` run
`ai-masked <real harness> <arguments>` in place of
`<real harness> <arguments>`. `ai-masked` hides the protected folders (the
clinical trees) from the harness, sets `AI_FOOTING=masked`, drops
`ANTHROPIC_API_KEY`, and then becomes the harness in the same process, so the
wrapper waits for it and reads its exit code as before. The Nushell functions
start the wrappers through the launcher too, but Zellij's session resurrection
and other callers run `continuum-claude …` directly, so the wrappers do it
themselves. If `ai-masked` is not on `PATH` or in `~/.local/bin`, the wrapper
reports that and exits without starting the harness: it never starts it
directly instead (install the launcher with `rust-redeploy --only ai-masked`).
When the launcher itself refuses, it prints its reason and the wrapper exits
with the launcher's code, 113. The usage probes that start a harness follow the
same rule (section 3). The rule is one file, `continuum-core/src/masked_launch.rs`,
which `continuum-usage-core` also compiles. On macOS the harness is still
started directly, until the mask reaches the Mac. What is hidden, and how to
tell that a session is masked: `~/Assistants/shared/AI-FOOTING-MASK.md`.

**A no-save request is used up only by a session that started.** A wrapper
that finds `~/.continuum-nosave` does not delete it at once. It reserves it,
by renaming it to `.continuum-nosave.reserved-<its own PID>`, so of two
wrappers started together exactly one takes the request. This part is the same
on macOS. The request is then settled when the child has ended
(`continuum-core/src/nosave_request.rs`):

| How the child ended | The no-save request | Import, deletion, DayPage line |
|---|---|---|
| The start itself failed (the launcher could not be run) | given back | none |
| Exit with a status that is not one of the launcher's | used up | as usual |
| Exit 113 (the launcher refused), 126 or 127 (it could not run the harness) | given back | only if the wrapper saw a session |
| A signal | used up only if the wrapper saw a session; otherwise given back | only if the wrapper saw a session |

"Given back" means the reservation is renamed to the marker again, so the next
start is still not saved. Only an exit with a status that is not the
launcher's own shows that a harness ran. A signal does not: the child is the
launcher until its exec, and a signal may have ended it before that. Without
this rule a no-save `continuum-claude --continue` whose launcher refused or
was killed would have deleted the newest earlier transcript, which that start
never ran.

"The wrapper saw a session" is evidence of the wrapper's own, tied to its
child's PID. In interactive mode `continuum-claude` looks while the child
runs, in two places. The first is Claude Code's record of itself,
`~/.claude/sessions/<pid>.json`, which names the session that process is
running; it counts only if it was written after the child was spawned and
names that PID. Claude Code appends to its transcript and closes it again, so
this record is what identifies a session in practice. The second is a
transcript the child holds open, read from `/proc/<pid>/fd` on Linux (`lsof` is
asked only where there is no `/proc`). In print mode the evidence is a session
event on the child's output. `continuum-codex`, `continuum-gemini` and
`continuum-goose` have none: they only know the newest session, which may be
another start's.

**What keeps a no-save session out of Continuum when the wrapper did not see
it.** Every importer, the timers included, consults the no-save registry
(`~/.local/share/continuum/nosave-sessions/`) before it reads a transcript
(`continuum-core/src/import.rs`). Who enters the session there differs:

- **Claude Code.** The wrapper starts it with `CONTINUUM_NOSAVE=1`, and its
  SessionStart and Stop hooks (`continuum import --hook`) enter the session's
  id. So a no-save session that a signal ended, and that the wrapper did not
  see, is refused by the later sync; what the wrapper leaves undone is
  deleting Claude Code's own transcript. When the wrapper did see the session,
  it enters it itself and deletes the transcript, as at any no-save exit.
- **Codex.** Its hooks do the same, but Codex runs a hook only once it is
  trusted on that machine, and `continuum-codex` cannot tell which rollout is
  its own. So a no-save Codex start does not rely on its hooks
  (`continuum-core/src/codex_nosave_start.rs`):
  - **Before Codex runs.** A no-save `codex resume <id>` (also `codex exec
    resume <id>`) enters that thread id, and the file stem of every rollout of
    that thread, so the turns it appends are never stored; the turns stored
    before the resume are kept. A no-save resume that names no thread id
    (`--last`, the picker, a thread name) is refused, with the request kept,
    because the rollout it would write on cannot be known. Then the start is
    recorded in `~/.local/share/continuum/codex-nosave-starts/`: the rollouts
    that are there now, and a lock the wrapper holds while it lives.
  - **While it is open.** Every Codex import (the timer, the hooks, another
    wrapper's exit) stores no rollout that is not on that list, answering
    "held". A start is open while the wrapper holds its lock or the Codex
    child it recorded is alive, so a wrapper killed with its terminal tab
    leaves the start open until Codex has ended too.
  - **When it closes.** However Codex ended, the wrapper enters every rollout
    made since the start and removes any stored copy of one. A start whose
    wrapper was killed is closed in the same way by the first import after
    its Codex has ended. Rollouts that were there before are not touched.

  The cost, towards not saving: another Codex session that begins while a
  no-save Codex start is open is not stored while it is open, and is entered
  as no-save when it closes. A Codex session that was already running is
  stored as usual. Not covered: a wrapper killed by SIGKILL in the instant
  between starting Codex and recording it (other signals are held back for
  that instant); the start is then taken as closed once the wrapper is gone.
- **Gemini and Goose.** No hook or timer imports their sessions: only their
  own wrapper does, at exit. So a session the wrapper does not import is not
  put in Continuum later, and there is no importer for a registry entry to
  stop. (`continuum import -a goose`, run by hand, imports the newest Goose
  session and does not consult the registry.)

Two consequences are accepted, both towards not saving. The launcher's codes
are not reserved, and a signal can end a real harness: a harness that ends in
one of those ways after a real session the wrapper did not see gets its
request back, and the next session is then not saved either. The wrapper also
does no import at exit for that session. A Claude Code or Codex session that
was not no-save is still recorded by its hooks and the timers; a Gemini or
Goose session that a signal ended is not imported.

One case is not covered. A wrapper that is itself killed between reserving
and settling leaves its reservation file behind. If the harness had started,
the request was delivered. If it had not (the wrapper was killed before the
launcher's exec), the request is lost, as it was when the marker was deleted
first. No wrapper touches a reservation that is not its own.

**Continuum owns the real Codex CLI.** It installs Codex off `PATH`, under
`~/.local/share/continuum/codex/`. `continuum codex update` updates that copy,
and `continuum codex which` reports which binary is selected. One resolver in
`continuum-core` serves both the wrapper and the Codex usage probe, and it
refuses to start itself recursively.

**Codex clipboard in Linux Zellij.** For a launch with both input and output
attached to a terminal and a nonempty `ZELLIJ`, `continuum-codex` gives the child
empty `DISPLAY` and `WAYLAND_DISPLAY` values and removes `WAYLAND_SOCKET`.
Codex 0.163 otherwise writes only to the server's desktop clipboard when a
persistent pane was started locally, even after a remote terminal attaches.
Disabling its native backend selects Codex's existing OSC 52 terminal fallback.
Zellij and the attached terminal must allow clipboard writes; sending OSC 52
cannot acknowledge delivery. Restart a client and resume its saved conversation
to pick up the launcher change; existing clients keep their environment.

The parent shell, redirected launches, non-Zellij launches and macOS launches
retain their display environment. The Codex child and its tools cannot use the
Linux desktop display or native image clipboard in this mode. Attach images by
file, or explicitly retain the local desktop connection with Nushell
`with-env {CONTINUUM_CODEX_NATIVE_CLIPBOARD: "1"} { codex resume }`.
That opt-out restores native copying and may reproduce the remote-copy issue.
The wrapper does not alter SSH identity, Codex arguments or clipboard contents.

Offline process controls: build `cargo build --locked -p continuum-codex`, then
run `python3 continuum-codex/tests/clipboard_launch.py target/debug/continuum-codex`.
These use a fake child in a private home and pseudo-terminals, with no provider
calls or access to a real clipboard. On Linux they put a stand-in `ai-masked`
first on `PATH`, so the real launcher is not run. Physical Mac/Ghostty/mosh delivery still
needs a selection and paste check after reopening a client.

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

Four probes start a harness: the three smoke probes, and `usage-probe-codex`,
which runs `codex app-server` to read the rate limits. On Linux each starts it
through `ai-masked`, by the same rule as the wrappers (section 2). Without the
launcher the probe reports a failed reading that names `ai-masked`, and starts
nothing. This matters most for `usage-probe-codex`: a `codex app-server` outside
the mask makes the launcher refuse every other Codex start while it lives.

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
