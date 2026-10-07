// Continuum CLI - Plain-Text Assistant Log Management
// Manages conversation logs stored as JSONL files in ~/Assistants/continuum-logs

use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

use clap::{Args, Parser, Subcommand};
use color_eyre::{eyre::Context, Result};
use continuum_core::adapters::claude_code::ClaudeCodeAdapter;
use continuum_core::adapters::codex::CodexAdapter;
use continuum_core::adapters::goose::{parse_goose_content, GooseAdapter};
use continuum_core::import::{self, Imported};
use continuum_core::{LogAdapter, LoopSeverity, MessageCompressor, PlainTextWriter};

fn main() -> Result<()> {
    color_eyre::install()?;
    // A Stop hook that exits 2 tells Claude Code and Codex to continue the
    // turn, and clap exits 2 on any usage error (and prints help on stdout).
    // In hook mode, report a parse failure on stderr and exit 1 instead.
    let hook_mode = std::env::args().any(|a| a == "--hook");
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) if hook_mode => {
            eprintln!("continuum hook: {e}");
            std::process::exit(1);
        }
        Err(e) => e.exit(),
    };
    match &cli.command {
        Command::Import(cmd) => handle_import(cmd)?,
        Command::Stats => handle_stats()?,
        Command::Codex(cmd) => handle_codex(cmd)?,
    }
    Ok(())
}

#[derive(Parser, Debug)]
#[command(
    name = "continuum",
    author,
    version,
    about = "Continuum: Plain-text assistant conversation logs",
    long_about = "Manage assistant conversations as plain-text JSONL files.\nUse Nushell functions for querying: continuum-search, continuum-timeline, continuum-stats"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Import sessions from assistant native logs to plain-text JSONL
    Import(ImportArgs),
    /// Show statistics about stored conversations
    Stats,
    /// Manage the real Codex CLI used by Continuum
    Codex(CodexArgs),
}

#[derive(Args, Debug)]
struct CodexArgs {
    #[command(subcommand)]
    command: CodexCommand,
}

#[derive(Subcommand, Debug)]
enum CodexCommand {
    /// Show the resolved real Codex CLI and its ownership source
    Which,
    /// Install or update the Continuum-managed Codex CLI
    Update,
}

fn handle_codex(args: &CodexArgs) -> Result<()> {
    match args.command {
        CodexCommand::Which => {
            let resolution = continuum_core::codex_cli::resolve_codex(None)?;
            println!("path: {}", resolution.path.display());
            println!("source: {}", resolution.source);
            println!("managed: {}", resolution.is_managed());
            println!(
                "managed target: {}",
                continuum_core::codex_cli::managed_codex_bin()?.display()
            );
        }
        CodexCommand::Update => update_managed_codex()?,
    }
    Ok(())
}

fn update_managed_codex() -> Result<()> {
    let prefix = continuum_core::codex_cli::managed_codex_prefix()?;
    std::fs::create_dir_all(&prefix)
        .with_context(|| format!("failed to create {}", prefix.display()))?;
    let _lock = UpdateLock::acquire(prefix.join(".update.lock"))?;
    eprintln!(
        "Installing @openai/codex@latest into {}...",
        prefix.display()
    );
    let status = ProcessCommand::new("npm")
        .args(["install", "--global", "--prefix"])
        .arg(&prefix)
        .arg("@openai/codex@latest")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to launch npm")?;
    if !status.success() {
        color_eyre::eyre::bail!("npm failed with status {status}");
    }

    let resolution = continuum_core::codex_cli::resolve_codex(None)?;
    if !resolution.is_managed() {
        color_eyre::eyre::bail!(
            "managed install completed but resolver selected {} from {}",
            resolution.path.display(),
            resolution.source
        );
    }
    let version = ProcessCommand::new(&resolution.path)
        .arg("--version")
        .output()
        .context("failed to verify managed Codex")?;
    if !version.status.success() {
        color_eyre::eyre::bail!("managed Codex failed its version check");
    }
    print!("{}", String::from_utf8_lossy(&version.stdout));
    eprintln!("Managed Codex ready at {}", resolution.path.display());
    Ok(())
}

struct UpdateLock {
    path: PathBuf,
}

