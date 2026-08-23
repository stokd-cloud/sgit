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

    ensure_worktree_not_bare(bare_dir, worktree_dir);

    if !is_valid_linked_worktree(worktree_dir, bare_dir) {
        return Err(format!(
            "worktree was created at {} but has no usable git connection to {}",
            worktree_dir.display(),
            bare_dir.display()
        ));
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

/// Guarantee `worktree` is usable as a working tree under a hub that enables
/// `extensions.worktreeConfig`.
///
/// The fix belongs to the hub: [`ensure_hub_bare_honored`] migrates `core.bare`
/// out of the shared config, which frees every linked worktree at once —
/// including trees created by plain `git worktree add`, which never reach this
/// code. Only if that leaves the tree unusable (an unwritable hub, a shared
/// config we declined to rewrite) do we fall back to the historical
/// per-worktree `core.bare=false` override, and then the failure is reported
/// rather than swallowed — a silently-skipped write is what let the broken
/// state ship unnoticed.
pub(crate) fn ensure_worktree_not_bare(bare: &Path, worktree: &Path) {
    heal_hub_bare_with_notice(bare);

    if git_config_bool(bare, "extensions.worktreeConfig") != Some(true) {
        return;
    }
    if git_config_bool(worktree, "core.bare") != Some(true) {
        return;
    }

    let result = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["config", "--worktree", "core.bare", "false"])
        .output()
        .map_err(|e| format!("failed to run git config --worktree: {e}"))
        .and_then(|out| {
            if out.status.success() {
                Ok(())
            } else {
                Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
            }
        });
    match result {
        Ok(()) => eprintln!(
            "sgit: wrote a per-worktree core.bare=false at {} because hub {} \
             still declares bare-ness to its worktrees",
            worktree.display(),
            bare.display()
        ),
        Err(e) => eprintln!(
            "warning: {} inherits core.bare=true from hub {} and the \
             per-worktree override could not be written: {e}",
            worktree.display(),
            bare.display()
        ),
    }
}

