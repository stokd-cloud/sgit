//! Generic repositories configuration (D002 / VAL-SGIT-REPOCONFIG-008).
//!
//! Discovery precedence (first hit wins):
//! 1. `SGIT_CONFIG` — absolute or relative path to a YAML file
//! 2. XDG — `$XDG_CONFIG_HOME/sgit/config.yaml` (default `~/.config/sgit/config.yaml`)
//!    when that file exists
//! 3. Continuity fallback — `~/.stokd/config.yaml` `repositories:` block
//!
//! There is no code dependency on stokd; the fallback is only a filesystem path.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Root layout for bare clones and worktrees.
///
/// Field names match the existing stokd `repositories:` YAML keys so both tools
/// can share `~/.stokd/config.yaml` without a schema fork.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RepositoriesConfig {
    #[serde(rename = "bareRoot")]
    pub bare_root: String,
    /// Root directory holding checked-out worktrees. Canonical key is `root`;
    /// legacy `worktreeRoot` is accepted via serde alias.
    #[serde(rename = "root", alias = "worktreeRoot")]
    pub worktree_root: String,
    #[serde(rename = "mainWorktreeName")]
    pub main_worktree_name: String,
    #[serde(rename = "trackNonGitWorkspaces")]
    pub track_non_git_workspaces: bool,
}

impl Default for RepositoriesConfig {
    fn default() -> Self {
        Self {
            bare_root: "/opt/dev".to_string(),
            worktree_root: "/opt/worktrees".to_string(),
            main_worktree_name: "{branch}".to_string(),
            track_non_git_workspaces: false,
        }
    }
}

/// Which discovery source produced the loaded config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// `SGIT_CONFIG` environment variable.
    SgitConfigEnv(PathBuf),
    /// XDG config file (`$XDG_CONFIG_HOME/sgit/config.yaml` or `~/.config/sgit/config.yaml`).
    Xdg(PathBuf),
    /// Continuity path `~/.stokd/config.yaml`.
    StokdContinuity(PathBuf),
    /// No config file found; compiled defaults.
    Defaults,
}

impl ConfigSource {
    pub fn path(&self) -> Option<&Path> {
        match self {
            ConfigSource::SgitConfigEnv(p)
            | ConfigSource::Xdg(p)
            | ConfigSource::StokdContinuity(p) => Some(p.as_path()),
            ConfigSource::Defaults => None,
        }
    }
}

/// Wrapper used when the YAML document is a full config with a `repositories:` key
/// (as in `~/.stokd/config.yaml`). A bare `RepositoriesConfig` document is also accepted.
#[derive(Debug, Deserialize)]
struct ConfigDocument {
    #[serde(default)]
    repositories: Option<RepositoriesConfig>,
}

/// Resolve the config file path according to D002 precedence.
///
/// Returns `None` only when neither an override path nor the continuity file
/// can be located (caller may still use [`RepositoriesConfig::default`]).
pub fn resolve_config_path() -> (Option<PathBuf>, ConfigSource) {
    // 1. SGIT_CONFIG — explicit override (even if the file is missing; load will error).
    if let Ok(raw) = env::var("SGIT_CONFIG") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let path = PathBuf::from(trimmed);
            return (Some(path.clone()), ConfigSource::SgitConfigEnv(path));
        }
    }

    // 2. XDG — only when the file exists (absent override → fall through).
    if let Some(xdg_file) = xdg_sgit_config_path() {
        if xdg_file.is_file() {
            return (Some(xdg_file.clone()), ConfigSource::Xdg(xdg_file));
        }
    }

    // 3. Continuity — ~/.stokd/config.yaml when present.
    if let Some(home) = dirs::home_dir() {
        let stokd = home.join(".stokd").join("config.yaml");
        if stokd.is_file() {
            return (Some(stokd.clone()), ConfigSource::StokdContinuity(stokd));
        }
    }

    (None, ConfigSource::Defaults)
}

/// `$XDG_CONFIG_HOME/sgit/config.yaml`, defaulting `XDG_CONFIG_HOME` to `~/.config`.
fn xdg_sgit_config_path() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))?;
    Some(base.join("sgit").join("config.yaml"))
}

/// Load [`RepositoriesConfig`] using D002 discovery.
///
/// On a missing file when `SGIT_CONFIG` is set, returns an error. When no file
/// is found via XDG or continuity, returns compiled defaults.
pub fn load_repositories_config() -> Result<(RepositoriesConfig, ConfigSource), String> {
    let (path, source) = resolve_config_path();
    match path {
        Some(p) => {
            let cfg = load_repositories_config_from_path(&p)?;
            Ok((cfg, source))
        }
        None => Ok((RepositoriesConfig::default(), ConfigSource::Defaults)),
    }
}

/// Parse a YAML file as either a full document with `repositories:` or a bare
/// repositories object.
pub fn load_repositories_config_from_path(path: &Path) -> Result<RepositoriesConfig, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("failed to read config {}: {e}", path.display()))?;
    parse_repositories_yaml(&text)
        .map_err(|e| format!("failed to parse config {}: {e}", path.display()))
}

fn parse_repositories_yaml(text: &str) -> Result<RepositoriesConfig, String> {
    // Prefer a top-level `repositories:` document (stokd-shaped).
    if let Ok(doc) = serde_yaml::from_str::<ConfigDocument>(text) {
        if let Some(repos) = doc.repositories {
            return Ok(repos);
        }
    }
    // Bare repositories object (standalone SGIT_CONFIG / XDG fixtures).
    serde_yaml::from_str::<RepositoriesConfig>(text)
        .map_err(|e| format!("YAML parse error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stokd_shaped_document() {
        let yaml = r#"
repositories:
  bareRoot: /tmp/bare
  root: /tmp/wt
  mainWorktreeName: "{branch}"
"#;
        let cfg = parse_repositories_yaml(yaml).unwrap();
        assert_eq!(cfg.bare_root, "/tmp/bare");
        assert_eq!(cfg.worktree_root, "/tmp/wt");
    }

    #[test]
    fn parses_bare_repositories_object() {
        let yaml = r#"
bareRoot: /x/bare
root: /x/wt
"#;
        let cfg = parse_repositories_yaml(yaml).unwrap();
        assert_eq!(cfg.bare_root, "/x/bare");
        assert_eq!(cfg.worktree_root, "/x/wt");
        assert_eq!(cfg.main_worktree_name, "{branch}");
    }

    #[test]
    fn accepts_legacy_worktree_root_alias() {
        let yaml = "bareRoot: /b\nworktreeRoot: /w\n";
        let cfg = parse_repositories_yaml(yaml).unwrap();
        assert_eq!(cfg.worktree_root, "/w");
    }
}