impl UpdateLock {
    fn acquire(path: PathBuf) -> Result<Self> {
        use std::io::Write;

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| {
                format!(
                    "another Codex update may be running (lock: {}); if no updater is active, remove this stale lock",
                    path.display()
                )
            })?;
        writeln!(file, "pid={}", std::process::id())?;
        Ok(Self { path })
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Args, Debug)]
struct ImportArgs {
    /// Assistant to import from (claude-code, codex, goose)
    #[arg(short, long)]
    assistant: String,
    /// Session file to import (uses adapter's latest if not specified)
    #[arg(short, long)]
    session: Option<String>,
    /// Output directory (default: $CONTINUUM_HOME/continuum-logs, i.e. ~/Assistants/continuum-logs)
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Run as the agent's SessionStart or per-turn Stop hook (claude-code,
    /// codex): read the hook's JSON from stdin and import that one session,
    /// or register it as no-save in a no-save launch. Prints nothing on
    /// stdout; exits 0 or 1, never 2.
    #[arg(long, conflicts_with = "session")]
    hook: bool,
}

fn handle_import(args: &ImportArgs) -> Result<()> {
    let writer = if let Some(ref output) = args.output {
        PlainTextWriter::with_base_dir(output.clone())
    } else {
        PlainTextWriter::new()?
    };

    let adapter_name = args.assistant.to_lowercase();

    if args.hook {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let stdin = std::io::read_to_string(std::io::stdin()).unwrap_or_default();
        let launch = Launch {
            clinical: std::env::var_os("CC_CLINICAL_PID").is_some_and(|v| !v.is_empty()),
            nosave: std::env::var_os(import::NOSAVE_ENV).is_some_and(|v| v == "1"),
        };
        let codex_home = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()).map(PathBuf::from);
        let code = match run_hook(&adapter_name, &stdin, home.as_deref(), launch, codex_home.as_deref(), &writer) {
            Ok(HookResult::Imported(note)) | Ok(HookResult::Skipped(note)) => {
                eprintln!("continuum hook: {note}");
                0
            }
            Err(e) => {
                eprintln!("continuum hook: {e:#}");
                1
            }
        };
        std::process::exit(code);
    }

    match adapter_name.as_str() {
        "codex" => {
            let adapter = CodexAdapter::new();
            import_codex_session(&writer, &adapter, args)
        }
        "goose" => {
            let adapter = GooseAdapter::new()?;
            import_goose_session(&writer, &adapter, args)
        }
        "claude-code" => {
            let adapter = ClaudeCodeAdapter::new();
            import_claude_code_session(&writer, &adapter, args)
        }
        _ => {
            eprintln!(
                "Error: Unknown assistant '{}'. Supported: codex, goose, claude-code",
                args.assistant
            );
            std::process::exit(1);
        }
    }
}

fn import_codex_session(
    writer: &PlainTextWriter,
    adapter: &CodexAdapter,
    args: &ImportArgs,
) -> Result<()> {
    let session_path = if let Some(ref session) = args.session {
        PathBuf::from(session)
    } else {
        adapter.find_latest_session()?
    };

    eprintln!("Importing Codex session: {}", session_path.display());

    let (outcome, detections) = match import::import_codex(writer, &session_path)? {
        Imported::Stored(found) => found,
        Imported::Empty => {
            eprintln!("⚠ No messages found in Codex session: {}", session_path.display());
            return Ok(());
        }
        Imported::NoSave => {
            eprintln!("= Codex session is marked no-save; not imported: {}", session_path.display());
            return Ok(());
        }
    };

    if !detections.is_empty() {
        eprintln!("\n⚠️  LOOP DETECTION WARNINGS ⚠️");
        eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        for detection in &detections {
            let icon = match detection.severity {
                LoopSeverity::Warning => "⚠️ ",
                LoopSeverity::Critical => "🚨",
            };
            eprintln!("{} {}", icon, detection.message);
        }
        eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
    }

    report("Codex", &outcome);
    Ok(())
}

