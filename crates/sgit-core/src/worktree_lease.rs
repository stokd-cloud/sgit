//! Generic worktree lease *bucket* helpers (stable repo identity).
//!
//! Only pure status/bucket primitives live here. Stokd-specific lease
//! persistence, sanction binding, session heartbeats, takeover, and governance
//! stay in `apps/cli` (`worktree_lease.rs`).

use std::path::{Path, PathBuf};

/// Sanitise a repository label into a single filesystem-safe path segment.
/// Alphanumerics are lower-cased and every other run collapses to a single dash.
pub fn sanitize_repo(repo: &str) -> String {
    let mut output = String::new();
    let mut last_was_dash = false;
    for ch in repo.chars() {
        if ch.is_ascii_alphanumeric() {
            output.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash && !output.is_empty() {
            output.push('-');
            last_was_dash = true;
        }
    }
    let trimmed: String = output.trim_matches('-').chars().take(64).collect();
    if trimmed.is_empty() {
        "repo".to_string()
    } else {
        trimmed
    }
}

/// Resolve the git common dir (bare repo / primary `.git`) for a path inside a
/// linked worktree or primary checkout. Stable repo identity for lease
/// bucketing — two worktrees of the same repo share it; different repos never
/// do. Returns `None` when the path is not in a git repo.
pub fn git_common_dir(worktree_path: &str) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if dir.is_empty() {
        None
    } else {
        Some(PathBuf::from(dir))
    }
}

/// Stable lease-bucket key for a worktree: `sanitize_repo` of its absolute git
/// common dir. `None` when the worktree path cannot resolve a git common dir.
pub fn stable_repo_bucket(worktree_path: &str) -> Option<String> {
    git_common_dir(worktree_path).map(|p| sanitize_repo(&p.to_string_lossy()))
}

/// Convenience: stable bucket from a [`Path`].
pub fn stable_repo_bucket_path(worktree_path: &Path) -> Option<String> {
    stable_repo_bucket(&worktree_path.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn sanitize_repo_collapses_and_lowercases() {
        assert_eq!(
            sanitize_repo("Stokd Cloud/Stokd Mono"),
            "stokd-cloud-stokd-mono"
        );
        assert_eq!(sanitize_repo("owner/repo.git"), "owner-repo-git");
        assert_eq!(sanitize_repo("///"), "repo");
    }

    #[test]
    fn stable_repo_bucket_is_shared_across_linked_worktrees_of_one_repo() {
        let here = env!("CARGO_MANIFEST_DIR"); // packages/sgit-core
        let repo_root = Path::new(here)
            .parent()
            .and_then(|p| p.parent())
            .expect("repo root");
        let bucket_a = stable_repo_bucket(&repo_root.to_string_lossy())
            .expect("repo root must resolve a common dir");
        let bucket_b = stable_repo_bucket(here).expect("sgit-core path must resolve a common dir");
        assert_eq!(
            bucket_a, bucket_b,
            "two paths in the same repo must share one stable bucket"
        );
        assert!(!bucket_a.is_empty());
        assert_ne!(
            bucket_a,
            sanitize_repo("main"),
            "must not key on worktree basename alone"
        );
    }
}
