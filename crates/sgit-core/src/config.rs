//! Generic repositories configuration (D002 / VAL-SGIT-REPOCONFIG-008).
//!
//! Discovery precedence (first existing file wins) — AX-CORE-CONFIG-DISCOVERY-ORDER:
//! 1. `SGIT_CONFIG` — absolute or relative path to a YAML file (wins even when missing)
//! 2. Continuity — `~/.stokd/config.yaml` `git:` / `repositories:` block
//! 3. sgit's default home — `~/.sgit/config.yaml`
//! 4. XDG — `$XDG_CONFIG_HOME/sgit/config.yaml` (default `~/.config/sgit/config.yaml`)
//!
//! There is no code dependency on stokd; the continuity path is only a filesystem path.
//! Target keys are `git.bareRoot` / `git.root` / `git.worktree.primaryDirName`
//! (VAL-GIT-002); legacy `repositories.*` remains dual-read for unmigrated files.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::submodule_checkout::SubmoduleCheckoutConfig;

/// Root layout for bare clones and worktrees.
///
/// Field names match the existing stokd `repositories:` YAML keys so both tools
/// can share `~/.stokd/config.yaml` without a schema fork. Loading prefers
/// target `git.*` keys when present (VAL-GIT-002).
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
    /// How submodules are materialized when a worktree is created.
    /// Scalar (`worktree` | `none`) or per-repo map (`"@owner/repo": worktree`).
    /// Default: `worktree`. See [`SubmoduleCheckoutConfig`].
    #[serde(rename = "submoduleCheckout", default)]
    pub submodule_checkout: SubmoduleCheckoutConfig,
}

impl RepositoriesConfig {
    /// Defaults for an explicitly-supplied environment
    /// (AX-REPO-REPO-ROOTS-PRIVILEGE-FREE-AND-PARITY). `Default` is this with
    /// the real environment probed; tests take this seam so they never depend
    /// on the developer's `$HOME` or on whether `/opt` happens to exist.
    pub fn default_with_root_env(env: &crate::roots::RootEnv) -> Self {
        let roots = crate::roots::resolve_root_defaults(env);
        Self {
            bare_root: roots.bare_root,
            worktree_root: roots.worktree_root,
            main_worktree_name: "{branch}".to_string(),
            track_non_git_workspaces: false,
            submodule_checkout: SubmoduleCheckoutConfig::default(),
        }
    }
}

