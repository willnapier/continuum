use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use chrono::Utc;
use color_eyre::{
    eyre::{bail, Context, ContextCompat},
    Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const SCHEMA_VERSION: u32 = 1;
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const OPPORTUNITY_HOURS: i64 = 48;
const OPPORTUNITY_MAX_USED_PERCENT: i64 = 80;
const REMINDER_HOURS: i64 = 6;
const RESERVE_THREAT_USED_PERCENT: i64 = 85;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageObservation {
    pub schema_version: u32,
    pub observed_at: String,
    pub observed_at_unix: i64,
    pub assistant: String,
    pub machine_id: String,
    pub provenance: String,
    pub vendor: VendorUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VendorUsage {
    pub limit_id: Option<String>,
    pub limit_name: Option<String>,
    pub plan_type: Option<String>,
    pub window_kind: String,
    pub used_percent: i64,
    pub window_duration_mins: Option<i64>,
    pub resets_at: Option<i64>,
    pub secondary: Option<Value>,
    pub credits: Option<Value>,
    pub rate_limit_reached_type: Option<String>,
    pub raw_snapshot: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageAssessment {
    pub seconds_to_reset: Option<i64>,
    pub opportunity_lead_seconds: i64,
    pub opportunity: bool,
    pub reminder: bool,
    pub reserve_threat: bool,
    pub window_id: String,
}

pub fn machine_id() -> String {
    if let Ok(value) = std::env::var("CONTINUUM_MACHINE_ID") {
        if !value.trim().is_empty() {
            return sanitize_component(&value);
        }
    }
    if let Ok(value) = fs::read_to_string("/etc/hostname") {
        if !value.trim().is_empty() {
            return sanitize_component(value.trim());
        }
    }
    let output = Command::new("hostname").output().ok();
    let value = output
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    if !value.trim().is_empty() {
        return sanitize_component(value.trim());
    }
    #[cfg(target_os = "macos")]
    if let Ok(output) = Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
    {
        if output.status.success() {
            if let Ok(value) = String::from_utf8(output.stdout) {
                if !value.trim().is_empty() {
                    return sanitize_component(value.trim());
                }
            }
        }
    }
    "unknown-machine".to_string()
}

pub fn usage_root() -> Result<PathBuf> {
    if let Ok(value) = std::env::var("CONTINUUM_USAGE_DIR") {
        return Ok(PathBuf::from(value));
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join("Assistants/continuum-usage"))
}

fn local_state_root() -> Result<PathBuf> {
    if let Ok(value) = std::env::var("CONTINUUM_USAGE_STATE_DIR") {
        return Ok(PathBuf::from(value));
    }
    if let Ok(value) = std::env::var("XDG_STATE_HOME") {
        return Ok(PathBuf::from(value).join("continuum/usage"));
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/state/continuum/usage"))
}

pub fn refresh_codex_usage(provenance: &str) -> Result<UsageObservation> {
    let snapshot = read_codex_rate_limits()?;
    let primary = snapshot.get("primary").and_then(Value::as_object);
    let used_percent = primary
        .and_then(|p| p.get("usedPercent"))
        .and_then(Value::as_i64)
        .context("Codex rate-limit response omitted primary.usedPercent")?;
    let duration = primary
        .and_then(|p| p.get("windowDurationMins"))
        .and_then(Value::as_i64);
    let resets_at = primary
        .and_then(|p| p.get("resetsAt"))
        .and_then(Value::as_i64);
    let now = Utc::now();

    let observation = UsageObservation {
        schema_version: SCHEMA_VERSION,
        observed_at: now.to_rfc3339(),
        observed_at_unix: now.timestamp(),
        assistant: "codex".to_string(),
        machine_id: machine_id(),
        provenance: provenance.to_string(),
        vendor: VendorUsage {
            limit_id: snapshot
                .get("limitId")
                .and_then(Value::as_str)
                .map(str::to_string),
            limit_name: snapshot
                .get("limitName")
                .and_then(Value::as_str)
                .map(str::to_string),
            plan_type: snapshot
                .get("planType")
                .and_then(Value::as_str)
                .map(str::to_string),
            window_kind: if resets_at.is_some() {
                "fixed_reset"
            } else {
                "unknown"
            }
            .to_string(),
            used_percent,
            window_duration_mins: duration,
            resets_at,
            secondary: snapshot.get("secondary").cloned().filter(|v| !v.is_null()),
            credits: snapshot.get("credits").cloned().filter(|v| !v.is_null()),
            rate_limit_reached_type: snapshot
                .get("rateLimitReachedType")
                .and_then(Value::as_str)
                .map(str::to_string),
            raw_snapshot: snapshot,
        },
    };
    persist_observation(&observation)?;
    Ok(observation)
}

pub fn load_latest(assistant: &str) -> Result<Option<UsageObservation>> {
    let path = latest_path(assistant)?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
        format!("failed to parse {}", path.display())
    })?))
}

