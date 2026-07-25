//! Submodule materialization policy for worktree creation.
//!
//! Config key: `repositories.submoduleCheckout` / `git.submoduleCheckout`.
//!
//! Forms:
//! - scalar: `submoduleCheckout: worktree` (default for every repo)
//! - map: per superproject repo:
//!   ```yaml
//!   submoduleCheckout:
//!     "@owner/repo1": worktree
//!     "@owner/repo2": none
//!   ```
//!
//! Modes:
//! - `none` (default): leave gitlinks unpopulated (fast empty worktrees).
//! - `worktree`: after a superproject worktree is created, populate each
//!   submodule by attaching a linked worktree of the matching bare under
//!   `bareRoot` when present; otherwise best-effort `git submodule update --init`.
//!   When the bare uses `extensions.worktreeConfig` with a shared
//!   `core.bare=true`, a per-worktree `core.bare=false` override is written so
//!   the attached worktree is usable (else git rejects it as "not a work tree").
//! - `inline`: git-native embedded submodule via `git submodule update --init`
//!   (pinned to the gitlink commit, gitdir under the superproject).
//! - `link`: symlink the submodule path to a canonical shared checkout under
//!   `bareRoot`, and set `submodule.<name>.ignore=all` on the superproject so a
//!   symlinked gitlink never dirties `git status`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::layout::parse_git_remote_url;

/// How submodules are materialized inside a newly created superproject worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmoduleCheckoutMode {
    /// Do not populate submodule working trees (default).
    None,
    /// Attach shared linked worktrees from `bareRoot` (or submodule update --init fallback).
    Worktree,
    /// Git-native embedded submodule (`git submodule update --init`), pinned to the gitlink.
    Inline,
    /// Symlink the submodule path to a canonical shared checkout under `bareRoot`.
    Link,
}

impl SubmoduleCheckoutMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SubmoduleCheckoutMode::None => "none",
            SubmoduleCheckoutMode::Worktree => "worktree",
            SubmoduleCheckoutMode::Inline => "inline",
            SubmoduleCheckoutMode::Link => "link",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "false" | "no" => Ok(SubmoduleCheckoutMode::None),
            "worktree" | "wt" => Ok(SubmoduleCheckoutMode::Worktree),
            "inline" | "embedded" | "embed" => Ok(SubmoduleCheckoutMode::Inline),
            "link" | "symlink" | "shared" => Ok(SubmoduleCheckoutMode::Link),
            other => Err(format!(
                "invalid submoduleCheckout mode '{other}' (expected none|worktree|inline|link)"
            )),
        }
    }
}

impl Default for SubmoduleCheckoutMode {
    fn default() -> Self {
        SubmoduleCheckoutMode::None
    }
}

impl Serialize for SubmoduleCheckoutMode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SubmoduleCheckoutMode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// Scalar global mode, or per-superproject-repo map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmoduleCheckoutConfig {
    /// One mode for every repository.
    Global(SubmoduleCheckoutMode),
    /// Per-repo overrides. Unlisted repos use [`SubmoduleCheckoutMode::None`].
    PerRepo(BTreeMap<String, SubmoduleCheckoutMode>),
}

impl Default for SubmoduleCheckoutConfig {
    fn default() -> Self {
        SubmoduleCheckoutConfig::Global(SubmoduleCheckoutMode::None)
    }
}

