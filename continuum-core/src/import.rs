// Whole-session importers shared by every capture path: the exit-time
// wrappers (continuum-claude, continuum-codex), `continuum import` (the
// 5-minute sync and timer jobs) and the per-turn Stop hooks.
//
// Until 2026-10-06 each path had its own importer and they disagreed. The
// sync's Claude Code importer appended the whole session on every run, so an
// active session was stored several times over until the wrapper rewrote it
// at exit, and continuum-codex filed every session under the day it exited.
// One importer per assistant, written by `PlainTextWriter::replace_session`,
// makes any number of re-imports harmless.

use color_eyre::{eyre::Context, eyre::eyre, Result};
use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::{CodexLogEntry, LoopDetection, LoopDetector, MessageCompressor, PlainTextWriter};

/// What one import did.
#[derive(Debug)]
pub struct ImportOutcome {
    pub session_id: String,
    pub dir: PathBuf,
    pub message_count: usize,
    /// False when the stored copy already matched and nothing was written.
    pub written: bool,
    /// Lines that were not valid JSON, normally one still being written.
    pub skipped_lines: usize,
}

/// What an import stored, or why it stored nothing.
#[derive(Debug)]
pub enum Imported<T> {
    Stored(T),
    /// The transcript holds no messages yet.
    Empty,
    /// The session is in the no-save registry.
    NoSave,
}

impl<T> Imported<T> {
    pub fn stored(self) -> Option<T> {
        match self {
            Imported::Stored(t) => Some(t),
            _ => None,
        }
    }
}

/// Set to `1` by the continuum-claude / continuum-codex wrappers on the agent
/// process when `~/.continuum-nosave` was present at launch. The agent's
/// SessionStart and Stop hooks inherit it and put the session in the no-save
/// registry (`register_nosave`), which every importer consults. The env alone
/// is not enough: the 5-minute Claude sync and the Codex timer never see it.
pub const NOSAVE_ENV: &str = "CONTINUUM_NOSAVE";

/// Longest tool result kept, in bytes (cut back to a character boundary).
const TOOL_RESULT_MAX: usize = 500;

/// Import one Claude Code transcript (`~/.claude/projects/<dir>/<uuid>.jsonl`).
/// A session in the no-save registry is never read.
///
/// The caller must decide first whether the session may be imported at all:
/// see `ensure_claude_session_importable`. Content rules are the ones the
/// continuum-claude wrapper has always used, since its exit-time import is
/// the copy that has survived for most sessions: user text, assistant text and
/// `TOOL_USE` lines. An event that appears twice (same uuid) is kept once;
/// identical text in separate events is kept each time.
pub fn import_claude_code(writer: &PlainTextWriter, session_path: &Path) -> Result<Imported<ImportOutcome>> {
    let session_id = file_stem(session_path)?;
    if nosave_marked(writer, &[&session_id])? {
        return Ok(Imported::NoSave);
    }
    writer.with_session_lock("claude-code", &session_id, || import_claude_code_locked(writer, session_path, session_id.clone()))
}

