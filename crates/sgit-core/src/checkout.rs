//! Pin-safe branch checkout (AX-CLI-CHECKOUT-SIBLING-WORKTREE).
//!
//! `sgit checkout <branch>` never switches the current worktree's branch in place.
//! It reuses an existing linked worktree for the branch, or creates a sibling
//! worktree under the configured worktree root whose leaf is the sanitized
//! branch name, then returns that path for shell navigation.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::RepositoriesConfig;
use crate::layout::{parse_git_remote_url, render_worktree_name_pattern};
use crate::worktree_pin::{resolve_common_git_dir, write_pin_marker};
use crate::workspace::{detect_repo_root_at, find_worktree_for_branch_at, resolve_default_branch_at};

/// Outcome of ensuring a worktree for a branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsureBranchWorktree {
    /// Absolute path of the worktree checked out on the requested branch.
    pub path: PathBuf,
    /// True when this call created a new linked worktree.
    pub created: bool,
    /// Human-readable source of the branch content (for stderr diagnostics).
    pub source: String,
}

/// Normalize a user-supplied branch name: strip `refs/heads/`, reject empty.
pub fn normalize_branch_name(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("branch name must not be empty".to_string());
    }
    let name = trimmed
        .strip_prefix("refs/heads/")
        .unwrap_or(trimmed)
        .trim();
    if name.is_empty() {
        return Err("branch name must not be empty".to_string());
    }
    if name.contains("..") || name.starts_with('-') {
        return Err(format!("invalid branch name '{name}'"));
    }
    Ok(name.to_string())
}

/// Leaf directory name for a branch under the worktree layout (slashes → dashes).
pub fn branch_worktree_leaf(branch: &str) -> String {
    // Pattern `{branch}` matches the default `mainWorktreeName` and yields a
    // single sanitized segment (e.g. `feature/foo` → `feature-foo`).
    render_worktree_name_pattern("{branch}", "", "", branch)
}

/// Preferred absolute path for a new worktree of `branch` belonging to the repo
/// at `repo_root`, using configured worktree root + owner/repo when known.
pub fn preferred_branch_worktree_path(
    repo_root: &Path,
    cfg: &RepositoriesConfig,
    branch: &str,
) -> PathBuf {
    let leaf = branch_worktree_leaf(branch);
    let worktree_root = PathBuf::from(&cfg.worktree_root);
    if let Some((owner, repo)) = detect_owner_repo(repo_root) {
        return worktree_root.join(owner).join(repo).join(leaf);
    }
    let repo_name = repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo");
    worktree_root.join("local").join(repo_name).join(leaf)
}

/// Ensure a linked worktree exists for `branch` and return its path.
///
/// Never runs in-place `git checkout` / `git switch` on the current worktree.
/// `git worktree add` runs from the common (unpinned) git dir so the pin hook
/// cannot mistake a sibling add for an in-place branch switch.
pub fn ensure_branch_worktree(
    cwd: &Path,
    branch: &str,
    cfg: &RepositoriesConfig,
) -> Result<EnsureBranchWorktree, String> {
    let branch = normalize_branch_name(branch)?;
    let repo_root = detect_repo_root_at(cwd).ok_or_else(|| {
        format!(
            "not inside a git repository: {}",
            cwd.display()
        )
    })?;

    // Reuse an existing worktree for this branch (any location).
    if let Ok(existing) = find_worktree_for_branch_at(&repo_root, &branch) {
        let path = canonicalize_existing(&existing);
        return Ok(EnsureBranchWorktree {
            path,
            created: false,
            source: format!("existing worktree for branch {branch}"),
        });
    }

    let path = preferred_branch_worktree_path(&repo_root, cfg, &branch);
    if path.exists() {
        // Path occupied but not listed as a worktree on this branch — refuse.
        return Err(format!(
            "worktree path already exists but is not checked out on '{branch}': {}",
            path.display()
        ));
    }

    let (created_path, source) = create_branch_worktree(&repo_root, &path, &branch)?;

    // Pin the new worktree (best-effort; pin may no-op if not linked yet).
    if let Err(e) = write_pin_marker(&created_path) {
        eprintln!(
            "warning: could not write pin marker at {}: {e}",
            created_path.display()
        );
    }

    // Submodule policy (best-effort).
    if let Some((owner, repo)) = detect_owner_repo(&repo_root) {
        if let Err(e) =
            crate::submodule_checkout::apply_submodule_checkout_for_repo(&created_path, cfg, &owner, &repo)
        {
            eprintln!("warning: submodule checkout: {e}");
        }
    }

    Ok(EnsureBranchWorktree {
        path: canonicalize_existing(&created_path),
        created: true,
        source,
    })
}

