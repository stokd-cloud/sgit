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
//! - `worktree` (default): after a superproject worktree is created, populate
//!   each submodule by attaching a worktree of the matching bare under
//!   `bareRoot` when present; otherwise best-effort `git submodule update --init`.
//! - `none`: leave gitlinks unpopulated (fast empty worktrees).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::layout::parse_git_remote_url;

/// How submodules are materialized inside a newly created superproject worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmoduleCheckoutMode {
    /// Attach shared worktrees from `bareRoot` (or submodule update --init fallback).
    Worktree,
    /// Do not populate submodule working trees.
    None,
}

impl SubmoduleCheckoutMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SubmoduleCheckoutMode::Worktree => "worktree",
            SubmoduleCheckoutMode::None => "none",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "worktree" | "wt" => Ok(SubmoduleCheckoutMode::Worktree),
            "none" | "off" | "false" | "no" => Ok(SubmoduleCheckoutMode::None),
            other => Err(format!(
                "invalid submoduleCheckout mode '{other}' (expected worktree|none)"
            )),
        }
    }
}

impl Default for SubmoduleCheckoutMode {
    fn default() -> Self {
        SubmoduleCheckoutMode::Worktree
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
    /// Per-repo overrides. Unlisted repos use [`SubmoduleCheckoutMode::Worktree`].
    PerRepo(BTreeMap<String, SubmoduleCheckoutMode>),
}

impl Default for SubmoduleCheckoutConfig {
    fn default() -> Self {
        SubmoduleCheckoutConfig::Global(SubmoduleCheckoutMode::Worktree)
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
                            "submoduleCheckout['{key}'] must be a mode string (worktree|none)"
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
            SubmoduleCheckoutMode::Worktree
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
        if let Err(e) = materialize_one_submodule(worktree_dir, bare_root, &entry) {
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
) -> Result<(), String> {
    let dest = worktree_dir.join(&entry.path);
    // Already populated (has .git file/dir or non-empty tree beyond placeholder).
    if submodule_path_populated(&dest) {
        return Ok(());
    }

    let commit = gitlink_commit(worktree_dir, &entry.path);
    if let Some((owner, repo)) = parse_git_remote_url(&entry.url).or_else(|| {
        // Relative submodule URLs are uncommon in our layout; ignore.
        None
    }) {
        let bare = PathBuf::from(bare_root).join(&owner).join(format!("{repo}.git"));
        if bare.is_dir() {
            if let Some(ref sha) = commit {
                return attach_worktree_at(&bare, &dest, sha);
            }
        }
    }

    // Fallback: classic submodule init for this path only.
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
    Ok(())
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
    fn default_mode_is_worktree() {
        assert_eq!(
            SubmoduleCheckoutMode::default(),
            SubmoduleCheckoutMode::Worktree
        );
        assert_eq!(
            SubmoduleCheckoutConfig::default(),
            SubmoduleCheckoutConfig::Global(SubmoduleCheckoutMode::Worktree)
        );
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
    fn resolve_map_unlisted_defaults_to_worktree() {
        let map = BTreeMap::new();
        let cfg = SubmoduleCheckoutConfig::PerRepo(map);
        assert_eq!(
            resolve_submodule_checkout(&cfg, "x", "y"),
            SubmoduleCheckoutMode::Worktree
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
}
