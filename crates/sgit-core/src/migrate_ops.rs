//! Pure apply primitives for bare/worktree relocation (used by `sgit repo migrate`
//! and `sgit repo rename`). No discovery/planning UI — just filesystem + git ops.

use crate::layout::ensure_worktree_not_bare;
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

/// How a linked worktree relocation completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkedWorktreeMove {
    /// Git performed the move and updated its administrative records.
    MovedByGit,
    /// Git refused only because the worktree contains initialized submodules;
    /// a same-filesystem rename followed by outer + nested worktree repair
    /// completed instead.
    MovedByRenameRepair,
    /// The source remains registered at its original path.
    FailedUnchanged(String),
    /// Files may be at `current_path`; the error is durable/recoverable and the
    /// caller must not infer that either path is disposable.
    RecoverableIncomplete {
        current_path: PathBuf,
        error: String,
    },
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
        Self { path, broken: true }
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

/// Move a linked worktree without deleting or reconstructing its working
/// files. Ordinary trees use `git worktree move`. Git deliberately rejects an
/// initialized-submodule tree; only that exact refusal may take the fallback:
/// a same-filesystem atomic rename followed by repair of the outer worktree and
/// every nested git admin link discovered before the rename.
pub fn move_linked_worktree_preserving_state(
    common_git_dir: &Path,
    from: &Path,
    to: &Path,
) -> LinkedWorktreeMove {
    move_linked_worktree_preserving_state_with(
        common_git_dir,
        from,
        to,
        same_filesystem,
        run_git_worktree_move,
        rename_noreplace,
    )
}

fn move_linked_worktree_preserving_state_with<FilesystemCheck, GitMove, Rename>(
    common_git_dir: &Path,
    from: &Path,
    to: &Path,
    filesystem_check: FilesystemCheck,
    git_move: GitMove,
    rename: Rename,
) -> LinkedWorktreeMove
where
    FilesystemCheck: Fn(&Path, &Path) -> Result<bool, String>,
    GitMove: Fn(&Path, &Path, &Path) -> Result<(), String>,
    Rename: Fn(&Path, &Path) -> std::io::Result<()>,
{
    match destination_occupied(to) {
        Ok(true) => {
            return LinkedWorktreeMove::FailedUnchanged(format!(
                "destination already exists: {}",
                to.display()
            ))
        }
        Ok(false) => {}
        Err(error) => return LinkedWorktreeMove::FailedUnchanged(error),
    }

    // This is deliberately before `git worktree move`: adoption is a
    // same-filesystem topology rename even when Git could implement something
    // broader. Refusing here also proves no Git/admin mutation happened.
    match filesystem_check(from, to) {
        Ok(true) => {}
        Ok(false) => {
            return LinkedWorktreeMove::FailedUnchanged(
                "worktree adoption requires source and destination on the same filesystem"
                    .to_string(),
            )
        }
        Err(error) => return LinkedWorktreeMove::FailedUnchanged(error),
    }

    let failure = match git_move(common_git_dir, from, to) {
        Ok(()) => return LinkedWorktreeMove::MovedByGit,
        Err(failure) => failure,
    };

    if !is_submodule_move_refusal(&failure) {
        return LinkedWorktreeMove::FailedUnchanged(failure.trim().to_string());
    }

    let nested_admins = match discover_nested_git_admins(from) {
        Ok(admins) => admins,
        Err(error) => return LinkedWorktreeMove::FailedUnchanged(error),
    };
    if let Some(parent) = to.parent() {
        if let Err(error) = fs::create_dir_all(parent) {
            return LinkedWorktreeMove::FailedUnchanged(format!(
                "cannot create destination parent {}: {error}",
                parent.display()
            ));
        }
    }

    // Re-attest immediately before the raw rename. The rename primitive is
    // itself exclusive, so a destination created after this check still wins
    // without being replaced.
    match destination_occupied(to) {
        Ok(true) => {
            return LinkedWorktreeMove::FailedUnchanged(format!(
                "destination appeared before fallback rename: {}",
                to.display()
            ))
        }
        Ok(false) => {}
        Err(error) => return LinkedWorktreeMove::FailedUnchanged(error),
    }
    if let Err(error) = rename(from, to) {
        return LinkedWorktreeMove::FailedUnchanged(format!(
            "same-filesystem exclusive worktree rename failed: {error}"
        ));
    }

    if let Err(error) = repair_moved_worktree(common_git_dir, to, &nested_admins) {
        return rollback_failed_fallback(common_git_dir, from, to, &nested_admins, error);
    }

    LinkedWorktreeMove::MovedByRenameRepair
}

