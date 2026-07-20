//! Pure apply primitives for bare/worktree relocation (used by `sgit repo migrate`
//! and `sgit repo rename`). No discovery/planning UI — just filesystem + git ops.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Outcome of an apply step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyStatus {
    Moved,
    MovedWithWarnings(Vec<String>),
    Skipped(UnsafeReason),
    Failed(String),
}

/// Why an action was not auto-applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsafeReason {
    DestinationExists,
    DirtyWorktree,
    UnpushedCommits,
    BrokenAdmin,
    Other(String),
}

/// A worktree that needs repair after a bare relocation.
#[derive(Debug, Clone)]
pub struct WorktreeRepairTarget {
    pub path: PathBuf,
    pub broken: bool,
}

impl WorktreeRepairTarget {
    pub fn healthy(path: PathBuf) -> Self {
        Self {
            path,
            broken: false,
        }
    }

    pub fn broken(path: PathBuf) -> Self {
        Self {
            path,
            broken: true,
        }
    }
}

/// Create the canonical main worktree from the bare: `git worktree add`.
pub fn materialize_main_worktree(bare: &Path, worktree: &Path, branch: &str) -> ApplyStatus {
    if worktree.exists() {
        return ApplyStatus::Skipped(UnsafeReason::DestinationExists);
    }
    if let Some(parent) = worktree.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return ApplyStatus::Failed(format!("cannot create {}: {e}", parent.display()));
        }
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(bare)
        .args(["worktree", "add"])
        .arg(worktree)
        .arg(branch)
        .output();
    match output {
        Ok(o) if o.status.success() => ApplyStatus::Moved,
        Ok(o) => ApplyStatus::Failed(format!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => ApplyStatus::Failed(format!("git worktree add failed: {e}")),
    }
}

/// `git worktree move` for a linked worktree.
pub fn move_worktree(bare: &Path, from: &Path, to: &Path) -> ApplyStatus {
    if let Some(parent) = to.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return ApplyStatus::Failed(format!("cannot create {}: {e}", parent.display()));
        }
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(bare)
        .args(["worktree", "move"])
        .arg(from)
        .arg(to)
        .output();
    match output {
        Ok(o) if o.status.success() => ApplyStatus::Moved,
        Ok(o) => ApplyStatus::Failed(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => ApplyStatus::Failed(e.to_string()),
    }
}

/// Relocate a bare repo to `to`, then repair linked worktrees.
pub fn move_bare(from: &Path, to: &Path, worktrees: &[WorktreeRepairTarget]) -> ApplyStatus {
    if to.exists() {
        return ApplyStatus::Failed(format!("destination already exists: {}", to.display()));
    }
    if let Some(parent) = to.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return ApplyStatus::Failed(format!("cannot create {}: {e}", parent.display()));
        }
    }

    // Nested destination: stage through a temp sibling to avoid EINVAL.
    if to.starts_with(from) {
        let staged = staging_path(from);
        if staged.exists() {
            return ApplyStatus::Failed(format!(
                "staging path already exists: {}",
                staged.display()
            ));
        }
        if let Err(e) = fs::rename(from, &staged) {
            return ApplyStatus::Failed(format!("rename (stage) failed: {e}"));
        }
        if let Some(parent) = to.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                let _ = fs::rename(&staged, from);
                return ApplyStatus::Failed(format!("cannot create {}: {e}", parent.display()));
            }
        }
        if let Err(e) = fs::rename(&staged, to) {
            let _ = fs::rename(&staged, from);
            return ApplyStatus::Failed(format!("rename (final) failed: {e}"));
        }
    } else if let Err(e) = fs::rename(from, to) {
        return ApplyStatus::Failed(format!("rename failed: {e}"));
    }

    let mut warnings = Vec::new();

    for wt in worktrees.iter().filter(|w| w.broken) {
        warnings.push(format!(
            "could not repair already-broken worktree: {}",
            wt.path.display()
        ));
    }

    for wt in worktrees.iter().filter(|w| !w.broken) {
        let repair = Command::new("git")
            .arg("-C")
            .arg(to)
            .args(["worktree", "repair"])
            .arg(&wt.path)
            .output();
        match repair {
            Ok(o) if o.status.success() => {
                // Per-worktree core.bare=false when worktreeConfig is enabled.
                ensure_worktree_not_bare(to, &wt.path);
            }
            Ok(o) => warnings.push(format!(
                "worktree repair failed for {}: {}",
                wt.path.display(),
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            Err(e) => warnings.push(format!(
                "worktree repair failed for {}: {e}",
                wt.path.display()
            )),
        }
    }

    if warnings.is_empty() {
        ApplyStatus::Moved
    } else {
        ApplyStatus::MovedWithWarnings(warnings)
    }
}

fn staging_path(from: &Path) -> PathBuf {
    let name = from
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "bare".into());
    from.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{name}.sgit-migrate-stage"))
}

/// When the bare uses `extensions.worktreeConfig` with shared `core.bare=true`,
/// set a per-worktree `core.bare=false` override so working-tree ops work.
fn ensure_worktree_not_bare(bare: &Path, worktree: &Path) {
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
