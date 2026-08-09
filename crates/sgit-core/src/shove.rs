//! Pure shove git mechanics: stage / commit / push, conflict detection, and
//! backup-branch creation, behind a [`ConflictResolver`] seam.
//!
//! No agent, cloud, or stokd dependency. Callers supply conflict resolution
//! policy (agent dispatch in stokd, shell/`$EDITOR` in sgit).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

// ── ConflictResolver seam ────────────────────────────────────────────────────

/// Kind of in-progress conflict the resolver is asked to clear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    /// `git pull --rebase` / `git rebase` stopped with unmerged paths.
    Rebase,
    /// `git merge` left unmerged paths.
    Merge,
}

/// Live conflict state passed to a [`ConflictResolver`].
///
/// Files listed in `unmerged_files` still contain conflict markers when the
/// resolver is invoked; the caller verifies markers are gone afterward.
#[derive(Debug, Clone)]
pub struct ConflictContext {
    pub repo_root: PathBuf,
    pub branch: String,
    pub unmerged_files: Vec<String>,
    pub kind: ConflictKind,
}

/// Policy for resolving content conflicts during shove's push/rebase path.
///
/// Implementations must edit conflicted files in place (or abort with `Err`).
/// They must NOT `git commit`, `git push`, or `git rebase --continue` — the
/// pure shove flow owns those steps after verifying markers are cleared.
pub trait ConflictResolver {
    fn resolve(&self, ctx: &ConflictContext) -> Result<(), String>;
}

// ── Options / outcomes ───────────────────────────────────────────────────────

/// Options for the pure [`shove`] flow (no agent, no stokd config).
#[derive(Debug, Clone, Default)]
pub struct ShoveOptions {
    /// Explicit commit message. When `None`, a small fallback is generated.
    pub message: Option<String>,
    /// Print "already up to date" skips (single-repo interactive mode).
    pub verbose_skip: bool,
}

/// Classified outcome of attempting a `git commit` during a shove.
///
/// The key distinction is `NothingToCommit`: git exits non-zero when the index
/// matches HEAD ("nothing to commit, working tree clean"), but that is NOT a
/// failure — shove must fall through to the push phase in that case.
#[derive(Debug, PartialEq, Eq)]
pub enum CommitOutcome {
    /// A new commit was created.
    Committed,
    /// The working tree was already clean at commit time — nothing new to commit.
    NothingToCommit,
    /// A pre-commit hook (lint-staged / husky) rejected the commit — auto-fixable.
    PreCommitFailure,
    /// A genuine commit failure that shove cannot auto-recover.
    HardError,
}

/// Whether shove should attempt to push, given the branch's divergence from its
/// origin upstream.
#[derive(Debug, PartialEq, Eq)]
pub enum PushDecision {
    /// The branch has unpushed local commits, or has no upstream yet — push it.
    Push,
    /// The branch has no local commits ahead of origin — nothing of ours to push.
    UpToDate,
}

