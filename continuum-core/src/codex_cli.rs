use color_eyre::{eyre::ContextCompat, Result};
use std::fmt;
use std::path::{Path, PathBuf};

pub const CODEX_BIN_OVERRIDE: &str = "CONTINUUM_CODEX_BIN";
pub const CODEX_DEPTH_ENV: &str = "CONTINUUM_CODEX_DEPTH";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CodexSource {
    Override,
    Managed,
    LegacyUser,
    Path,
    System,
}

impl fmt::Display for CodexSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Override => "override",
            Self::Managed => "managed",
            Self::LegacyUser => "legacy-user",
            Self::Path => "path",
            Self::System => "system-fallback",
        };
        f.write_str(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexResolution {
    pub path: PathBuf,
    pub source: CodexSource,
}

impl CodexResolution {
    pub fn is_managed(&self) -> bool {
        self.source == CodexSource::Managed
    }
}

pub fn managed_codex_prefix() -> Result<PathBuf> {
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(data_home).join("continuum/codex"));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/share/continuum/codex"))
}

pub fn managed_codex_bin() -> Result<PathBuf> {
    Ok(managed_codex_prefix()?.join("bin/codex"))
}

pub fn resolve_codex(excluded_executable: Option<&Path>) -> Result<CodexResolution> {
    let override_path = std::env::var_os(CODEX_BIN_OVERRIDE).map(PathBuf::from);
    let managed = managed_codex_bin()?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let legacy = home
        .as_ref()
        .map(|path| vec![path.join(".local/bin/codex-real")])
        .unwrap_or_default();
    let system = vec![
        PathBuf::from("/usr/bin/codex"),
        PathBuf::from("/usr/local/bin/codex"),
        PathBuf::from("/opt/homebrew/bin/codex"),
        PathBuf::from("/opt/homebrew/opt/codex/bin/codex"),
    ];
    let path_candidates = which::which_all("codex")
        .map(|paths| paths.filter(|path| !same_as_any(path, &system)).collect())
        .unwrap_or_default();

    resolve_from_candidates(
        override_path,
        managed,
        legacy,
        path_candidates,
        system,
        excluded_executable,
    )
}

fn same_as_any(path: &Path, candidates: &[PathBuf]) -> bool {
    candidates.iter().any(|candidate| {
        if path == candidate {
            return true;
        }
        match (
            std::fs::canonicalize(path),
            std::fs::canonicalize(candidate),
        ) {
            (Ok(path), Ok(candidate)) => path == candidate,
            _ => false,
        }
    })
}

fn resolve_from_candidates(
    override_path: Option<PathBuf>,
    managed: PathBuf,
    legacy: Vec<PathBuf>,
    path_candidates: Vec<PathBuf>,
    system: Vec<PathBuf>,
    excluded_executable: Option<&Path>,
) -> Result<CodexResolution> {
    if let Some(path) = override_path {
        if !path.exists() {
            color_eyre::eyre::bail!(
                "{CODEX_BIN_OVERRIDE} points to missing path {}",
                path.display()
            );
        }
        if is_excluded(&path, excluded_executable) {
            color_eyre::eyre::bail!(
                "{CODEX_BIN_OVERRIDE} resolves to the Continuum wrapper: {}",
                path.display()
            );
        }
        return Ok(CodexResolution {
            path,
            source: CodexSource::Override,
        });
    }

    let tiers = [
        (CodexSource::Managed, vec![managed]),
        (CodexSource::LegacyUser, legacy),
        (CodexSource::Path, path_candidates),
        (CodexSource::System, system),
    ];
    for (source, candidates) in tiers {
        for path in candidates {
            if path.exists() && !is_excluded(&path, excluded_executable) {
                return Ok(CodexResolution { path, source });
            }
        }
    }

    color_eyre::eyre::bail!(
        "Could not find the real Codex CLI. Run `continuum codex update` to install the managed copy."
    )
}

fn is_excluded(path: &Path, excluded_executable: Option<&Path>) -> bool {
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains("continuum-codex") || name.contains("misdeployed"))
    {
        return true;
    }
    let Some(excluded) = excluded_executable else {
        return false;
    };
    match (std::fs::canonicalize(path), std::fs::canonicalize(excluded)) {
        (Ok(candidate), Ok(excluded)) => candidate == excluded,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn executable(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, "fixture").unwrap();
    }

    #[test]
    fn managed_install_beats_path_and_system() {
        let dir = tempdir().unwrap();
        let managed = dir.path().join("managed/bin/codex");
        let path = dir.path().join("path/codex");
        let system = dir.path().join("system/codex");
        executable(&managed);
        executable(&path);
        executable(&system);

        let result = resolve_from_candidates(
            None,
            managed.clone(),
            vec![],
            vec![path],
            vec![system],
            None,
        )
        .unwrap();
        assert_eq!(result.path, managed);
        assert_eq!(result.source, CodexSource::Managed);
    }

    #[test]
    fn explicit_missing_override_is_an_error() {
        let dir = tempdir().unwrap();
        let error = resolve_from_candidates(
            Some(dir.path().join("missing")),
            dir.path().join("managed"),
            vec![],
            vec![],
            vec![],
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("missing path"));
    }

    #[test]
    fn wrapper_identity_is_skipped() {
        let dir = tempdir().unwrap();
        let wrapper = dir.path().join("wrapper");
        let alias = dir.path().join("codex");
        let system = dir.path().join("system/codex");
        executable(&wrapper);
        std::os::unix::fs::symlink(&wrapper, &alias).unwrap();
        executable(&system);

        let result = resolve_from_candidates(
            None,
            dir.path().join("missing-managed"),
            vec![],
            vec![alias],
            vec![system.clone()],
            Some(&wrapper),
        )
        .unwrap();
        assert_eq!(result.path, system);
        assert_eq!(result.source, CodexSource::System);
    }
}
