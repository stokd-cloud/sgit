//! Pure workspace / worktree helpers shared by `sgit` and staying `stokd` commands.
//!
//! No stokd config, no agent shove, no cloud — only git plumbing and pure
//! decision helpers. Callers that need dirty-worktree fight-forward or
//! build-on-land orchestration supply those policies at the call site.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Detect the git repository toplevel for `path` (`git rev-parse --show-toplevel`).
pub fn detect_repo_root_at(path: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let root = String::from_utf8(output.stdout).ok()?;
    let trimmed = root.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

/// Locate a local worktree checked out on `branch` relative to any path in the
/// repo. Excludes the bare common gitdir (AX-CLI-WORKTREE-FINDER-EXCLUDES-COMMON-DIR).
pub fn find_worktree_for_branch_at(path: &Path, branch: &str) -> Result<PathBuf, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map_err(|error| format!("failed to list git worktrees: {error}"))?;

    if !output.status.success() {
        return Err(format!(
            "failed to list git worktrees:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let text = String::from_utf8(output.stdout)
        .map_err(|error| format!("git worktree list returned invalid utf-8: {error}"))?;
    let common_dir = git_common_dir_at(path);
    parse_worktree_for_branch_from_porcelain(&text, branch, common_dir.as_deref())
        .ok_or_else(|| format!("failed to locate a local worktree checked out on '{branch}'"))
}

/// The repository's common git dir as an absolute path, or `None` if unresolved.
pub fn git_common_dir_at(path: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let candidate = PathBuf::from(trimmed);
    let absolute = if candidate.is_absolute() {
        candidate
    } else {
        path.join(candidate)
    };
    Some(absolute)
}

/// Resolve a repo's default branch short name (e.g. `main`, `master`) from any
/// path inside it. Order: `origin/HEAD` symbolic ref, then probe known names.
/// Falls back to `"main"`.
pub fn resolve_default_branch_at(path: &Path) -> String {
    let head = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"])
        .output();
    if let Ok(output) = head {
        if output.status.success() {
            let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let short = value.strip_prefix("origin/").unwrap_or(&value);
            if !short.is_empty() {
                return short.to_string();
            }
        }
    }

    for candidate in ["origin/main", "origin/master", "main", "master"] {
        // Use `.output()` (not `.status()`): even with `--quiet`, a successful
        // `git rev-parse --verify` prints the resolved oid to stdout. Inheriting
        // that would leak a raw SHA into `sgit checkout`'s path-only stdout.
        let verify = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "--verify", "--quiet", candidate])
            .output();
        if matches!(verify, Ok(output) if output.status.success()) {
            return candidate
                .strip_prefix("origin/")
                .unwrap_or(candidate)
                .to_string();
        }
    }

    "main".to_string()
}

/// True when `git status --porcelain` reports no real changes (lock files, build
/// artifacts, and hidden-dir paths are ignored — same policy as worktree clean).
pub fn worktree_is_clean(worktree_path: &Path) -> Result<bool, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["status", "--porcelain"])
        .output()
        .map_err(|error| {
            format!(
                "failed to inspect working tree {}: {error}",
                worktree_path.display()
            )
        })?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }

    let has_real_changes = String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| {
            if line.len() > 3 {
                let filename = &line[3..];
                !should_ignore_changed_file(filename)
            } else {
                false
            }
        });

    Ok(!has_real_changes)
}

