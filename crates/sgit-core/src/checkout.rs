//! Pin-safe checkout (AX-CLI-CHECKOUT-SIBLING-WORKTREE + AX-CLI-CHECKOUT-REPO-TARGET).
//!
//! `sgit checkout` accepts two target classes:
//!
//! 1. **Branch** — never switches the current worktree in place; reuses or
//!    creates a sibling linked worktree under the configured worktree root.
//! 2. **Repo** (`owner/repo` or bare `reponame`) — ensures the bare + main
//!    worktree layout (creating missing parents, clone if needed), repairs
//!    destinations that exist without a valid git connection when safe, and
//!    returns the absolute main worktree path for the shell wrapper to `cd`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::RepositoriesConfig;
use crate::layout::{
    bare_clone_from_url, create_worktree, is_valid_linked_worktree, parse_git_remote_url,
    render_worktree_name_pattern, resolve_default_branch, resolve_repo_layout,
    worktree_dir_for_branch,
};
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

/// Outcome of ensuring the main worktree for a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsureRepoWorktree {
    /// Absolute path of the main worktree.
    pub path: PathBuf,
    /// True when bare and/or main worktree were created or re-materialized.
    pub created: bool,
    /// Human-readable source for stderr diagnostics.
    pub source: String,
}

/// How `sgit checkout <target>` should interpret its argument (first pass).
///
/// Repo specs still need owner resolution (bare names) and may fall back to
/// branch create when resolution fails while inside a git repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutKind {
    /// Ensure a sibling worktree for this branch of the current repo.
    Branch(String),
    /// Ensure the main worktree for this repo spec (`owner/repo` or bare name).
    RepoSpec(String),
}

/// Classify a checkout argument.
///
/// Rules (AX-CLI-CHECKOUT-REPO-TARGET):
/// - `owner/repo` → [`CheckoutKind::RepoSpec`] unless `repo_root` has a **local**
///   branch of that exact name (then branch).
/// - bare name + inside a repo with local or `origin/<name>` branch → Branch.
/// - bare name otherwise → RepoSpec (caller resolves owner; may fall back to
///   branch create if resolution fails while inside a repo).
/// - outside any git repo → always RepoSpec.
pub fn classify_checkout_target(raw: &str, repo_root: Option<&Path>) -> CheckoutKind {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return CheckoutKind::RepoSpec(String::new());
    }

    let has_slash = trimmed.contains('/');
    let Some(root) = repo_root else {
        return CheckoutKind::RepoSpec(trimmed.to_string());
    };

    if has_slash {
        // Prefer branch only when a *local* branch exists (origin/owner/repo is
        // not a meaningful remote branch name for this purpose).
        if branch_exists_local(root, trimmed) {
            return CheckoutKind::Branch(trimmed.to_string());
        }
        return CheckoutKind::RepoSpec(trimmed.to_string());
    }

    if branch_exists_local(root, trimmed) || branch_exists_remote(root, trimmed) {
        return CheckoutKind::Branch(trimmed.to_string());
    }
    CheckoutKind::RepoSpec(trimmed.to_string())
}

/// Ensure bare + main worktree for `owner/repo` using the GitHub SSH remote.
pub fn ensure_repo_main_worktree(
    owner: &str,
    repo_name: &str,
    cfg: &RepositoriesConfig,
) -> Result<EnsureRepoWorktree, String> {
    let remote_url = format!("git@github.com:{owner}/{repo_name}.git");
    ensure_repo_main_worktree_from_url(owner, repo_name, &remote_url, cfg)
}