/// Create a linked worktree at `path` for `branch` from the common git dir.
fn create_branch_worktree(
    repo_root: &Path,
    path: &Path,
    branch: &str,
) -> Result<(PathBuf, String), String> {
    // Best-effort fetch so origin-only branches are visible.
    let _ = Command::new("git")
        .args(["fetch", "origin"])
        .current_dir(repo_root)
        .output();

    let common = resolve_common_git_dir(repo_root)
        .unwrap_or_else(|| repo_root.to_path_buf());

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "cannot create worktree parent {}: {e}",
                parent.display()
            )
        })?;
    }

    let path_str = path.to_string_lossy().to_string();

    let run = |args: &[&str]| -> std::io::Result<std::process::Output> {
        Command::new("git")
            .arg("worktree")
            .arg("add")
            .args(args)
            .current_dir(&common)
            .output()
    };

    let local = branch_exists_local(&common, branch);
    let remote = branch_exists_remote(&common, branch);

    let (args, source): (Vec<String>, String) = if local {
        (
            vec![path_str.clone(), branch.to_string()],
            format!("existing local branch {branch}"),
        )
    } else if remote {
        (
            vec![
                "--track".to_string(),
                "-b".to_string(),
                branch.to_string(),
                path_str.clone(),
                format!("origin/{branch}"),
            ],
            format!("origin/{branch} (new tracking branch)"),
        )
    } else {
        let default = resolve_default_branch_at(repo_root);
        (
            vec![
                "-b".to_string(),
                branch.to_string(),
                path_str.clone(),
                format!("origin/{default}"),
            ],
            format!("new branch off origin/{default}"),
        )
    };

    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    match run(&borrowed) {
        Ok(o) if o.status.success() => Ok((path.to_path_buf(), source)),
        Ok(first_err) => {
            // Only fall back to HEAD when we were creating a *new* branch.
            if local || remote {
                return Err(format!(
                    "failed to create worktree: {}",
                    String::from_utf8_lossy(&first_err.stderr).trim()
                ));
            }
            let head_commit = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(repo_root)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "HEAD".to_string());
            match run(&["-b", branch, &path_str, &head_commit]) {
                Ok(o) if o.status.success() => Ok((
                    path.to_path_buf(),
                    format!("new branch off HEAD ({head_commit})"),
                )),
                _ => Err(format!(
                    "failed to create worktree: {}",
                    String::from_utf8_lossy(&first_err.stderr).trim()
                )),
            }
        }
        Err(e) => Err(format!("failed to run git worktree add: {e}")),
    }
}

fn branch_exists_local(common: &Path, name: &str) -> bool {
    git_ref_ok(common, &format!("refs/heads/{name}"))
}

fn branch_exists_remote(common: &Path, name: &str) -> bool {
    git_ref_ok(common, &format!("refs/remotes/origin/{name}"))
}

fn git_ref_ok(common: &Path, refname: &str) -> bool {
    Command::new("git")
        .args(["show-ref", "--verify", "--quiet", refname])
        .current_dir(common)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn detect_owner_repo(repo_root: &Path) -> Option<(String, String)> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    parse_git_remote_url(&url)
}