/// `git -C <dir> config --bool <key>`, or `None` when unset/unreadable.
fn git_config_bool(dir: &Path, key: &str) -> Option<bool> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["config", "--bool", key])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    match String::from_utf8_lossy(&out.stdout).trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Complement of [`ensure_main_worktree_not_bare`], for the hub itself.
///
/// Once `extensions.worktreeConfig` is enabled, git honors `core.bare` only
/// from `$GIT_DIR/config.worktree`; the shared config's `bare = true` is
/// ignored by worktree-aware commands. A hub where the extension was enabled
/// by hand (git's own auto-enable migrates `core.bare`; a manual
/// `git config extensions.worktreeConfig true` does not) therefore reports
/// itself as a checked-out worktree of its HEAD branch — duplicating the real
/// main worktree and failing every working-tree op run against the hub path.
///
/// Detects that state and writes the missing override
/// (`git -C <hub> config --worktree core.bare true`), the same migration git
/// performs when it enables the extension itself. Returns `Ok(true)` when a
/// heal was applied, `Ok(false)` when nothing needed doing (extension off,
/// hub not meant to be bare, or already honored).
///
/// Detection reads the override file directly rather than `git worktree list`
/// output: porcelain computed *from the hub* still infers bare-ness from the
/// directory layout and looks healthy — the breakage is only visible from a
/// linked worktree, which is exactly where scanners and pins run.
pub fn ensure_hub_bare_honored(bare_dir: &Path) -> Result<bool, String> {
    if git_config_bool(bare_dir, "extensions.worktreeConfig") != Some(true) {
        return Ok(false);
    }
    // Merged read (common + hub override): an explicit `bare = false` override
    // or a hub that never declared bare-ness is intentionally non-bare — or at
    // least not ours to rewrite. Only heal hubs that say `bare = true`.
    if git_config_bool(bare_dir, "core.bare") != Some(true) {
        return Ok(false);
    }

    // Step 1 — the hub keeps its bare-ness, in the only place git still reads
    // it from. Must happen before step 2 so the hub is never momentarily
    // non-bare (a crash between the two would otherwise leave it a checkout).
    let mut healed = false;
    if hub_override_bare(bare_dir) != Some(true) {
        let out = Command::new("git")
            .arg("-C")
            .arg(bare_dir)
            .args(["config", "--worktree", "core.bare", "true"])
            .output()
            .map_err(|e| format!("failed to run git config --worktree: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "could not restore the core.bare override on hub {}: {}",
                bare_dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        if hub_override_bare(bare_dir) != Some(true) {
            return Err(format!(
                "hub {} still has no honored core.bare override after writing config.worktree",
                bare_dir.display()
            ));
        }
        healed = true;
    }

    // Step 2 — and the shared config stops broadcasting it to every worktree.
    if shared_bare_is_set(bare_dir) {
        let out = Command::new("git")
            .arg("-C")
            .arg(bare_dir)
            .args(["config", "--local", "--unset-all", "core.bare"])
            .output()
            .map_err(|e| format!("failed to run git config --local --unset-all: {e}"))?;
        // Exit 5 is "key not found" — a concurrent heal got there first.
        if !out.status.success() && out.status.code() != Some(5) {
            return Err(format!(
                "could not migrate core.bare out of the shared config on hub {}: {}",
                bare_dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        if shared_bare_is_set(bare_dir) {
            return Err(format!(
                "hub {} still declares core.bare in its shared config, so linked \
                 worktrees will keep inheriting bare-ness",
                bare_dir.display()
            ));
        }
        healed = true;
    }

    Ok(healed)
}

/// Whether the hub's *shared* `$GIT_DIR/config` declares `core.bare` at all.
///
/// Read with `--local` so the `config.worktree` override is excluded: the
/// question is not what the hub resolves to, but whether the shared file still
/// carries a key that `extensions.worktreeConfig` now broadcasts to every
/// linked worktree.
fn shared_bare_is_set(bare_dir: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(bare_dir)
        .args(["config", "--local", "--bool", "core.bare"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The hub's per-worktree `core.bare` override (`$GIT_DIR/config.worktree`),
/// or `None` when unset. Only meaningful while `extensions.worktreeConfig` is
/// enabled (callers gate on that; without it `--worktree` reads fall back to
/// `--local`).
fn hub_override_bare(bare_dir: &Path) -> Option<bool> {
    let out = Command::new("git")
        .arg("-C")
        .arg(bare_dir)
        .args(["config", "--worktree", "--bool", "core.bare"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    match String::from_utf8_lossy(&out.stdout).trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Run [`ensure_hub_bare_honored`] for the repo whose common git dir is
/// `common`, surfacing the outcome on stderr (heal note or warning) without
/// failing the caller — the heal is an invariant repair, never a blocker.
pub(crate) fn heal_hub_bare_with_notice(common: &Path) {
    match ensure_hub_bare_honored(common) {
        Ok(true) => eprintln!(
            "sgit: healed hub {}: extensions.worktreeConfig was enabled without \
             the core.bare override, so git read the hub as a second checkout",
            common.display()
        ),
        Ok(false) => {}
        Err(e) => eprintln!("warning: {e}"),
    }
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

    fn tgit(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Whether the hub's stanza in `git worktree list --porcelain` carries the
    /// `bare` flag when the command runs from `from` — the exact observable
    /// that broke in the incident. The vantage point matters: from the hub
    /// itself git re-infers bare-ness from the directory layout, so the
    /// breakage is only visible from a linked worktree.
    fn hub_stanza_reports_bare_from(from: &Path, hub: &Path) -> bool {
        let out = Command::new("git")
            .arg("-C")
            .arg(from)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git worktree list failed");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let mut in_hub = false;
        for line in text.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                in_hub = same_path(Path::new(p.trim()), hub);
            } else if line == "bare" && in_hub {
                return true;
            }
        }
        false
    }

    /// Bare hub + linked `main` worktree, mirroring the /opt/dev layout.
    fn scratch_hub() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let seed = tmp.path().join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        tgit(&seed, &["init", "-q", "-b", "main"]);
        tgit(&seed, &["config", "user.email", "t@t.co"]);
        tgit(&seed, &["config", "user.name", "t"]);
        tgit(&seed, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let hub = tmp.path().join("hub.git");
        tgit(
            tmp.path(),
            &[
                "clone",
                "-q",
                "--bare",
                seed.to_str().unwrap(),
                hub.to_str().unwrap(),
            ],
        );
        let wt = tmp.path().join("main");
        tgit(&hub, &["worktree", "add", "-q", wt.to_str().unwrap(), "main"]);
        (tmp, hub, wt)
    }

    #[test]
    fn hub_bare_honored_heals_missing_override() {
        let (_tmp, hub, wt) = scratch_hub();
        // Reproduce the incident: extension enabled by hand without migrating
        // core.bare into $GIT_DIR/config.worktree; linked worktree carries the
        // usual per-worktree `bare = false` override.
        tgit(&hub, &["config", "extensions.worktreeConfig", "true"]);
        tgit(&wt, &["config", "--worktree", "core.bare", "false"]);
        assert!(
            !hub_stanza_reports_bare_from(&wt, &hub),
            "precondition: from a linked worktree, git must read the hub as a \
             non-bare checkout (the bug being healed)"
        );

        assert_eq!(ensure_hub_bare_honored(&hub), Ok(true), "heal applies");
        assert!(
            hub_stanza_reports_bare_from(&wt, &hub),
            "heal must restore honored bare-ness as seen from linked worktrees"
        );
        let cfg = std::fs::read_to_string(hub.join("config.worktree"))
            .expect("config.worktree written");
        assert!(cfg.contains("bare = true"), "override holds bare = true: {cfg}");

        assert_eq!(
            ensure_hub_bare_honored(&hub),
            Ok(false),
            "second call is a no-op"
        );
    }

    #[test]
    fn hub_bare_honored_noop_without_extension() {
        let (_tmp, hub, wt) = scratch_hub();
        assert!(
            hub_stanza_reports_bare_from(&wt, &hub),
            "healthy hub reads as bare"
        );
        assert_eq!(ensure_hub_bare_honored(&hub), Ok(false));
        assert!(
            !hub.join("config.worktree").exists(),
            "must not create an override when the extension is off"
        );
        assert!(hub_stanza_reports_bare_from(&wt, &hub));
        assert_eq!(
            shared_bare(&hub),
            Some(true),
            "must not touch the shared config when the extension is off"
        );
    }

    /// `core.bare` as recorded in the hub's *shared* `$GIT_DIR/config`
    /// (`--local`), independent of any `config.worktree` override.
    fn shared_bare(hub: &Path) -> Option<bool> {
        let out = Command::new("git")
            .arg("-C")
            .arg(hub)
            .args(["config", "--local", "--bool", "core.bare"])
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        match String::from_utf8_lossy(&out.stdout).trim() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }
    }

    /// Whether `git status` succeeds in `wt` — i.e. git accepts it as a real
    /// working tree rather than failing with "must be run in a work tree".
    fn worktree_status_ok(wt: &Path) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(wt)
            .args(["status", "--porcelain"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn is_bare_repository(hub: &Path) -> bool {
        let out = Command::new("git")
            .arg("-C")
            .arg(hub)
            .args(["rev-parse", "--is-bare-repository"])
            .output()
            .expect("git runs");
        String::from_utf8_lossy(&out.stdout).trim() == "true"
    }

    /// The real recurrence (2026-08-22): with `extensions.worktreeConfig` on,
    /// git drops the special case that confines `core.bare` to the main
    /// worktree, so the shared config's `bare = true` applies to *every* linked
    /// worktree. Trees created by plain `git worktree add` — which never get
    /// sgit's per-worktree `core.bare=false` band-aid — are born bare and fail
    /// every working-tree command. Healing the hub must free them without
    /// touching their own config.
    #[test]
    fn hub_bare_honored_frees_linked_worktrees() {
        let (tmp, hub, _main_wt) = scratch_hub();
        tgit(&hub, &["config", "extensions.worktreeConfig", "true"]);

        let feat = tmp.path().join("feat");
        tgit(
            &hub,
            &[
                "worktree",
                "add",
                "-q",
                feat.to_str().unwrap(),
                "-b",
                "feat",
            ],
        );

        assert!(
            !worktree_status_ok(&feat),
            "precondition: a plainly-added worktree inherits the shared \
             `bare = true` and git refuses it as a work tree"
        );
        let feat_override = hub.join("worktrees/feat/config.worktree");
        let before = std::fs::read(&feat_override).unwrap_or_default();

        assert_eq!(ensure_hub_bare_honored(&hub), Ok(true), "heal applies");

        assert!(
            worktree_status_ok(&feat),
            "healing the hub must free linked worktrees"
        );
        assert_eq!(
            std::fs::read(&feat_override).unwrap_or_default(),
            before,
            "the worktree's own config must not be edited — the fix belongs to \
             the hub, so trees created by any tool are covered"
        );
        assert!(
            is_bare_repository(&hub),
            "the hub itself must stay bare after the migration"
        );
    }

    #[test]
    fn hub_bare_honored_migration_is_idempotent() {
        let (tmp, hub, _main_wt) = scratch_hub();
        tgit(&hub, &["config", "extensions.worktreeConfig", "true"]);
        let feat = tmp.path().join("feat");
        tgit(
            &hub,
            &[
                "worktree",
                "add",
                "-q",
                feat.to_str().unwrap(),
                "-b",
                "feat",
            ],
        );

        assert_eq!(ensure_hub_bare_honored(&hub), Ok(true), "first call heals");
        assert_eq!(
            ensure_hub_bare_honored(&hub),
            Ok(false),
            "second call is a no-op"
        );

        assert!(is_bare_repository(&hub), "hub stays bare");
        assert_eq!(
            shared_bare(&hub),
            None,
            "`bare` is migrated out of the shared config, not left to leak \
             into every linked worktree"
        );
        assert_eq!(
            hub_override_bare(&hub),
            Some(true),
            "bare-ness now lives in the hub's config.worktree"
        );
        assert!(worktree_status_ok(&feat), "worktrees stay usable");
    }
}