fn run_git_worktree_move(common_git_dir: &Path, from: &Path, to: &Path) -> Result<(), String> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(common_git_dir)
        .args(["worktree", "move"])
        .arg(from)
        .arg(to)
        .output();
    match output {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stderr).trim(),
            String::from_utf8_lossy(&output.stdout).trim()
        )),
        Err(error) => Err(format!("git worktree move failed to start: {error}")),
    }
}

fn destination_occupied(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "cannot inspect destination {}: {error}",
            path.display()
        )),
    }
}

#[cfg(target_os = "macos")]
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in source path"))?;
    let to = CString::new(to.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in destination path")
    })?;
    // SAFETY: both arguments are live NUL-terminated path buffers, and
    // RENAME_EXCL asks the kernel to fail atomically if `to` exists.
    let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in source path"))?;
    let to = CString::new(to.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in destination path")
    })?;
    // SAFETY: arguments are valid C paths. renameat2 with RENAME_NOREPLACE is
    // one kernel transaction and cannot overwrite a racing destination.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    // Windows' rename primitive refuses an existing destination when no
    // replacement flag is supplied.
    fs::rename(from, to)
}

#[cfg(all(
    unix,
    not(target_os = "macos"),
    not(target_os = "linux"),
    not(target_os = "android")
))]
fn rename_noreplace(_from: &Path, _to: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this platform has no configured atomic no-replace directory rename",
    ))
}

fn is_submodule_move_refusal(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("working trees containing submodules")
        && message.contains("cannot be moved or removed")
}

#[cfg(unix)]
fn same_filesystem(from: &Path, to: &Path) -> Result<bool, String> {
    use std::os::unix::fs::MetadataExt;

    let source_device = fs::metadata(from)
        .map_err(|error| format!("cannot stat source {}: {error}", from.display()))?
        .dev();
    let destination_ancestor = nearest_existing_ancestor(to).ok_or_else(|| {
        format!(
            "destination {} has no existing ancestor for filesystem check",
            to.display()
        )
    })?;
    let destination_device = fs::metadata(&destination_ancestor)
        .map_err(|error| {
            format!(
                "cannot stat destination ancestor {}: {error}",
                destination_ancestor.display()
            )
        })?
        .dev();
    Ok(source_device == destination_device)
}

#[cfg(windows)]
fn same_filesystem(from: &Path, to: &Path) -> Result<bool, String> {
    use std::path::Component;

    let prefix = |path: &Path| {
        path.components().find_map(|component| match component {
            Component::Prefix(prefix) => Some(prefix.as_os_str().to_owned()),
            _ => None,
        })
    };
    let source = fs::canonicalize(from)
        .map_err(|error| format!("cannot canonicalize source {}: {error}", from.display()))?;
    let destination_ancestor = nearest_existing_ancestor(to).ok_or_else(|| {
        format!(
            "destination {} has no existing ancestor for volume check",
            to.display()
        )
    })?;
    let destination = fs::canonicalize(&destination_ancestor).map_err(|error| {
        format!(
            "cannot canonicalize destination ancestor {}: {error}",
            destination_ancestor.display()
        )
    })?;
    Ok(prefix(&source) == prefix(&destination))
}

fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut candidate = path.parent();
    while let Some(value) = candidate {
        if value.exists() {
            return Some(value.to_path_buf());
        }
        candidate = value.parent();
    }
    None
}

#[derive(Debug, Clone)]
struct NestedGitAdmin {
    relative_root: PathBuf,
    git_dir: PathBuf,
}