/// True when porcelain status is non-empty (no ignore filters). Used by branch
/// sync before deciding whether to fight-forward.
pub fn worktree_has_uncommitted_changes_at(path: &Path) -> Result<bool, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain"])
        .output()
        .map_err(|error| format!("failed to inspect worktree status: {error}"))?;

    if !output.status.success() {
        return Err(format!(
            "failed to inspect worktree status at {}:\n{}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(!output.stdout.is_empty())
}

/// Sync the local worktree for `branch` with `origin/<branch>` (fetch + ff-only
/// pull). When the worktree is dirty, `on_dirty` is invoked to fight forward
/// (stokd supplies `stokd shove`; sgit may supply a generic resolver later).
/// After a successful pull, `after_sync` runs (stokd: build-on-land trigger).
///
/// This is the pure `sync_branch` helper relocated from stokd's workspace module.
pub fn sync_branch(
    path: &Path,
    branch: &str,
    on_dirty: impl FnOnce(&Path) -> Result<(), String>,
    after_sync: impl FnOnce(&Path),
) -> Result<PathBuf, String> {
    let worktree_path = find_worktree_for_branch_at(path, branch)?;

    if current_branch_at(&worktree_path)? != branch {
        return Err(format!(
            "local {branch} worktree at {} is not checked out on '{branch}'",
            worktree_path.display()
        ));
    }

    if worktree_has_uncommitted_changes_at(&worktree_path)? {
        on_dirty(&worktree_path)?;
    }

    let fetch_output = Command::new("git")
        .arg("-C")
        .arg(&worktree_path)
        .args(["fetch", "origin", branch])
        .output()
        .map_err(|error| format!("failed to fetch origin/{branch}: {error}"))?;
    if !fetch_output.status.success() {
        return Err(format!(
            "failed to fetch origin/{branch}:\n{}",
            String::from_utf8_lossy(&fetch_output.stderr).trim()
        ));
    }

    let pull_output = Command::new("git")
        .arg("-C")
        .arg(&worktree_path)
        .args(["pull", "--ff-only", "origin", branch])
        .output()
        .map_err(|error| format!("failed to fast-forward local {branch}: {error}"))?;
    if !pull_output.status.success() {
        return Err(format!(
            "failed to fast-forward local {branch} worktree at {}:\n{}",
            worktree_path.display(),
            String::from_utf8_lossy(&pull_output.stderr).trim()
        ));
    }

    after_sync(&worktree_path);
    Ok(worktree_path)
}

/// Pure: the trimmed command to run after a land syncs local `main`, or `None`
/// to skip. Skips when build-on-land is off, the command is blank, or a build is
/// already in progress.
pub fn build_on_land_command<'a>(
    build_on_land: bool,
    build_command: &'a str,
    build_in_progress: bool,
) -> Option<&'a str> {
    let trimmed = build_command.trim();
    if build_on_land && !trimmed.is_empty() && !build_in_progress {
        Some(trimmed)
    } else {
        None
    }
}

/// True iff `lock_path` names a live process pid (a build is in progress). A
/// missing/garbage file or a dead pid means no build is in progress.
pub fn build_in_progress_at(lock_path: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(lock_path) else {
        return false;
    };
    let Ok(pid) = contents.trim().parse::<i32>() else {
        return false;
    };
    pid_is_alive(pid)
}

#[cfg(unix)]
fn pid_is_alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

#[cfg(not(unix))]
fn pid_is_alive(_pid: i32) -> bool {
    false
}

// ── internal helpers ─────────────────────────────────────────────────────────

/// Whether a porcelain path should be ignored for "real change" status checks.
pub fn should_ignore_changed_file(path: &str) -> bool {
    if path.ends_with("pnpm-lock.yaml")
        || path.ends_with("yarn.lock")
        || path.ends_with("package-lock.json")
        || path.ends_with("Cargo.lock")
    {
        return true;
    }

    if path.split('/').any(|segment| segment.starts_with('.')) {
        return true;
    }

    let ignored_dirs = [
        "node_modules",
        "dist",
        "build",
        "target",
        ".cache",
        "coverage",
        ".turbo",
        "__pycache__",
        ".pytest_cache",
    ];
    if ignored_dirs
        .iter()
        .any(|dir| path.contains(&format!("/{dir}/")) || path.starts_with(&format!("{dir}/")))
    {
        return true;
    }

    false
}

