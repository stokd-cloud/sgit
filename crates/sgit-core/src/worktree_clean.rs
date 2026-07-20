//! `sgit worktree clean` — remove landed/merged linked worktrees safely.
//!
//! Parity with stokd's clean policy: only remove clean worktrees whose branch is
//! already reachable from the primary base ref. Removal goes through
//! `git worktree remove` (not a blind `rm -rf` of the tree root).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::workspace::{detect_repo_root_at, worktree_is_clean};

/// Bare-hub placeholder branch short name (must never be cleaned as a candidate).
const BARE_HUB_PLACEHOLDER: &str = "__stokd_hub__";

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeEntry {
    path: PathBuf,
    branch: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorktreeStatus {
    Dirty,
    Unmerged,
    Broken,
    Removable,
}

/// Outcome of a clean run (for callers / tests).
#[derive(Debug, Default, Clone)]
pub struct CleanSummary {
    pub removed: usize,
    pub skipped_dirty: usize,
    pub skipped_unmerged: usize,
    pub skipped_broken: usize,
    pub skipped_protected: usize,
    pub skipped_detached: usize,
    pub errors: usize,
    pub removed_paths: Vec<PathBuf>,
    pub preserved_paths: Vec<PathBuf>,
}

/// Run worktree clean for the repository containing `cwd` (usually `.`).
///
/// When `dry_run` is true, candidates are reported but not removed.
pub fn run_clean_at(cwd: &Path, dry_run: bool) -> Result<CleanSummary, String> {
    let repo_root = detect_repo_root_at(cwd).ok_or_else(|| {
        "not inside a git repository".to_string()
    })?;

    let base_ref = resolve_primary_base_ref(&repo_root).ok_or_else(|| {
        "failed to resolve a main branch reference for this repository".to_string()
    })?;
    let base_branch = short_branch_name(&base_ref);

    let worktrees = list_worktrees(&repo_root)?;

    let mut summary = CleanSummary::default();

    println!(
        "{} worktree cleanup for {}",
        if dry_run { "Dry run" } else { "Running" },
        repo_root.display()
    );
    println!("Base branch reference: {base_ref}");

    for entry in worktrees {
        let branch = match entry.branch.as_deref() {
            Some(branch) => branch,
            None => {
                summary.skipped_detached += 1;
                println!("Skipping detached worktree: {}", entry.path.display());
                continue;
            }
        };

        if is_protected_branch(branch, &base_branch) {
            summary.skipped_protected += 1;
            continue;
        }

        if !entry.path.exists() {
            println!(
                "Skipping missing worktree path (will rely on prune): {}",
                entry.path.display()
            );
            continue;
        }

        match classify_worktree(&repo_root, &entry, &base_ref) {
            Ok(WorktreeStatus::Dirty) => {
                summary.skipped_dirty += 1;
                summary.preserved_paths.push(entry.path.clone());
                println!(
                    "Skipping dirty worktree [{}]: {}",
                    branch,
                    entry.path.display()
                );
            }
            Ok(WorktreeStatus::Unmerged) => {
                summary.skipped_unmerged += 1;
                summary.preserved_paths.push(entry.path.clone());
                println!(
                    "Skipping unmerged worktree [{}]: {}",
                    branch,
                    entry.path.display()
                );
            }
            Ok(WorktreeStatus::Broken) => {
                summary.skipped_broken += 1;
                summary.preserved_paths.push(entry.path.clone());
                println!(
                    "Skipping broken worktree [{}]: {}",
                    branch,
                    entry.path.display()
                );
            }
            Ok(WorktreeStatus::Removable) => {
                if dry_run {
                    println!("Would remove [{}]: {}", branch, entry.path.display());
                    summary.removed += 1;
                    summary.removed_paths.push(entry.path.clone());
                } else {
                    match remove_worktree(&repo_root, &entry.path) {
                        Ok(()) => {
                            println!("Removed [{}]: {}", branch, entry.path.display());
                            summary.removed += 1;
                            summary.removed_paths.push(entry.path.clone());
                        }
                        Err(error) => {
                            summary.errors += 1;
                            eprintln!(
                                "Failed to remove [{}] {}: {}",
                                branch,
                                entry.path.display(),
                                error
                            );
                        }
                    }
                }
            }
            Err(error) => {
                summary.errors += 1;
                eprintln!(
                    "Failed to inspect [{}] {}: {}",
                    branch,
                    entry.path.display(),
                    error
                );
            }
        }
    }

    let _ = Command::new("git")
        .arg("-C")
        .arg(&repo_root)
        .args(["worktree", "prune"])
        .status();

    println!();
    println!("Worktree clean summary:");
    println!("  Removed                    : {}", summary.removed);
    println!("  Skipped dirty              : {}", summary.skipped_dirty);
    println!("  Skipped unmerged           : {}", summary.skipped_unmerged);
    println!("  Skipped broken             : {}", summary.skipped_broken);
    println!("  Skipped protected          : {}", summary.skipped_protected);
    println!("  Skipped detached           : {}", summary.skipped_detached);
    println!("  Errors                     : {}", summary.errors);

    if summary.errors > 0 {
        return Err(format!("{} error(s) during worktree clean", summary.errors));
    }
    Ok(summary)
}

fn classify_worktree(
    repo_root: &Path,
    entry: &WorktreeEntry,
    base_ref: &str,
) -> Result<WorktreeStatus, String> {
    let branch = entry
        .branch
        .as_deref()
        .ok_or_else(|| "worktree entry is missing a branch".to_string())?;

    let clean_state = worktree_is_clean_or_broken(&entry.path);
    let merged_to_base = branch_is_merged(repo_root, branch, base_ref)?;

    Ok(match clean_state {
        Ok(false) => WorktreeStatus::Dirty,
        Ok(true) if !merged_to_base => WorktreeStatus::Unmerged,
        Ok(true) => WorktreeStatus::Removable,
        Err(_) => WorktreeStatus::Broken,
    })
}

fn worktree_is_clean_or_broken(worktree_path: &Path) -> Result<bool, String> {
    if let Some(target) = broken_gitdir_target(worktree_path) {
        return Err(format!(
            "gitdir pointer is broken: {}",
            target.to_string_lossy()
        ));
    }
    worktree_is_clean(worktree_path)
}

fn broken_gitdir_target(worktree_path: &Path) -> Option<PathBuf> {
    let git_file = worktree_path.join(".git");
    let raw = std::fs::read_to_string(git_file).ok()?;
    let target = raw.trim().strip_prefix("gitdir:")?.trim();
    let path = PathBuf::from(target);
    let resolved = if path.is_absolute() {
        path
    } else {
        worktree_path.join(path)
    };
    if resolved.exists() {
        None
    } else {
        Some(resolved)
    }
}

fn branch_is_merged(repo_root: &Path, branch: &str, base_ref: &str) -> Result<bool, String> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["merge-base", "--is-ancestor", branch, base_ref])
        .status()
        .map_err(|error| format!("failed to compare branch {branch} to {base_ref}: {error}"))?;
    Ok(status.success())
}

