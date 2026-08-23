//! Pure shove git mechanics: stage / commit / push, conflict detection, and
//! backup-branch creation, behind a [`ConflictResolver`] seam.
//!
//! No agent, cloud, or stokd dependency. Callers supply conflict resolution
//! policy (agent dispatch in stokd, shell/`$EDITOR` in sgit).

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
        prepare_artifact_exclusions(repo_root)?;

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

/// Explicit push refspec for `branch` (AX-SGIT-BRANCH-UPSTREAM-SELF).
///
/// A source-only refspec (`push origin <branch>`) leaves the DESTINATION to
/// `push.default`. Under `push.default = upstream` — and a branch that inherited
/// `merge = refs/heads/<base>` from the remote-tracking start point it was cut
/// from — that destination resolves to the BASE branch, so pushing a feature
/// branch silently writes onto main. Naming the destination removes the
/// ambiguity regardless of local config. Pure; unit-tested.
pub fn push_refspec(branch: &str) -> String {
    let short = branch
        .trim()
        .strip_prefix("refs/heads/")
        .unwrap_or_else(|| branch.trim());
    format!("HEAD:refs/heads/{short}")
}

/// Push `branch` to origin. On non-fast-forward divergence: snapshot backup
/// branches, `pull --rebase`, invoke `resolver` when content conflicts remain,
/// then push again.
///
/// `--set-upstream` is retained deliberately: combined with the explicit
/// refspec it REPAIRS a branch that already carries a base-pointing upstream
/// from before this fix, so historical branches self-heal on their next push.
pub fn push_with_sync(
    repo_root: &Path,
    branch: &str,
    resolver: &dyn ConflictResolver,
) -> Result<(), String> {
    let refspec = push_refspec(branch);
    match run_git_with_stderr(repo_root, &["push", "--set-upstream", "origin", &refspec]) {
        Ok(_) => Ok(()),
        Err(combined) if is_divergence_error(&combined) => {
            println!("Remote has diverged. Creating safety backups, then rebasing local commits on top of remote...");
            create_shove_backup_branches(repo_root, branch)?;
            if run_git(repo_root, &["pull", "--rebase", "origin", branch]).is_err() {
                resolve_rebase_conflicts(repo_root, branch, resolver)?;
            }
            run_git(repo_root, &["push", "--set-upstream", "origin", &refspec])?;
            Ok(())
        }
        Err(error) => Err(format!("git push failed: {error}")),
    }
}

/// Snapshot local HEAD and `origin/<branch>` (best-effort fetch first) as
/// backup branches so a conflicting rebase cannot lose either side.
pub fn create_shove_backup_branches(repo_root: &Path, branch: &str) -> Result<(), String> {
    create_backup_branches(repo_root, branch, SHOVE_BACKUP_PREFIX).map(|_| ())
}

/// Backup-branch namespace for shove's both-sides snapshots.
pub const SHOVE_BACKUP_PREFIX: &str = "sgit-shove-backup";

/// Snapshot both sides under `prefix`, returning the branch names actually
/// created as `(local, remote)`. `remote` is `None` when `origin/<branch>` does
/// not exist yet — there is nothing on that side that could be lost.
///
/// A best-effort `git fetch origin <branch>` runs first so the remote snapshot
/// records the tip about to be integrated rather than a stale one.
pub fn create_backup_branches(
    repo_root: &Path,
    branch: &str,
    prefix: &str,
) -> Result<(String, Option<String>), String> {
    let stamp = backup_stamp();
    let (local_backup, remote_backup) = backup_branch_names(branch, &stamp, prefix);
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
            return Ok((local_backup, None));
        }
    }
    println!("Safety snapshots: {local_backup} (local), {remote_backup} (origin/{branch}).");
    Ok((local_backup, Some(remote_backup)))
}

/// Deterministic, distinct backup-branch names for BOTH sides of a shove
/// rebase so conflicting resolution can never lose either side.
pub fn shove_backup_branch_names(branch: &str, stamp: &str) -> (String, String) {
    backup_branch_names(branch, stamp, SHOVE_BACKUP_PREFIX)
}

