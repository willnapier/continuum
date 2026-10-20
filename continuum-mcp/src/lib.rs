use anyhow::{bail, ensure, Context, Result};
use rustix::fs::{open, openat, statat, AtFlags, Dir, FileType, Mode, OFlags};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

/// Message from a conversation
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Message {
    pub id: u32,
    pub role: String,
    pub content: String,
    pub timestamp: String,
}

/// Session metadata
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Session {
    pub id: String,
    pub assistant: String,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub status: Option<String>,
    pub message_count: Option<u32>,
    pub created_at: Option<String>,
}

/// Search result with context
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConversationResult {
    pub session_id: String,
    pub assistant: String,
    pub date: String,
    pub message: Message,
    pub preview: String,
}

/// Semantic search result
#[derive(Debug, Serialize, Deserialize, JsonSchema, Clone)]
pub struct SemanticResult {
    pub session_id: String,
    pub assistant: String,
    pub date: String,
    pub filename: String,
    pub path: String,
    pub similarity: f64,
    pub snippet: Option<String>,
}

const MAX_FILE: u64 = 32 * 1024 * 1024;
fn component(s: &str) -> Result<()> {
    ensure!(
        !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', '\0']),
        "invalid archive path component"
    );
    Ok(())
}
fn date_filter(s: &str) -> Result<()> {
    let valid = match s.len() {
        7 => chrono::NaiveDate::parse_from_str(&format!("{s}-01"), "%Y-%m-%d").is_ok(),
        10 => chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok(),
        _ => false,
    };
    ensure!(
        valid
            && s.bytes().enumerate().all(|(i, b)| if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }),
        "invalid date filter"
    );
    Ok(())
}
fn limit(n: usize) -> Result<()> {
    ensure!((1..=1000).contains(&n), "result limit must be 1..1000");
    Ok(())
}
fn preview(s: &str) -> String {
    let mut chars = s.chars();
    let prefix: String = chars.by_ref().take(150).collect();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}