fn import_goose_session(
    writer: &PlainTextWriter,
    adapter: &GooseAdapter,
    args: &ImportArgs,
) -> Result<()> {
    let session_path = if let Some(ref session) = args.session {
        // User provided session ID, construct pseudo-path
        let home = std::env::var("HOME").context("HOME not set")?;
        let db_path = PathBuf::from(home).join(".local/share/goose/sessions/sessions.db");
        PathBuf::from(format!("{}#{}", db_path.display(), session))
    } else {
        adapter.find_latest_session()?
    };

    // Extract session ID from pseudo-path
    let path_str = session_path.to_string_lossy();
    let session_id = if let Some(hash_pos) = path_str.rfind('#') {
        &path_str[hash_pos + 1..]
    } else {
        eprintln!("Error: Invalid Goose session path");
        std::process::exit(1);
    };

    eprintln!("Importing Goose session: {}", session_id);

    let compressor = MessageCompressor::new();
    let mut messages: Vec<(String, String)> = Vec::new();
    let start_time = chrono::Utc::now().to_rfc3339();

    // Read all messages
    for msg_result in adapter.stream_session(&session_path)? {
        let msg_json = msg_result?;

        #[derive(serde::Deserialize)]
        struct GooseMessage {
            role: String,
            content_json: String,
        }

        let msg: GooseMessage = serde_json::from_str(&msg_json)?;
        let content = parse_goose_content(&msg.content_json)?;

        if !content.is_empty() {
            messages.push((msg.role, content));
        }
    }

    // Compress messages
    let compressed = compressor.compress_batch(&messages);
    let message_count = compressed.len();

    if message_count == 0 {
        eprintln!("⚠ No messages found in Goose session: {}", session_id);
        return Ok(());
    }

    // Extract date
    let date = PlainTextWriter::extract_date(Some(&start_time));

    // Write session
    writer.write_session(
        session_id,
        "goose",
        Some(&start_time),
        None,
        "closed",
        message_count,
        &[],
    )?;

    // Write messages
    for (idx, (role, content)) in compressed.iter().enumerate() {
        writer.append_message(
            session_id,
            "goose",
            &date,
            idx + 1,
            role,
            content,
            Some(&start_time),
        )?;
    }

    println!(
        "✓ Imported {} messages from Goose session: {}",
        message_count, session_id
    );
    println!(
        "  Location: {}",
        writer
            .base_dir()
            .join("goose")
            .join(&date)
            .join(session_id)
            .display()
    );

    Ok(())
}

fn import_claude_code_session(
    writer: &PlainTextWriter,
    adapter: &ClaudeCodeAdapter,
    args: &ImportArgs,
) -> Result<()> {
    let session_path = if let Some(ref session) = args.session {
        PathBuf::from(session)
    } else {
        adapter.find_latest_session()?
    };

    import::ensure_claude_session_importable(&session_path, &import::claude_clinical_registry()?)?;

    eprintln!("Importing Claude Code session: {}", session_path.display());

    match import::import_claude_code(writer, &session_path)? {
        Imported::Stored(outcome) => report("Claude Code", &outcome),
        Imported::Empty => eprintln!("⚠ No messages found in Claude Code session: {}", session_path.display()),
        Imported::NoSave => eprintln!("= Claude Code session is marked no-save; not imported: {}", session_path.display()),
    }
    Ok(())
}

fn report(label: &str, outcome: &import::ImportOutcome) {
    if outcome.written {
        println!(
            "✓ Imported {} messages from {} session: {}",
            outcome.message_count, label, outcome.session_id
        );
    } else {
        println!(
            "= {} session {} already imported ({} messages); nothing to do",
            label, outcome.session_id, outcome.message_count
        );
    }
    if outcome.skipped_lines > 0 {
        eprintln!("  ({} unreadable line(s) skipped)", outcome.skipped_lines);
    }
    println!("  Location: {}", outcome.dir.display());
}

/// What the hook payload carries that we use (Claude Code and Codex both
/// send these fields; everything else is ignored).
#[derive(serde::Deserialize)]
struct HookInput {
    session_id: String,
    transcript_path: Option<String>,
    hook_event_name: Option<String>,
}

/// How the agent was launched, from the environment the hook inherits.
#[derive(Clone, Copy, Default)]
struct Launch {
    /// Started by cc-clinical (`CC_CLINICAL_PID`).
    clinical: bool,
    /// Started by a wrapper in no-save mode (`CONTINUUM_NOSAVE=1`).
    nosave: bool,
}