/// Deterministic, distinct `(local, remote)` backup-branch names under `prefix`.
pub fn backup_branch_names(branch: &str, stamp: &str, prefix: &str) -> (String, String) {
    let safe = branch.replace('/', "-");
    (
        format!("{prefix}/{safe}/{stamp}-local"),
        format!("{prefix}/{safe}/{stamp}-remote"),
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

const ARTIFACT_DIRS: &[(&str, &str)] = &[
    // Slash-less by design: worktree dependency installers may create a
    // node_modules symlink, and `node_modules/` would match directories only.
    ("node_modules", "node_modules"),
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

// These names are ecosystem-specific enough to classify below the repository
// root. Ambiguous names such as build/dist/out remain top-level-only so a
// legitimate source path like src/build/main.rs is never hidden.
const NESTED_ARTIFACT_DIRS: &[&str] = &[
    "node_modules",
    ".next",
    ".nuxt",
    ".output",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".nyc_output",
    ".turbo",
    ".parcel-cache",
    ".expo",
    ".serverless",
    "cdk.out",
    ".terraform",
    ".gradle",
    ".idea",
    ".docusaurus",
    ".storybook-out",
    "storybook-static",
    ".astro",
    ".svelte-kit",
    "playwright-report",
    "test-results",
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
    (".pdb", "*.pdb"),
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

/// Result of the pre-stage artifact pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArtifactExclusion {
    /// Ignore patterns discovered from individual candidate paths.
    pub patterns: Vec<String>,
    /// Previously staged additions removed from the index (working files stay).
    pub unstaged_paths: Vec<String>,
}

#[derive(Debug, Default)]
struct ArtifactClassification {
    patterns: Vec<String>,
    paths: HashSet<String>,
}

/// Inventory untracked leaf files and already-staged additions, install ignore
/// patterns, and remove generated additions from the index without deleting
/// their working-tree files. Call this before any blanket `git add -A`.
pub fn prepare_artifact_exclusions(repo_root: &Path) -> Result<ArtifactExclusion, String> {
    // `git status --porcelain` normally collapses an untracked tree to its
    // highest directory. `ls-files --others` gives the leaf paths needed to
    // recognize nested build output such as project/bin/Debug/net10.0/*.dll.
    let untracked = git_nul_paths(
        repo_root,
        &["ls-files", "--others", "--exclude-standard", "-z", "--"],
    )?;
    // A retry after an interrupted shove sees those same files as staged `A`,
    // not untracked. Inspect the index explicitly so ignores can take effect.
    let staged_additions = git_nul_paths(
        repo_root,
        &[
            "diff",
            "--cached",
            "--name-only",
            "--diff-filter=A",
            "-z",
            "--",
        ],
    )?;

    let mut candidates = untracked;
    candidates.extend(staged_additions.iter().cloned());
    candidates.sort();
    candidates.dedup();

    let classified = classify_artifact_paths(&candidates);
    if classified.patterns.is_empty() {
        return Ok(ArtifactExclusion::default());
    }

    let staged_artifacts: Vec<String> = staged_additions
        .into_iter()
        .filter(|path| classified.paths.contains(path))
        .collect();
    ensure_worktree_copies(repo_root, &staged_artifacts)?;
    validate_gitignore_patterns(&classified.patterns)?;

    let gitignore_before = read_gitignore_bytes(repo_root)?;
    update_gitignore(repo_root, &classified.patterns)?;
    if let Err(error) = ensure_artifacts_are_ignored(repo_root, &classified.paths) {
        restore_gitignore(repo_root, gitignore_before)?;
        return Err(error);
    }

    if !staged_artifacts.is_empty() {
        remove_index_paths(repo_root, &staged_artifacts)?;
        println!(
            "[gitignore] Removed {} staged artifact file(s) from the index; working files were preserved.",
            staged_artifacts.len()
        );
    }

    Ok(ArtifactExclusion {
        patterns: classified.patterns,
        unstaged_paths: staged_artifacts,
    })
}

/// Returns the set of `.gitignore` patterns for untracked or index-added
/// porcelain entries. Kept as a pure compatibility helper for land/task paths;
/// shove itself uses [`prepare_artifact_exclusions`] so it sees leaf paths and
/// can repair an interrupted staging pass.
pub fn detect_artifact_patterns(status: &str) -> Vec<String> {
    let paths: Vec<String> = status
        .lines()
        .filter_map(|line| {
            let bytes = line.as_bytes();
            if bytes.len() < 4 || !(line.starts_with("??") || bytes[0] == b'A') {
                return None;
            }
            Some(line[3..].trim().trim_matches('"').to_string())
        })
        .collect();
    classify_artifact_paths(&paths).patterns
}

fn classify_artifact_paths(paths: &[String]) -> ArtifactClassification {
    let mut patterns = HashSet::new();
    for path in paths {
        if let Some(pattern) = artifact_pattern_for_path(path) {
            patterns.insert(pattern);
        }
    }

    let mut sorted_patterns: Vec<String> = patterns.into_iter().collect();
    sorted_patterns.sort();
    let artifact_paths = paths
        .iter()
        .filter(|path| {
            sorted_patterns
                .iter()
                .any(|pattern| artifact_pattern_matches_path(pattern, path))
        })
        .cloned()
        .collect();

    ArtifactClassification {
        patterns: sorted_patterns,
        paths: artifact_paths,
    }
}

fn artifact_pattern_for_path(path: &str) -> Option<String> {
    let normalized = path.trim_end_matches('/');
    let components: Vec<&str> = normalized
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    if components.is_empty() {
        return None;
    }

    for (index, component) in components.iter().enumerate() {
        let lower = component.to_ascii_lowercase();
        if let Some((_, pattern)) = ARTIFACT_DIRS.iter().find(|(dir, _)| lower == *dir) {
            if *pattern == "node_modules" {
                return Some((*pattern).to_string());
            }
            let is_directory = index + 1 < components.len() || path.ends_with('/');
            let nested_is_recognized = index == 0 || NESTED_ARTIFACT_DIRS.contains(&lower.as_str());
            if is_directory && nested_is_recognized {
                return Some(if index == 0 {
                    format!("/{}/", pattern.trim_end_matches('/'))
                } else {
                    scoped_directory_pattern(&components, index)
                });
            }
        }
    }

    // `obj` is unambiguously generated in .NET projects. Anchor the ignore to
    // the observed project path instead of adding a repo-wide `obj/` rule.
    if let Some(index) = components
        .iter()
        .position(|component| component.eq_ignore_ascii_case("obj"))
    {
        if index + 1 < components.len() || path.ends_with('/') {
            return Some(scoped_directory_pattern(&components, index));
        }
    }

    // `bin` is a common source-script directory, so only classify it when the
    // observed leaf path carries a .NET build signal. Once one signal discovers
    // the scoped bin root, the second classification pass covers every sibling.
    if let Some(index) = components
        .iter()
        .position(|component| component.eq_ignore_ascii_case("bin"))
    {
        let suffix = &components[index + 1..];
        if suffix.iter().any(|component| dotnet_path_signal(component)) {
            return Some(scoped_directory_pattern(&components, index));
        }
    }

    let path_lower = normalized.to_ascii_lowercase();
    if let Some((_, pattern)) = ARTIFACT_EXTENSIONS
        .iter()
        .find(|(extension, _)| path_lower.ends_with(*extension))
    {
        return Some((*pattern).to_string());
    }

    let filename = components.last().copied().unwrap_or(normalized);
    ARTIFACT_FILES
        .iter()
        .find(|exact| filename == **exact)
        .map(|exact| (*exact).to_string())
}

fn scoped_directory_pattern(components: &[&str], index: usize) -> String {
    let escaped = components[..=index]
        .iter()
        .map(|component| escape_gitignore_component(component))
        .collect::<Vec<_>>()
        .join("/");
    if index == 0 {
        format!("/{escaped}/")
    } else {
        format!("{escaped}/")
    }
}

fn escape_gitignore_component(component: &str) -> String {
    let mut escaped = String::with_capacity(component.len());
    for character in component.chars() {
        if matches!(character, '\\' | '*' | '?' | '[' | ']' | '!' | '#' | ' ') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn unescape_gitignore_literal(pattern: &str) -> String {
    let mut literal = String::with_capacity(pattern.len());
    let mut characters = pattern.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            if let Some(escaped) = characters.next() {
                literal.push(escaped);
            } else {
                literal.push(character);
            }
        } else {
            literal.push(character);
        }
    }
    literal
}

fn dotnet_path_signal(component: &str) -> bool {
    let lower = component.to_ascii_lowercase();
    lower.ends_with(".dll")
        || lower.ends_with(".exe")
        || lower.ends_with(".pdb")
        || lower.ends_with(".deps.json")
        || lower.ends_with(".runtimeconfig.json")
}

fn artifact_pattern_matches_path(pattern: &str, path: &str) -> bool {
    let normalized = path.trim_end_matches('/');
    let lower = normalized.to_ascii_lowercase();
    if let Some(extension) = pattern.strip_prefix('*') {
        return lower.ends_with(&extension.to_ascii_lowercase());
    }
    if pattern.ends_with('/') {
        let directory = unescape_gitignore_literal(pattern.trim_end_matches('/'));
        if let Some(root_directory) = directory.strip_prefix('/') {
            return normalized == root_directory
                || normalized.starts_with(&format!("{root_directory}/"));
        }
        if directory.contains('/') {
            return normalized == directory || normalized.starts_with(&format!("{directory}/"));
        }
        return normalized
            .split('/')
            .any(|component| component.eq_ignore_ascii_case(&directory));
    }
    if pattern == "node_modules" {
        return normalized
            .split('/')
            .any(|component| component.eq_ignore_ascii_case(pattern));
    }
    normalized
        .split('/')
        .next_back()
        .is_some_and(|filename| filename == pattern)
}

fn git_nul_paths(repo_root: &Path, args: &[&str]) -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .map_err(|error| format!("git {} failed: {error}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            String::from_utf8(path.to_vec())
                .map_err(|_| "shove cannot safely classify a non-UTF-8 Git path".to_string())
        })
        .collect()
}

fn ensure_worktree_copies(repo_root: &Path, paths: &[String]) -> Result<(), String> {
    let missing: Vec<&str> = paths
        .iter()
        .filter_map(|path| {
            std::fs::symlink_metadata(repo_root.join(path))
                .is_err()
                .then_some(path.as_str())
        })
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "refusing to unstage artifact additions with no working-tree copy: {}",
            missing.join(", ")
        ))
    }
}

