//! Generic worktree lease *bucket* helpers (stable repo identity).
//!
//! Only pure status/bucket primitives live here. Stokd-specific lease
//! persistence, sanction binding, session heartbeats, takeover, and governance
//! stay in `apps/cli` (`worktree_lease.rs`).
//!
//! Lease buckets are keyed on the stable **API `repo_id`** (UUID), never a
//! filesystem path. Path-derived keys (e.g. a worktree leaf named `main`)
//! spanned multiple repos and created a multi-repo reconcile blast radius.

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

/// Deterministic lease-bucket key from an API `repo_id` (UUID or other stable id).
/// Same `repo_id` always maps to the same bucket; path is irrelevant.
pub fn repo_id_bucket(repo_id: &str) -> String {
    sanitize_repo(repo_id)
}

/// Resolve the git common dir (bare repo / primary `.git`) for a path inside a
/// linked worktree or primary checkout. Useful for layout helpers; **not** the
/// lease-bucket identity (buckets use `repo_id` — see [`stable_repo_bucket`]).
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

/// Read a local `.stokd/repo_id` file content (trimmed), if present and non-empty.
pub fn read_repo_id_file(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Walk `start` and its parents looking for `.stokd/repo_id`. Also checks the
/// git common dir's parent (primary checkout / bare-adjacent) so linked
/// worktrees that share one repo resolve the same id.
pub fn find_local_repo_id(worktree_path: &str) -> Option<String> {
    let start = Path::new(worktree_path);
    // 1. Walk ancestors from the worktree path.
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if let Some(id) = read_repo_id_file(&dir.join(".stokd").join("repo_id")) {
            return Some(id);
        }
        cur = dir.parent();
    }
    // 2. Linked worktree: common dir parent (e.g. bare repo sibling layout).
    if let Some(common) = git_common_dir(worktree_path) {
        if let Some(parent) = common.parent() {
            if let Some(id) = read_repo_id_file(&parent.join(".stokd").join("repo_id")) {
                return Some(id);
            }
            // Bare repo often lives next to a main worktree leaf that holds .stokd.
            // Also try the common dir itself if it is a `.git` directory's parent.
            if common.ends_with(".git") {
                if let Some(checkout) = common.parent() {
                    if let Some(id) = read_repo_id_file(&checkout.join(".stokd").join("repo_id")) {
                        return Some(id);
                    }
                }
            }
        }
    }
    None
}

/// Stable lease-bucket key for a worktree: derived from the API `repo_id`
/// (via local `.stokd/repo_id`), **not** the filesystem path of the git common
/// dir. `None` when no repo_id can be resolved — callers must not invent a
/// path-derived bucket (VAL-DRAIN-001).
pub fn stable_repo_bucket(worktree_path: &str) -> Option<String> {
    find_local_repo_id(worktree_path).map(|id| repo_id_bucket(&id))
}

/// Convenience: stable bucket from a [`Path`].
pub fn stable_repo_bucket_path(worktree_path: &Path) -> Option<String> {
    stable_repo_bucket(&worktree_path.to_string_lossy())
}

/// Legacy path-derived bucket (git common dir). Used only for migration
/// diagnostics / identifying pre-repo_id leases — never for new writes.
pub fn legacy_path_repo_bucket(worktree_path: &str) -> Option<String> {
    git_common_dir(worktree_path).map(|p| sanitize_repo(&p.to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
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
    fn repo_id_bucket_is_deterministic_sanitize_of_uuid() {
        let uuid = "5b249d4f-6fc5-4ecb-b649-bbf225f798d4";
        let a = repo_id_bucket(uuid);
        let b = repo_id_bucket(uuid);
        assert_eq!(a, b);
        assert_eq!(a, "5b249d4f-6fc5-4ecb-b649-bbf225f798d4");
        assert_ne!(a, sanitize_repo("main"), "must not be path leaf main");
    }

    #[test]
    fn stable_repo_bucket_uses_repo_id_not_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        // Two "worktrees" (dirs) of the same logical repo share .stokd/repo_id
        // via a shared parent — plant repo_id at each so find_local_repo_id works.
        let wt_a = dir.path().join("wt-a");
        let wt_b = dir.path().join("wt-b");
        fs::create_dir_all(wt_a.join(".stokd")).unwrap();
        fs::create_dir_all(wt_b.join(".stokd")).unwrap();
        fs::write(wt_a.join(".stokd").join("repo_id"), repo_id).unwrap();
        fs::write(wt_b.join(".stokd").join("repo_id"), repo_id).unwrap();

        let bucket_a = stable_repo_bucket(&wt_a.to_string_lossy()).expect("bucket a");
        let bucket_b = stable_repo_bucket(&wt_b.to_string_lossy()).expect("bucket b");
        assert_eq!(bucket_a, bucket_b, "same repo_id → same bucket");
        assert_eq!(bucket_a, repo_id_bucket(repo_id));
        assert_ne!(bucket_a, "main");
        assert_ne!(bucket_a, sanitize_repo("main"));

        // Path move: rename wt-a → wt-moved; bucket must be unchanged.
        let wt_moved = dir.path().join("wt-moved");
        fs::rename(&wt_a, &wt_moved).unwrap();
        let bucket_moved = stable_repo_bucket(&wt_moved.to_string_lossy()).expect("moved");
        assert_eq!(bucket_moved, bucket_a, "path move must not change bucket");
    }

    #[test]
    fn stable_repo_bucket_none_without_repo_id_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bare = dir.path().join("no-id");
        fs::create_dir_all(&bare).unwrap();
        // Not a git repo and no .stokd/repo_id → None (never invent path key).
        assert!(stable_repo_bucket(&bare.to_string_lossy()).is_none());
    }

    #[test]
    fn legacy_path_bucket_still_resolves_git_common_dir() {
        let here = env!("CARGO_MANIFEST_DIR"); // packages/sgit-core
        let repo_root = Path::new(here)
            .parent()
            .and_then(|p| p.parent())
            .expect("repo root");
        let bucket = legacy_path_repo_bucket(&repo_root.to_string_lossy())
            .expect("repo root must resolve a common dir");
        assert!(!bucket.is_empty());
    }
}