/// Ensure bare + main worktree for `owner/repo` from an arbitrary remote URL
/// (tests use a local `file://` or path remote).
pub fn ensure_repo_main_worktree_from_url(
    owner: &str,
    repo_name: &str,
    remote_url: &str,
    cfg: &RepositoriesConfig,
) -> Result<EnsureRepoWorktree, String> {
    // Layout roots must exist so later create_dir_all on nested parents never
    // fails with "can't write to a folder that isn't there".
    std::fs::create_dir_all(&cfg.bare_root).map_err(|e| {
        format!(
            "cannot create bareRoot {}: {e}",
            cfg.bare_root
        )
    })?;
    std::fs::create_dir_all(&cfg.worktree_root).map_err(|e| {
        format!(
            "cannot create worktreeRoot {}: {e}",
            cfg.worktree_root
        )
    })?;

    let mut layout = resolve_repo_layout(cfg, owner, repo_name);
    let mut source_parts: Vec<String> = Vec::new();
    let mut bare_created = false;

    if !layout.bare_dir.exists() {
        bare_clone_from_url(remote_url, &layout.bare_dir)?;
        bare_created = true;
        source_parts.push(format!("bare-cloned {owner}/{repo_name}"));
    }

    let branch = resolve_default_branch(&layout.bare_dir);
    layout.worktree_dir = worktree_dir_for_branch(cfg, owner, repo_name, &branch);

    if is_valid_linked_worktree(&layout.worktree_dir, &layout.bare_dir) {
        return Ok(EnsureRepoWorktree {
            path: canonicalize_existing(&layout.worktree_dir),
            created: bare_created,
            source: if bare_created {
                source_parts.join("; ")
            } else {
                format!("existing main worktree for {owner}/{repo_name} ({branch})")
            },
        });
    }

    if layout.worktree_dir.exists() {
        safe_remove_invalid_worktree_dir(&layout.worktree_dir, &layout.bare_dir)?;
        source_parts.push("re-materialized invalid worktree path".to_string());
    }

    create_worktree(&layout.bare_dir, &layout.worktree_dir, &branch, true)?;
    if source_parts.is_empty() {
        source_parts.push(format!("created main worktree on {branch}"));
    }
    let created = true;

    if let Err(e) = crate::submodule_checkout::apply_submodule_checkout_for_repo(
        &layout.worktree_dir,
        cfg,
        owner,
        repo_name,
    ) {
        eprintln!("warning: submodule checkout: {e}");
    }

    Ok(EnsureRepoWorktree {
        path: canonicalize_existing(&layout.worktree_dir),
        created,
        source: source_parts.join("; "),
    })
}

/// Remove `path` when it is safe to re-materialize: empty, or only a broken
/// `.git` marker with no usable connection. Refuses when non-git content would
/// be destroyed.
fn safe_remove_invalid_worktree_dir(path: &Path, bare_dir: &Path) -> Result<(), String> {
    // Drop stale admin entries that may still point at this path.
    let _ = Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(bare_dir)
        .output();
    let _ = Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            &path.to_string_lossy(),
        ])
        .current_dir(bare_dir)
        .output();

    if !path.exists() {
        return Ok(());
    }

    if is_dir_empty(path) {
        std::fs::remove_dir_all(path).map_err(|e| {
            format!("cannot remove empty worktree path {}: {e}", path.display())
        })?;
        return Ok(());
    }

    if only_broken_git_marker(path) {
        std::fs::remove_dir_all(path).map_err(|e| {
            format!(
                "cannot remove broken worktree path {}: {e}",
                path.display()
            )
        })?;
        return Ok(());
    }

    Err(format!(
        "worktree path exists but has no git connection to {}: {}; move or remove it, then retry",
        bare_dir.display(),
        path.display()
    ))
}

fn is_dir_empty(path: &Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut rd| rd.next().is_none())
        .unwrap_or(false)
}