impl Serialize for SubmoduleCheckoutConfig {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            SubmoduleCheckoutConfig::Global(m) => m.serialize(serializer),
            SubmoduleCheckoutConfig::PerRepo(map) => map.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for SubmoduleCheckoutConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_yaml::Value::deserialize(deserializer)?;
        match value {
            serde_yaml::Value::String(s) => {
                let mode = SubmoduleCheckoutMode::parse(&s).map_err(serde::de::Error::custom)?;
                Ok(SubmoduleCheckoutConfig::Global(mode))
            }
            serde_yaml::Value::Mapping(map) => {
                let mut out = BTreeMap::new();
                for (k, v) in map {
                    let key = k
                        .as_str()
                        .ok_or_else(|| serde::de::Error::custom("submoduleCheckout map keys must be strings"))?
                        .to_string();
                    let mode_str = v.as_str().ok_or_else(|| {
                        serde::de::Error::custom(format!(
                            "submoduleCheckout['{key}'] must be a mode string (none|worktree|inline|link)"
                        ))
                    })?;
                    let mode =
                        SubmoduleCheckoutMode::parse(mode_str).map_err(serde::de::Error::custom)?;
                    out.insert(key, mode);
                }
                Ok(SubmoduleCheckoutConfig::PerRepo(out))
            }
            serde_yaml::Value::Null => Ok(SubmoduleCheckoutConfig::default()),
            other => Err(serde::de::Error::custom(format!(
                "submoduleCheckout: expected string or map, got {other:?}"
            ))),
        }
    }
}

/// Normalize `@Owner/Repo.git` / `owner/repo` → `owner/repo` (lowercase).
pub fn normalize_repo_slug(owner: &str, repo: &str) -> String {
    let owner = owner.trim().trim_start_matches('@').to_ascii_lowercase();
    let repo = repo
        .trim()
        .trim_end_matches(".git")
        .to_ascii_lowercase();
    format!("{owner}/{repo}")
}

/// Normalize a config map key the same way.
pub fn normalize_repo_key(raw: &str) -> String {
    let raw = raw.trim();
    let raw = raw.trim_start_matches('@');
    let (owner, repo) = if let Some((o, r)) = raw.split_once('/') {
        (o, r)
    } else {
        return raw.to_ascii_lowercase();
    };
    normalize_repo_slug(owner, repo)
}

/// Resolve the effective mode for `owner/repo` from config.
pub fn resolve_submodule_checkout(
    cfg: &SubmoduleCheckoutConfig,
    owner: &str,
    repo: &str,
) -> SubmoduleCheckoutMode {
    match cfg {
        SubmoduleCheckoutConfig::Global(m) => *m,
        SubmoduleCheckoutConfig::PerRepo(map) => {
            let target = normalize_repo_slug(owner, repo);
            // Exact common spellings first.
            for candidate in [
                format!("@{target}"),
                target.clone(),
                format!("@{target}.git"),
                format!("{target}.git"),
            ] {
                if let Some(m) = map.get(&candidate) {
                    return *m;
                }
            }
            // Case-insensitive / alternate spellings in the map.
            for (k, m) in map {
                if normalize_repo_key(k) == target {
                    return *m;
                }
            }
            SubmoduleCheckoutMode::None
        }
    }
}

#[derive(Debug, Clone)]
struct SubmoduleEntry {
    path: String,
    url: String,
}

/// Read `.gitmodules` entries from a worktree (or any tree that has the file).
fn read_gitmodules(worktree_dir: &Path) -> Vec<SubmoduleEntry> {
    let path = worktree_dir.join(".gitmodules");
    if !path.is_file() {
        return Vec::new();
    }
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    parse_gitmodules(&text)
}

fn parse_gitmodules(text: &str) -> Vec<SubmoduleEntry> {
    let mut out = Vec::new();
    let mut name = String::new();
    let mut path = String::new();
    let mut url = String::new();
    let mut in_sub = false;

    let flush = |out: &mut Vec<SubmoduleEntry>, name: &mut String, path: &mut String, url: &mut String| {
        if !name.is_empty() && !path.is_empty() {
            out.push(SubmoduleEntry {
                path: path.clone(),
                url: url.clone(),
            });
        }
        name.clear();
        path.clear();
        url.clear();
    };

    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            if in_sub {
                flush(&mut out, &mut name, &mut path, &mut url);
            }
            in_sub = false;
            // [submodule "apps/sgit"]
            if let Some(rest) = line.strip_prefix("[submodule ") {
                let rest = rest.trim_end_matches(']');
                let n = rest.trim().trim_matches('"');
                if !n.is_empty() {
                    name = n.to_string();
                    in_sub = true;
                }
            }
            continue;
        }
        if !in_sub {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            let v = v.trim();
            match k {
                "path" => path = v.to_string(),
                "url" => url = v.to_string(),
                _ => {}
            }
        }
    }
    if in_sub {
        flush(&mut out, &mut name, &mut path, &mut url);
    }
    out
}