impl Default for RepositoriesConfig {
    fn default() -> Self {
        Self::default_with_root_env(&crate::roots::probe_root_env())
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
    /// sgit's own default config home (`~/.sgit/config.yaml`).
    SgitHome(PathBuf),
    /// No config file found; compiled defaults.
    Defaults,
}

impl ConfigSource {
    pub fn path(&self) -> Option<&Path> {
        match self {
            ConfigSource::SgitConfigEnv(p)
            | ConfigSource::StokdContinuity(p)
            | ConfigSource::SgitHome(p)
            | ConfigSource::Xdg(p) => Some(p.as_path()),
            ConfigSource::Defaults => None,
        }
    }
}

/// Target `git.worktree` block (partial — sgit only needs naming/pin keys it uses).
#[derive(Debug, Default, Deserialize)]
struct GitWorktreeBlock {
    #[serde(rename = "primaryDirName")]
    primary_dir_name: Option<String>,
}

/// Target top-level `git:` block (VAL-GIT-002).
#[derive(Debug, Default, Deserialize)]
struct GitBlock {
    #[serde(rename = "bareRoot")]
    bare_root: Option<String>,
    root: Option<String>,
    #[serde(default)]
    worktree: Option<GitWorktreeBlock>,
    #[serde(rename = "submoduleCheckout", default)]
    submodule_checkout: Option<SubmoduleCheckoutConfig>,
}

/// Wrapper used when the YAML document is a full config with a `repositories:`
/// and/or `git:` key (as in `~/.stokd/config.yaml`). A bare `RepositoriesConfig`
/// document is also accepted.
#[derive(Debug, Deserialize)]
struct ConfigDocument {
    #[serde(default)]
    repositories: Option<RepositoriesConfig>,
    #[serde(default)]
    git: Option<GitBlock>,
}

/// Resolve the config file path according to
/// [`AX-CORE-CONFIG-DISCOVERY-ORDER`](../../../.axioms.md) precedence.
///
/// Returns `None` only when no override is set and none of the discoverable
/// files exist (caller may still use [`RepositoriesConfig::default`]).
pub fn resolve_config_path() -> (Option<PathBuf>, ConfigSource) {
    let sgit_config = env::var("SGIT_CONFIG").ok();
    let home = dirs::home_dir();
    let xdg = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    resolve_config_path_from(sgit_config.as_deref(), home.as_deref(), xdg.as_deref())
}

/// Pure precedence resolution — the single place the order is defined.
///
/// `sgit_config_env` is the raw `SGIT_CONFIG` value (blank/whitespace is
/// ignored), `home` the user's home directory, `xdg_config_home` the
/// `XDG_CONFIG_HOME` root (defaults to `<home>/.config` when absent).
fn resolve_config_path_from(
    sgit_config_env: Option<&str>,
    home: Option<&Path>,
    xdg_config_home: Option<&Path>,
) -> (Option<PathBuf>, ConfigSource) {
    // 1. SGIT_CONFIG — explicit override (even if the file is missing; load will error).
    if let Some(trimmed) = sgit_config_env.map(str::trim).filter(|s| !s.is_empty()) {
        let path = PathBuf::from(trimmed);
        return (Some(path.clone()), ConfigSource::SgitConfigEnv(path));
    }

    // 2. Continuity — ~/.stokd/config.yaml outranks sgit's own locations so a
    //    machine already configured by stokd keeps working unchanged.
    if let Some(home) = home {
        let stokd = home.join(".stokd").join("config.yaml");
        if stokd.is_file() {
            return (Some(stokd.clone()), ConfigSource::StokdContinuity(stokd));
        }

        // 3. sgit's own default home — ~/.sgit/config.yaml.
        let sgit = home.join(".sgit").join("config.yaml");
        if sgit.is_file() {
            return (Some(sgit.clone()), ConfigSource::SgitHome(sgit));
        }
    }

    // 4. XDG — last resort, so a stale ~/.config/sgit/config.yaml never
    //    silently overrides the operator's real config.
    if let Some(xdg_file) = xdg_sgit_config_path(home, xdg_config_home) {
        if xdg_file.is_file() {
            return (Some(xdg_file.clone()), ConfigSource::Xdg(xdg_file));
        }
    }

    (None, ConfigSource::Defaults)
}

/// `$XDG_CONFIG_HOME/sgit/config.yaml`, defaulting `XDG_CONFIG_HOME` to `<home>/.config`.
fn xdg_sgit_config_path(home: Option<&Path>, xdg_config_home: Option<&Path>) -> Option<PathBuf> {
    let base = xdg_config_home
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".config")))?;
    Some(base.join("sgit").join("config.yaml"))
}

/// Load [`RepositoriesConfig`] using D002 discovery.
///
/// On a missing file when `SGIT_CONFIG` is set, returns an error. When none of
/// the discoverable files exist, returns compiled defaults.
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

/// Parse a YAML file as either a full document with `git:` / `repositories:` or a bare
/// repositories object.
pub fn load_repositories_config_from_path(path: &Path) -> Result<RepositoriesConfig, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("failed to read config {}: {e}", path.display()))?;
    parse_repositories_yaml(&text)
        .map_err(|e| format!("failed to parse config {}: {e}", path.display()))
}

fn parse_repositories_yaml(text: &str) -> Result<RepositoriesConfig, String> {
    // Prefer a top-level document (stokd-shaped) with `git:` and/or `repositories:`.
    if let Ok(doc) = serde_yaml::from_str::<ConfigDocument>(text) {
        if doc.git.is_some() || doc.repositories.is_some() {
            return Ok(merge_git_and_repositories(doc.git, doc.repositories));
        }
    }
    // Bare repositories object (standalone SGIT_CONFIG / XDG fixtures).
    serde_yaml::from_str::<RepositoriesConfig>(text)
        .map_err(|e| format!("YAML parse error: {e}"))
}