fn canonicalize_existing(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::tempdir;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Bare-style multi-worktree fixture: primary on main + optional extra.
    fn scratch_repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempdir().expect("tempdir");
        let primary = tmp.path().join("primary");
        std::fs::create_dir_all(&primary).unwrap();
        git(&primary, &["init", "-q", "-b", "main"]);
        git(&primary, &["config", "user.email", "t@t.co"]);
        git(&primary, &["config", "user.name", "t"]);
        git(&primary, &["commit", "-q", "--allow-empty", "-m", "init"]);
        (tmp, primary)
    }

    fn test_cfg(tmp: &Path) -> RepositoriesConfig {
        RepositoriesConfig {
            bare_root: tmp.join("dev").to_string_lossy().to_string(),
            worktree_root: tmp.join("worktrees").to_string_lossy().to_string(),
            main_worktree_name: "{branch}".to_string(),
            track_non_git_workspaces: false,
            submodule_checkout: Default::default(),
        }
    }

    #[test]
    fn normalize_branch_strips_refs_heads() {
        assert_eq!(normalize_branch_name("refs/heads/foo").unwrap(), "foo");
        assert_eq!(normalize_branch_name("  bar  ").unwrap(), "bar");
        assert!(normalize_branch_name("").is_err());
        assert!(normalize_branch_name("-bad").is_err());
    }

    #[test]
    fn branch_leaf_sanitizes_slashes() {
        assert_eq!(branch_worktree_leaf("feature/foo"), "feature-foo");
        assert_eq!(branch_worktree_leaf("main"), "main");
    }

    #[test]
    fn reuse_existing_worktree_for_branch() {
        let (tmp, primary) = scratch_repo();
        let cfg = test_cfg(tmp.path());
        let wt = tmp.path().join("already");
        git(
            &primary,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature-x",
                wt.to_str().unwrap(),
            ],
        );

        let result = ensure_branch_worktree(&primary, "feature-x", &cfg).unwrap();
        assert!(!result.created);
        assert_eq!(
            canonicalize_existing(&result.path),
            canonicalize_existing(&wt)
        );
        // No path under configured worktree root was created for the reuse case.
        assert!(!PathBuf::from(&cfg.worktree_root).join("local").exists()
            || !PathBuf::from(&cfg.worktree_root)
                .join("local")
                .join("primary")
                .join("feature-x")
                .exists());
    }

    #[test]
    fn create_worktree_from_local_branch() {
        let (tmp, primary) = scratch_repo();
        let cfg = test_cfg(tmp.path());
        git(&primary, &["branch", "local-only"]);

        let result = ensure_branch_worktree(&primary, "local-only", &cfg).unwrap();
        assert!(result.created);
        assert!(result.path.is_dir());
        assert!(result.source.contains("local"));
        // Second call reuses.
        let again = ensure_branch_worktree(&primary, "local-only", &cfg).unwrap();
        assert!(!again.created);
        assert_eq!(
            canonicalize_existing(&again.path),
            canonicalize_existing(&result.path)
        );
        // Pin marker present on linked worktree.
        assert!(
            crate::worktree_pin::pin_marker_path(&result.path)
                .map(|p| p.exists())
                .unwrap_or(false),
            "new worktree should be pinned"
        );
    }

    #[test]
    fn create_new_branch_worktree_off_default() {
        let (tmp, primary) = scratch_repo();
        let cfg = test_cfg(tmp.path());

        let result = ensure_branch_worktree(&primary, "brand-new", &cfg).unwrap();
        assert!(result.created);
        assert!(result.path.is_dir());
        // Path leaf matches branch name under local/<repo>/ when no origin.
        assert!(
            result.path.ends_with("brand-new")
                || result
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    == Some("brand-new"),
            "path should end with brand-new: {}",
            result.path.display()
        );
        let branch = Command::new("git")
            .arg("-C")
            .arg(&result.path)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&branch.stdout).trim(),
            "brand-new"
        );
    }

    #[test]
    fn never_switches_invoking_worktree_branch() {
        let (tmp, primary) = scratch_repo();
        let cfg = test_cfg(tmp.path());
        let before = Command::new("git")
            .arg("-C")
            .arg(&primary)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&before.stdout).trim(), "main");

        let _ = ensure_branch_worktree(&primary, "elsewhere", &cfg).unwrap();

        let after = Command::new("git")
            .arg("-C")
            .arg(&primary)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&after.stdout).trim(),
            "main",
            "invoking worktree must stay on its branch"
        );
    }
}