/// Gitlink commit recorded for `sub_path` at HEAD of `worktree_dir`.
fn gitlink_commit(worktree_dir: &Path, sub_path: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["ls-tree", "HEAD", sub_path])
        .current_dir(worktree_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    // 160000 commit <sha>\tpath
    let line = String::from_utf8_lossy(&output.stdout);
    let line = line.lines().next()?.trim();
    let mut parts = line.split_whitespace();
    let mode = parts.next()?;
    let kind = parts.next()?;
    let sha = parts.next()?;
    if mode == "160000" && kind == "commit" {
        Some(sha.to_string())
    } else {
        None
    }
}

/// Materialize submodules for `worktree_dir` according to `mode`.
///
/// `bare_root` is the stokd/sgit bare-clone root used for shared worktree attach.
pub fn apply_submodule_checkout(
    worktree_dir: &Path,
    bare_root: &Path,
    mode: SubmoduleCheckoutMode,
) -> Result<(), String> {
    if matches!(mode, SubmoduleCheckoutMode::None) {
        return Ok(());
    }
    if !worktree_dir.is_dir() {
        return Ok(());
    }
    let entries = read_gitmodules(worktree_dir);
    if entries.is_empty() {
        return Ok(());
    }

    let mut errors = Vec::new();
    for entry in entries {
        if let Err(e) = materialize_one_submodule(worktree_dir, bare_root, &entry, mode) {
            errors.push(format!("{}: {e}", entry.path));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        // Best-effort: warn via Err so callers can log; do not hard-fail clone.
        Err(format!(
            "submodule checkout incomplete: {}",
            errors.join("; ")
        ))
    }
}

fn materialize_one_submodule(
    worktree_dir: &Path,
    bare_root: &Path,
    entry: &SubmoduleEntry,
    mode: SubmoduleCheckoutMode,
) -> Result<(), String> {
    let dest = worktree_dir.join(&entry.path);
    // Already populated (has .git file/dir or non-empty tree beyond placeholder).
    if submodule_path_populated(&dest) {
        return Ok(());
    }

    match mode {
        // Handled by the caller before we get here.
        SubmoduleCheckoutMode::None => Ok(()),
        // Git-native embedded submodule, pinned to the gitlink.
        SubmoduleCheckoutMode::Inline => inline_one_submodule(worktree_dir, entry),
        // Symlink to the canonical shared checkout; fall back to inline when
        // no canonical checkout exists (or on non-unix).
        SubmoduleCheckoutMode::Link => {
            match link_one_submodule(worktree_dir, bare_root, entry) {
                Ok(true) => Ok(()),
                Ok(false) => inline_one_submodule(worktree_dir, entry),
                Err(e) => Err(e),
            }
        }
        // Attach a linked worktree of the matching bare; inline fallback otherwise.
        SubmoduleCheckoutMode::Worktree => {
            let commit = gitlink_commit(worktree_dir, &entry.path);
            if let Some((owner, repo)) = parse_git_remote_url(&entry.url) {
                let bare = PathBuf::from(bare_root)
                    .join(&owner)
                    .join(format!("{repo}.git"));
                if bare.is_dir() {
                    if let Some(ref sha) = commit {
                        return attach_worktree_at(&bare, &dest, sha);
                    }
                }
            }
            inline_one_submodule(worktree_dir, entry)
        }
    }
}

/// Classic `git submodule update --init -- <path>` for one submodule.
fn inline_one_submodule(worktree_dir: &Path, entry: &SubmoduleEntry) -> Result<(), String> {
    let output = Command::new("git")
        .args(["submodule", "update", "--init", "--", &entry.path])
        .current_dir(worktree_dir)
        .output()
        .map_err(|e| format!("git submodule update failed to start: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git submodule update --init -- {}: {}",
            entry.path,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Symlink the submodule path to the canonical shared checkout of its bare.
///
/// Returns `Ok(true)` when a symlink was created, `Ok(false)` when no canonical
/// checkout could be resolved (caller should fall back to inline). Sets
/// `submodule.<name>.ignore=all` and `--skip-worktree` on the gitlink so the
/// symlinked path never dirties the superproject's `git status`.
fn link_one_submodule(
    worktree_dir: &Path,
    bare_root: &Path,
    entry: &SubmoduleEntry,
) -> Result<bool, String> {
    let Some((owner, repo)) = parse_git_remote_url(&entry.url) else {
        return Ok(false);
    };
    let bare = PathBuf::from(bare_root)
        .join(&owner)
        .join(format!("{repo}.git"));
    if !bare.is_dir() {
        return Ok(false);
    }
    let Some(canonical) = resolve_canonical_checkout(&bare) else {
        return Ok(false);
    };
    let dest = worktree_dir.join(&entry.path);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create submodule parent {}: {e}", parent.display()))?;
    }
    // Remove empty placeholder so the symlink can claim the path.
    if dest.is_dir() {
        let _ = fs::remove_dir(&dest);
    } else if dest.is_file() {
        let _ = fs::remove_file(&dest);
    }
    if !symlink_path(&canonical, &dest) {
        return Ok(false);
    }
    // Keep the symlinked gitlink from ever dirtying the superproject.
    let _ = Command::new("git")
        .args([
            "config",
            &format!("submodule.{}.ignore", entry.path),
            "all",
        ])
        .current_dir(worktree_dir)
        .output();
    let _ = Command::new("git")
        .args(["update-index", "--skip-worktree", "--", &entry.path])
        .current_dir(worktree_dir)
        .output();
    Ok(true)
}

/// The canonical shared checkout for a bare: the linked worktree that is on a
/// branch (not detached), preferring the bare's default branch. `None` when the
/// bare has no branch-checked-out worktree.
fn resolve_canonical_checkout(bare: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(bare)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let default_branch = crate::layout::resolve_default_branch(bare);
    let mut current: Option<PathBuf> = None;
    let mut fallback: Option<PathBuf> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            current = Some(PathBuf::from(p.trim()));
        } else if let Some(b) = line.strip_prefix("branch ") {
            let branch = b.trim().trim_start_matches("refs/heads/");
            if let Some(ref path) = current {
                if branch == default_branch {
                    return Some(path.clone());
                }
                if fallback.is_none() {
                    fallback = Some(path.clone());
                }
            }
        }
    }
    fallback
}

#[cfg(unix)]
fn symlink_path(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).is_ok()
}