fn discover_nested_git_admins(root: &Path) -> Result<Vec<NestedGitAdmin>, String> {
    fn visit(root: &Path, current: &Path, out: &mut Vec<NestedGitAdmin>) -> Result<(), String> {
        let entries = fs::read_dir(current).map_err(|error| {
            format!(
                "cannot scan nested Git roots in {}: {error}",
                current.display()
            )
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                format!(
                    "cannot read nested Git entry in {}: {error}",
                    current.display()
                )
            })?;
            let path = entry.path();
            if entry.file_name() == ".git" {
                continue;
            }
            let file_type = entry.file_type().map_err(|error| {
                format!(
                    "cannot inspect nested Git entry {}: {error}",
                    path.display()
                )
            })?;
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let marker = path.join(".git");
            let has_marker = match fs::symlink_metadata(&marker) {
                Ok(_) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => {
                    return Err(format!(
                        "cannot inspect nested Git marker {}: {error}",
                        marker.display()
                    ))
                }
            };
            if has_marker {
                let output = Command::new("git")
                    .arg("-C")
                    .arg(&path)
                    .args(["rev-parse", "--path-format=absolute", "--git-dir"])
                    .output()
                    .map_err(|error| {
                        format!(
                            "cannot resolve nested Git admin for {}: {error}",
                            path.display()
                        )
                    })?;
                if !output.status.success() {
                    return Err(format!(
                        "cannot resolve nested Git admin for {}: {}",
                        path.display(),
                        String::from_utf8_lossy(&output.stderr).trim()
                    ));
                }
                let git_dir =
                    PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string());
                let relative_root = path.strip_prefix(root).map_err(|error| {
                    format!(
                        "nested Git root {} escaped {}: {error}",
                        path.display(),
                        root.display()
                    )
                })?;
                out.push(NestedGitAdmin {
                    relative_root: relative_root.to_path_buf(),
                    git_dir,
                });
            }
            // Initialized submodules may themselves contain initialized
            // submodules. Recurse through their worktree, skipping only the
            // `.git` marker/admin entry above.
            visit(root, &path, out)?;
        }
        Ok(())
    }

    let mut out = Vec::new();
    visit(root, root, &mut out)?;
    out.sort_by(|left, right| {
        left.relative_root
            .components()
            .count()
            .cmp(&right.relative_root.components().count())
            .then_with(|| left.relative_root.cmp(&right.relative_root))
    });
    Ok(out)
}

fn repair_moved_worktree(
    common_git_dir: &Path,
    destination: &Path,
    nested_admins: &[NestedGitAdmin],
) -> Result<(), String> {
    run_worktree_repair(common_git_dir, destination)?;
    for nested in nested_admins {
        let nested_destination = destination.join(&nested.relative_root);
        run_worktree_repair(&nested.git_dir, &nested_destination)?;
        retarget_nested_core_worktree(&nested.git_dir, &nested_destination)?;
    }
    ensure_worktree_not_bare(common_git_dir, destination);
    Ok(())
}