pub fn get_continuum_logs_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME missing")?;
    let raw = std::env::var("CONTINUUM_DIR")
        .unwrap_or_else(|_| format!("{}/continuum-logs", PathBuf::from(&home).display()));
    let path = if raw == "~" {
        PathBuf::from(&home)
    } else if let Some(s) = raw.strip_prefix("~/") {
        PathBuf::from(&home).join(s)
    } else {
        PathBuf::from(raw)
    };
    ensure!(path.is_dir(), "Continuum archive directory unavailable");
    Ok(path)
}
/// Every descendant directory/file is opened relative to a pinned directory fd,
/// with NOFOLLOW. No archive-controlled symlink is followed, including on races.
pub struct Archive {
    root: File,
    path: PathBuf,
    configured_path: PathBuf,
}
impl Archive {
    pub fn open(path: &Path) -> Result<Self> {
        let configured_path = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        let path = path.canonicalize().context("archive root unavailable")?;
        let root = File::from(open(
            &path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        Ok(Self {
            root,
            path,
            configured_path,
        })
    }
    fn directory(parent: &File, name: &str) -> Result<File> {
        component(name)?;
        Ok(File::from(
            openat(
                parent,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .context("archive directory inaccessible or symlink refused")?,
        ))
    }
    fn directories(parent: &File) -> Result<Vec<String>> {
        let mut names = vec![];
        for e in Dir::read_from(parent)? {
            let e = e?;
            let name = e.file_name().to_str().context("non-UTF8 archive entry")?;
            if name.starts_with('.') {
                continue;
            }
            let stat = statat(parent, e.file_name(), AtFlags::SYMLINK_NOFOLLOW)?;
            match FileType::from_raw_mode(stat.st_mode) {
                FileType::Directory => {
                    component(name)?;
                    names.push(name.to_owned());
                }
                FileType::Symlink => bail!("archive directory symlink refused"),
                _ => {}
            }
        }
        names.sort();
        Ok(names)
    }
    fn read(parent: &File, name: &str, optional: bool) -> Result<Option<String>> {
        component(name)?;
        let fd = match openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) if optional => return Ok(None),
            Err(e) => return Err(e).context("archive file inaccessible or symlink refused"),
        };
        let file = File::from(fd);
        ensure!(
            file.metadata()?.is_file(),
            "archive entry must be a regular file"
        );
        let mut text = String::new();
        file.take(MAX_FILE + 1).read_to_string(&mut text)?;
        ensure!(
            text.len() as u64 <= MAX_FILE,
            "archive file exceeds 32 MiB limit"
        );
        Ok(Some(text))
    }
    fn session(&self, a: &str, d: &str, id: &str) -> Result<File> {
        component(a)?;
        date_filter(d)?;
        ensure!(d.len() == 10, "session date must be YYYY-MM-DD");
        component(id)?;
        let a = Self::directory(&self.root, a)?;
        let d = Self::directory(&a, d)?;
        Self::directory(&d, id)
    }
    fn messages(dir: &File) -> Result<Vec<Message>> {
        let text = Self::read(dir, "messages.jsonl", false)?.unwrap();
        text.lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
            .map(|(i, l)| {
                serde_json::from_str(l)
                    .map_err(|_| anyhow::anyhow!("malformed archive message at line {}", i + 1))
            })
            .collect()
    }
    pub fn conversation(
        &self,
        a: &str,
        d: &str,
        id: &str,
    ) -> Result<(Vec<Message>, Option<Session>)> {
        let dir = self.session(a, d, id)?;
        let messages = Self::messages(&dir)?;
        let metadata = Self::read(&dir, "session.json", true)?
            .map(|s| serde_json::from_str(&s).context("malformed archive session metadata"))
            .transpose()?;
        Ok((messages, metadata))
    }
    fn scan(&self, assistant: Option<&str>, date: Option<&str>) -> Result<Vec<ConversationResult>> {
        if let Some(a) = assistant {
            component(a)?;
        }
        if let Some(d) = date {
            date_filter(d)?;
        }
        let mut rows = vec![];
        for a in Self::directories(&self.root)? {
            if assistant.is_some_and(|v| v != a) {
                continue;
            }
            let ad = Self::directory(&self.root, &a)?;
            for d in Self::directories(&ad)? {
                if date.is_some_and(|v| !d.starts_with(v)) {
                    continue;
                }
                let dd = Self::directory(&ad, &d)?;
                for id in Self::directories(&dd)? {
                    let sd = Self::directory(&dd, &id)?;
                    for message in Self::messages(&sd)? {
                        rows.push(ConversationResult {
                            assistant: a.clone(),
                            date: d.clone(),
                            session_id: id.clone(),
                            preview: preview(&message.content),
                            message,
                        });
                    }
                }
            }
        }
        Ok(rows)
    }
    pub fn search(
        &self,
        query: &str,
        assistant: Option<&str>,
        date: Option<&str>,
        max: usize,
    ) -> Result<Vec<ConversationResult>> {
        limit(max)?;
        let regex = regex::RegexBuilder::new(query)
            .case_insensitive(true)
            .build()
            .context("invalid search expression")?;
        Ok(self
            .scan(assistant, date)?
            .into_iter()
            .filter(|r| regex.is_match(&r.message.content))
            .take(max)
            .collect())
    }
    pub fn timeline(
        &self,
        date: Option<&str>,
        assistant: Option<&str>,
        max: usize,
    ) -> Result<Vec<ConversationResult>> {
        limit(max)?;
        let mut rows = self.scan(assistant, date)?;
        rows.sort_by(|a, b| a.message.timestamp.cmp(&b.message.timestamp));
        rows.truncate(max);
        Ok(rows)
    }
    pub fn source_text(&self, path: &Path) -> Result<String> {
        let parts = self.source_parts(path)?;
        let dir = self.session(&parts[0], &parts[1], &parts[2])?;
        if parts[3] == "session.json" {
            let text = Self::read(&dir, "session.json", false)?.unwrap();
            let _: Session =
                serde_json::from_str(&text).context("malformed archive session metadata")?;
            return Ok(text);
        }
        Ok(Self::messages(&dir)?
            .into_iter()
            .map(|m| m.content)
            .collect::<Vec<_>>()
            .join("\n"))
    }
    fn source_parts(&self, path: &Path) -> Result<Vec<String>> {
        let relative = path
            .strip_prefix(&self.path)
            .or_else(|_| path.strip_prefix(&self.configured_path))
            .context("semantic result outside archive")?;
        let parts: Vec<_> = relative
            .iter()
            .map(|p| p.to_str().map(str::to_owned).context("non-UTF8 result"))
            .collect::<Result<_>>()?;
        ensure!(
            parts.len() == 4 && matches!(parts[3].as_str(), "messages.jsonl" | "session.json"),
            "invalid semantic archive result path"
        );
        for p in &parts {
            component(p)?;
        }
        Ok(parts)
    }
    pub fn validate_source_path(&self, path: &Path) -> Result<()> {
        let p = self.source_parts(path)?;
        let dir = self.session(&p[0], &p[1], &p[2])?;
        let fd = openat(
            &dir,
            p[3].as_str(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        ensure!(
            File::from(fd).metadata()?.is_file(),
            "semantic result must be a regular file"
        );
        Ok(())
    }
    pub fn parse_semantic(&self, text: &str) -> Result<Vec<SemanticResult>> {
        #[derive(Deserialize)]
        struct Hit {
            path: PathBuf,
            similarity: f64,
            snippet: Option<String>,
        }
        let hits: Vec<Hit> =
            serde_json::from_str(text).context("semantic helper must return a JSON array")?;
        let mut rows = vec![];
        for h in hits {
            ensure!(h.similarity.is_finite(), "invalid similarity");
            let parts = self.source_parts(&h.path)?;
            self.source_text(&h.path)?; // validate again before exposing helper content
            rows.push(SemanticResult {
                session_id: parts[2].clone(),
                assistant: parts[0].clone(),
                date: parts[1].clone(),
                filename: parts[3].clone(),
                path: h.path.to_string_lossy().into(),
                similarity: h.similarity,
                snippet: h.snippet,
            });
        }
        Ok(rows)
    }
}
pub fn search_conversations(
    q: &str,
    a: Option<&str>,
    d: Option<&str>,
    n: usize,
) -> Result<Vec<ConversationResult>> {
    Archive::open(&get_continuum_logs_path()?)?.search(q, a, d, n)
}
pub fn get_timeline(d: Option<&str>, a: Option<&str>, n: usize) -> Result<Vec<ConversationResult>> {
    Archive::open(&get_continuum_logs_path()?)?.timeline(d, a, n)
}
pub fn semantic_search_conversations(query: &str, max: usize) -> Result<Vec<SemanticResult>> {
    limit(max)?;
    let archive = Archive::open(&get_continuum_logs_path()?)?;
    let helper = std::env::current_exe()?
        .parent()
        .context("executable directory")?
        .join("semantic-query-continuum");
    let mut cmd = Command::new(helper);
    cmd.args([
        "--text",
        query,
        "--limit",
        &max.to_string(),
        "--json",
        "--root",
    ])
    .arg(&archive.configured_path);
    if let Some(c) = std::env::var_os("CONTINUUM_SEMANTIC_CONFIG") {
        cmd.arg("--config").arg(c);
    }
    let output = bounded_output(&mut cmd, None, 90)
        .context("semantic-query-continuum helper unavailable")?;
    ensure!(
        output.status.success(),
        "semantic helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    archive.parse_semantic(std::str::from_utf8(&output.stdout)?)
}

/// Fixed trusted helpers only. Bound time and output without logging query/key material.
pub fn bounded_output(
    cmd: &mut Command,
    input: Option<Vec<u8>>,
    seconds: u64,
) -> Result<std::process::Output> {
    use std::{
        io::Write,
        process::Stdio,
        thread,
        time::{Duration, Instant},
    };
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let writer = thread::spawn(move || {
        if let Some(bytes) = input {
            stdin.write_all(&bytes)
        } else {
            Ok(())
        }
    });
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    fn read(stream: impl Read) -> std::io::Result<Vec<u8>> {
        let mut b = vec![];
        stream.take(2_000_001).read_to_end(&mut b)?;
        Ok(b)
    }
    let out = thread::spawn(move || read(stdout));
    let err = thread::spawn(move || read(stderr));
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let (status, timed_out) = loop {
        if let Some(s) = child.try_wait()? {
            break (s, false);
        }
        if Instant::now() >= deadline {
            child.kill()?;
            break (child.wait()?, true);
        }
        thread::sleep(Duration::from_millis(20));
    };
    let stdout = out
        .join()
        .map_err(|_| anyhow::anyhow!("helper reader failed"))??;
    let stderr = err
        .join()
        .map_err(|_| anyhow::anyhow!("helper reader failed"))??;
    let write = writer
        .join()
        .map_err(|_| anyhow::anyhow!("helper writer failed"))?;
    ensure!(!timed_out, "semantic helper timed out");
    ensure!(
        stdout.len() <= 2_000_000 && stderr.len() <= 2_000_000,
        "semantic helper output exceeded limit"
    );
    if status.success() {
        write?;
    }
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}
#[cfg(test)]
mod tests;
