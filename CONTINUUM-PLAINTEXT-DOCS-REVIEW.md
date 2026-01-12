# Continuum Documentation Review (Plain-Text Architecture)

**Date**: 2025-11-09  
**Reviewer**: Codex (GPT-5)

## Overview

Read through the new plain-text documentation set (`README.md`, `QUICK-START.md`, `USAGE.md`, `PLAIN-TEXT-ARCHITECTURE.md`, `ARCHITECTURE.md`, `MIGRATION-STATUS.md`, `PLAIN-TEXT-MIGRATION-COMPLETE.md`). The narrative is cohesive: Continuum now focuses on plain-text JSONL storage, Nushell queries, and Unix-style tooling.

## Strengths

1. **Clear storage story** – multiple docs (README, Architecture) reinforce the `~/continuum-logs/assistant/date/session/` layout and `session.json` + `messages.jsonl` schema, making the model obvious.
2. **Workflow-first docs** – Quick Start, Usage, and README include copy/paste-ready commands for building, capturing, importing, and searching; newcomers can verify functionality instantly.
3. **Migration transparency** – `MIGRATION-STATUS.md` and `PLAIN-TEXT-MIGRATION-COMPLETE.md` explicitly track what was removed, what was added, and the remaining tasks, so it’s easy to see the current state.
4. **Unix philosophy emphasized** – repeated reminders that ripgrep, Nushell, git, and other standard tools are the primary interface match the user’s stated goals.

## Gaps / Concerns

1. **Manual imports for Codex/Goose** – current workflow requires running `continuum import` after each session (`USAGE.md`, `QUICK-START.md`). Without automation, it’s easy to fall out of sync, which weakens “unified history”. Consider documenting a watcher/systemd timer or building ingestion automation back in.
2. **CLI regressions** – the CLI now exposes only import/stats while search/timeline/context live solely in Nushell scripts. Other assistants or scripts can’t query Continuum unless they also source `continuum.nu`, which conflicts with the cross-assistant goal. Document the limitation or reintroduce thin CLI wrappers.
3. **Compaction/context roadmap missing** – earlier plans for multi-stage compaction, token budgeting, and context chunks aren’t addressed in the new docs. Plain-text storage doesn’t preclude those features, but it’s unclear if they’re intentionally deferred or dropped.
4. **Adapter responsibilities implied, not detailed** – Architecture doc hints that adapters still perform noise filtering and normalization before writing, but there’s no operational guidance on how Claude/Codex adapters behave now (file watchers vs on-demand imports, error handling, etc.).

## Suggestions

1. Add an “Automation” section (or script) documenting how to run `continuum import` via cron/systemd so Codex/Goose sessions land automatically.
2. Provide CLI entry points (`continuum search`, `continuum timeline`, etc.) that internally call ripgrep/Nushell or reuse shared Rust code, so other assistants and MCP clients can consume Continuum without custom shell config.
3. Update the roadmap to clarify how compaction/context delivery will evolve in the plain-text world (e.g., regex-based filter now, LLM summary later, context builder planned for M2).
4. Include a short “Adapter behavior” section summarizing how each assistant’s logs reach `~/continuum-logs/` (wrapper vs import) and what guarantees exist (dedup, timestamps, error reporting).

Overall the documentation is readable, practical, and aligned with the chosen philosophy. Addressing the gaps above would make it easier for other assistants—and future you—to rely on Continuum without manual steps or assumptions.
