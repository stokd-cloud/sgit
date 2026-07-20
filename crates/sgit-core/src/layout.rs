//! Canonical bare + main-worktree layout helpers shared by `sgit` and callers.
//!
//! Paths:
//! - bare:     `<bareRoot>/<owner>/<repo>.git`
//! - worktree: `<root>/<owner>/<repo>/<mainWorktreeName>`
//!
//! Default-branch resolution order matches stokd/`RepoProvisioner` so both
//! sides agree on the main worktree leaf (incl. `stokd.defaultBranch` continuity).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::RepositoriesConfig;

/// Placeholder branch the bare repo's HEAD points at after a clone so no real
/// working branch is pinned by the bare. Never checked out in a worktree.
/// Name kept for on-disk continuity with existing stokd bare clones.
pub const BARE_PLACEHOLDER_HEAD: &str = "refs/heads/__stokd_hub__";

/// Canonical bare-clone and main-worktree locations for a repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoLayout {
    pub bare_dir: PathBuf,
    pub worktree_dir: PathBuf,
}

/// Resolve the canonical bare + main-worktree paths for `owner/repo` from config.
///
/// When the bare clone already exists, the worktree leaf uses the repo's real
/// default branch. Otherwise the `"main"` placeholder leaf is used; fresh-clone
/// callers recompute via [`worktree_dir_for_branch`] once the bare exists.
pub fn resolve_repo_layout(cfg: &RepositoriesConfig, owner: &str, repo_name: &str) -> RepoLayout {
    let bare_root = PathBuf::from(&cfg.bare_root);
    let bare_dir = bare_root.join(owner).join(format!("{repo_name}.git"));

    let leaf = if bare_dir.exists() {
        render_worktree_name_pattern(
            &cfg.main_worktree_name,
            owner,
            repo_name,
            &resolve_default_branch(&bare_dir),
        )
    } else {
        main_worktree_leaf(cfg, owner, repo_name)
    };

    RepoLayout {
        worktree_dir: PathBuf::from(&cfg.worktree_root)
            .join(owner)
            .join(repo_name)
            .join(leaf),
        bare_dir,
    }
}

/// Main-worktree directory for `owner/repo` at a known `branch`.
pub fn worktree_dir_for_branch(
    cfg: &RepositoriesConfig,
    owner: &str,
    repo_name: &str,
    branch: &str,
) -> PathBuf {
    let leaf = render_worktree_name_pattern(&cfg.main_worktree_name, owner, repo_name, branch);
    PathBuf::from(&cfg.worktree_root)
        .join(owner)
        .join(repo_name)
        .join(leaf)
}

/// Leaf name for a not-yet-cloned repo (default branch assumed `"main"`).
pub fn main_worktree_leaf(cfg: &RepositoriesConfig, owner: &str, repo_name: &str) -> String {
    render_worktree_name_pattern(&cfg.main_worktree_name, owner, repo_name, "main")
}

/// Render `mainWorktreeName` with `{owner}`/`{repo}`/`{branch}` substitution.
pub fn render_worktree_name_pattern(
    pattern: &str,
    owner: &str,
    repo_name: &str,
    branch: &str,
) -> String {
    let fallback = format!(
        "{}-{}",
        sanitize_worktree_name_part(repo_name),
        sanitize_worktree_name_part(branch)
    );
    let trimmed = pattern.trim();
    let template = if trimmed.is_empty() {
        "{repo}-{branch}"
    } else {
        trimmed
    };

    let rendered = template
        .replace("{owner}", &sanitize_worktree_name_part(owner))
        .replace("{repo}", &sanitize_worktree_name_part(repo_name))
        .replace("{branch}", &sanitize_worktree_name_part(branch));
    let sanitized = sanitize_worktree_name_part(&rendered);
    if sanitized.is_empty() {
        fallback
    } else {
        sanitized
    }
}

fn sanitize_worktree_name_part(value: &str) -> String {
    value
        .trim()
        .replace(['/', '\\'], "-")
        .trim_matches(['-', '.', ' '])
        .to_string()
}