/// Parse porcelain `git worktree list` output for a worktree on `branch`.
pub fn parse_worktree_for_branch_from_porcelain(
    text: &str,
    branch: &str,
    exclude: Option<&Path>,
) -> Option<PathBuf> {
    let expected_branch = format!("refs/heads/{branch}");
    let exclude_norm = exclude.map(normalize_path);
    let mut current_worktree: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;
    let mut current_is_bare = false;

    for line in text.lines().chain(std::iter::once("")) {
        if line.trim().is_empty() {
            let matches_branch = current_branch.as_deref() == Some(expected_branch.as_str());
            let is_excluded = current_is_bare
                || match (current_worktree.as_deref(), exclude_norm.as_deref()) {
                    (Some(worktree), Some(common)) => normalize_path(worktree).as_path() == common,
                    _ => false,
                };
            if matches_branch && !is_excluded {
                return current_worktree.take();
            }
            current_worktree = None;
            current_branch = None;
            current_is_bare = false;
            continue;
        }

        if let Some(value) = line.strip_prefix("worktree ") {
            current_worktree = Some(PathBuf::from(value.trim()));
            continue;
        }

        if line.trim() == "bare" {
            current_is_bare = true;
            continue;
        }

        if let Some(value) = line.strip_prefix("branch ") {
            current_branch = Some(value.trim().to_string());
        }
    }

    None
}

fn normalize_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn current_branch_at(path: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .map_err(|error| format!("failed to determine current branch: {error}"))?;

    if !output.status.success() {
        return Err(format!(
            "failed to determine current branch at {}:\n{}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let branch = String::from_utf8(output.stdout)
        .map_err(|error| format!("git rev-parse returned invalid utf-8: {error}"))?;
    let branch = branch.trim();
    if branch.is_empty() || branch == "HEAD" {
        return Err(format!(
            "failed to determine current branch at {}: detached HEAD",
            path.display()
        ));
    }

    Ok(branch.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_on_land_command_decision() {
        assert_eq!(build_on_land_command(false, "pnpm done:local", false), None);
        assert_eq!(build_on_land_command(true, "", false), None);
        assert_eq!(build_on_land_command(true, "   ", false), None);
        assert_eq!(build_on_land_command(true, "pnpm done:local", true), None);
        assert_eq!(
            build_on_land_command(true, "  pnpm done:local  ", false),
            Some("pnpm done:local")
        );
    }

    #[test]
    fn build_in_progress_lock_liveness() {
        use std::fs;
        let dir = tempfile::TempDir::new().unwrap();
        let lock = dir.path().join(".build-on-land.lock");

        assert!(!build_in_progress_at(&lock));

        fs::write(&lock, std::process::id().to_string()).unwrap();
        assert!(build_in_progress_at(&lock));

        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        fs::write(&lock, dead_pid.to_string()).unwrap();
        assert!(!build_in_progress_at(&lock));

        fs::write(&lock, "not-a-pid").unwrap();
        assert!(!build_in_progress_at(&lock));
    }

    #[test]
    fn parse_worktree_for_branch_from_porcelain_finds_matching_branch() {
        let result = parse_worktree_for_branch_from_porcelain(
            "worktree /tmp/main\nHEAD abc123\nbranch refs/heads/main\n\nworktree /tmp/task\nHEAD def456\nbranch refs/heads/task/test\n",
            "main",
            None,
        );
        assert_eq!(result.as_deref(), Some(Path::new("/tmp/main")));
    }

    #[test]
    fn parse_worktree_excludes_bare_common_dir_listed_first() {
        let porcelain = "worktree /opt/dev/repo.git\nHEAD abc123\nbranch refs/heads/main\n\nworktree /opt/worktrees/repo/main\nHEAD abc123\nbranch refs/heads/main\n";
        let result = parse_worktree_for_branch_from_porcelain(
            porcelain,
            "main",
            Some(Path::new("/opt/dev/repo.git")),
        );
        assert_eq!(
            result.as_deref(),
            Some(Path::new("/opt/worktrees/repo/main")),
            "must return the real linked worktree, not the bare common gitdir"
        );
    }

    #[test]
    fn parse_worktree_skips_bare_marked_entry() {
        let porcelain = "worktree /opt/dev/repo.git\nbare\n\nworktree /opt/worktrees/repo/main\nHEAD abc123\nbranch refs/heads/main\n";
        let result = parse_worktree_for_branch_from_porcelain(porcelain, "main", None);
        assert_eq!(
            result.as_deref(),
            Some(Path::new("/opt/worktrees/repo/main"))
        );
    }
}