/// Captured output of a git subprocess that is NOT streamed to the terminal.
#[derive(Debug, Clone)]
pub struct CapturedGit {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

// ── High-level shove ─────────────────────────────────────────────────────────

/// Stage uncommitted changes, commit when needed, then push (with rebase +
/// [`ConflictResolver`] on divergence). Pure git only — no agent/cloud.
pub fn shove(
    repo_root: &Path,
    opts: &ShoveOptions,
    resolver: &dyn ConflictResolver,
) -> Result<(), String> {
    let branch =
        detect_branch_at(repo_root).ok_or_else(|| "Could not detect current branch".to_string())?;

    if detect_origin_remote_at(repo_root).is_none() {
        return Err(format!(
            "No `origin` remote found for {}. `sgit shove` requires a push target.",
            repo_root.display()
        ));
    }

    // --- Commit phase ---------------------------------------------------------
    let status = git_status_porcelain(repo_root)?;
    if !status.is_empty() {
        let artifact_patterns = detect_artifact_patterns(&status);
        if !artifact_patterns.is_empty() {
            update_gitignore(repo_root, &artifact_patterns)?;
        }

        println!("[{}] Staging all changes...", repo_root.display());
        run_git(repo_root, &["add", "-A"])?;

        let staged_stat = git_staged_stat(repo_root)?;
        if staged_stat.trim().is_empty() {
            println!(
                "[{}] Nothing staged after `git add -A` — all changes are gitignored.",
                repo_root.display()
            );
        } else {
            println!("{staged_stat}");
            let commit_msg = opts
                .message
                .clone()
                .filter(|m| !m.trim().is_empty())
                .unwrap_or_else(|| fallback_commit_message(&staged_stat));
            println!(
                "Committing:\n  {}\n",
                commit_msg.lines().next().unwrap_or("")
            );
            match simple_commit(repo_root, &commit_msg)? {
                CommitOutcome::Committed | CommitOutcome::NothingToCommit => {}
                CommitOutcome::PreCommitFailure => {
                    return Err("git commit failed: pre-commit hook rejected the commit \
                         (sgit has no agent remediation — fix hooks manually and retry)"
                        .to_string());
                }
                CommitOutcome::HardError => {
                    return Err("git commit failed".to_string());
                }
            }
        }
    }

    // --- Push phase -----------------------------------------------------------
    let (ahead, behind, has_upstream) = ahead_behind_vs_origin(repo_root);
    match push_decision(ahead, behind, has_upstream) {
        PushDecision::UpToDate => {
            if opts.verbose_skip {
                println!(
                    "[{}] Already up to date — nothing to commit or push.",
                    repo_root.display()
                );
            }
            Ok(())
        }
        PushDecision::Push => {
            if let Err(err) = push_with_sync(repo_root, &branch, resolver) {
                let (a2, b2, _) = ahead_behind_vs_origin(repo_root);
                return Err(format_push_failure(
                    &branch,
                    ahead.max(a2),
                    behind.max(b2),
                    &err,
                ));
            }
            println!("[{}] Pushed to origin/{branch}.", repo_root.display());
            Ok(())
        }
    }
}

/// Push `branch` to origin. On non-fast-forward divergence: snapshot backup
/// branches, `pull --rebase`, invoke `resolver` when content conflicts remain,
/// then push again.
pub fn push_with_sync(
    repo_root: &Path,
    branch: &str,
    resolver: &dyn ConflictResolver,
) -> Result<(), String> {
    match run_git_with_stderr(repo_root, &["push", "--set-upstream", "origin", branch]) {
        Ok(_) => Ok(()),
        Err(combined) if is_divergence_error(&combined) => {
            println!("Remote has diverged. Creating safety backups, then rebasing local commits on top of remote...");
            create_shove_backup_branches(repo_root, branch)?;
            if run_git(repo_root, &["pull", "--rebase", "origin", branch]).is_err() {
                resolve_rebase_conflicts(repo_root, branch, resolver)?;
            }
            run_git(repo_root, &["push", "--set-upstream", "origin", branch])?;
            Ok(())
        }
        Err(error) => Err(format!("git push failed: {error}")),
    }
}

/// Snapshot local HEAD and `origin/<branch>` (best-effort fetch first) as
/// backup branches so a conflicting rebase cannot lose either side.
pub fn create_shove_backup_branches(repo_root: &Path, branch: &str) -> Result<(), String> {
    let stamp = backup_stamp();
    let (local_backup, remote_backup) = shove_backup_branch_names(branch, &stamp);
    // Local side first (always available).
    run_git(repo_root, &["branch", "-f", &local_backup, "HEAD"])?;
    // Remote side — fetch then point at origin/branch when present.
    let _ = run_git(repo_root, &["fetch", "origin", branch]);
    let remote_ref = format!("origin/{branch}");
    match run_git(repo_root, &["rev-parse", "--verify", &remote_ref]) {
        Ok(()) => {
            run_git(repo_root, &["branch", "-f", &remote_backup, &remote_ref])?;
        }
        Err(_) => {
            // No remote tip yet — still record a local-only backup name note.
            println!(
                "Safety snapshots: {local_backup} (local); remote tip {remote_ref} unavailable."
            );
            return Ok(());
        }
    }
    println!("Safety snapshots: {local_backup} (local), {remote_backup} (origin/{branch}).");
    Ok(())
}

/// Deterministic, distinct backup-branch names for BOTH sides of a shove
/// rebase so conflicting resolution can never lose either side.
pub fn shove_backup_branch_names(branch: &str, stamp: &str) -> (String, String) {
    let safe = branch.replace('/', "-");
    (
        format!("sgit-shove-backup/{safe}/{stamp}-local"),
        format!("sgit-shove-backup/{safe}/{stamp}-remote"),
    )
}

fn backup_stamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

/// Parse unmerged paths, dispatch the resolver when real conflict markers are
/// present, verify markers are gone, then `git add -A` + `git rebase --continue`.
pub fn resolve_rebase_conflicts(
    repo_root: &Path,
    branch: &str,
    resolver: &dyn ConflictResolver,
) -> Result<(), String> {
    let name_only = run_git_captured(repo_root, &["diff", "--name-only", "--diff-filter=U"]);
    let files = parse_unmerged_paths(&name_only.stdout);
    if files.is_empty() {
        return Ok(());
    }
    let has_markers = files.iter().any(|file| {
        let content = std::fs::read_to_string(repo_root.join(file)).unwrap_or_default();
        contains_conflict_markers(&content)
    });
    if !has_markers {
        // No content markers — stage what the rebase produced and continue.
        run_git(repo_root, &["add", "-A"])?;
        run_git_rebase_continue(repo_root)?;
        return Ok(());
    }
    println!(
        "Rebase hit conflicts in {} file(s); invoking ConflictResolver...",
        files.len()
    );
    resolver.resolve(&ConflictContext {
        repo_root: repo_root.to_path_buf(),
        branch: branch.to_string(),
        unmerged_files: files.clone(),
        kind: ConflictKind::Rebase,
    })?;

    for file in &files {
        let content = std::fs::read_to_string(repo_root.join(file)).unwrap_or_default();
        if contains_conflict_markers(&content) {
            return Err(format!(
                "conflict markers still present in {file} after resolution; the rebase is left in place so both sides are preserved"
            ));
        }
    }
    run_git(repo_root, &["add", "-A"])?;
    run_git_rebase_continue(repo_root)?;
    Ok(())
}

// ── Unmerged index stages (modify/delete-safe conflict handling) ─────────────

/// How git's index stages classify one unmerged path. Stage data is the only
/// reliable signal for marker-less structural conflicts such as
/// modify/delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnmergedKind {
    BothModified,
    DeletedByThem,
    DeletedByUs,
    BothAdded,
    AddedByUs,
    AddedByThem,
    Other,
}