fn validate_gitignore_patterns(patterns: &[String]) -> Result<(), String> {
    if let Some(pattern) = patterns
        .iter()
        .find(|pattern| pattern.contains('\n') || pattern.contains('\r'))
    {
        Err(format!(
            "refusing to write a .gitignore pattern containing a line break: {pattern:?}"
        ))
    } else {
        Ok(())
    }
}

fn read_gitignore_bytes(repo_root: &Path) -> Result<Option<Vec<u8>>, String> {
    let path = repo_root.join(".gitignore");
    match std::fs::read(&path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Failed to snapshot .gitignore: {error}")),
    }
}

fn restore_gitignore(repo_root: &Path, previous: Option<Vec<u8>>) -> Result<(), String> {
    let path = repo_root.join(".gitignore");
    match previous {
        Some(content) => {
            if std::fs::read(&path).ok().as_deref() != Some(content.as_slice()) {
                std::fs::write(&path, content)
                    .map_err(|error| format!("failed to restore .gitignore: {error}"))?;
            }
        }
        None if path.exists() => {
            std::fs::remove_file(&path)
                .map_err(|error| format!("failed to remove generated .gitignore: {error}"))?;
        }
        None => {}
    }
    Ok(())
}

fn ensure_artifacts_are_ignored(
    repo_root: &Path,
    artifact_paths: &HashSet<String>,
) -> Result<(), String> {
    if artifact_paths.is_empty() {
        return Ok(());
    }

    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["check-ignore", "--no-index", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to verify artifact ignore rules: {error}"))?;

    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to open git check-ignore stdin".to_string())?;
        for path in artifact_paths {
            stdin
                .write_all(path.as_bytes())
                .and_then(|_| stdin.write_all(&[0]))
                .map_err(|error| format!("failed to verify artifact path: {error}"))?;
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|error| format!("failed waiting for git check-ignore: {error}"))?;
    // check-ignore uses 1 for "none matched", which is a valid verification
    // result handled below. Any other non-zero code is an execution failure.
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(format!(
            "failed to verify artifact ignore rules:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let ignored: HashSet<String> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            String::from_utf8(path.to_vec())
                .map_err(|_| "git check-ignore returned a non-UTF-8 path".to_string())
        })
        .collect::<Result<_, _>>()?;
    let mut missing: Vec<&str> = artifact_paths
        .iter()
        .filter(|path| !ignored.contains(*path))
        .map(String::as_str)
        .collect();
    missing.sort();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            ".gitignore did not exclude these detected artifacts; refusing blanket staging: {}",
            missing.into_iter().take(10).collect::<Vec<_>>().join(", ")
        ))
    }
}