/// Resolve a bare repo's default branch:
/// 1. `stokd.defaultBranch` config (continuity key)
/// 2. symbolic HEAD (ignoring placeholder)
/// 3. probe `main` then `master`
/// 4. fallback `"main"`
pub fn resolve_default_branch(bare_dir: &Path) -> String {
    if let Some(configured) = read_configured_default_branch(bare_dir) {
        return configured;
    }

    if let Some(head) = read_bare_symbolic_head(bare_dir) {
        if head != bare_placeholder_branch() {
            write_default_branch_config(bare_dir, &head);
            return head;
        }
    }

    for candidate in ["main", "master"] {
        if git_ref_exists(bare_dir, &format!("refs/heads/{candidate}")) {
            write_default_branch_config(bare_dir, candidate);
            return candidate.to_string();
        }
    }

    "main".to_string()
}

/// Short placeholder branch name (`__stokd_hub__`).
pub fn bare_placeholder_branch() -> &'static str {
    BARE_PLACEHOLDER_HEAD
        .strip_prefix("refs/heads/")
        .unwrap_or(BARE_PLACEHOLDER_HEAD)
}

/// Point a freshly-cloned bare repo's HEAD at [`BARE_PLACEHOLDER_HEAD`].
pub fn point_bare_head_to_placeholder(bare_dir: &Path) -> Result<(), String> {
    run_git_dir(
        bare_dir,
        &["symbolic-ref", "HEAD", BARE_PLACEHOLDER_HEAD],
    )
}