impl UnmergedKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::BothModified => "both-modified",
            Self::DeletedByThem => "modify/delete",
            Self::DeletedByUs => "delete/modify",
            Self::BothAdded => "add/add",
            Self::AddedByUs => "added-by-us",
            Self::AddedByThem => "added-by-them",
            Self::Other => "other",
        }
    }

    pub fn is_structural(self) -> bool {
        !matches!(self, Self::BothModified)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmergedEntry {
    pub path: String,
    pub kind: UnmergedKind,
}

/// Parse `git ls-files -u` into one classified entry per path, preserving
/// first-seen order. Each input line is `<mode> <sha> <stage>\t<path>`.
pub fn parse_unmerged_entries(ls_files_u: &str) -> Vec<UnmergedEntry> {
    let mut order = Vec::new();
    let mut stages: std::collections::HashMap<String, [bool; 3]> = std::collections::HashMap::new();
    for line in ls_files_u.lines() {
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        let path = path.trim_end_matches('\r');
        let Some(stage) = meta
            .split_whitespace()
            .next_back()
            .and_then(|value| value.parse::<usize>().ok())
        else {
            continue;
        };
        if path.is_empty() || !(1..=3).contains(&stage) {
            continue;
        }
        let seen = stages.entry(path.to_string()).or_insert_with(|| {
            order.push(path.to_string());
            [false; 3]
        });
        seen[stage - 1] = true;
    }
    order
        .into_iter()
        .map(|path| {
            let kind = match stages.get(&path).copied().unwrap_or([false; 3]) {
                [true, true, true] => UnmergedKind::BothModified,
                [true, true, false] => UnmergedKind::DeletedByThem,
                [true, false, true] => UnmergedKind::DeletedByUs,
                [false, true, true] => UnmergedKind::BothAdded,
                [false, true, false] => UnmergedKind::AddedByUs,
                [false, false, true] => UnmergedKind::AddedByThem,
                _ => UnmergedKind::Other,
            };
            UnmergedEntry { path, kind }
        })
        .collect()
}

pub fn read_unmerged_entries(repo_root: &Path) -> Vec<UnmergedEntry> {
    let output = run_git_captured(repo_root, &["-c", "core.quotePath=false", "ls-files", "-u"]);
    parse_unmerged_entries(&output.stdout)
}

pub fn unmerged_entry_paths(entries: &[UnmergedEntry]) -> Vec<String> {
    entries.iter().map(|entry| entry.path.clone()).collect()
}

/// Marker-bearing content and every structural conflict require an explicit
/// resolver decision. A marker-less ordinary both-modified entry may be staged
/// directly when the rebase already materialized its resolution.
pub fn conflict_round_needs_resolver(entries: &[UnmergedEntry], any_markers: bool) -> bool {
    any_markers || entries.iter().any(|entry| entry.kind.is_structural())
}

pub fn summarize_unmerged_kinds(entries: &[UnmergedEntry]) -> String {
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    for entry in entries {
        let label = entry.kind.label();
        match counts.iter_mut().find(|(known, _)| *known == label) {
            Some((_, count)) => *count += 1,
            None => counts.push((label, 1)),
        }
    }
    counts
        .into_iter()
        .map(|(label, count)| format!("{count} {label}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageAction {
    Add,
    Remove,
}

pub fn stage_action_for(path_exists_on_disk: bool) -> StageAction {
    if path_exists_on_disk {
        StageAction::Add
    } else {
        StageAction::Remove
    }
}

fn path_present(repo_root: &Path, path: &str) -> bool {
    repo_root.join(path).symlink_metadata().is_ok()
}

pub fn any_conflict_markers_on_disk(repo_root: &Path, files: &[String]) -> bool {
    files.iter().any(|file| {
        std::fs::read_to_string(repo_root.join(file))
            .map(|content| contains_conflict_markers(&content))
            .unwrap_or(false)
    })
}

pub fn verify_conflict_markers_cleared(
    repo_root: &Path,
    entries: &[UnmergedEntry],
) -> Result<(), String> {
    for entry in entries {
        if !path_present(repo_root, &entry.path) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(repo_root.join(&entry.path)) else {
            continue;
        };
        if contains_conflict_markers(&content) {
            return Err(format!(
                "conflict markers still present in {} after resolution; the operation is left in place so both sides are preserved",
                entry.path
            ));
        }
    }
    Ok(())
}

/// Stage exactly the paths from this conflict round. A path the resolver left
/// absent is recorded as a deletion; blanket `git add -A` would resurrect it
/// and stage unrelated working-tree state.
pub fn stage_conflict_resolution(
    repo_root: &Path,
    entries: &[UnmergedEntry],
) -> Result<(), String> {
    for entry in entries {
        match stage_action_for(path_present(repo_root, &entry.path)) {
            StageAction::Add => run_git(repo_root, &["add", "--", &entry.path])?,
            StageAction::Remove => {
                let removed = run_git_captured(repo_root, &["rm", "-f", "--", &entry.path]);
                if !removed.success {
                    run_git(repo_root, &["rm", "--cached", "-f", "--", &entry.path])?;
                }
            }
        }
    }
    Ok(())
}

pub fn verify_conflict_staged(repo_root: &Path, entries: &[UnmergedEntry]) -> Result<(), String> {
    for entry in entries {
        let unmerged = run_git_captured(repo_root, &["ls-files", "-u", "--", &entry.path]);
        if unmerged.success && !unmerged.stdout.trim().is_empty() {
            return Err(format!(
                "{} is still unmerged after staging the resolution",
                entry.path
            ));
        }
        if path_present(repo_root, &entry.path) {
            continue;
        }
        let indexed = run_git_captured(repo_root, &["ls-files", "--", &entry.path]);
        if indexed.success && !indexed.stdout.trim().is_empty() {
            return Err(format!(
                "{} was resolved as deleted but is still present in the index (staging it would resurrect it)",
                entry.path
            ));
        }
    }
    Ok(())
}

// ── Pure decision helpers ────────────────────────────────────────────────────

/// Classify a `git commit` result into a [`CommitOutcome`].
pub fn classify_commit_outcome(success: bool, combined_output: &str) -> CommitOutcome {
    if success {
        return CommitOutcome::Committed;
    }
    if combined_output.contains("nothing to commit")
        && combined_output.contains("working tree clean")
    {
        return CommitOutcome::NothingToCommit;
    }
    if is_pre_commit_failure(combined_output) {
        return CommitOutcome::PreCommitFailure;
    }
    CommitOutcome::HardError
}

/// Decide whether shove should push. `behind` does not gate the decision.
pub fn push_decision(ahead: usize, behind: usize, has_upstream: bool) -> PushDecision {
    let _ = behind;
    if !has_upstream || ahead > 0 {
        PushDecision::Push
    } else {
        PushDecision::UpToDate
    }
}

/// Parse `git rev-list --left-right --count @{upstream}...HEAD` output.
/// Returns `(ahead, behind)`.
pub fn parse_ahead_behind(rev_list_left_right_count: &str) -> (usize, usize) {
    let mut parts = rev_list_left_right_count.split_whitespace();
    let behind = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let ahead = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (ahead, behind)
}

/// Human-readable report for a push that could not complete on a diverged branch.
pub fn format_push_failure(branch: &str, ahead: usize, behind: usize, err: &str) -> String {
    format!(
        "Could not push {branch} to origin: your branch and origin/{branch} have diverged \
         ({ahead} local commit(s) not on origin, {behind} origin commit(s) not local), and shove \
         could not complete the rebase/resolve. Nothing was pushed.\n{err}"
    )
}

pub fn is_divergence_error(output: &str) -> bool {
    output.contains("rejected")
        || output.contains("non-fast-forward")
        || output.contains("fetch first")
        || output.contains("Updates were rejected")
}

pub fn is_pre_commit_failure(output: &str) -> bool {
    output.contains("pre-commit")
        || output.contains("husky")
        || output.contains("lint-staged")
        || output.contains("ERR_PNPM_RECURSIVE_EXEC_FIRST_FAIL")
}

/// Parse `git diff --name-only --diff-filter=U` output into an ordered,
/// de-duplicated list of conflicted paths.
pub fn parse_unmerged_paths(name_only: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for line in name_only.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
            paths.push(trimmed.to_string());
        }
    }
    paths
}

/// True when `content` still contains any git conflict marker line.
/// A real conflict requires all three markers: `<<<<<<<`, `=======` (exactly 7+),
/// and `>>>>>>>`.
pub fn contains_conflict_markers(content: &str) -> bool {
    let has_open = content.lines().any(|l| l.starts_with("<<<<<<<"));
    let has_separator = content.lines().any(|l| l.starts_with("======="));
    let has_close = content.lines().any(|l| l.starts_with(">>>>>>>"));
    has_open && has_separator && has_close
}

// ── Git subprocess helpers ───────────────────────────────────────────────────

pub fn run_git_captured(path: &Path, args: &[&str]) -> CapturedGit {
    let mut full_args = vec!["-C", path.to_str().unwrap_or(".")];
    full_args.extend_from_slice(args);
    match Command::new("git").args(&full_args).output() {
        Ok(output) => CapturedGit {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        },
        Err(e) => CapturedGit {
            success: false,
            stdout: String::new(),
            stderr: format!("git {}: {e}", args.join(" ")),
        },
    }
}

pub fn run_git(path: &Path, args: &[&str]) -> Result<(), String> {
    let mut full_args = vec!["-C", path.to_str().unwrap_or(".")];
    full_args.extend_from_slice(args);
    let status = Command::new("git")
        .args(&full_args)
        .status()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "`git {}` exited with status {}",
            args.join(" "),
            status.code().unwrap_or(-1)
        ))
    }
}