fn retarget_nested_core_worktree(git_dir: &Path, worktree: &Path) -> Result<(), String> {
    let absolute = fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf());
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .arg("--work-tree")
        .arg(&absolute)
        .args(["config", "core.worktree"])
        .arg(&absolute)
        .output()
        .map_err(|error| format!("git config core.worktree failed to start: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "cannot retarget nested core.worktree for {}: {}",
            worktree.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn run_worktree_repair(git_dir: &Path, worktree: &Path) -> Result<(), String> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        // A manually-renamed submodule admin still has core.worktree pointing
        // at the vanished source. Override it for this repair invocation so
        // Git can start and rewrite the durable link to `worktree`.
        .arg("--work-tree")
        .arg(worktree)
        .args(["worktree", "repair"])
        .arg(worktree)
        .output()
        .map_err(|error| format!("git worktree repair failed to start: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git worktree repair failed for {}: {}",
            worktree.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn rollback_failed_fallback(
    common_git_dir: &Path,
    from: &Path,
    to: &Path,
    nested_admins: &[NestedGitAdmin],
    repair_error: String,
) -> LinkedWorktreeMove {
    match destination_occupied(from) {
        Ok(true) => {
            return LinkedWorktreeMove::RecoverableIncomplete {
                current_path: to.to_path_buf(),
                error: format!(
                    "{repair_error}; rollback source path is occupied: {}",
                    from.display()
                ),
            }
        }
        Ok(false) => {}
        Err(error) => {
            return LinkedWorktreeMove::RecoverableIncomplete {
                current_path: to.to_path_buf(),
                error: format!("{repair_error}; rollback source inspection failed: {error}"),
            }
        }
    }
    if let Err(rollback_error) = rename_noreplace(to, from) {
        return LinkedWorktreeMove::RecoverableIncomplete {
            current_path: to.to_path_buf(),
            error: format!(
                "{repair_error}; rollback rename to {} failed: {rollback_error}",
                from.display()
            ),
        };
    }
    if let Err(rollback_repair_error) = repair_moved_worktree(common_git_dir, from, nested_admins) {
        return LinkedWorktreeMove::RecoverableIncomplete {
            current_path: from.to_path_buf(),
            error: format!(
                "{repair_error}; path restored but admin repair failed: {rollback_repair_error}"
            ),
        };
    }
    LinkedWorktreeMove::FailedUnchanged(format!(
        "{repair_error}; source path was restored and repaired"
    ))
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
                // Keep the relocated hub's bare-ness off its worktrees.
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

#[cfg(test)]
mod adoption_tests {
    use super::*;
    use std::cell::Cell;

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[test]
    fn fallback_classifier_is_submodule_specific() {
        assert!(is_submodule_move_refusal(
            "fatal: working trees containing submodules cannot be moved or removed"
        ));
        assert!(!is_submodule_move_refusal(
            "fatal: destination already exists"
        ));
        assert!(!is_submodule_move_refusal("fatal: worktree is locked"));
    }

    #[cfg(unix)]
    #[test]
    fn same_filesystem_guard_compares_source_and_destination_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let destination = tmp.path().join("new/deep/destination");
        std::fs::create_dir_all(&source).unwrap();
        assert_eq!(same_filesystem(&source, &destination), Ok(true));
    }

    #[test]
    fn cross_filesystem_refusal_happens_before_git_is_invoked() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let destination = tmp.path().join("destination");
        fs::create_dir_all(&source).unwrap();
        let git_invoked = Cell::new(false);

        let outcome = move_linked_worktree_preserving_state_with(
            tmp.path(),
            &source,
            &destination,
            |_from, _to| Ok(false),
            |_common, _from, _to| {
                git_invoked.set(true);
                Ok(())
            },
            rename_noreplace,
        );

        assert!(matches!(outcome, LinkedWorktreeMove::FailedUnchanged(_)));
        assert!(
            !git_invoked.get(),
            "Git must not run before the device guard"
        );
        assert!(source.exists());
        assert!(!destination.exists());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_destination_symlink_is_a_collision_before_git() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let destination = tmp.path().join("destination");
        fs::create_dir_all(&source).unwrap();
        symlink("missing-target", &destination).unwrap();

        let outcome = move_linked_worktree_preserving_state(
            &tmp.path().join("not-a-git-dir"),
            &source,
            &destination,
        );
        match outcome {
            LinkedWorktreeMove::FailedUnchanged(error) => {
                assert!(error.contains("destination already exists"), "{error}");
            }
            other => panic!("dangling destination must refuse unchanged: {other:?}"),
        }
        assert!(source.exists());
        assert!(fs::symlink_metadata(&destination).is_ok());
    }

    #[test]
    fn fallback_rename_never_replaces_a_destination_created_after_preflight() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let destination = tmp.path().join("destination");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("source.txt"), "source\n").unwrap();

        let outcome = move_linked_worktree_preserving_state_with(
            tmp.path(),
            &source,
            &destination,
            |_from, _to| Ok(true),
            |_common, _from, _to| {
                Err(
                    "fatal: working trees containing submodules cannot be moved or removed"
                        .to_string(),
                )
            },
            |from, to| {
                fs::create_dir(to)?;
                fs::write(to.join("winner.txt"), "racing destination\n")?;
                rename_noreplace(from, to)
            },
        );

        assert!(matches!(outcome, LinkedWorktreeMove::FailedUnchanged(_)));
        assert_eq!(
            fs::read_to_string(source.join("source.txt")).unwrap(),
            "source\n"
        );
        assert_eq!(
            fs::read_to_string(destination.join("winner.txt")).unwrap(),
            "racing destination\n"
        );
    }

    #[test]
    fn nested_git_discovery_errors_surface_before_fallback_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let destination = tmp.path().join("destination");
        fs::create_dir_all(source.join("broken-child")).unwrap();
        fs::write(
            source.join("broken-child/.git"),
            "gitdir: /definitely/missing/stokd-test-admin\n",
        )
        .unwrap();
        let rename_invoked = Cell::new(false);

        let outcome = move_linked_worktree_preserving_state_with(
            tmp.path(),
            &source,
            &destination,
            |_from, _to| Ok(true),
            |_common, _from, _to| {
                Err(
                    "fatal: working trees containing submodules cannot be moved or removed"
                        .to_string(),
                )
            },
            |_from, _to| {
                rename_invoked.set(true);
                Ok(())
            },
        );

        match outcome {
            LinkedWorktreeMove::FailedUnchanged(error) => {
                assert!(error.contains("broken-child"), "{error}");
            }
            other => panic!("discovery failure must preserve the source: {other:?}"),
        }
        assert!(!rename_invoked.get());
        assert!(source.exists());
        assert!(!destination.exists());
    }

    #[test]
    fn linked_worktree_move_preserves_initialized_submodule_admin_links() {
        let tmp = tempfile::tempdir().unwrap();
        let nested_submodule = tmp.path().join("nested-submodule");
        let submodule = tmp.path().join("submodule");
        let primary = tmp.path().join("primary");
        let source = tmp.path().join("wrong-name");
        let destination = tmp.path().join("task-abc1234-adopted");
        fs::create_dir_all(&nested_submodule).unwrap();
        fs::create_dir_all(&submodule).unwrap();
        fs::create_dir_all(&primary).unwrap();

        git(&nested_submodule, &["init", "-q", "-b", "main"]);
        git(
            &nested_submodule,
            &["config", "user.email", "test@stokd.test"],
        );
        git(&nested_submodule, &["config", "user.name", "Stokd Test"]);
        fs::write(nested_submodule.join("grandchild.txt"), "grandchild\n").unwrap();
        git(&nested_submodule, &["add", "grandchild.txt"]);
        git(&nested_submodule, &["commit", "-qm", "grandchild"]);

        git(&submodule, &["init", "-q", "-b", "main"]);
        git(&submodule, &["config", "user.email", "test@stokd.test"]);
        git(&submodule, &["config", "user.name", "Stokd Test"]);
        fs::write(submodule.join("child.txt"), "child\n").unwrap();
        git(&submodule, &["add", "child.txt"]);
        git(&submodule, &["commit", "-qm", "child"]);
        git(
            &submodule,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                nested_submodule.to_str().unwrap(),
                "nested/child",
            ],
        );
        git(&submodule, &["commit", "-qam", "add nested submodule"]);

        git(&primary, &["init", "-q", "-b", "main"]);
        git(&primary, &["config", "user.email", "test@stokd.test"]);
        git(&primary, &["config", "user.name", "Stokd Test"]);
        fs::write(primary.join("base.txt"), "base\n").unwrap();
        git(&primary, &["add", "base.txt"]);
        git(&primary, &["commit", "-qm", "base"]);
        git(
            &primary,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                submodule.to_str().unwrap(),
                "modules/demo",
            ],
        );
        git(&primary, &["commit", "-qam", "add submodule"]);
        git(
            &primary,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature/submodule-adoption",
                source.to_str().unwrap(),
            ],
        );
        git(
            &source,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "--recursive",
            ],
        );
        let common = PathBuf::from(git(
            &source,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ));

        let outcome = move_linked_worktree_preserving_state(&common, &source, &destination);
        assert!(
            matches!(
                outcome,
                LinkedWorktreeMove::MovedByRenameRepair | LinkedWorktreeMove::MovedByGit
            ),
            "move must complete without reconstruction: {outcome:?}"
        );
        assert!(!source.exists());
        assert!(destination.exists());
        git(&destination, &["status", "--porcelain=v2"]);
        git(
            &destination.join("modules/demo"),
            &["status", "--porcelain=v2"],
        );
        let nested_destination = destination.join("modules/demo/nested/child");
        git(&nested_destination, &["status", "--porcelain=v2"]);
        assert_eq!(
            PathBuf::from(git(&nested_destination, &["rev-parse", "--show-toplevel"])),
            fs::canonicalize(&nested_destination).unwrap()
        );
        let topology = git(&destination, &["worktree", "list", "--porcelain"]);
        assert!(topology.contains(destination.to_str().unwrap()));
        assert!(!topology.contains(source.to_str().unwrap()));
    }
}