/// Bare-clone from `remote_url` into `bare_dir`, configure fetch refspec,
/// persist default branch, repoint HEAD to the placeholder.
pub fn bare_clone_from_url(remote_url: &str, bare_dir: &Path) -> Result<(), String> {
    if bare_dir.exists() {
        return Err(format!(
            "bare clone directory already exists: {}",
            bare_dir.display()
        ));
    }

    if let Some(parent) = bare_dir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create bare directory parent: {e}"))?;
    }

    let output = Command::new("git")
        .args(["clone", "--bare", remote_url, &bare_dir.to_string_lossy()])
        .output()
        .map_err(|e| format!("failed to run git clone --bare: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("bare clone failed: {stderr}"));
    }

    run_git_dir(
        bare_dir,
        &[
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
    )?;

    if let Some(branch) = read_bare_symbolic_head(bare_dir) {
        write_default_branch_config(bare_dir, &branch);
    }

    point_bare_head_to_placeholder(bare_dir)?;
    Ok(())
}

/// Bare-clone `owner/repo` from GitHub (SSH URL).
pub fn bare_clone(owner: &str, repo_name: &str, bare_dir: &Path) -> Result<(), String> {
    let remote_url = format!("git@github.com:{owner}/{repo_name}.git");
    bare_clone_from_url(&remote_url, bare_dir)
}

/// `git worktree add <worktree_dir> <branch>` against a bare repo.
///
/// Does not install stokd-specific hooks or cloud worktree-count refresh —
/// those stay stokd-domain. Optionally pins the worktree when `pin` is true.
pub fn create_worktree(
    bare_dir: &Path,
    worktree_dir: &Path,
    branch: &str,
    pin: bool,
) -> Result<(), String> {
    if worktree_dir.exists() {
        return Err(format!(
            "worktree directory already exists: {}",
            worktree_dir.display()
        ));
    }

    if let Some(parent) = worktree_dir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create worktree directory parent: {e}"))?;
    }

    let output = Command::new("git")
        .args(["worktree", "add", &worktree_dir.to_string_lossy(), branch])
        .current_dir(bare_dir)
        .output()
        .map_err(|e| format!("failed to run git worktree add: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("worktree creation failed: {stderr}"));
    }

    if pin {
        if let Err(e) = crate::worktree_pin::write_pin_marker(worktree_dir) {
            eprintln!(
                "warning: could not update worktree pin marker at {}: {e}",
                worktree_dir.display()
            );
        }
    }

    Ok(())
}

/// Enumerate working-tree directories linked to a bare repo (excludes the bare).
///
/// Paths are compared via [`same_path`] so macOS `/tmp` vs `/private/tmp`
/// (and other resolve-equivalent spellings) do not mis-classify the bare itself
/// as a linked worktree.
pub fn list_linked_worktrees(bare_dir: &Path) -> Vec<PathBuf> {
    let output = Command::new("git")
        .args([
            "--git-dir",
            &bare_dir.to_string_lossy(),
            "worktree",
            "list",
            "--porcelain",
        ])
        .output();
    let Ok(out) = output else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(|p| PathBuf::from(p.trim()))
        .filter(|p| !same_path(p, bare_dir))
        .collect()
}

/// True when two paths refer to the same filesystem location after
/// best-effort canonicalization (handles macOS `/tmp` → `/private/tmp`).
pub fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

/// Best-effort absolute/canonical form for path prefix remaps.
pub fn normalize_path(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Parse a git remote URL string into `(owner, repo)`.
///
/// Supports SSH (`git@host:owner/repo.git`) and HTTPS
/// (`https://host/owner/repo.git`). The `.git` suffix is optional.
pub fn parse_git_remote_url(url: &str) -> Option<(String, String)> {
    let url = url.trim();

    if let Some(colon_pos) = url.find(':') {
        let prefix = &url[..colon_pos];
        if prefix.contains('@') && !prefix.contains('/') {
            let path = &url[colon_pos + 1..];
            return split_owner_repo(path);
        }
    }

    if url.starts_with("https://") || url.starts_with("http://") {
        let without_scheme = url.split_once("://").map(|x| x.1).unwrap_or("");
        let path = without_scheme.split_once('/').map(|x| x.1).unwrap_or("");
        return split_owner_repo(path);
    }

    None
}

fn split_owner_repo(path: &str) -> Option<(String, String)> {
    let path = path.trim_end_matches('/');
    let mut parts = path.splitn(2, '/');
    let owner = parts.next().filter(|s| !s.is_empty())?.to_string();
    let repo_raw = parts.next().filter(|s| !s.is_empty())?;
    let repo = repo_raw.trim_end_matches(".git").to_string();
    if repo.is_empty() {
        return None;
    }
    Some((owner, repo))
}

/// Read a bare repo's `remote.origin.url`.
pub fn resolve_origin_url(bare_dir: &Path) -> Option<String> {
    Command::new("git")
        .args([
            "--git-dir",
            &bare_dir.to_string_lossy(),
            "config",
            "--get",
            "remote.origin.url",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn read_bare_symbolic_head(bare_dir: &Path) -> Option<String> {
    Command::new("git")
        .args([
            "--git-dir",
            &bare_dir.to_string_lossy(),
            "symbolic-ref",
            "--short",
            "HEAD",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn read_configured_default_branch(bare_dir: &Path) -> Option<String> {
    Command::new("git")
        .args([
            "--git-dir",
            &bare_dir.to_string_lossy(),
            "config",
            "--get",
            "stokd.defaultBranch",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn write_default_branch_config(bare_dir: &Path, branch: &str) {
    let _ = Command::new("git")
        .args([
            "--git-dir",
            &bare_dir.to_string_lossy(),
            "config",
            "stokd.defaultBranch",
            branch,
        ])
        .output();
}

pub fn git_ref_exists(bare_dir: &Path, refname: &str) -> bool {
    Command::new("git")
        .args([
            "--git-dir",
            &bare_dir.to_string_lossy(),
            "show-ref",
            "--verify",
            "--quiet",
            refname,
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run `git --git-dir <bare> <args>` (or `git -C` when bare is a worktree dir).
pub fn run_git_dir(cwd: &Path, args: &[&str]) -> Result<(), String> {
    // Prefer --git-dir for bare dirs; -C also works for both.
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(|e| format!("failed to run git {}: {e}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {} failed: {stderr}", args.join(" ")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_repo_layout_uses_canonical_paths() {
        let cfg = RepositoriesConfig {
            bare_root: "/opt/dev".into(),
            worktree_root: "/opt/worktrees".into(),
            main_worktree_name: "{branch}".into(),
            track_non_git_workspaces: false,
        };
        let layout = resolve_repo_layout(&cfg, "stokd-cloud", "autores");
        assert_eq!(
            layout.bare_dir,
            PathBuf::from("/opt/dev/stokd-cloud/autores.git")
        );
        assert_eq!(
            layout.worktree_dir,
            PathBuf::from("/opt/worktrees/stokd-cloud/autores/main")
        );
    }

    #[test]
    fn render_sanitizes_path_separators() {
        assert_eq!(
            render_worktree_name_pattern("{repo}/{branch}", "o", "auto/res", "feature/x"),
            "auto-res-feature-x"
        );
    }

    #[test]
    fn parse_ssh_and_https() {
        assert_eq!(
            parse_git_remote_url("git@github.com:owner/repo.git"),
            Some(("owner".into(), "repo".into()))
        );
        assert_eq!(
            parse_git_remote_url("https://github.com/owner/repo"),
            Some(("owner".into(), "repo".into()))
        );
        assert_eq!(parse_git_remote_url("not-a-url"), None);
    }
}