fn remove_index_paths(repo_root: &Path, paths: &[String]) -> Result<(), String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["update-index", "--force-remove", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to unstage artifact files: {error}"))?;

    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to open git update-index stdin".to_string())?;
        for path in paths {
            stdin
                .write_all(path.as_bytes())
                .and_then(|_| stdin.write_all(&[0]))
                .map_err(|error| format!("failed to send artifact path to git: {error}"))?;
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|error| format!("failed waiting for git update-index: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "failed to unstage artifact files:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
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
        assert!(p.iter().any(|x| x == "node_modules"));
        assert!(p.iter().any(|x| x == "/dist/"));
    }

    #[test]
    fn detect_artifacts_finds_nested_dotnet_output_without_hiding_source_bin() {
        let status = "\
?? tools/dotnet/status-push/bin/Debug/net10.0/app.dll\n\
?? tools/dotnet/status-push/bin/Debug/net10.0/app.deps.json\n\
?? tools/dotnet/status-push/obj/Debug/net10.0/App.AssemblyInfo.cs\n\
?? bin/Debug/net10.0/root-app.dll\n\
?? obj/Debug/net10.0/Root.AssemblyInfo.cs\n\
?? build/app.js\n\
?? packages/web/build/assets/app.js\n\
?? src/build/main.rs\n\
?? bin/release/deploy.sh\n\
?? bin/net8/deploy.sh\n\
?? tools/bin/net8/deploy.sh\n\
?? bin/release.sh\n";
        let patterns = detect_artifact_patterns(status);
        assert!(patterns.contains(&"tools/dotnet/status-push/bin/".to_string()));
        assert!(patterns.contains(&"tools/dotnet/status-push/obj/".to_string()));
        assert!(patterns.contains(&"/bin/".to_string()));
        assert!(patterns.contains(&"/obj/".to_string()));
        assert!(patterns.contains(&"/build/".to_string()));
        assert!(!patterns.contains(&"packages/web/build/".to_string()));
        assert!(!patterns.contains(&"src/build/".to_string()));
        assert!(!patterns.contains(&"build/".to_string()));
        assert!(!patterns.contains(&"bin/".to_string()));
    }

    #[test]
    fn prepare_artifacts_repairs_interrupted_staging_and_preserves_worktree() {
        let repo = tempfile::tempdir().expect("temp repo");
        git_test(repo.path(), &["init"]);
        git_test(repo.path(), &["config", "user.name", "Shove Test"]);
        git_test(
            repo.path(),
            &["config", "user.email", "shove-test@example.com"],
        );
        write_test_file(repo.path(), "README.md", "base\n");
        git_test(repo.path(), &["add", "README.md"]);
        git_test(repo.path(), &["commit", "-m", "base"]);

        let project = "tools/dotnet/Status [Push]";
        let source = format!("{project}/Program.cs");
        let dll = format!("{project}/bin/Debug/net10.0/app.dll");
        let deps = format!("{project}/bin/Debug/net10.0/app.deps.json");
        let generated = format!("{project}/obj/Debug/net10.0/App.AssemblyInfo.cs");
        let root_dll = "bin/Debug/net10.0/root-app.dll";
        let root_generated = "obj/Debug/net10.0/Root.AssemblyInfo.cs";
        let root_build = "build/app.js";
        let nested_bin_source = "tools/bin/net8/deploy.sh";
        let nested_build_source = "src/build/main.rs";
        write_test_file(repo.path(), &source, "class Program {}\n");
        write_test_file(repo.path(), &dll, "binary\n");
        write_test_file(repo.path(), &deps, "{}\n");
        write_test_file(repo.path(), &generated, "// generated\n");
        write_test_file(repo.path(), root_dll, "binary\n");
        write_test_file(repo.path(), root_generated, "// generated\n");
        write_test_file(repo.path(), root_build, "generated\n");
        write_test_file(repo.path(), nested_bin_source, "#!/bin/sh\n");
        write_test_file(repo.path(), nested_build_source, "fn build() {}\n");

        // Simulate Ctrl-C after the old shove's blanket staging step.
        git_test(repo.path(), &["add", "-A"]);
        let outcome = prepare_artifact_exclusions(repo.path()).expect("prepare exclusions");
        assert_eq!(
            outcome.patterns,
            vec![
                "/bin/".to_string(),
                "/build/".to_string(),
                "/obj/".to_string(),
                "tools/dotnet/Status\\ \\[Push\\]/bin/".to_string(),
                "tools/dotnet/Status\\ \\[Push\\]/obj/".to_string(),
            ]
        );
        assert!(outcome.unstaged_paths.contains(&dll));
        assert!(outcome.unstaged_paths.contains(&deps));
        assert!(outcome.unstaged_paths.contains(&generated));
        assert!(outcome.unstaged_paths.iter().any(|path| path == root_dll));
        assert!(outcome
            .unstaged_paths
            .iter()
            .any(|path| path == root_generated));
        assert!(outcome.unstaged_paths.iter().any(|path| path == root_build));
        assert!(!outcome.unstaged_paths.contains(&source));
        assert!(!outcome
            .unstaged_paths
            .iter()
            .any(|path| path == nested_bin_source));
        assert!(!outcome
            .unstaged_paths
            .iter()
            .any(|path| path == nested_build_source));

        // Source stays staged. Generated files leave the index but remain on
        // disk, and a resumed blanket add cannot stage them again.
        let staged_before = run_git_captured(
            repo.path(),
            &["diff", "--cached", "--name-only", "--diff-filter=A"],
        )
        .stdout;
        assert!(staged_before.lines().any(|path| path == source));
        assert!(staged_before.lines().any(|path| path == nested_bin_source));
        assert!(staged_before
            .lines()
            .any(|path| path == nested_build_source));
        assert!(!staged_before.lines().any(|path| path == dll));
        assert!(!staged_before.lines().any(|path| path == root_dll));
        assert!(!staged_before.lines().any(|path| path == root_build));
        assert!(repo.path().join(&dll).is_file());
        assert!(repo.path().join(&deps).is_file());
        assert!(repo.path().join(&generated).is_file());
        assert!(repo.path().join(root_dll).is_file());
        assert!(repo.path().join(root_generated).is_file());
        assert!(repo.path().join(root_build).is_file());

        git_test(repo.path(), &["add", "-A"]);
        let staged_after =
            run_git_captured(repo.path(), &["diff", "--cached", "--name-only"]).stdout;
        assert!(staged_after.lines().any(|path| path == ".gitignore"));
        assert!(staged_after.lines().any(|path| path == source));
        assert!(staged_after.lines().any(|path| path == nested_bin_source));
        assert!(staged_after.lines().any(|path| path == nested_build_source));
        assert!(!staged_after.lines().any(|path| path == dll));
        assert!(!staged_after.lines().any(|path| path == deps));
        assert!(!staged_after.lines().any(|path| path == generated));
        assert!(!staged_after.lines().any(|path| path == root_dll));
        assert!(!staged_after.lines().any(|path| path == root_generated));
        assert!(!staged_after.lines().any(|path| path == root_build));
        git_test(repo.path(), &["check-ignore", "-q", "--", &dll]);
        git_test(repo.path(), &["check-ignore", "-q", "--", &generated]);
        git_test(repo.path(), &["check-ignore", "-q", "--", root_dll]);
        git_test(repo.path(), &["check-ignore", "-q", "--", root_generated]);
        git_test(repo.path(), &["check-ignore", "-q", "--", root_build]);
    }

    #[test]
    fn prepare_artifacts_refuses_to_drop_staged_only_content() {
        let repo = initialized_test_repo();
        let artifact = "project/obj/Debug/net10.0/generated.cs";
        write_test_file(repo.path(), artifact, "// generated\n");
        git_test(repo.path(), &["add", artifact]);
        std::fs::remove_file(repo.path().join(artifact)).expect("remove working copy");

        let error = prepare_artifact_exclusions(repo.path()).expect_err("must fail closed");
        assert!(error.contains("no working-tree copy"), "{error}");
        let staged = run_git_captured(repo.path(), &["diff", "--cached", "--name-only"]).stdout;
        assert!(staged.lines().any(|path| path == artifact));
        assert!(!repo.path().join(".gitignore").exists());
    }

    #[test]
    fn prepare_artifacts_refuses_when_later_negation_defeats_existing_rule() {
        let repo = initialized_test_repo();
        write_test_file(
            repo.path(),
            ".gitignore",
            "project/obj/\n!project/obj/\n!project/obj/generated.cs\n",
        );
        git_test(repo.path(), &["add", ".gitignore"]);
        git_test(repo.path(), &["commit", "-m", "ignore rules"]);
        let artifact = "project/obj/generated.cs";
        write_test_file(repo.path(), artifact, "// generated\n");
        git_test(repo.path(), &["add", artifact]);

        let error = prepare_artifact_exclusions(repo.path()).expect_err("must fail closed");
        assert!(error.contains("refusing blanket staging"), "{error}");
        let staged = run_git_captured(repo.path(), &["diff", "--cached", "--name-only"]).stdout;
        assert!(staged.lines().any(|path| path == artifact));
        assert!(repo.path().join(artifact).is_file());
    }

    #[test]
    fn validate_gitignore_patterns_rejects_line_injection() {
        let error = validate_gitignore_patterns(&["project\n*.rs".to_string()])
            .expect_err("line breaks must fail closed");
        assert!(error.contains("line break"), "{error}");
    }

    /// AX-SGIT-BRANCH-UPSTREAM-SELF: the refspec must always name the
    /// destination ref, so `push.default` can never redirect it to the base.
    #[test]
    fn push_refspec_always_names_the_destination_branch() {
        assert_eq!(push_refspec("feat"), "HEAD:refs/heads/feat");
        assert_eq!(
            push_refspec("task/abc1234-slug"),
            "HEAD:refs/heads/task/abc1234-slug"
        );
        // Already-qualified input must not double-prefix.
        assert_eq!(
            push_refspec("refs/heads/feat"),
            "HEAD:refs/heads/feat"
        );
        // Surrounding whitespace never leaks into the ref.
        assert_eq!(push_refspec("  feat  "), "HEAD:refs/heads/feat");
        // The destination is present in every case — the property that matters.
        for branch in ["a", "feature/x", "refs/heads/y", " z "] {
            assert!(
                push_refspec(branch).contains(":refs/heads/"),
                "refspec for {branch:?} must name a destination ref"
            );
        }
    }

    struct NoopResolver;
    impl ConflictResolver for NoopResolver {
        fn resolve(&self, _ctx: &ConflictContext) -> Result<(), String> {
            Err("test resolver must not be invoked".to_string())
        }
    }

    fn rev_parse(dir: &Path, rev: &str) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", rev])
            .output()
            .expect("run git");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// AX-SGIT-BRANCH-UPSTREAM-SELF (push half): a push must name its
    /// destination explicitly, so a feature branch that inherited
    /// `merge = refs/heads/main` still lands on ITSELF and never on the base.
    ///
    /// The fixture deliberately reproduces the destructive combination: a branch
    /// cut off `origin/main` WITHOUT `--no-track` (so it inherits the base as
    /// its upstream) plus `push.default = upstream` (so a source-only refspec
    /// resolves its destination to that upstream). Without an explicit refspec
    /// this pushes onto main.
    #[test]
    fn push_lands_on_the_feature_branch_never_the_base() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let seed = tmp.path().join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        git_test(&seed, &["init", "-q", "-b", "main"]);
        git_test(&seed, &["config", "user.name", "Shove Test"]);
        git_test(&seed, &["config", "user.email", "shove-test@example.com"]);
        write_test_file(&seed, "README.md", "base\n");
        git_test(&seed, &["add", "README.md"]);
        git_test(&seed, &["commit", "-q", "-m", "base"]);

        let remote = tmp.path().join("remote.git");
        git_test(
            tmp.path(),
            &[
                "clone",
                "-q",
                "--bare",
                seed.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );

        let local = tmp.path().join("local");
        git_test(
            tmp.path(),
            &[
                "clone",
                "-q",
                remote.to_str().unwrap(),
                local.to_str().unwrap(),
            ],
        );
        git_test(&local, &["config", "user.name", "Shove Test"]);
        git_test(&local, &["config", "user.email", "shove-test@example.com"]);
        git_test(&local, &["config", "push.default", "upstream"]);

        // Reproduce the inherited-upstream state explicitly. Do not rely on
        // git's default `branch.autoSetupMerge=true` — an operator global of
        // `simple` (or sgit's repo-local equivalent) would skip the inherit
        // and this test would stop covering the shove refspec at all.
        git_test(&local, &["checkout", "-q", "--no-track", "-b", "feat", "origin/main"]);
        git_test(&local, &["config", "branch.feat.remote", "origin"]);
        git_test(&local, &["config", "branch.feat.merge", "refs/heads/main"]);
        assert_eq!(
            {
                let out = Command::new("git")
                    .arg("-C")
                    .arg(&local)
                    .args(["config", "--local", "--get", "branch.feat.merge"])
                    .output()
                    .expect("run git");
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            },
            "refs/heads/main",
            "fixture precondition: feat must start with the inherited upstream"
        );

        write_test_file(&local, "feature.txt", "work\n");
        git_test(&local, &["add", "feature.txt"]);
        git_test(&local, &["commit", "-q", "-m", "feature work"]);

        let main_before = rev_parse(&remote, "refs/heads/main");
        let feat_tip = rev_parse(&local, "HEAD");

        push_with_sync(&local, "feat", &NoopResolver).expect("push succeeds");

        assert_eq!(
            rev_parse(&remote, "refs/heads/main"),
            main_before,
            "pushing a feature branch must NOT move the base branch"
        );
        assert_eq!(
            rev_parse(&remote, "refs/heads/feat"),
            feat_tip,
            "the feature branch's own ref must carry the commit"
        );
    }

    fn initialized_test_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("temp repo");
        git_test(repo.path(), &["init"]);
        git_test(repo.path(), &["config", "user.name", "Shove Test"]);
        git_test(
            repo.path(),
            &["config", "user.email", "shove-test@example.com"],
        );
        write_test_file(repo.path(), "README.md", "base\n");
        git_test(repo.path(), &["add", "README.md"]);
        git_test(repo.path(), &["commit", "-m", "base"]);
        repo
    }

    fn git_test(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {} failed:\n{}{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write_test_file(repo: &Path, relative: &str, content: &str) {
        let path = repo.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, content).expect("write fixture");
    }
}