/// Merge target `git.*` over legacy `repositories.*` (git wins when set).
fn merge_git_and_repositories(
    git: Option<GitBlock>,
    repositories: Option<RepositoriesConfig>,
) -> RepositoriesConfig {
    let mut cfg = repositories.unwrap_or_default();
    let Some(git) = git else {
        return cfg;
    };
    if let Some(bare) = git.bare_root.filter(|s| !s.trim().is_empty()) {
        cfg.bare_root = bare;
    }
    if let Some(root) = git.root.filter(|s| !s.trim().is_empty()) {
        cfg.worktree_root = root;
    }
    if let Some(primary) = git
        .worktree
        .and_then(|w| w.primary_dir_name)
        .filter(|s| !s.trim().is_empty())
    {
        cfg.main_worktree_name = primary;
    }
    // git.submoduleCheckout wins when present (including explicit `none`).
    if let Some(sc) = git.submodule_checkout {
        cfg.submodule_checkout = sc;
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fake `$HOME` with any subset of the discoverable config files
    /// present, plus a separate `XDG_CONFIG_HOME` root.
    struct FakeHome {
        _dir: tempfile::TempDir,
        home: PathBuf,
        xdg: PathBuf,
    }

    impl FakeHome {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            let xdg = dir.path().join("xdg");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&xdg).unwrap();
            Self {
                _dir: dir,
                home,
                xdg,
            }
        }

        fn write(&self, rel: &str) -> PathBuf {
            let path = if let Some(sub) = rel.strip_prefix("xdg/") {
                self.xdg.join(sub)
            } else {
                self.home.join(rel)
            };
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "git:\n  bareRoot: /b\n  root: /w\n").unwrap();
            path
        }

        fn resolve(&self, sgit_config_env: Option<&str>) -> (Option<PathBuf>, ConfigSource) {
            resolve_config_path_from(sgit_config_env, Some(self.home.as_path()), Some(self.xdg.as_path()))
        }
    }

    #[test]
    fn stokd_continuity_wins_over_sgit_home_and_xdg() {
        let h = FakeHome::new();
        let stokd = h.write(".stokd/config.yaml");
        h.write(".sgit/config.yaml");
        h.write("xdg/sgit/config.yaml");

        let (path, source) = h.resolve(None);
        assert_eq!(path.as_deref(), Some(stokd.as_path()));
        assert_eq!(source, ConfigSource::StokdContinuity(stokd));
    }

    #[test]
    fn sgit_home_wins_over_xdg_when_stokd_absent() {
        let h = FakeHome::new();
        let sgit = h.write(".sgit/config.yaml");
        h.write("xdg/sgit/config.yaml");

        let (path, source) = h.resolve(None);
        assert_eq!(path.as_deref(), Some(sgit.as_path()));
        assert_eq!(source, ConfigSource::SgitHome(sgit));
    }

    #[test]
    fn xdg_is_last_resort() {
        let h = FakeHome::new();
        let xdg = h.write("xdg/sgit/config.yaml");

        let (path, source) = h.resolve(None);
        assert_eq!(path.as_deref(), Some(xdg.as_path()));
        assert_eq!(source, ConfigSource::Xdg(xdg));
    }

    #[test]
    fn no_files_yields_compiled_defaults() {
        let h = FakeHome::new();
        let (path, source) = h.resolve(None);
        assert_eq!(path, None);
        assert_eq!(source, ConfigSource::Defaults);
    }

    #[test]
    fn sgit_config_env_wins_even_when_missing() {
        let h = FakeHome::new();
        h.write(".stokd/config.yaml");
        h.write(".sgit/config.yaml");
        h.write("xdg/sgit/config.yaml");

        let (path, source) = h.resolve(Some("/nowhere/override.yaml"));
        assert_eq!(path.as_deref(), Some(Path::new("/nowhere/override.yaml")));
        assert_eq!(
            source,
            ConfigSource::SgitConfigEnv(PathBuf::from("/nowhere/override.yaml"))
        );
    }

    #[test]
    fn blank_sgit_config_env_falls_through_to_file_discovery() {
        let h = FakeHome::new();
        let sgit = h.write(".sgit/config.yaml");

        let (path, source) = h.resolve(Some("  "));
        assert_eq!(path.as_deref(), Some(sgit.as_path()));
        assert_eq!(source, ConfigSource::SgitHome(sgit));
    }

    #[test]
    fn xdg_falls_back_to_home_dot_config_when_unset() {
        let h = FakeHome::new();
        let xdg = h.write(".config/sgit/config.yaml");

        let (path, source) = resolve_config_path_from(None, Some(h.home.as_path()), None);
        assert_eq!(path.as_deref(), Some(xdg.as_path()));
        assert_eq!(source, ConfigSource::Xdg(xdg));
    }

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

    #[test]
    fn prefers_git_block_over_repositories() {
        // VAL-GIT-002: target git.* keys win over legacy repositories.*.
        let yaml = r#"
git:
  bareRoot: /opt/dev
  root: /opt/worktrees
  worktree:
    primaryDirName: "{branch}"
repositories:
  bareRoot: /legacy/bare
  root: /legacy/wt
  mainWorktreeName: "{repo}-main"
"#;
        let cfg = parse_repositories_yaml(yaml).unwrap();
        assert_eq!(cfg.bare_root, "/opt/dev");
        assert_eq!(cfg.worktree_root, "/opt/worktrees");
        assert_eq!(cfg.main_worktree_name, "{branch}");
    }

    #[test]
    fn parses_git_only_document() {
        let yaml = r#"
git:
  bareRoot: /git/bare
  root: /git/wt
  worktree:
    pin: true
    primaryDirName: "{branch}"
    taskDirName: "task/{hash}-{32}"
"#;
        let cfg = parse_repositories_yaml(yaml).unwrap();
        assert_eq!(cfg.bare_root, "/git/bare");
        assert_eq!(cfg.worktree_root, "/git/wt");
        assert_eq!(cfg.main_worktree_name, "{branch}");
    }

    #[test]
    fn parses_submodule_checkout_scalar_on_repositories() {
        use crate::submodule_checkout::{SubmoduleCheckoutConfig, SubmoduleCheckoutMode};
        let yaml = r#"
repositories:
  bareRoot: /b
  root: /w
  submoduleCheckout: none
"#;
        let cfg = parse_repositories_yaml(yaml).unwrap();
        assert_eq!(
            cfg.submodule_checkout,
            SubmoduleCheckoutConfig::Global(SubmoduleCheckoutMode::None)
        );
    }

    #[test]
    fn parses_submodule_checkout_map_on_git_wins() {
        use crate::submodule_checkout::{
            resolve_submodule_checkout, SubmoduleCheckoutConfig, SubmoduleCheckoutMode,
        };
        let yaml = r#"
git:
  bareRoot: /opt/dev
  root: /opt/worktrees
  submoduleCheckout:
    "@acme/widget": none
    "@stokd-cloud/mono": worktree
repositories:
  submoduleCheckout: none
"#;
        let cfg = parse_repositories_yaml(yaml).unwrap();
        assert_eq!(
            resolve_submodule_checkout(&cfg.submodule_checkout, "acme", "widget"),
            SubmoduleCheckoutMode::None
        );
        assert_eq!(
            resolve_submodule_checkout(&cfg.submodule_checkout, "stokd-cloud", "mono"),
            SubmoduleCheckoutMode::Worktree
        );
        // Map form from git (not scalar none from repositories).
        assert!(matches!(
            cfg.submodule_checkout,
            SubmoduleCheckoutConfig::PerRepo(_)
        ));
    }

    #[test]
    fn submodule_checkout_defaults_to_none_when_absent() {
        use crate::submodule_checkout::{SubmoduleCheckoutConfig, SubmoduleCheckoutMode};
        let yaml = r#"
repositories:
  bareRoot: /b
  root: /w
"#;
        let cfg = parse_repositories_yaml(yaml).unwrap();
        assert_eq!(
            cfg.submodule_checkout,
            SubmoduleCheckoutConfig::Global(SubmoduleCheckoutMode::None)
        );
    }
}