pub fn run_git_with_stderr(path: &Path, args: &[&str]) -> Result<(), String> {
    let mut full_args = vec!["-C", path.to_str().unwrap_or(".")];
    full_args.extend_from_slice(args);
    let output = Command::new("git")
        .args(&full_args)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        Err(format!("{stdout}{stderr}"))
    }
}

pub fn run_git_rebase_continue(path: &Path) -> Result<(), String> {
    let mut full_args = vec!["-C", path.to_str().unwrap_or(".")];
    full_args.extend_from_slice(&["rebase", "--continue"]);
    let status = Command::new("git")
        .args(&full_args)
        .env("GIT_EDITOR", "true")
        .status()
        .map_err(|e| format!("git rebase --continue: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "`git rebase --continue` exited with status {}",
            status.code().unwrap_or(-1)
        ))
    }
}

pub fn detect_origin_remote_at(path: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let remote = String::from_utf8(output.stdout).ok()?;
    let remote = remote.trim();
    if remote.is_empty() {
        None
    } else {
        Some(remote.to_string())
    }
}

pub fn detect_branch_at(path: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let branch = String::from_utf8(output.stdout).ok()?;
    let branch = branch.trim();
    if branch.is_empty() {
        None
    } else {
        Some(branch.to_string())
    }
}