#[derive(Debug, PartialEq)]
enum HookResult {
    Imported(String),
    Skipped(String),
}

/// The SessionStart and per-turn Stop hook. Stop imports the session that
/// just finished a turn, so a crash or power cut loses at most the turn in
/// progress. In a no-save launch either event registers the session as
/// no-save instead. SessionStart fires on startup, resume, /clear and
/// compaction, so a session is registered before the 5-minute sync could
/// read it, and a /clear's new session id is covered too.
///
/// `Skipped` covers sessions that must not or cannot be imported (a
/// cc-clinical session, a transcript outside the agent's own store); `Err`
/// is a real failure. The caller maps both to exit 0 and 1 — never 2, which
/// Claude Code and Codex read from a Stop hook as "continue the turn".
fn run_hook(
    assistant: &str,
    stdin: &str,
    home: Option<&Path>,
    launch: Launch,
    codex_home: Option<&Path>,
    writer: &PlainTextWriter,
) -> Result<HookResult> {
    let home = home.ok_or_else(|| color_eyre::eyre::eyre!("HOME is not set"))?;
    let input: HookInput = serde_json::from_str(stdin).context("hook payload is not the expected JSON")?;
    if !import::session_id_is_safe(&input.session_id) {
        color_eyre::eyre::bail!("unusable session id in hook payload");
    }
    if !matches!(assistant, "claude-code" | "codex") {
        color_eyre::eyre::bail!("--hook supports claude-code and codex, not {assistant}");
    }
    if launch.nosave {
        let marker = import::register_nosave(writer, &input.session_id)?;
        return Ok(HookResult::Skipped(format!("no-save session, registered at {}", marker.display())));
    }
    if input.hook_event_name.as_deref() == Some("SessionStart") {
        return Ok(HookResult::Skipped("session start; nothing to import yet".into()));
    }

    match assistant {
        "claude-code" => {
            // Continuum stays PHI-free. A cc-clinical launch is refused before
            // anything is read; the registry check below is the second guard.
            if launch.clinical {
                return Ok(HookResult::Skipped("cc-clinical session, not imported".into()));
            }
            let path = input
                .transcript_path
                .map(PathBuf::from)
                .ok_or_else(|| color_eyre::eyre::eyre!("Stop hook payload has no transcript_path"))?;
            let projects = home.join(".claude/projects");
            let inside = match (path.canonicalize(), projects.canonicalize()) {
                (Ok(p), Ok(root)) => p.starts_with(root),
                _ => false,
            };
            let stem_matches = path.file_stem().and_then(|s| s.to_str()) == Some(input.session_id.as_str());
            if !inside || !stem_matches || path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                return Ok(HookResult::Skipped(format!(
                    "transcript is not this session's file under {}",
                    projects.display()
                )));
            }
            let registry = home.join(".local/share/continuum/claude-clinical-sessions");
            if let Err(refusal) = import::ensure_claude_session_importable(&path, &registry) {
                return Ok(HookResult::Skipped(refusal.to_string()));
            }
            Ok(match import::import_claude_code(writer, &path)? {
                Imported::Stored(o) => HookResult::Imported(describe(&o)),
                Imported::Empty => HookResult::Skipped("no messages yet".into()),
                Imported::NoSave => HookResult::Skipped("no-save session, not imported".into()),
            })
        }
        "codex" => {
            let sessions = codex_home.map(Path::to_path_buf).unwrap_or_else(|| home.join(".codex")).join("sessions");
            let path = input
                .transcript_path
                .map(PathBuf::from)
                .filter(|p| p.is_file())
                .or_else(|| import::find_codex_rollout(&sessions, &input.session_id))
                .ok_or_else(|| color_eyre::eyre::eyre!("no rollout file for Codex session {}", input.session_id))?;
            let stem_ok = path.file_stem().and_then(|s| s.to_str()).is_some_and(import::session_id_is_safe);
            if !stem_ok {
                color_eyre::eyre::bail!("unusable rollout file name: {}", path.display());
            }
            Ok(match import::import_codex(writer, &path)? {
                Imported::Stored((o, _)) => HookResult::Imported(describe(&o)),
                Imported::Empty => HookResult::Skipped("no messages yet".into()),
                Imported::NoSave => HookResult::Skipped("no-save session, not imported".into()),
            })
        }
        _ => unreachable!("assistant checked above"),
    }
}