pub fn assess(observation: &UsageObservation, now: i64) -> UsageAssessment {
    let seconds_to_reset = observation.vendor.resets_at.map(|reset| reset - now);
    // The 48-hour design threshold was proposed for a 7-day vendor window.
    // Scale it down for shorter windows so a fresh 5-hour window is not
    // immediately labelled an expiry opportunity.
    let proportional_lead = observation
        .vendor
        .window_duration_mins
        .map(|minutes| minutes * 60 * 2 / 7)
        .unwrap_or(OPPORTUNITY_HOURS * 3600);
    let opportunity_lead_seconds = proportional_lead.min(OPPORTUNITY_HOURS * 3600);
    let opportunity = matches!(seconds_to_reset, Some(seconds) if (0..=opportunity_lead_seconds).contains(&seconds))
        && observation.vendor.used_percent <= OPPORTUNITY_MAX_USED_PERCENT;
    let reminder_lead_seconds = (opportunity_lead_seconds / 2).min(REMINDER_HOURS * 3600);
    let reminder = opportunity
        && matches!(seconds_to_reset, Some(seconds) if (0..=reminder_lead_seconds).contains(&seconds));
    let reserve_threat = observation.vendor.used_percent >= RESERVE_THREAT_USED_PERCENT;
    let reset_key = observation
        .vendor
        .resets_at
        .map(|v| v.to_string())
        .unwrap_or_else(|| "unknown-reset".to_string());
    let limit = observation
        .vendor
        .limit_id
        .as_deref()
        .unwrap_or("unknown-limit");
    UsageAssessment {
        seconds_to_reset,
        opportunity_lead_seconds,
        opportunity,
        reminder,
        reserve_threat,
        window_id: format!("{}:{}", limit, reset_key),
    }
}