fn import_claude_code_locked(
    writer: &PlainTextWriter,
    session_path: &Path,
    session_id: String,
) -> Result<Imported<ImportOutcome>> {
    #[derive(serde::Deserialize)]
    struct Entry {
        #[serde(rename = "type")]
        entry_type: String,
        uuid: Option<String>,
        message: Option<serde_json::Value>,
        timestamp: Option<String>,
    }

    let mut messages: Vec<(String, String)> = Vec::new();
    // An event written twice keeps its uuid, so uuid identifies a duplicate.
    // Text does not: two separate "yes" replies are two turns, so an entry
    // without a uuid (none seen in current transcripts) is always kept.
    let mut seen_events: HashSet<String> = HashSet::new();
    let mut start_time: Option<String> = None;
    let mut skills: Vec<String> = Vec::new();

    let skipped_lines = for_each_record(session_path, |entry: Entry| {
        if start_time.is_none() {
            start_time = entry.timestamp.clone();
        }
        if entry.entry_type != "user" && entry.entry_type != "assistant" {
            return;
        }
        if let Some(uuid) = &entry.uuid {
            if !seen_events.insert(uuid.clone()) {
                return;
            }
        }
        let mut keep = |role: &str, content: String| messages.push((role.to_string(), content));
        let Some(msg) = entry.message else { return };
        match msg["role"].as_str() {
            Some("user") => {
                if let Some(content) = msg["content"].as_str() {
                    keep("user", content.to_string());
                }
            }
            Some("assistant") => {
                let Some(blocks) = msg["content"].as_array() else { return };
                for block in blocks {
                    match block["type"].as_str().unwrap_or("") {
                        "text" => {
                            if let Some(text) = block["text"].as_str().filter(|t| !t.is_empty()) {
                                keep("assistant", text.to_string());
                            }
                        }
                        "tool_use" => {
                            let name = block["name"].as_str().unwrap_or("unknown");
                            if name == "Skill" {
                                if let Some(skill) = block.pointer("/input/skill").and_then(|v| v.as_str()) {
                                    if !skills.iter().any(|s| s == skill) {
                                        skills.push(skill.to_string());
                                    }
                                }
                            }
                            let input = block.get("input").map(|i| i.to_string()).unwrap_or_default();
                            keep("assistant", format!("TOOL_USE: {name} -> {input}"));
                        }
                        "tool_result" => {
                            let output = match block.get("content") {
                                Some(serde_json::Value::String(s)) => s.clone(),
                                Some(serde_json::Value::Array(parts)) => parts
                                    .iter()
                                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                                _ => String::new(),
                            };
                            if !output.is_empty() {
                                keep("user", format!("TOOL_RESULT: {}", truncate(&output, TOOL_RESULT_MAX)));
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    })?;

    let compressed = MessageCompressor::new().compress_batch(&messages);
    if compressed.is_empty() {
        return Ok(Imported::Empty);
    }
    let start_time = start_time.unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    // Again under the lock: the session may have been registered while it was read.
    if nosave_marked(writer, &[&session_id])? {
        return Ok(Imported::NoSave);
    }
    let (dir, written) = writer.replace_session(&session_id, "claude-code", Some(&start_time), &skills, &compressed)?;
    Ok(Imported::Stored(ImportOutcome { session_id, dir, message_count: compressed.len(), written, skipped_lines }))
}

/// Import one Codex rollout (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`),
/// filed under the session's own start date. Also returns the loop
/// detections, for a caller that wants to show them. A session in the
/// no-save registry, under its thread id, is never read.
pub fn import_codex(
    writer: &PlainTextWriter,
    session_path: &Path,
) -> Result<Imported<(ImportOutcome, Vec<LoopDetection>)>> {
    let session_id = file_stem(session_path)?;
    if nosave_marked(writer, &codex_ids(&session_id))? {
        return Ok(Imported::NoSave);
    }
    writer.with_session_lock("codex", &session_id, || import_codex_locked(writer, session_path, session_id.clone()))
}

fn import_codex_locked(
    writer: &PlainTextWriter,
    session_path: &Path,
    session_id: String,
) -> Result<Imported<(ImportOutcome, Vec<LoopDetection>)>> {
    let mut messages: Vec<(String, String)> = Vec::new();
    let mut start_time: Option<String> = None;

    let skipped_lines = for_each_record(session_path, |entry: CodexLogEntry| {
        if start_time.is_none() {
            start_time = entry.timestamp.clone();
        }
        if entry.entry_type != "response_item" {
            return;
        }
        if let Some(payload) = entry.payload {
            if let (Some(role), Some(content)) = (payload.role, payload.content) {
                let text = content.iter().filter_map(|c| c.text.as_deref()).collect::<String>();
                messages.push((role, text));
            }
        }
    })?;

    let compressed = MessageCompressor::new().compress_batch(&messages);
    if compressed.is_empty() {
        return Ok(Imported::Empty);
    }
    // The session's own start, never the import time: stamping with `now`
    // filed one session under a new date on every re-import (2026-09-02).
    let start_time = start_time
        .or_else(|| {
            std::fs::metadata(session_path)
                .and_then(|m| m.modified())
                .ok()
                .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339())
        })
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    let detections = LoopDetector::new().analyze(&messages);
    // Again under the lock: the session may have been registered while it was read.
    if nosave_marked(writer, &codex_ids(&session_id))? {
        return Ok(Imported::NoSave);
    }
    let (dir, written) = writer.replace_session(&session_id, "codex", Some(&start_time), &[], &compressed)?;
    let outcome = ImportOutcome { session_id, dir, message_count: compressed.len(), written, skipped_lines };
    Ok(Imported::Stored((outcome, detections)))
}

/// Put `session_id` in the no-save registry: one empty marker file per
/// session that no importer may store. The wrappers can't know the id of the
/// session they launch, so the agent's SessionStart and Stop hooks call this
/// (through `continuum import --hook`) when they carry `NOSAVE_ENV`.
/// Registering an id twice is harmless.
pub fn register_nosave(writer: &PlainTextWriter, session_id: &str) -> Result<PathBuf> {
    if !session_id_is_safe(session_id) {
        return Err(eyre!("not registering an unusable session id as no-save"));
    }
    let dir = writer
        .nosave_dir()
        .ok_or_else(|| eyre!("HOME is unavailable, so the no-save registry cannot be found"))?;
    std::fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    let marker = dir.join(session_id);
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        options.mode(0o600);
    }
    options.open(&marker).with_context(|| format!("Failed to register no-save session at {}", marker.display()))?;
    Ok(marker)
}

/// Make a session no-save after the fact: register it, then delete every
/// stored copy (`<base>/<assistant>/<date>/<id>/`) under its import lock, so
/// an import already running sees the marker instead of writing. The
/// wrappers call this when a no-save launch exits (a copy may have been
/// stored before a hook registered it, if the hook failed or Codex had not
/// trusted it yet) and when the user discards a conversation. Returns how
/// many copies were removed.
pub fn discard_session(writer: &PlainTextWriter, assistant: &str, session_id: &str) -> Result<usize> {
    register_nosave(writer, session_id)?;
    writer.with_session_lock(assistant, session_id, || {
        let Ok(days) = std::fs::read_dir(writer.base_dir().join(assistant)) else { return Ok(0) };
        let mut removed = 0;
        for day in days.flatten() {
            let copy = day.path().join(session_id);
            if copy.is_dir() {
                std::fs::remove_dir_all(&copy).with_context(|| format!("Failed to remove {}", copy.display()))?;
                removed += 1;
            }
        }
        Ok(removed)
    })
}

/// Whether any of `ids` is in the no-save registry. Only a confirmed absence
/// counts as "not marked": an error checking for a marker is an error, so a
/// scheduled import fails visibly rather than storing the session, and so is
/// having no registry to consult (HOME unknown).
fn nosave_marked(writer: &PlainTextWriter, ids: &[&str]) -> Result<bool> {
    let dir = writer
        .nosave_dir()
        .ok_or_else(|| eyre!("Refusing import: HOME is unavailable, so the no-save registry cannot be checked"))?;
    for id in ids.iter().filter(|id| session_id_is_safe(id)) {
        match std::fs::symlink_metadata(dir.join(id)) {
            Ok(_) => return Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(eyre!("cannot check the no-save registry for {id}: {e}")),
        }
    }
    Ok(false)
}

/// The ids a Codex session may be registered under: its rollout stem (the
/// wrappers) and its thread id (the hooks).
fn codex_ids(stem: &str) -> Vec<&str> {
    std::iter::once(stem).chain(codex_thread_id(stem)).collect()
}

/// The thread id that ends a Codex rollout's file stem
/// (`rollout-<timestamp>-<uuid>`): the id Codex hooks report.
fn codex_thread_id(stem: &str) -> Option<&str> {
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    let shaped = id
        .char_indices()
        .all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_hexdigit() });
    shaped.then_some(id)
}

/// The cc-clinical session registry: one marker file per clinical session id.
pub fn claude_clinical_registry() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        eyre!("Refusing Claude Code import: HOME is unavailable, so the clinical-session registry cannot be checked")
    })?;
    Ok(PathBuf::from(home).join(".local/share/continuum/claude-clinical-sessions"))
}