#[cfg(not(unix))]
fn symlink_path(_target: &Path, _link: &Path) -> bool {
    // Symlink-based link mode is unix-only; caller falls back to inline.
    false
}

fn submodule_path_populated(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    // Nested repo: .git file or directory.
    if path.join(".git").exists() {
        return true;
    }
    // Non-empty directory that is more than an empty placeholder.
    if path.is_dir() {
        if let Ok(mut rd) = fs::read_dir(path) {
            return rd.next().is_some();
        }
    }
    false
}

fn attach_worktree_at(bare: &Path, dest: &Path, commit: &str) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create submodule parent {}: {e}", parent.display()))?;
    }
    // Remove empty placeholder dir so worktree add can claim the path.
    if dest.is_dir() {
        let _ = fs::remove_dir(dest);
    } else if dest.is_file() {
        let _ = fs::remove_file(dest);
    }

    let output = Command::new("git")
        .args([
            "worktree",
            "add",
            "--detach",
            &dest.to_string_lossy(),
            commit,
        ])
        .current_dir(bare)
        .output()
        .map_err(|e| format!("git worktree add failed to start: {e}"))?;
    if !output.status.success() {
        // Path may already be registered — leave a useful error.
        return Err(format!(
            "git worktree add --detach {} {}: {}",
            dest.display(),
            commit,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    // When the bare uses `extensions.worktreeConfig` with a shared
    // `core.bare=true`, the freshly-attached worktree inherits bare-ness and git
    // rejects it as "not a work tree". Write a per-worktree `core.bare=false`.
    ensure_worktree_not_bare(bare, dest);
    Ok(())
}

/// When `bare` uses `extensions.worktreeConfig` with shared `core.bare=true`,
/// set a per-worktree `core.bare=false` override in `worktree` so working-tree
/// ops succeed. No-op when `extensions.worktreeConfig` is not enabled.
fn ensure_worktree_not_bare(bare: &Path, worktree: &Path) {
    let wt_config = Command::new("git")
        .arg("-C")
        .arg(bare)
        .args(["config", "--bool", "extensions.worktreeConfig"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    if wt_config.as_deref() != Some("true") {
        return;
    }
    let _ = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["config", "--worktree", "core.bare", "false"])
        .output();
}

/// Convenience: resolve mode for owner/repo then apply.
pub fn apply_submodule_checkout_for_repo(
    worktree_dir: &Path,
    cfg: &crate::config::RepositoriesConfig,
    owner: &str,
    repo: &str,
) -> Result<(), String> {
    let mode = resolve_submodule_checkout(&cfg.submodule_checkout, owner, repo);
    apply_submodule_checkout(worktree_dir, Path::new(&cfg.bare_root), mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::tempdir;

    #[test]
    fn default_mode_is_none() {
        assert_eq!(
            SubmoduleCheckoutMode::default(),
            SubmoduleCheckoutMode::None
        );
        assert_eq!(
            SubmoduleCheckoutConfig::default(),
            SubmoduleCheckoutConfig::Global(SubmoduleCheckoutMode::None)
        );
    }

    #[test]
    fn parses_inline_and_link_modes() {
        for (raw, want) in [
            ("inline", SubmoduleCheckoutMode::Inline),
            ("embedded", SubmoduleCheckoutMode::Inline),
            ("link", SubmoduleCheckoutMode::Link),
            ("symlink", SubmoduleCheckoutMode::Link),
            ("shared", SubmoduleCheckoutMode::Link),
        ] {
            assert_eq!(SubmoduleCheckoutMode::parse(raw).unwrap(), want, "raw={raw}");
        }
        // Round-trip through as_str().
        for m in [
            SubmoduleCheckoutMode::None,
            SubmoduleCheckoutMode::Worktree,
            SubmoduleCheckoutMode::Inline,
            SubmoduleCheckoutMode::Link,
        ] {
            assert_eq!(SubmoduleCheckoutMode::parse(m.as_str()).unwrap(), m);
        }
    }

    #[test]
    fn deserializes_scalar_worktree() {
        let cfg: SubmoduleCheckoutConfig = serde_yaml::from_str("worktree").unwrap();
        assert_eq!(
            cfg,
            SubmoduleCheckoutConfig::Global(SubmoduleCheckoutMode::Worktree)
        );
    }

    #[test]
    fn deserializes_scalar_none() {
        let cfg: SubmoduleCheckoutConfig = serde_yaml::from_str("none").unwrap();
        assert_eq!(
            cfg,
            SubmoduleCheckoutConfig::Global(SubmoduleCheckoutMode::None)
        );
    }

    #[test]
    fn deserializes_per_repo_map() {
        let yaml = r#"
"@owner/repo1": worktree
"@owner/repo2": none
"#;
        let cfg: SubmoduleCheckoutConfig = serde_yaml::from_str(yaml).unwrap();
        match cfg {
            SubmoduleCheckoutConfig::PerRepo(map) => {
                assert_eq!(
                    map.get("@owner/repo1"),
                    Some(&SubmoduleCheckoutMode::Worktree)
                );
                assert_eq!(map.get("@owner/repo2"), Some(&SubmoduleCheckoutMode::None));
            }
            other => panic!("expected PerRepo, got {other:?}"),
        }
    }

    #[test]
    fn rejects_invalid_mode() {
        let err = serde_yaml::from_str::<SubmoduleCheckoutConfig>("banana").unwrap_err();
        assert!(
            err.to_string().contains("invalid submoduleCheckout mode"),
            "err={err}"
        );
    }

    #[test]
    fn resolve_scalar_applies_to_all() {
        let cfg = SubmoduleCheckoutConfig::Global(SubmoduleCheckoutMode::None);
        assert_eq!(
            resolve_submodule_checkout(&cfg, "a", "b"),
            SubmoduleCheckoutMode::None
        );
    }

    #[test]
    fn resolve_map_matches_at_owner_repo() {
        let mut map = BTreeMap::new();
        map.insert(
            "@stokd-cloud/mono".into(),
            SubmoduleCheckoutMode::Worktree,
        );
        map.insert("@other/repo".into(), SubmoduleCheckoutMode::None);
        let cfg = SubmoduleCheckoutConfig::PerRepo(map);
        assert_eq!(
            resolve_submodule_checkout(&cfg, "stokd-cloud", "mono"),
            SubmoduleCheckoutMode::Worktree
        );
        assert_eq!(
            resolve_submodule_checkout(&cfg, "other", "repo"),
            SubmoduleCheckoutMode::None
        );
    }

    #[test]
    fn resolve_map_normalizes_case_and_git_suffix() {
        let mut map = BTreeMap::new();
        map.insert(
            "@Owner/Repo.git".into(),
            SubmoduleCheckoutMode::None,
        );
        let cfg = SubmoduleCheckoutConfig::PerRepo(map);
        assert_eq!(
            resolve_submodule_checkout(&cfg, "owner", "repo"),
            SubmoduleCheckoutMode::None
        );
    }

    #[test]
    fn resolve_map_unlisted_defaults_to_none() {
        let map = BTreeMap::new();
        let cfg = SubmoduleCheckoutConfig::PerRepo(map);
        assert_eq!(
            resolve_submodule_checkout(&cfg, "x", "y"),
            SubmoduleCheckoutMode::None
        );
    }

    #[test]
    fn parse_gitmodules_entries() {
        let text = r#"
[submodule "apps/sgit"]
	path = apps/sgit
	url = git@github.com:stokd-cloud/sgit.git
	branch = main
[submodule "apps/code"]
	path = apps/code
	url = git@github.com:stokd-cloud/code.git
"#;
        let entries = parse_gitmodules(text);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "apps/sgit");
        assert_eq!(entries[0].url, "git@github.com:stokd-cloud/sgit.git");
        assert_eq!(entries[1].path, "apps/code");
    }

    fn git(args: &[&str], cwd: &Path) {
        let o = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git");
        assert!(
            o.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    }

    fn init_repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        git(&["init", "-b", "main"], dir);
        git(&["config", "user.email", "t@example.com"], dir);
        git(&["config", "user.name", "t"], dir);
    }

    /// mode=none leaves submodule path empty even when .gitmodules exists.
    #[test]
    fn apply_none_leaves_submodule_unpopulated() {
        let tmp = tempdir().unwrap();
        let sub = tmp.path().join("sub");
        let super_repo = tmp.path().join("super");
        init_repo(&sub);
        fs::write(sub.join("file.txt"), "hi").unwrap();
        git(&["add", "."], &sub);
        git(&["commit", "-m", "sub"], &sub);

        init_repo(&super_repo);
        // Add submodule via gitlink without populating (simulate worktree without recurse).
        let sha = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&sub)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        // Record submodule without checkout.
        fs::write(
            super_repo.join(".gitmodules"),
            format!(
                "[submodule \"nested\"]\n\tpath = nested\n\turl = {}\n",
                sub.display()
            ),
        )
        .unwrap();
        // git update-index --add --cacheinfo 160000 <sha> nested
        git(
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{sha},nested"),
            ],
            &super_repo,
        );
        git(&["add", ".gitmodules"], &super_repo);
        git(&["commit", "-m", "add sub"], &super_repo);

        apply_submodule_checkout(&super_repo, tmp.path(), SubmoduleCheckoutMode::None).unwrap();
        assert!(
            !super_repo.join("nested").join("file.txt").exists(),
            "none must not populate submodule"
        );
    }

    /// mode=worktree attaches from an existing bare of the submodule.
    #[test]
    fn apply_worktree_attaches_from_bare() {
        let tmp = tempdir().unwrap();
        let bare_root = tmp.path().join("bares");
        let sub_src = tmp.path().join("sub-src");
        let super_repo = tmp.path().join("super");

        init_repo(&sub_src);
        fs::write(sub_src.join("marker.txt"), "from-sub").unwrap();
        git(&["add", "."], &sub_src);
        git(&["commit", "-m", "sub"], &sub_src);
        let sha = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&sub_src)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        // Bare under bareRoot/owner/repo.git matching the submodule URL owner/repo.
        let bare = bare_root.join("acme").join("widget.git");
        fs::create_dir_all(bare.parent().unwrap()).unwrap();
        let o = Command::new("git")
            .args(["clone", "--bare", &sub_src.to_string_lossy(), &bare.to_string_lossy()])
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

        init_repo(&super_repo);
        fs::write(
            super_repo.join(".gitmodules"),
            "[submodule \"nested\"]\n\tpath = nested\n\turl = git@github.com:acme/widget.git\n",
        )
        .unwrap();
        git(
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{sha},nested"),
            ],
            &super_repo,
        );
        git(&["add", ".gitmodules"], &super_repo);
        git(&["commit", "-m", "add sub"], &super_repo);

        apply_submodule_checkout(&super_repo, &bare_root, SubmoduleCheckoutMode::Worktree)
            .expect("worktree attach should succeed");

        let marker = super_repo.join("nested").join("marker.txt");
        assert!(
            marker.is_file(),
            "submodule path should be populated via worktree"
        );
        assert_eq!(fs::read_to_string(marker).unwrap().trim(), "from-sub");
    }

    /// mode=inline embeds the submodule (gitdir under the superproject).
    #[test]
    fn apply_inline_embeds_submodule() {
        let tmp = tempdir().unwrap();
        let sub_src = tmp.path().join("sub-src");
        let super_repo = tmp.path().join("super");

        init_repo(&sub_src);
        fs::write(sub_src.join("marker.txt"), "inlined").unwrap();
        git(&["add", "."], &sub_src);
        git(&["commit", "-m", "sub"], &sub_src);

        init_repo(&super_repo);
        // Real add so the gitlink + .gitmodules are consistent for `submodule update`.
        let mut add = Command::new("git");
        add.args([
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            &sub_src.to_string_lossy(),
            "nested",
        ])
        .current_dir(&super_repo);
        let o = add.output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        git(&["commit", "-m", "add sub"], &super_repo);
        // Deinit so inline has to repopulate.
        git(&["submodule", "deinit", "-f", "nested"], &super_repo);
        assert!(!super_repo.join("nested").join("marker.txt").exists());

        apply_submodule_checkout(&super_repo, tmp.path(), SubmoduleCheckoutMode::Inline)
            .expect("inline should populate");
        assert!(
            super_repo.join("nested").join(".git").exists(),
            "inline must embed a .git under the submodule path"
        );
    }

    /// The worktree-mode fix: a linked worktree of a bare that uses
    /// extensions.worktreeConfig must get a per-worktree core.bare=false, or git
    /// rejects it as "not a work tree". Simulates the real breakage by removing
    /// the config.worktree, then repairs it via ensure_worktree_not_bare.
    #[test]
    fn ensure_worktree_not_bare_repairs_missing_override() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("src");
        let bare = tmp.path().join("repo.git");
        init_repo(&src);
        fs::write(src.join("f.txt"), "x").unwrap();
        git(&["add", "."], &src);
        git(&["commit", "-m", "c"], &src);
        let o = Command::new("git")
            .args(["clone", "--bare", &src.to_string_lossy(), &bare.to_string_lossy()])
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        // Bare uses worktreeConfig with shared core.bare=true (the real setup).
        git(&["config", "extensions.worktreeConfig", "true"], &bare);
        git(&["config", "core.bare", "true"], &bare);

        let wt = tmp.path().join("wt");
        let o = Command::new("git")
            .args(["worktree", "add", "--detach", &wt.to_string_lossy(), "HEAD"])
            .current_dir(&bare)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

        // Force the broken state: remove the per-worktree config override.
        let wt_meta = bare.join("worktrees");
        if let Ok(rd) = fs::read_dir(&wt_meta) {
            for e in rd.flatten() {
                let _ = fs::remove_file(e.path().join("config.worktree"));
            }
        }
        let broken = Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(&wt)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&broken.stdout).trim(),
            "false",
            "without the override the worktree must be rejected"
        );

        ensure_worktree_not_bare(&bare, &wt);

        let fixed = Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(&wt)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&fixed.stdout).trim(),
            "true",
            "override must make the worktree usable"
        );
    }

    /// mode=link symlinks the submodule path to the bare's canonical (branch)
    /// checkout and keeps the superproject `git status` clean.
    #[test]
    fn apply_link_symlinks_and_keeps_status_clean() {
        if cfg!(not(unix)) {
            return;
        }
        let tmp = tempdir().unwrap();
        let bare_root = tmp.path().join("bares");
        let sub_src = tmp.path().join("sub-src");
        let super_repo = tmp.path().join("super");

        init_repo(&sub_src);
        fs::write(sub_src.join("marker.txt"), "canonical").unwrap();
        git(&["add", "."], &sub_src);
        git(&["commit", "-m", "sub"], &sub_src);
        let sha = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&sub_src)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        // Bare + a canonical checkout on branch `main`.
        let bare = bare_root.join("acme").join("widget.git");
        fs::create_dir_all(bare.parent().unwrap()).unwrap();
        let o = Command::new("git")
            .args(["clone", "--bare", &sub_src.to_string_lossy(), &bare.to_string_lossy()])
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let canonical = tmp.path().join("canonical-main");
        let o = Command::new("git")
            .args(["worktree", "add", &canonical.to_string_lossy(), "main"])
            .current_dir(&bare)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

        init_repo(&super_repo);
        fs::write(
            super_repo.join(".gitmodules"),
            "[submodule \"nested\"]\n\tpath = nested\n\turl = git@github.com:acme/widget.git\n",
        )
        .unwrap();
        git(
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{sha},nested"),
            ],
            &super_repo,
        );
        git(&["add", ".gitmodules"], &super_repo);
        git(&["commit", "-m", "add sub"], &super_repo);

        apply_submodule_checkout(&super_repo, &bare_root, SubmoduleCheckoutMode::Link)
            .expect("link should succeed");

        let dest = super_repo.join("nested");
        assert!(
            fs::symlink_metadata(&dest).unwrap().file_type().is_symlink(),
            "link mode must create a symlink at the submodule path"
        );
        assert!(dest.join("marker.txt").is_file(), "symlink must resolve to the checkout");

        let status = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&super_repo)
            .output()
            .unwrap();
        let status = String::from_utf8_lossy(&status.stdout);
        assert!(
            !status.lines().any(|l| l.contains("nested")),
            "link must not dirty the superproject status, got: {status:?}"
        );
    }
}