pub fn cached_banner(assistant: &str) -> Result<Option<String>> {
    let Some(observation) = load_latest(assistant)? else {
        return Ok(None);
    };
    let now = Utc::now().timestamp();
    let reset = observation
        .vendor
        .resets_at
        .and_then(|v| chrono::DateTime::from_timestamp(v, 0))
        .map(|v| {
            v.with_timezone(&chrono::Local)
                .format("%a %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "unknown".to_string());
    let age = human_duration((now - observation.observed_at_unix).max(0));
    Ok(Some(format!(
        "Codex allocation: {}% remaining · resets {} · snapshot {} old",
        100 - observation.vendor.used_percent,
        reset,
        age
    )))
}

pub fn render_usage(assistant: &str) -> Result<String> {
    let Some(observation) = load_latest(assistant)? else {
        return Ok(format!(
            "No cached usage observation for {assistant}. Run with --refresh."
        ));
    };
    let now = Utc::now().timestamp();
    let assessment = assess(&observation, now);
    let reset = observation
        .vendor
        .resets_at
        .and_then(|v| chrono::DateTime::from_timestamp(v, 0))
        .map(|v| {
            v.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S %Z")
                .to_string()
        })
        .unwrap_or_else(|| "unknown".to_string());
    let remaining_time = assessment
        .seconds_to_reset
        .map(human_duration)
        .unwrap_or_else(|| "unknown".to_string());
    let state = if assessment.reserve_threat {
        "reserve threat"
    } else if assessment.reminder {
        "Fast opportunity (near reset)"
    } else if assessment.opportunity {
        "Fast opportunity"
    } else {
        "normal"
    };
    Ok(format!(
        "{} — {}\nRaw vendor observation\n  Used:              {}%\n  Remaining:         {}%\n  Window:            {} minutes ({})\n  Reset:             {}\n  Plan:              {}\n  Snapshot:          {} ({} old, {})\nDerived Continuum state\n  Time to reset:     {}\n  State:             {}\n  Opportunity rule:  <= {} and <= {}% used\n  Reserve rule:      >= {}% used",
        assistant,
        observation.vendor.limit_id.as_deref().unwrap_or("unknown limit"),
        observation.vendor.used_percent,
        100 - observation.vendor.used_percent,
        observation.vendor.window_duration_mins.map(|v| v.to_string()).unwrap_or_else(|| "unknown".to_string()),
        observation.vendor.window_kind,
        reset,
        observation.vendor.plan_type.as_deref().unwrap_or("unknown"),
        observation.observed_at,
        human_duration((now - observation.observed_at_unix).max(0)),
        observation.provenance,
        remaining_time,
        state,
        human_duration(assessment.opportunity_lead_seconds),
        OPPORTUNITY_MAX_USED_PERCENT,
        RESERVE_THREAT_USED_PERCENT,
    ))
}

pub fn notify_transitions(observation: &UsageObservation) -> Result<Vec<String>> {
    let assessment = assess(observation, Utc::now().timestamp());
    let assistant = &observation.assistant;
    let mut delivered = Vec::new();
    let opportunity_id = format!("{}:{}:opportunity-enter", assistant, assessment.window_id);
    let opportunity_was_delivered = was_delivered(&opportunity_id)?;
    if assessment.opportunity {
        if deliver_once(
            &opportunity_id,
            "Codex Fast opportunity",
            &opportunity_body(observation, &assessment),
        )? {
            delivered.push(opportunity_id);
        }
    }
    if assessment.reminder && opportunity_was_delivered {
        let id = format!(
            "{}:{}:opportunity-reminder",
            assistant, assessment.window_id
        );
        if deliver_once(
            &id,
            "Codex Fast opportunity — reset soon",
            &opportunity_body(observation, &assessment),
        )? {
            delivered.push(id);
        }
    }
    if assessment.reserve_threat {
        let id = format!("{}:{}:reserve-threat", assistant, assessment.window_id);
        let body = format!(
            "Only {}% remains before this allocation resets. Consider Standard mode to preserve capacity.",
            100 - observation.vendor.used_percent
        );
        if deliver_once(&id, "Codex allocation reserve", &body)? {
            delivered.push(id);
        }
    }
    Ok(delivered)
}

fn opportunity_body(observation: &UsageObservation, assessment: &UsageAssessment) -> String {
    let remaining = 100 - observation.vendor.used_percent;
    let until = assessment
        .seconds_to_reset
        .map(human_duration)
        .unwrap_or_else(|| "an unknown interval".to_string());
    format!(
        "{remaining}% remains and resets in {until}. Use /fast on when earlier completion would be useful; keep Standard for unattended work."
    )
}

fn persist_observation(observation: &UsageObservation) -> Result<()> {
    let root = usage_root()?;
    let observations = root.join("observations");
    fs::create_dir_all(&observations)?;
    let line_path = observations.join(format!("{}.jsonl", observation.machine_id));
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&line_path)?;
    serde_json::to_writer(&mut file, observation)?;
    writeln!(file)?;

    let latest = latest_path(&observation.assistant)?;
    if let Some(parent) = latest.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = latest.with_extension(format!("json.tmp-{}", std::process::id()));
    fs::write(&temp, serde_json::to_vec_pretty(observation)?)?;
    fs::rename(temp, latest)?;
    Ok(())
}

fn latest_path(assistant: &str) -> Result<PathBuf> {
    Ok(usage_root()?.join("latest").join(format!(
        "{}-{}.json",
        sanitize_component(assistant),
        machine_id()
    )))
}

fn delivered_path() -> Result<PathBuf> {
    Ok(local_state_root()?.join("delivered-events.jsonl"))
}

fn deliver_once(event_id: &str, title: &str, body: &str) -> Result<bool> {
    if was_delivered(event_id)? {
        return Ok(false);
    }
    let path = delivered_path()?;
    deliver_notification(title, body)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{event_id}")?;
    Ok(true)
}

fn deliver_notification(title: &str, body: &str) -> Result<()> {
    if std::env::var_os("CONTINUUM_USAGE_DISABLE_NOTIFICATIONS").is_some() {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            applescript_escape(body),
            applescript_escape(title)
        );
        let status = Command::new("osascript").args(["-e", &script]).status()?;
        if !status.success() {
            bail!("osascript notification failed")
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let status = Command::new("notify-send").args([title, body]).status()?;
        if !status.success() {
            bail!("notify-send notification failed")
        }
    }
    Ok(())
}

fn was_delivered(event_id: &str) -> Result<bool> {
    let path = delivered_path()?;
    if !path.exists() {
        return Ok(false);
    }
    let file = fs::File::open(path)?;
    Ok(BufReader::new(file)
        .lines()
        .map_while(|line| line.ok())
        .any(|line| line == event_id))
}

#[cfg(target_os = "macos")]
fn applescript_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn read_codex_rate_limits() -> Result<Value> {
    let codex = find_real_codex()?;
    let mut child = Command::new(codex)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start Codex app-server usage probe")?;
    let mut stdin = child
        .stdin
        .take()
        .context("Codex probe stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("Codex probe stdout unavailable")?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    writeln!(
        stdin,
        "{}",
        json!({"method":"initialize","id":0,"params":{"clientInfo":{"name":"continuum_usage","title":"Continuum Usage","version":env!("CARGO_PKG_VERSION")}}})
    )?;
    stdin.flush()?;

    let result = (|| -> Result<Value> {
        loop {
            let line = rx
                .recv_timeout(PROBE_TIMEOUT)
                .context("timed out initializing Codex usage probe")??;
            let value: Value = serde_json::from_str(&line)?;
            if value.get("id").and_then(Value::as_i64) == Some(0) {
                if let Some(error) = value.get("error") {
                    bail!("Codex usage probe initialization failed: {error}")
                }
                break;
            }
        }
        writeln!(stdin, "{}", json!({"method":"initialized","params":{}}))?;
        writeln!(
            stdin,
            "{}",
            json!({"method":"account/rateLimits/read","id":1})
        )?;
        stdin.flush()?;
        loop {
            let line = rx
                .recv_timeout(PROBE_TIMEOUT)
                .context("timed out reading Codex rate limits")??;
            let value: Value = serde_json::from_str(&line)?;
            if value.get("id").and_then(Value::as_i64) == Some(1) {
                if let Some(error) = value.get("error") {
                    bail!("Codex usage probe failed: {error}")
                }
                return value
                    .get("result")
                    .and_then(|r| r.get("rateLimits"))
                    .cloned()
                    .context("Codex usage response omitted result.rateLimits");
            }
        }
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn find_real_codex() -> Result<PathBuf> {
    for path in [
        "/usr/bin/codex",
        "/usr/local/bin/codex",
        "/opt/homebrew/bin/codex",
    ] {
        let candidate = Path::new(path);
        if candidate.exists() {
            return Ok(candidate.to_path_buf());
        }
    }
    which::which("codex").context("could not find the real Codex CLI")
}

fn sanitize_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

fn human_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(used: i64, resets_in_hours: i64) -> UsageObservation {
        UsageObservation {
            schema_version: 1,
            observed_at: "2026-08-25T00:00:00Z".to_string(),
            observed_at_unix: 1_000,
            assistant: "codex".to_string(),
            machine_id: "test".to_string(),
            provenance: "test".to_string(),
            vendor: VendorUsage {
                limit_id: Some("codex".to_string()),
                limit_name: None,
                plan_type: Some("plus".to_string()),
                window_kind: "fixed_reset".to_string(),
                used_percent: used,
                window_duration_mins: Some(10_080),
                resets_at: Some(1_000 + resets_in_hours * 3_600),
                secondary: None,
                credits: None,
                rate_limit_reached_type: None,
                raw_snapshot: json!({}),
            },
        }
    }

    #[test]
    fn opportunity_requires_late_window_and_surplus() {
        assert!(assess(&observation(40, 24), 1_000).opportunity);
        assert!(!assess(&observation(40, 72), 1_000).opportunity);
        assert!(!assess(&observation(90, 24), 1_000).opportunity);
    }

    #[test]
    fn short_window_does_not_become_an_immediate_opportunity() {
        let mut short = observation(10, 4);
        short.vendor.window_duration_mins = Some(300);
        assert!(!assess(&short, 1_000).opportunity);
        short.vendor.resets_at = Some(1_000 + 60 * 60);
        assert!(assess(&short, 1_000).opportunity);
    }

    #[test]
    fn reminder_and_threat_are_independent() {
        assert!(assess(&observation(40, 5), 1_000).reminder);
        let threat = assess(&observation(90, 5), 1_000);
        assert!(threat.reserve_threat);
        assert!(!threat.opportunity);
    }

    #[test]
    fn window_id_is_stable_for_same_reset() {
        assert_eq!(assess(&observation(10, 24), 1_000).window_id, "codex:87400");
    }
}