/// Continuum must stay PHI-free, so a session registered by cc-clinical is
/// never imported. Fails closed: with no registry to consult, refuse.
pub fn ensure_claude_session_importable(session_path: &Path, registry: &Path) -> Result<()> {
    if !registry.is_dir() {
        return Err(eyre!(
            "Refusing Claude Code import: clinical-session registry is unavailable at {}",
            registry.display()
        ));
    }
    let session_id = session_path
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|id| session_id_is_safe(id))
        .ok_or_else(|| eyre!("Refusing Claude Code import: invalid session path"))?;
    // Only a confirmed absence of the marker allows the import. Any marker,
    // of any type, refuses it, and so does any error checking for one.
    match std::fs::symlink_metadata(registry.join(session_id)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(eyre!("Refusing to import protected cc-clinical session {session_id}")),
        Err(e) => Err(eyre!(
            "Refusing Claude Code import: cannot check the clinical-session registry for {session_id}: {e}"
        )),
    }
}

/// A session id that is safe to use as one path component.
pub fn session_id_is_safe(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 128
        && session_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The rollout file for a Codex session id under `sessions_dir`
/// (`YYYY/MM/DD/rollout-<timestamp>-<session id>.jsonl`), newest first.
pub fn find_codex_rollout(sessions_dir: &Path, session_id: &str) -> Option<PathBuf> {
    if !session_id_is_safe(session_id) {
        return None;
    }
    let suffix = format!("-{session_id}.jsonl");
    let mut dirs = vec![sessions_dir.to_path_buf()];
    let mut found: Vec<PathBuf> = Vec::new();
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("rollout-") && n.ends_with(&suffix)) {
                found.push(path);
            }
        }
    }
    found.sort();
    found.pop()
}

