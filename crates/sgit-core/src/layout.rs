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

    ensure_origin_head(bare_dir);

    point_bare_head_to_placeholder(bare_dir)?;
    Ok(())
}

/// Populate `refs/remotes/origin/*` and resolve `refs/remotes/origin/HEAD`.
///
/// `git clone --bare` writes neither: it lands branches in `refs/heads/*` and
/// never records the remote's advertised HEAD. Readers of the primary branch
/// (`resolve_default_branch_at`, `resolve_primary_base_ref`) consult
/// `origin/HEAD` first and silently fall through to a guess-ladder without it.
///
/// Best-effort: an unreachable remote warns and still leaves a usable hub.
pub fn ensure_origin_head(bare_dir: &Path) {
    // Objects are already local after the clone, so this only moves refs.
    if let Err(e) = run_git_dir(bare_dir, &["fetch", "origin"]) {
        eprintln!(
            "warning: could not fetch origin refs for {}: {e}",
            bare_dir.display()
        );
        return;
    }

    if let Err(e) = run_git_dir(bare_dir, &["remote", "set-head", "origin", "--auto"]) {
        eprintln!(
            "warning: could not resolve refs/remotes/origin/HEAD for {}: {e}",
            bare_dir.display()
        );
    }
}

/// Configure upstream tracking for `branch` in `worktree_dir`.
///
/// `git worktree add <path> <branch>` never sets upstream, so a freshly
/// provisioned tree has no `branch.<b>.remote` / `branch.<b>.merge` and
/// `git pull` / bare `git push` / ahead-behind are all broken until some later
/// push repairs it.
///
/// No-op when the branch does not exist on origin — a not-yet-pushed branch
/// must not be given an upstream that does not resolve. No-op when upstream is
/// already configured. Best-effort: failures warn, never abort provisioning.
pub fn ensure_branch_upstream(bare_dir: &Path, worktree_dir: &Path, branch: &str) {
    if !git_ref_exists(bare_dir, &format!("refs/remotes/origin/{branch}")) {
        return;
    }

    let already_set = Command::new("git")
        .arg("-C")
        .arg(worktree_dir)
        .args(["rev-parse", "--abbrev-ref", &format!("{branch}@{{upstream}}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if already_set {
        return;
    }

    let remote_key = format!("branch.{branch}.remote");
    let merge_key = format!("branch.{branch}.merge");
    let merge_ref = format!("refs/heads/{branch}");
    let writes: [[&str; 3]; 2] = [
        ["config", &remote_key, "origin"],
        ["config", &merge_key, &merge_ref],
    ];

    for args in writes {
        if let Err(e) = run_git_dir(worktree_dir, &args) {
            eprintln!(
                "warning: could not set upstream for {branch} in {}: {e}",
                worktree_dir.display()
            );
            return;
        }
    }
}

/// Bare-clone `owner/repo` from GitHub (SSH URL).
pub fn bare_clone(owner: &str, repo_name: &str, bare_dir: &Path) -> Result<(), String> {
    let remote_url = format!("git@github.com:{owner}/{repo_name}.git");
    bare_clone_from_url(&remote_url, bare_dir)
}

/// True when `worktree_dir` is a usable linked worktree of `bare_dir`:
/// `git rev-parse --is-inside-work-tree` succeeds and the common git dir is the bare.
pub fn is_valid_linked_worktree(worktree_dir: &Path, bare_dir: &Path) -> bool {
    if !worktree_dir.is_dir() {
        return false;
    }
    let inside = Command::new("git")
        .arg("-C")
        .arg(worktree_dir)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false);
    if !inside {
        return false;
    }
    let Some(common) = crate::worktree_pin::resolve_common_git_dir(worktree_dir) else {
        return false;
    };
    same_path(&common, bare_dir)
}

/// `git worktree add <worktree_dir> <branch>` against a bare repo.
///
/// Creates all missing parent directories of `worktree_dir`. Does not install
/// stokd-specific hooks or cloud worktree-count refresh — those stay
/// stokd-domain. Optionally pins the worktree when `pin` is true.
///
/// When the bare uses `extensions.worktreeConfig`, writes a per-worktree
/// `core.bare=false` so the new tree is usable (same fix as submodule attach).
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
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "cannot create worktree directory parent {}: {e}",
                parent.display()
            )
        })?;
    }

    // Ensure bareRoot / worktreeRoot themselves exist when this is the first
    // repo under a fresh layout (create_dir_all on the immediate parent is not
    // enough if bareRoot itself is missing and bare_clone already created it).
    if let Some(grand) = worktree_dir.parent().and_then(|p| p.parent()) {
        let _ = std::fs::create_dir_all(grand);
    }

    // Drop stale admin entries when the directory was deleted out-of-band
    // ("missing but already registered worktree").
    prune_worktree_registration(bare_dir, worktree_dir);

    let path_str = worktree_dir.to_string_lossy().to_string();
    let mut output = Command::new("git")
        .args(["worktree", "add", &path_str, branch])
        .current_dir(bare_dir)
        .output()
        .map_err(|e| format!("failed to run git worktree add: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Retry once with -f after another prune for "already registered" races.
        if stderr.contains("already registered") || stderr.contains("already exists") {
            prune_worktree_registration(bare_dir, worktree_dir);
            output = Command::new("git")
                .args(["worktree", "add", "-f", &path_str, branch])
                .current_dir(bare_dir)
                .output()
                .map_err(|e| format!("failed to run git worktree add -f: {e}"))?;
        }
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("worktree creation failed: {stderr}"));
    }

    ensure_main_worktree_not_bare(bare_dir, worktree_dir);

    if !is_valid_linked_worktree(worktree_dir, bare_dir) {
        return Err(format!(
            "worktree was created at {} but has no usable git connection to {}",
            worktree_dir.display(),
            bare_dir.display()
        ));
    }

    ensure_branch_upstream(bare_dir, worktree_dir, branch);

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