/// True when the only entry is a non-functional `.git` file/dir (or the tree is
/// empty of meaningful content after prune).
fn only_broken_git_marker(path: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(path) else {
        return false;
    };
    let entries: Vec<_> = rd.flatten().collect();
    if entries.is_empty() {
        return true;
    }
    if entries.len() != 1 {
        return false;
    }
    let name = entries[0].file_name();
    if name != *".git" {
        return false;
    }
    // Presence of .git alone does not mean usable; caller already checked
    // is_valid_linked_worktree is false.
    true
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

    #[test]
    fn classify_owner_repo_as_repo_unless_local_branch() {
        let (tmp, primary) = scratch_repo();
        assert_eq!(
            classify_checkout_target("acme/widget", Some(&primary)),
            CheckoutKind::RepoSpec("acme/widget".into())
        );
        git(&primary, &["branch", "acme/widget"]);
        assert_eq!(
            classify_checkout_target("acme/widget", Some(&primary)),
            CheckoutKind::Branch("acme/widget".into())
        );
        // Outside a repo always repo.
        assert_eq!(
            classify_checkout_target("acme/widget", None),
            CheckoutKind::RepoSpec("acme/widget".into())
        );
        let _ = tmp;
    }

    #[test]
    fn classify_bare_name_prefers_existing_branch() {
        let (tmp, primary) = scratch_repo();
        git(&primary, &["branch", "release"]);
        assert_eq!(
            classify_checkout_target("release", Some(&primary)),
            CheckoutKind::Branch("release".into())
        );
        // No such branch → repo spec (owner resolution happens later).
        assert_eq!(
            classify_checkout_target("totally-unknown-xyz", Some(&primary)),
            CheckoutKind::RepoSpec("totally-unknown-xyz".into())
        );
        let _ = tmp;
    }

    /// Seed a bare-compatible remote and provision via ensure_repo_main_worktree_from_url.
    fn seed_remote_repo(tmp: &Path) -> (PathBuf, String) {
        let seed = tmp.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "-q", "-b", "main"]);
        git(&seed, &["config", "user.email", "t@t.co"]);
        git(&seed, &["config", "user.name", "t"]);
        std::fs::write(seed.join("README"), "hello\n").unwrap();
        git(&seed, &["add", "README"]);
        git(&seed, &["commit", "-q", "-m", "init"]);
        let remote = tmp.join("remote.git");
        git(
            tmp,
            &[
                "clone",
                "-q",
                "--bare",
                seed.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );
        let url = format!("file://{}", remote.display());
        (remote, url)
    }

    #[test]
    fn ensure_repo_creates_parents_bare_and_main_worktree() {
        let tmp = tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        // Deliberately do NOT create bareRoot / worktreeRoot first.
        let (_remote, url) = seed_remote_repo(tmp.path());

        let result =
            ensure_repo_main_worktree_from_url("acme", "widget", &url, &cfg).expect("ensure");
        assert!(result.created);
        assert!(result.path.is_dir());
        assert!(
            is_valid_linked_worktree(&result.path, &PathBuf::from(&cfg.bare_root).join("acme").join("widget.git")),
            "main worktree must have a git connection to the bare"
        );
        let inside = Command::new("git")
            .arg("-C")
            .arg(&result.path)
            .args(["rev-parse", "--is-inside-work-tree"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&inside.stdout).trim(), "true");
        assert!(result.path.join("README").is_file());

        // Reuse: second call does not re-create.
        let again =
            ensure_repo_main_worktree_from_url("acme", "widget", &url, &cfg).expect("reuse");
        assert!(!again.created);
        assert_eq!(
            canonicalize_existing(&again.path),
            canonicalize_existing(&result.path)
        );
    }

    #[test]
    fn ensure_repo_repairs_broken_git_marker_destination() {
        let tmp = tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let (_remote, url) = seed_remote_repo(tmp.path());

        // Pre-create a destination with only a broken .git pointer (no connection).
        let dest = PathBuf::from(&cfg.worktree_root)
            .join("acme")
            .join("widget")
            .join("main");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join(".git"), "gitdir: /nonexistent/path\n").unwrap();

        let result =
            ensure_repo_main_worktree_from_url("acme", "widget", &url, &cfg).expect("repair");
        assert!(result.created);
        assert!(is_valid_linked_worktree(
            &result.path,
            &PathBuf::from(&cfg.bare_root).join("acme").join("widget.git")
        ));
    }

    #[test]
    fn ensure_repo_recovers_when_directory_missing_but_still_registered() {
        let tmp = tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let (_remote, url) = seed_remote_repo(tmp.path());

        let first =
            ensure_repo_main_worktree_from_url("acme", "widget", &url, &cfg).expect("first");
        assert!(first.path.is_dir());
        // Out-of-band delete of the worktree files leaves a stale registration.
        std::fs::remove_dir_all(&first.path).unwrap();
        assert!(!first.path.exists());

        let again =
            ensure_repo_main_worktree_from_url("acme", "widget", &url, &cfg).expect("recover");
        assert!(again.created);
        assert!(is_valid_linked_worktree(
            &again.path,
            &PathBuf::from(&cfg.bare_root).join("acme").join("widget.git")
        ));
    }

    #[test]
    fn ensure_repo_refuses_non_git_content_at_destination() {
        let tmp = tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let (_remote, url) = seed_remote_repo(tmp.path());

        // Bare must exist so we hit the worktree path (not bare clone failure).
        let bare = PathBuf::from(&cfg.bare_root).join("acme").join("widget.git");
        bare_clone_from_url(&url, &bare).unwrap();

        let dest = PathBuf::from(&cfg.worktree_root)
            .join("acme")
            .join("widget")
            .join("main");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("precious.txt"), "keep me\n").unwrap();

        let err = ensure_repo_main_worktree_from_url("acme", "widget", &url, &cfg)
            .expect_err("must refuse");
        assert!(
            err.contains("no git connection") || err.contains("move or remove"),
            "err was: {err}"
        );
        assert!(dest.join("precious.txt").is_file(), "must not destroy content");
    }
}