/// Query `(ahead, behind, has_upstream)` for the current HEAD against its origin
/// upstream. No network fetch.
pub fn ahead_behind_vs_origin(repo_root: &Path) -> (usize, usize, bool) {
    let upstream = run_git_captured(
        repo_root,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    );
    if !upstream.success {
        return (0, 0, false);
    }
    let counts = run_git_captured(
        repo_root,
        &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"],
    );
    if !counts.success {
        return (0, 0, true);
    }
    let (ahead, behind) = parse_ahead_behind(&counts.stdout);
    (ahead, behind, true)
}

pub fn git_status_porcelain(path: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .args(["-C", path.to_str().unwrap_or("."), "status", "--porcelain"])
        .output()
        .map_err(|e| format!("git status failed: {e}"))?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn git_staged_stat(path: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .args([
            "-C",
            path.to_str().unwrap_or("."),
            "diff",
            "--staged",
            "--stat",
        ])
        .output()
        .map_err(|e| format!("git diff --staged --stat failed: {e}"))?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn git_staged_diff(path: &Path) -> String {
    let output = Command::new("git")
        .args(["-C", path.to_str().unwrap_or("."), "diff", "--staged"])
        .output()
        .ok();
    output
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

fn simple_commit(repo_root: &Path, commit_msg: &str) -> Result<CommitOutcome, String> {
    let result = run_git_captured(repo_root, &["commit", "-m", commit_msg]);
    let combined = format!("{}{}", result.stdout, result.stderr);
    let outcome = classify_commit_outcome(result.success, &combined);
    match outcome {
        CommitOutcome::Committed => {
            for line in result.stdout.lines() {
                println!("{line}");
            }
            Ok(CommitOutcome::Committed)
        }
        CommitOutcome::NothingToCommit => Ok(CommitOutcome::NothingToCommit),
        CommitOutcome::PreCommitFailure => Ok(CommitOutcome::PreCommitFailure),
        CommitOutcome::HardError => Err(format!("git commit failed:\n{combined}")),
    }
}

fn fallback_commit_message(staged_stat: &str) -> String {
    let file_count = staged_stat.lines().filter(|l| !l.trim().is_empty()).count();
    // `--stat` ends with a summary line like " N files changed, ..." — prefer that.
    let summary = staged_stat
        .lines()
        .rev()
        .find(|l| l.contains("changed"))
        .map(|l| l.trim().to_string());
    match (file_count, summary) {
        (0, _) => "shove: update repository".to_string(),
        (_, Some(s)) => format!("shove: {s}"),
        (n, None) => format!("shove: update {n} files"),
    }
}

// ── Artifact / gitignore helpers ─────────────────────────────────────────────

/// Returns the set of `.gitignore` patterns that should be added for untracked
/// files/dirs that look like build artifacts or generated dev output.
pub fn detect_artifact_patterns(status: &str) -> Vec<String> {
    const ARTIFACT_DIRS: &[(&str, &str)] = &[
        ("node_modules", "node_modules/"),
        ("dist", "dist/"),
        ("build", "build/"),
        ("out", "out/"),
        (".next", ".next/"),
        (".nuxt", ".nuxt/"),
        (".output", ".output/"),
        ("__pycache__", "__pycache__/"),
        (".pytest_cache", ".pytest_cache/"),
        (".mypy_cache", ".mypy_cache/"),
        (".ruff_cache", ".ruff_cache/"),
        ("coverage", "coverage/"),
        (".nyc_output", ".nyc_output/"),
        (".turbo", ".turbo/"),
        (".cache", ".cache/"),
        (".parcel-cache", ".parcel-cache/"),
        (".expo", ".expo/"),
        (".serverless", ".serverless/"),
        ("cdk.out", "cdk.out/"),
        (".terraform", ".terraform/"),
        (".gradle", ".gradle/"),
        (".idea", ".idea/"),
        ("target", "target/"),
        (".docusaurus", ".docusaurus/"),
        (".storybook-out", ".storybook-out/"),
        ("storybook-static", "storybook-static/"),
        (".astro", ".astro/"),
        (".svelte-kit", ".svelte-kit/"),
        ("playwright-report", "playwright-report/"),
        ("test-results", "test-results/"),
    ];

    const ARTIFACT_EXTENSIONS: &[(&str, &str)] = &[
        (".pyc", "*.pyc"),
        (".pyo", "*.pyo"),
        (".class", "*.class"),
        (".o", "*.o"),
        (".a", "*.a"),
        (".so", "*.so"),
        (".dylib", "*.dylib"),
        (".dll", "*.dll"),
        (".exe", "*.exe"),
        (".pid", "*.pid"),
    ];

    const ARTIFACT_FILES: &[&str] = &[
        ".DS_Store",
        "Thumbs.db",
        "ehthumbs.db",
        "Desktop.ini",
        ".env.local",
        ".env.development.local",
        ".env.test.local",
        ".env.production.local",
    ];

    let mut patterns: HashSet<String> = HashSet::new();

    for line in status.lines() {
        if !line.starts_with("??") {
            continue;
        }
        let path = line[3..].trim().trim_matches('"');
        let first = path.split('/').next().unwrap_or(path);
        let first_lower = first.to_lowercase();

        for (dir, pattern) in ARTIFACT_DIRS {
            if first_lower == *dir {
                patterns.insert(pattern.to_string());
                break;
            }
        }

        if !path.ends_with('/') {
            let path_lower = path.to_lowercase();
            for (ext, pattern) in ARTIFACT_EXTENSIONS {
                if path_lower.ends_with(ext) {
                    patterns.insert(pattern.to_string());
                    break;
                }
            }

            let filename = path.split('/').next_back().unwrap_or(path);
            for &exact in ARTIFACT_FILES {
                if filename == exact {
                    patterns.insert(exact.to_string());
                    break;
                }
            }
        }
    }

    let mut sorted: Vec<String> = patterns.into_iter().collect();
    sorted.sort();
    sorted
}

pub fn update_gitignore(repo_root: &Path, new_patterns: &[String]) -> Result<(), String> {
    let gitignore_path = repo_root.join(".gitignore");
    let existing = if gitignore_path.exists() {
        std::fs::read_to_string(&gitignore_path)
            .map_err(|e| format!("Failed to read .gitignore: {e}"))?
    } else {
        String::new()
    };

    let existing_lines: HashSet<&str> = existing.lines().map(|l| l.trim()).collect();
    let to_add: Vec<&str> = new_patterns
        .iter()
        .map(String::as_str)
        .filter(|p| !existing_lines.contains(p))
        .collect();

    if to_add.is_empty() {
        return Ok(());
    }

    println!(
        "[gitignore] Auto-ignoring {} artifact pattern(s): {}",
        to_add.len(),
        to_add.join(", ")
    );

    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str("\n# build/dev artifacts (auto-added by shove)\n");
    for pattern in &to_add {
        content.push_str(pattern);
        content.push('\n');
    }

    std::fs::write(&gitignore_path, content)
        .map_err(|e| format!("Failed to write .gitignore: {e}"))?;

    Ok(())
}

// ── Unit tests (pure helpers) ────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_commit_outcome_classifies_states() {
        assert_eq!(
            classify_commit_outcome(true, "ok"),
            CommitOutcome::Committed
        );
        assert_eq!(
            classify_commit_outcome(
                false,
                "On branch main\nnothing to commit, working tree clean\n"
            ),
            CommitOutcome::NothingToCommit
        );
        assert_eq!(
            classify_commit_outcome(false, "husky > pre-commit\nlint-staged failed"),
            CommitOutcome::PreCommitFailure
        );
        assert_eq!(
            classify_commit_outcome(false, "fatal: unable to write"),
            CommitOutcome::HardError
        );
    }

    #[test]
    fn push_decision_pushes_unpushed_or_no_upstream() {
        assert_eq!(push_decision(1, 0, true), PushDecision::Push);
        assert_eq!(push_decision(0, 0, false), PushDecision::Push);
        assert_eq!(push_decision(0, 2, true), PushDecision::UpToDate);
        assert_eq!(push_decision(3, 1, true), PushDecision::Push);
    }

    #[test]
    fn parse_ahead_behind_reads_left_right_count() {
        assert_eq!(parse_ahead_behind("2\t5\n"), (5, 2));
        assert_eq!(parse_ahead_behind("0 0"), (0, 0));
    }

    #[test]
    fn format_push_failure_reports_divergence() {
        let msg = format_push_failure("feat", 2, 1, "boom");
        assert!(msg.contains("feat"));
        assert!(msg.contains("2 local"));
        assert!(msg.contains("1 origin"));
        assert!(msg.contains("boom"));
    }

    #[test]
    fn is_divergence_error_detects_rejection() {
        assert!(is_divergence_error("! [rejected] non-fast-forward"));
        assert!(is_divergence_error("Updates were rejected"));
        assert!(is_divergence_error("fetch first"));
        assert!(!is_divergence_error("permission denied"));
    }

    #[test]
    fn parse_unmerged_paths_parses_name_only() {
        let raw = "src/a.rs\nsrc/b.rs\nsrc/a.rs\n\n";
        assert_eq!(
            parse_unmerged_paths(raw),
            vec!["src/a.rs".to_string(), "src/b.rs".to_string()]
        );
    }

    #[test]
    fn parse_unmerged_entries_classifies_structural_conflicts() {
        let entries = parse_unmerged_entries(
            "100644 aaa 1\tboth.txt\n\
             100644 bbb 2\tboth.txt\n\
             100644 ccc 3\tboth.txt\n\
             100644 ddd 1\tdeleted.txt\n\
             100644 eee 2\tdeleted.txt\n\
             100644 fff 2\tdir/a file.txt\n",
        );
        assert_eq!(entries[0].kind, UnmergedKind::BothModified);
        assert_eq!(entries[1].kind, UnmergedKind::DeletedByThem);
        assert_eq!(entries[2].path, "dir/a file.txt");
        assert_eq!(entries[2].kind, UnmergedKind::AddedByUs);
    }

    #[test]
    fn structural_conflict_requires_resolver_without_markers() {
        let text = vec![UnmergedEntry {
            path: "text".into(),
            kind: UnmergedKind::BothModified,
        }];
        let structural = vec![UnmergedEntry {
            path: "deleted".into(),
            kind: UnmergedKind::DeletedByUs,
        }];
        assert!(!conflict_round_needs_resolver(&text, false));
        assert!(conflict_round_needs_resolver(&text, true));
        assert!(conflict_round_needs_resolver(&structural, false));
        assert_eq!(stage_action_for(false), StageAction::Remove);
    }

    #[test]
    fn contains_conflict_markers_detects_and_clears() {
        let conflicted = "fn main() {\n\
<<<<<<< HEAD\n    println!(\"ours\");\n\
=======\n    println!(\"theirs\");\n\
>>>>>>> origin/main\n}\n";
        assert!(contains_conflict_markers(conflicted));
        let resolved = "fn main() {\n    println!(\"ours\");\n    println!(\"theirs\");\n}\n";
        assert!(!contains_conflict_markers(resolved));
        // Markdown heading underline alone is not a conflict.
        assert!(!contains_conflict_markers("Title\n=======\nbody\n"));
    }

    #[test]
    fn shove_backup_branch_names_are_distinct() {
        let (local, remote) = shove_backup_branch_names("feat/x", "1234");
        assert_ne!(local, remote);
        assert!(local.contains("local"));
        assert!(remote.contains("remote"));
        assert!(local.contains("feat-x"));
    }

    #[test]
    fn detect_artifacts_flags_known_dirs() {
        let status = "?? node_modules/foo\n?? dist/out.js\n M src/a.rs\n";
        let p = detect_artifact_patterns(status);
        assert!(p.iter().any(|x| x == "node_modules/"));
        assert!(p.iter().any(|x| x == "dist/"));
    }
}