/// Safe removal: `git worktree remove --force`, then only delete residual path
/// if git left it behind.
pub fn remove_worktree(repo_root: &Path, worktree_path: &Path) -> Result<(), String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args([
            "worktree",
            "remove",
            &worktree_path.to_string_lossy(),
            "--force",
        ])
        .output()
        .map_err(|error| format!("failed to remove worktree: {error}"))?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }

    if worktree_path.exists() {
        remove_path(worktree_path)?;
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("failed to stat {}: {error}", path.display()))?;
    if metadata.is_dir() {
        std::fs::remove_dir_all(path)
            .map_err(|error| format!("failed to delete {}: {error}", path.display()))
    } else {
        std::fs::remove_file(path)
            .map_err(|error| format!("failed to delete {}: {error}", path.display()))
    }
}

fn resolve_primary_base_ref(repo_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args([
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ])
        .output()
        .ok()?;
    if output.status.success() {
        let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
        if !value.is_empty() {
            return Some(value);
        }
    }

    for candidate in ["origin/main", "origin/master", "main", "master"] {
        let verify = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(["rev-parse", "--verify", "--quiet", candidate])
            .status()
            .ok()?;
        if verify.success() {
            return Some(candidate.to_string());
        }
    }
    None
}

fn short_branch_name(branch: &str) -> String {
    branch
        .strip_prefix("refs/heads/")
        .or_else(|| branch.strip_prefix("origin/"))
        .unwrap_or(branch)
        .to_string()
}