fn describe(o: &import::ImportOutcome) -> String {
    format!(
        "{} {} ({} messages)",
        if o.written { "saved" } else { "unchanged" },
        o.session_id,
        o.message_count
    )
}

fn handle_stats() -> Result<()> {
    println!("\n📊 Continuum Statistics\n");
    println!("To view detailed statistics, use the Nushell function:");
    println!("  continuum-stats\n");
    println!("To search conversations:");
    println!("  continuum-search \"your query\"\n");
    println!("To view timeline:");
    println!("  continuum-timeline 2025-11-09\n");
    let log_root = std::env::var("CONTINUUM_HOME").unwrap_or_else(|_| "~/Assistants".to_string());
    println!("📍 Log location: {log_root}/continuum-logs/\n");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{run_hook, HookResult, Launch};
    use continuum_core::import::Imported;
    use continuum_core::import::ensure_claude_session_importable;
    use continuum_core::PlainTextWriter;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    #[test]
    fn claude_import_fails_closed_without_clinical_registry() {
        let temp = tempdir().unwrap();
        let missing_registry = temp.path().join("missing");
        let session = temp
            .path()
            .join("11111111-1111-1111-1111-111111111111.jsonl");

        let error = ensure_claude_session_importable(&session, &missing_registry).unwrap_err();
        assert!(error.to_string().contains("registry is unavailable"));
    }

    #[test]
    fn claude_import_refuses_registered_clinical_session() {
        let temp = tempdir().unwrap();
        let registry = temp.path().join("registry");
        fs::create_dir(&registry).unwrap();
        fs::write(registry.join("11111111-1111-1111-1111-111111111111"), []).unwrap();
        let session = temp
            .path()
            .join("11111111-1111-1111-1111-111111111111.jsonl");

        let error = ensure_claude_session_importable(&session, &registry).unwrap_err();
        assert!(error.to_string().contains("protected cc-clinical session"));
    }

    #[test]
    fn claude_import_allows_unregistered_session() {
        let temp = tempdir().unwrap();
        let registry = temp.path().join("registry");
        fs::create_dir(&registry).unwrap();
        let session = temp
            .path()
            .join("22222222-2222-2222-2222-222222222222.jsonl");

        ensure_claude_session_importable(&session, &registry).unwrap();
    }

    const ID: &str = "33333333-3333-4333-8333-333333333333";

    /// A fake home with a Claude transcript, the clinical registry and an
    /// output tree. Returns (home, transcript, writer).
    fn claude_home() -> (tempfile::TempDir, PathBuf, PlainTextWriter) {
        let home = tempdir().unwrap();
        let project = home.path().join(".claude/projects/-home-x");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(home.path().join(".local/share/continuum/claude-clinical-sessions")).unwrap();
        let transcript = project.join(format!("{ID}.jsonl"));
        let line = serde_json::json!({"type": "user", "timestamp": "2026-10-06T22:00:00.000Z",
            "message": {"role": "user", "content": "a question about gearboxes"}});
        fs::write(&transcript, format!("{line}\n")).unwrap();
        let writer = PlainTextWriter::with_base_dir(home.path().join("out")).with_nosave_dir(home.path().join("nosave"));
        (home, transcript, writer)
    }

    fn payload(transcript: &Path) -> String {
        serde_json::json!({"session_id": ID, "transcript_path": transcript, "hook_event_name": "Stop",
            "stop_hook_active": false})
        .to_string()
    }

    fn stored(home: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut dirs = vec![home.join("out")];
        while let Some(d) = dirs.pop() {
            for e in fs::read_dir(&d).into_iter().flatten().flatten() {
                if e.path().is_dir() {
                    dirs.push(e.path());
                } else if e.file_name() == "messages.jsonl" {
                    found.push(e.path());
                }
            }
        }
        found
    }

    #[test]
    fn hook_imports_an_ordinary_claude_session_once() {
        let (home, transcript, writer) = claude_home();
        let first = run_hook("claude-code", &payload(&transcript), Some(home.path()), Launch::default(), None, &writer).unwrap();
        assert!(matches!(first, HookResult::Imported(ref s) if s.starts_with("saved")), "{first:?}");
        let again = run_hook("claude-code", &payload(&transcript), Some(home.path()), Launch::default(), None, &writer).unwrap();
        assert!(matches!(again, HookResult::Imported(ref s) if s.starts_with("unchanged")), "{again:?}");
        let files = stored(home.path());
        assert_eq!(files.len(), 1);
        assert_eq!(fs::read_to_string(&files[0]).unwrap().lines().count(), 1);
    }

    #[test]
    fn hook_never_imports_a_cc_clinical_launch() {
        let (home, transcript, writer) = claude_home();
        let r = run_hook("claude-code", &payload(&transcript), Some(home.path()), Launch { clinical: true, nosave: false }, None, &writer).unwrap();
        assert!(matches!(r, HookResult::Skipped(_)));
        assert!(stored(home.path()).is_empty(), "nothing may be written for a clinical launch");
    }

    #[test]
    fn hook_never_imports_a_registered_clinical_session() {
        let (home, transcript, writer) = claude_home();
        fs::write(home.path().join(".local/share/continuum/claude-clinical-sessions").join(ID), "").unwrap();
        let r = run_hook("claude-code", &payload(&transcript), Some(home.path()), Launch::default(), None, &writer).unwrap();
        assert!(matches!(r, HookResult::Skipped(ref s) if s.contains("protected cc-clinical")), "{r:?}");
        assert!(stored(home.path()).is_empty());
    }

    #[test]
    fn hook_fails_closed_without_the_clinical_registry() {
        let (home, transcript, writer) = claude_home();
        fs::remove_dir(home.path().join(".local/share/continuum/claude-clinical-sessions")).unwrap();
        let r = run_hook("claude-code", &payload(&transcript), Some(home.path()), Launch::default(), None, &writer).unwrap();
        assert!(matches!(r, HookResult::Skipped(ref s) if s.contains("registry is unavailable")), "{r:?}");
        assert!(stored(home.path()).is_empty());
    }

    #[test]
    fn hook_refuses_a_transcript_outside_the_claude_store_or_for_another_session() {
        let (home, transcript, writer) = claude_home();
        let elsewhere = home.path().join(format!("{ID}.jsonl"));
        fs::copy(&transcript, &elsewhere).unwrap();
        let r = run_hook("claude-code", &payload(&elsewhere), Some(home.path()), Launch::default(), None, &writer).unwrap();
        assert!(matches!(r, HookResult::Skipped(_)), "{r:?}");

        let other = transcript.with_file_name("44444444-4444-4444-8444-444444444444.jsonl");
        fs::copy(&transcript, &other).unwrap();
        let r = run_hook("claude-code", &payload(&other), Some(home.path()), Launch::default(), None, &writer).unwrap();
        assert!(matches!(r, HookResult::Skipped(_)), "a payload naming one session must not import another: {r:?}");
        assert!(stored(home.path()).is_empty());
    }

    #[test]
    fn hook_rejects_malformed_payloads_as_errors() {
        let (home, _, writer) = claude_home();
        assert!(run_hook("claude-code", "not json", Some(home.path()), Launch::default(), None, &writer).is_err());
        let bad_id = serde_json::json!({"session_id": "../../etc", "transcript_path": null}).to_string();
        assert!(run_hook("claude-code", &bad_id, Some(home.path()), Launch::default(), None, &writer).is_err());
    }

    #[test]
    fn codex_hook_finds_the_rollout_when_transcript_path_is_null() {
        let home = tempdir().unwrap();
        let day = home.path().join(".codex/sessions/2026/10/06");
        fs::create_dir_all(&day).unwrap();
        let sid = "019a0000-0000-7000-8000-000000000007";
        let line = serde_json::json!({"type": "response_item", "timestamp": "2026-10-06T22:30:00.000Z",
            "payload": {"role": "user", "content": [{"type": "input_text", "text": "list the gear ratios"}]}});
        fs::write(day.join(format!("rollout-2026-10-06T22-30-00-{sid}.jsonl")), format!("{line}\n")).unwrap();
        let writer = PlainTextWriter::with_base_dir(home.path().join("out")).with_nosave_dir(home.path().join("nosave"));
        let stdin = serde_json::json!({"session_id": sid, "transcript_path": null, "turn_id": "t1"}).to_string();
        let r = run_hook("codex", &stdin, Some(home.path()), Launch::default(), None, &writer).unwrap();
        assert!(matches!(r, HookResult::Imported(ref s) if s.starts_with("saved")), "{r:?}");
        assert_eq!(stored(home.path()).len(), 1);
    }

    const NOSAVE: Launch = Launch { clinical: false, nosave: true };

    fn event(transcript: &Path, name: &str) -> String {
        serde_json::json!({"session_id": ID, "transcript_path": transcript, "hook_event_name": name, "source": "startup"})
            .to_string()
    }

    /// The bug this closes: the wrapper's env reached the hooks, but the
    /// 5-minute sync (a plain `continuum import -s`, no env) stored the session.
    #[test]
    fn a_nosave_launch_registers_the_session_and_a_later_plain_import_skips_it() {
        let (home, transcript, writer) = claude_home();
        let r = run_hook("claude-code", &event(&transcript, "SessionStart"), Some(home.path()), NOSAVE, None, &writer).unwrap();
        assert!(matches!(r, HookResult::Skipped(ref s) if s.contains("no-save session, registered")), "{r:?}");
        assert!(home.path().join("nosave").join(ID).exists());
        let r = run_hook("claude-code", &payload(&transcript), Some(home.path()), NOSAVE, None, &writer).unwrap();
        assert!(matches!(r, HookResult::Skipped(_)), "Stop in a no-save launch: {r:?}");

        // What the sync does: no env, same session.
        let r = run_hook("claude-code", &payload(&transcript), Some(home.path()), Launch::default(), None, &writer).unwrap();
        assert!(matches!(r, HookResult::Skipped(ref s) if s.contains("no-save")), "{r:?}");
        assert!(matches!(continuum_core::import::import_claude_code(&writer, &transcript).unwrap(), Imported::NoSave));
        assert!(stored(home.path()).is_empty(), "a no-save session must never be stored");
    }

    #[test]
    fn session_start_in_an_ordinary_launch_imports_nothing() {
        let (home, transcript, writer) = claude_home();
        let r = run_hook("claude-code", &event(&transcript, "SessionStart"), Some(home.path()), Launch::default(), None, &writer).unwrap();
        assert!(matches!(r, HookResult::Skipped(ref s) if s.contains("session start")), "{r:?}");
        assert!(stored(home.path()).is_empty());
        assert!(!home.path().join("nosave").exists(), "an ordinary launch registers nothing");
    }

    #[test]
    fn a_codex_nosave_registration_by_thread_id_stops_the_timer_import() {
        let home = tempdir().unwrap();
        let day = home.path().join(".codex/sessions/2026/10/07");
        fs::create_dir_all(&day).unwrap();
        let sid = "019a0000-0000-7000-8000-00000000000c";
        let line = serde_json::json!({"type": "response_item", "timestamp": "2026-10-07T09:00:00.000Z",
            "payload": {"role": "user", "content": [{"type": "input_text", "text": "a private question"}]}});
        let rollout = day.join(format!("rollout-2026-10-07T09-00-00-{sid}.jsonl"));
        fs::write(&rollout, format!("{line}\n")).unwrap();
        let writer = PlainTextWriter::with_base_dir(home.path().join("out")).with_nosave_dir(home.path().join("nosave"));
        let start = serde_json::json!({"session_id": sid, "transcript_path": null, "hook_event_name": "SessionStart"}).to_string();
        let r = run_hook("codex", &start, Some(home.path()), NOSAVE, None, &writer).unwrap();
        assert!(matches!(r, HookResult::Skipped(ref s) if s.contains("registered")), "{r:?}");
        // What continuum-auto-import does: `continuum import -a codex`, no env.
        assert!(matches!(continuum_core::import::import_codex(&writer, &rollout).unwrap(), Imported::NoSave));
        assert!(stored(home.path()).is_empty());
    }

    #[test]
    fn hook_rejects_assistants_it_does_not_serve() {
        let (home, transcript, writer) = claude_home();
        assert!(run_hook("gemini", &payload(&transcript), Some(home.path()), NOSAVE, None, &writer).is_err());
        assert!(!home.path().join("nosave").exists());
    }
}