fn file_stem(path: &Path) -> Result<String> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .ok_or_else(|| eyre!("Not a session file: {}", path.display()))
}

/// Feed each JSONL record of `path` to `f`; returns how many lines were
/// skipped. An unparseable line is tolerated only as the last line, where it
/// is a record still being written. Anywhere else the import is abandoned
/// before anything is written, so a damaged transcript can never replace a
/// good stored copy with a shorter one.
fn for_each_record<T: serde::de::DeserializeOwned>(path: &Path, mut f: impl FnMut(T)) -> Result<usize> {
    let file = std::fs::File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut bad: Option<(usize, serde_json::Error)> = None;
    for (n, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some((at, e)) = bad.take() {
            return Err(eyre!("{} line {}: {e}; not importing a damaged transcript", path.display(), at + 1));
        }
        match serde_json::from_str::<T>(&line) {
            Ok(record) => f(record),
            Err(e) => bad = Some((n, e)),
        }
    }
    Ok(usize::from(bad.is_some()))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... [truncated]", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn claude_line(kind: &str, message: serde_json::Value) -> String {
        serde_json::json!({"type": kind, "message": message, "timestamp": "2026-10-06T21:00:00.000Z"}).to_string()
    }

    fn write_transcript(dir: &Path, name: &str, lines: &[String]) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        path
    }

    fn transcript(dir: &Path) -> PathBuf {
        write_transcript(
            dir,
            "0b9c6b0e-0000-4000-8000-000000000001.jsonl",
            &[
                claude_line("user", serde_json::json!({"role": "user", "content": "first question about widgets"})),
                claude_line(
                    "assistant",
                    serde_json::json!({"role": "assistant", "content": [
                        {"type": "text", "text": "widgets answer"},
                        {"type": "tool_use", "name": "Skill", "input": {"skill": "senior-dev"}}
                    ]}),
                ),
            ],
        )
    }

    fn stored(outcome: &ImportOutcome) -> Vec<String> {
        std::fs::read_to_string(outcome.dir.join("messages.jsonl")).unwrap().lines().map(str::to_string).collect()
    }

    #[test]
    fn reimport_of_an_unchanged_claude_session_writes_nothing_and_never_duplicates() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let writer = PlainTextWriter::with_base_dir(out.path().to_path_buf());
        let path = transcript(src.path());

        let first = import_claude_code(&writer, &path).unwrap().stored().unwrap();
        assert!(first.written);
        let lines = stored(&first);
        for _ in 0..3 {
            let again = import_claude_code(&writer, &path).unwrap().stored().unwrap();
            assert!(!again.written, "unchanged session must not be rewritten");
            assert_eq!(stored(&again), lines, "re-import must leave exactly one copy");
        }
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(first.dir.join("session.json")).unwrap()).unwrap();
        assert_eq!(meta["skills"], serde_json::json!(["senior-dev"]));
        assert_eq!(meta["message_count"], serde_json::json!(lines.len()));
    }

    #[test]
    fn a_grown_claude_session_is_replaced_not_appended() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let writer = PlainTextWriter::with_base_dir(out.path().to_path_buf());
        let path = transcript(src.path());
        let first = import_claude_code(&writer, &path).unwrap().stored().unwrap();

        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{}", claude_line("user", serde_json::json!({"role": "user", "content": "second question about gears"}))).unwrap();
        // A half-written final line is skipped, not fatal.
        write!(f, "{{\"type\":\"assistant\",\"mess").unwrap();
        drop(f);

        let second = import_claude_code(&writer, &path).unwrap().stored().unwrap();
        assert!(second.written);
        assert_eq!(second.skipped_lines, 1);
        assert_eq!(second.message_count, first.message_count + 1);
        let lines = stored(&second);
        assert_eq!(lines.len(), second.message_count, "one line per message, no duplicates");
        assert!(lines.last().unwrap().contains("second question about gears"));
        let leftovers: Vec<_> = std::fs::read_dir(&second.dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no temp files left behind");
    }

    fn with_uuid(line: &str, uuid: &str) -> String {
        let mut v: serde_json::Value = serde_json::from_str(line).unwrap();
        v["uuid"] = serde_json::json!(uuid);
        v.to_string()
    }

    #[test]
    fn duplicates_are_judged_by_event_uuid_not_by_text() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let writer = PlainTextWriter::with_base_dir(out.path().to_path_buf());
        let q = claude_line("user", serde_json::json!({"role": "user", "content": "repeatable question about widgets"}));
        let path = write_transcript(
            src.path(),
            "0b9c6b0e-0000-4000-8000-000000000002.jsonl",
            &[
                with_uuid(&q, "u-1"),
                with_uuid(&q, "u-2"), // the same words sent again: a second turn
                with_uuid(&q, "u-1"), // the same event written twice: one turn
            ],
        );
        let outcome = import_claude_code(&writer, &path).unwrap().stored().unwrap();
        assert_eq!(outcome.message_count, 2);
    }

    #[test]
    fn entries_without_a_uuid_are_all_kept() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let writer = PlainTextWriter::with_base_dir(out.path().to_path_buf());
        let q = claude_line("user", serde_json::json!({"role": "user", "content": "repeatable question about widgets"}));
        let path = write_transcript(src.path(), "0b9c6b0e-0000-4000-8000-000000000008.jsonl", &[q.clone(), q.clone(), q]);
        let outcome = import_claude_code(&writer, &path).unwrap().stored().unwrap();
        assert_eq!(outcome.message_count, 3, "identical text in separate events is not a duplicate");
    }

    #[test]
    fn a_damaged_interior_line_aborts_and_leaves_the_stored_copy_alone() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let writer = PlainTextWriter::with_base_dir(out.path().to_path_buf());
        let path = transcript(src.path());
        let first = import_claude_code(&writer, &path).unwrap().stored().unwrap();
        let before = stored(&first);

        let good = claude_line("user", serde_json::json!({"role": "user", "content": "later question about gears"}));
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{{not json\n{text}{good}\n")).unwrap();

        assert!(import_claude_code(&writer, &path).is_err(), "interior damage must abort the import");
        assert_eq!(stored(&first), before, "the stored copy must be untouched");
    }

    #[test]
    fn long_multibyte_tool_results_truncate_on_a_character_boundary() {
        let s = "é".repeat(400); // 800 bytes; byte 500 falls mid-character
        let t = truncate(&s, 501);
        assert!(t.ends_with("... [truncated]"));
    }

    #[test]
    fn clinical_registry_refuses_marked_sessions_and_fails_closed() {
        let home = tempfile::tempdir().unwrap();
        let registry = home.path().join("registry");
        let session = home.path().join("0b9c6b0e-0000-4000-8000-000000000003.jsonl");

        assert!(
            ensure_claude_session_importable(&session, &registry).is_err(),
            "missing registry must refuse (fail closed)"
        );
        std::fs::create_dir_all(&registry).unwrap();
        assert!(ensure_claude_session_importable(&session, &registry).is_ok(), "unmarked session imports");
        std::fs::write(registry.join("0b9c6b0e-0000-4000-8000-000000000003"), "").unwrap();
        assert!(ensure_claude_session_importable(&session, &registry).is_err(), "marked session is refused");

        let other = home.path().join("0b9c6b0e-0000-4000-8000-000000000009.jsonl");
        std::fs::create_dir(registry.join("0b9c6b0e-0000-4000-8000-000000000009")).unwrap();
        assert!(ensure_claude_session_importable(&other, &registry).is_err(), "a marker of any type refuses");
    }

    #[test]
    fn an_unreadable_registry_refuses_rather_than_assuming_no_marker() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let registry = home.path().join("registry");
        std::fs::create_dir(&registry).unwrap();
        std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o000)).unwrap();
        let session = home.path().join("0b9c6b0e-0000-4000-8000-00000000000a.jsonl");
        let result = ensure_claude_session_importable(&session, &registry);
        std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err(), "a marker that cannot be checked must refuse the import");
    }

    #[test]
    fn codex_import_is_idempotent_and_dated_by_the_session_start() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let writer = PlainTextWriter::with_base_dir(out.path().to_path_buf());
        let line = |role: &str, text: &str| {
            serde_json::json!({"type": "response_item", "timestamp": "2026-09-29T08:00:00.000Z",
                "payload": {"role": role, "content": [{"type": "input_text", "text": text}]}})
            .to_string()
        };
        let path = write_transcript(
            src.path(),
            "rollout-2026-09-29T08-00-00-019a0000-0000-7000-8000-000000000004.jsonl",
            &[line("user", "plan the journey to the coast"), line("assistant", "here is a plan for the coast")],
        );
        let (first, _) = import_codex(&writer, &path).unwrap().stored().unwrap();
        assert!(first.written);
        assert!(first.dir.to_string_lossy().contains("/codex/2026-09-29/"), "filed under its own start date");
        let (again, _) = import_codex(&writer, &path).unwrap().stored().unwrap();
        assert!(!again.written);
        assert_eq!(stored(&again).len(), first.message_count);
    }

    #[test]
    fn a_registered_nosave_claude_session_is_never_stored() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let writer = PlainTextWriter::with_base_dir(out.path().join("logs")).with_nosave_dir(out.path().join("nosave"));
        let path = transcript(src.path());
        let marker = register_nosave(&writer, "0b9c6b0e-0000-4000-8000-000000000001").unwrap();
        register_nosave(&writer, "0b9c6b0e-0000-4000-8000-000000000001").unwrap(); // twice is fine
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&marker).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(out.path().join("nosave")).unwrap().permissions().mode() & 0o777, 0o700);
        }
        assert!(matches!(import_claude_code(&writer, &path).unwrap(), Imported::NoSave));
        assert!(!out.path().join("logs").exists(), "nothing written for a no-save session");

        std::fs::remove_file(&marker).unwrap();
        assert!(matches!(import_claude_code(&writer, &path).unwrap(), Imported::Stored(_)), "known green: unmarked imports");
    }

    #[test]
    fn a_nosave_codex_session_is_found_by_its_thread_id() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let writer = PlainTextWriter::with_base_dir(out.path().join("logs")).with_nosave_dir(out.path().join("nosave"));
        let thread = "019a0000-0000-7000-8000-00000000000b";
        let line = serde_json::json!({"type": "response_item", "timestamp": "2026-10-07T08:00:00.000Z",
            "payload": {"role": "user", "content": [{"type": "input_text", "text": "a private question"}]}});
        let path = write_transcript(src.path(), &format!("rollout-2026-10-07T08-00-00-{thread}.jsonl"), &[line.to_string()]);
        register_nosave(&writer, thread).unwrap();
        assert!(matches!(import_codex(&writer, &path).unwrap(), Imported::NoSave));
        assert!(!out.path().join("logs").exists());
    }

    #[test]
    fn an_unreadable_nosave_registry_is_an_error_not_a_silent_import() {
        use std::os::unix::fs::PermissionsExt;
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let registry = out.path().join("nosave");
        std::fs::create_dir(&registry).unwrap();
        let writer = PlainTextWriter::with_base_dir(out.path().join("logs")).with_nosave_dir(registry.clone());
        let path = transcript(src.path());
        std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = import_claude_code(&writer, &path);
        std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err(), "{result:?}");
        assert!(!out.path().join("logs").exists());
    }

    #[test]
    fn discard_registers_and_removes_every_stored_copy() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let writer = PlainTextWriter::with_base_dir(out.path().join("logs")).with_nosave_dir(out.path().join("nosave"));
        let path = transcript(src.path());
        let id = "0b9c6b0e-0000-4000-8000-000000000001";
        let stored_copy = import_claude_code(&writer, &path).unwrap().stored().unwrap().dir;
        let stray = out.path().join("logs/claude-code/2026-01-01").join(id);
        std::fs::create_dir_all(&stray).unwrap();
        let unrelated = out.path().join("logs/claude-code/2026-01-01/0b9c6b0e-0000-4000-8000-0000000000ff");
        std::fs::create_dir_all(&unrelated).unwrap();

        assert_eq!(discard_session(&writer, "claude-code", id).unwrap(), 2);
        assert!(!stored_copy.exists() && !stray.exists());
        assert!(unrelated.exists(), "other sessions are untouched");
        assert!(matches!(import_claude_code(&writer, &path).unwrap(), Imported::NoSave), "and it stays out");
    }

    #[test]
    fn with_no_registry_to_consult_the_import_refuses() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let mut writer = PlainTextWriter::with_base_dir(out.path().join("logs"));
        writer.clear_nosave_dir_for_test();
        assert!(import_claude_code(&writer, &transcript(src.path())).is_err());
        assert!(!out.path().join("logs").exists());
    }

    #[test]
    fn codex_thread_ids_come_from_the_end_of_the_rollout_stem() {
        assert_eq!(
            codex_thread_id("rollout-2026-10-07T08-00-00-019a0000-0000-7000-8000-00000000000b"),
            Some("019a0000-0000-7000-8000-00000000000b")
        );
        assert_eq!(codex_thread_id("rollout-2026-10-07T08-00-00"), None);
        assert_eq!(codex_thread_id("short"), None);
        assert_eq!(codex_thread_id("é".repeat(20).as_str()), None);
        assert!(register_nosave(&PlainTextWriter::with_base_dir("/nonexistent".into()), "../etc").is_err());
    }

    #[test]
    fn codex_rollouts_are_found_by_session_id_only() {
        let root = tempfile::tempdir().unwrap();
        let day = root.path().join("2026/10/06");
        std::fs::create_dir_all(&day).unwrap();
        let id = "019a0000-0000-7000-8000-000000000005";
        let want = day.join(format!("rollout-2026-10-06T23-00-00-{id}.jsonl"));
        std::fs::write(&want, "").unwrap();
        std::fs::write(day.join("rollout-2026-10-06T23-00-00-019a0000-0000-7000-8000-000000000006.jsonl"), "").unwrap();
        assert_eq!(find_codex_rollout(root.path(), id), Some(want));
        assert_eq!(find_codex_rollout(root.path(), "../etc"), None);
    }
}