fn is_protected_branch(branch: &str, base_branch: &str) -> bool {
    matches!(branch, "main" | "master")
        || branch == base_branch
        || branch == BARE_HUB_PLACEHOLDER
}

fn list_worktrees(repo_root: &Path) -> Result<Vec<WorktreeEntry>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map_err(|error| format!("failed to list worktrees: {error}"))?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }

    let text = String::from_utf8(output.stdout)
        .map_err(|error| format!("failed to decode worktree list: {error}"))?;
    Ok(parse_worktree_list_porcelain(&text))
}

fn parse_worktree_list_porcelain(text: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;

    for line in text.lines().chain(std::iter::once("")) {
        if line.trim().is_empty() {
            if let Some(path) = current_path.take() {
                entries.push(WorktreeEntry {
                    path,
                    branch: current_branch.take(),
                });
            }
            current_branch = None;
            continue;
        }

        if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(path.trim()));
            current_branch = None;
            continue;
        }

        if let Some(branch) = line.strip_prefix("branch ") {
            current_branch = Some(short_branch_name(branch.trim()));
        }
    }

    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git")
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn clean_removes_merged_preserves_unmerged() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("repo.git");
        let main_wt = tmp.path().join("main");
        let merged_wt = tmp.path().join("merged");
        let unmerged_wt = tmp.path().join("unmerged");

        // Init a non-bare primary then convert pattern: primary + worktrees.
        std::fs::create_dir_all(&main_wt).unwrap();
        git(&main_wt, &["init", "-q", "-b", "main"]);
        git(&main_wt, &["config", "user.email", "t@t.co"]);
        git(&main_wt, &["config", "user.name", "t"]);
        git(&main_wt, &["commit", "-q", "--allow-empty", "-m", "init"]);

        // Merged branch: commit then merge into main.
        git(
            &main_wt,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature-merged",
                merged_wt.to_str().unwrap(),
            ],
        );
        git(&merged_wt, &["commit", "-q", "--allow-empty", "-m", "feat"]);
        git(&main_wt, &["merge", "-q", "--no-ff", "feature-merged", "-m", "m"]);

        // Unmerged branch: commit not merged.
        git(
            &main_wt,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature-open",
                unmerged_wt.to_str().unwrap(),
            ],
        );
        git(&unmerged_wt, &["commit", "-q", "--allow-empty", "-m", "open"]);

        let _ = bare; // layout is primary-linked, not bare — fine for clean
        let summary = run_clean_at(&main_wt, false).expect("clean ok");
        assert_eq!(summary.removed, 1, "one merged worktree removed: {summary:?}");
        assert!(
            !merged_wt.exists(),
            "merged worktree path should be gone"
        );
        assert!(
            unmerged_wt.exists(),
            "unmerged worktree must be preserved"
        );
        assert!(
            summary.skipped_unmerged >= 1,
            "unmerged counted: {summary:?}"
        );
    }

    #[test]
    fn is_protected_covers_hub_placeholder() {
        assert!(is_protected_branch("__stokd_hub__", "main"));
        assert!(is_protected_branch("main", "main"));
        assert!(!is_protected_branch("feature/x", "main"));
    }
}