/// Best-effort clear of a stale worktree registration for `worktree` on `bare`.
fn prune_worktree_registration(bare: &Path, worktree: &Path) {
    let _ = Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(bare)
        .output();
    let _ = Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            &worktree.to_string_lossy(),
        ])
        .current_dir(bare)
        .output();
}

/// When `bare` enables `extensions.worktreeConfig`, force `core.bare=false` on
/// the new worktree so working-tree ops succeed (mirrors submodule attach).
fn ensure_main_worktree_not_bare(bare: &Path, worktree: &Path) {
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
        // `.output()` (not `.status()`): the caller may probe a dir that is not
        // actually a git repo, and git's `fatal:` stderr must never reach the
        // user's terminal.
        .output()
        .map(|o| o.status.success())
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
            ..Default::default()
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

    // --- AX-CLI-PROVISION-REMOTE-STATE ---------------------------------------

    fn git_ok(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("failed to run git {}: {e}", args.join(" ")));
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_stdout(cwd: &Path, args: &[&str]) -> Option<String> {
        Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    }

    /// Seed a non-bare repo with one commit on `main`, usable as a local "origin".
    fn seed_remote(dir: &Path) -> PathBuf {
        let remote = dir.join("seed");
        std::fs::create_dir_all(&remote).unwrap();
        git_ok(&remote, &["init", "-b", "main"]);
        git_ok(&remote, &["config", "user.email", "test@example.com"]);
        git_ok(&remote, &["config", "user.name", "Test"]);
        std::fs::write(remote.join("README.md"), "seed\n").unwrap();
        git_ok(&remote, &["add", "-A"]);
        git_ok(&remote, &["commit", "-m", "Initial commit"]);
        remote
    }

    #[test]
    fn bare_clone_sets_origin_head() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = seed_remote(tmp.path());
        let bare = tmp.path().join("hub.git");

        bare_clone_from_url(&remote.to_string_lossy(), &bare).unwrap();

        let head = git_stdout(
            &bare,
            &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
        );
        assert_eq!(
            head.as_deref(),
            Some("refs/remotes/origin/main"),
            "freshly provisioned hub must carry an authoritative refs/remotes/origin/HEAD"
        );
    }

    #[test]
    fn create_worktree_sets_upstream_for_existing_remote_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = seed_remote(tmp.path());
        let bare = tmp.path().join("hub.git");
        bare_clone_from_url(&remote.to_string_lossy(), &bare).unwrap();

        let wt = tmp.path().join("worktrees").join("main");
        create_worktree(&bare, &wt, "main", false).unwrap();

        let upstream = git_stdout(&wt, &["rev-parse", "--abbrev-ref", "main@{upstream}"]);
        assert_eq!(
            upstream.as_deref(),
            Some("origin/main"),
            "a worktree for a branch that exists on origin must be push/pull-ready"
        );
    }

    #[test]
    fn create_worktree_leaves_upstream_unset_for_local_only_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = seed_remote(tmp.path());
        let bare = tmp.path().join("hub.git");
        bare_clone_from_url(&remote.to_string_lossy(), &bare).unwrap();

        // A branch that exists only locally — never pushed to origin.
        git_ok(&bare, &["branch", "local-only", "main"]);

        let wt = tmp.path().join("worktrees").join("local-only");
        create_worktree(&bare, &wt, "local-only", false).unwrap();

        assert_eq!(
            git_stdout(&wt, &["rev-parse", "--abbrev-ref", "local-only@{upstream}"]),
            None,
            "a branch absent from origin must not be given a bogus upstream"
        );
        assert_eq!(
            git_stdout(&wt, &["config", "--get", "branch.local-only.merge"]),
            None,
            "no upstream config may be written for a local-only branch"
        );
    }

    #[test]
    fn create_worktree_succeeds_when_origin_is_unreachable() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = seed_remote(tmp.path());
        let bare = tmp.path().join("hub.git");
        bare_clone_from_url(&remote.to_string_lossy(), &bare).unwrap();

        // Remote goes away after provisioning: origin/* refs are stale but no
        // fetch or set-head can succeed.
        std::fs::remove_dir_all(&remote).unwrap();

        let wt = tmp.path().join("worktrees").join("main");
        create_worktree(&bare, &wt, "main", false)
            .expect("an unreachable remote must degrade, not fail provisioning");
        assert!(is_valid_linked_worktree(&wt, &bare));

        // ensure_origin_head is likewise non-fatal on a dead remote.
        ensure_origin_head(&bare);
    }
}
