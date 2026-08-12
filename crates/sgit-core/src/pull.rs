//! Pure pull mechanics: the fast-forward → rebase → merge escalation ladder,
//! behind the same [`ConflictResolver`] seam as [`crate::shove`].
//!
//! The ladder exists because the three integration strategies have strictly
//! increasing cost and strictly decreasing pickiness:
//!
//! 1. **fast-forward** — cannot conflict, cannot rewrite, cannot lose anything.
//! 2. **rebase** — linear history, but replays each local commit separately, so
//!    the same textual conflict can be presented once per replayed commit.
//! 3. **merge** — a merge commit, but resolves once against the final tree.
//!
//! So when a rebase conflicts we abort it (losing nothing — the rebase had not
//! been committed) and retry as a merge, which is strictly less painful to
//! resolve. Only if the *merge* conflicts is the [`ConflictResolver`] invoked.
//!
//! No agent, cloud, or stokd dependency: resolution policy is the caller's
//! (agent dispatch in stokd, `$EDITOR` in sgit).

use std::path::Path;
use std::process::Command;

use crate::shove::{
    ahead_behind_vs_origin, contains_conflict_markers, create_backup_branches, detect_branch_at,
    detect_origin_remote_at, git_status_porcelain, parse_unmerged_paths, run_git_captured,
    ConflictContext, ConflictKind, ConflictResolver,
};

/// Backup-branch namespace for pull's both-sides snapshots.
pub const PULL_BACKUP_PREFIX: &str = "sgit-pull-backup";

// ── Options / outcomes ───────────────────────────────────────────────────────

/// Which rung of the ladder the branch's divergence calls for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullStrategy {
    /// No upstream is configured — tracking must be set before classifying.
    NeedsUpstream,
    /// Nothing on the remote that we do not already have.
    UpToDate,
    /// Purely behind: a fast-forward integrates the remote with zero risk.
    FastForward,
    /// Both sides moved: escalate through rebase, then merge.
    Diverged,
}

/// What [`pull`] actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullOutcome {
    /// Already had everything on the remote.
    AlreadyUpToDate,
    /// Fast-forwarded to the remote tip; no local commits were replayed.
    FastForwarded,
    /// Local commits were replayed on top of the remote tip.
    Rebased,
    /// A merge commit integrated the two sides (possibly after resolution).
    Merged,
}

/// Options for [`pull`].
#[derive(Debug, Clone, Default)]
pub struct PullOptions {
    /// Refuse to go past the fast-forward rung — never rebase or merge.
    pub ff_only: bool,
    /// Skip the rebase rung: fast-forward, else merge directly.
    pub no_rebase: bool,
}

// ── High-level pull ──────────────────────────────────────────────────────────

/// Integrate `origin/<branch>` into the current branch via the escalation
/// ladder, invoking `resolver` only if the final merge rung conflicts.
///
/// Refuses outright on a dirty tracked working tree — this function never
/// stashes, so it can never lose uncommitted work.
pub fn pull(
    repo_root: &Path,
    opts: &PullOptions,
    resolver: &dyn ConflictResolver,
) -> Result<PullOutcome, String> {
    let branch =
        detect_branch_at(repo_root).ok_or_else(|| "Could not detect current branch".to_string())?;
    if detect_origin_remote_at(repo_root).is_none() {
        return Err(format!(
            "No `origin` remote found for {}. `sgit pull` requires a fetch source.",
            repo_root.display()
        ));
    }

    // Refuse BEFORE any ref-touching work. sgit never stashes (SC_AXIOMS git
    // safety), so uncommitted tracked work is a hard stop rather than something
    // to shuffle out of the way.
    let dirty = dirty_tracked_paths(&git_status_porcelain(repo_root)?);
    if !dirty.is_empty() {
        return Err(format_dirty_tree_error(&branch, &dirty));
    }

    let fetch = run_git_captured(repo_root, &["fetch", "origin", &branch]);
    if !fetch.success {
        return Err(format!(
            "git fetch origin {branch} failed:\n{}{}",
            fetch.stdout, fetch.stderr
        ));
    }

    let (ahead, behind, has_upstream) = ahead_behind_vs_origin(repo_root);
    let (ahead, behind) = match classify_pull_strategy(ahead, behind, has_upstream) {
        PullStrategy::NeedsUpstream => {
            ensure_upstream(repo_root, &branch)?;
            let (a, b, _) = ahead_behind_vs_origin(repo_root);
            (a, b)
        }
        _ => (ahead, behind),
    };

    match classify_pull_strategy(ahead, behind, true) {
        // Unreachable: `ensure_upstream` either set tracking or returned Err.
        PullStrategy::NeedsUpstream => Err(format!(
            "branch `{branch}` still has no upstream after setup"
        )),
        PullStrategy::UpToDate => {
            println!("[{}] Already up to date with origin/{branch}.", repo_root.display());
            Ok(PullOutcome::AlreadyUpToDate)
        }
        PullStrategy::FastForward => {
            println!("Fast-forwarding {branch} to origin/{branch} ({behind} commit(s))...");
            run_git_no_editor(repo_root, &["merge", "--ff-only", "@{upstream}"])
                .map_err(|e| format!("fast-forward failed:\n{e}"))?;
            Ok(PullOutcome::FastForwarded)
        }
        PullStrategy::Diverged => {
            if opts.ff_only {
                return Err(format!(
                    "{branch} and origin/{branch} have diverged ({ahead} local, {behind} remote) \
                     and --ff-only was requested. Nothing was changed."
                ));
            }
            integrate_diverged(repo_root, &branch, ahead, behind, opts, resolver)
        }
    }
}

/// The escalation rungs below fast-forward: snapshot both sides, try rebase,
/// and fall back to merge (+ resolver) when the rebase conflicts.
fn integrate_diverged(
    repo_root: &Path,
    branch: &str,
    ahead: usize,
    behind: usize,
    opts: &PullOptions,
    resolver: &dyn ConflictResolver,
) -> Result<PullOutcome, String> {
    println!(
        "{branch} and origin/{branch} have diverged ({ahead} local, {behind} remote). \
         Snapshotting both sides..."
    );
    create_backup_branches(repo_root, branch, PULL_BACKUP_PREFIX)?;

    if !opts.no_rebase {
        println!("Trying rebase onto origin/{branch}...");
        if run_git_no_editor(repo_root, &["rebase", "@{upstream}"]).is_ok() {
            println!("Rebased {ahead} local commit(s) onto origin/{branch}.");
            return Ok(PullOutcome::Rebased);
        }
        // A stopped rebase has committed nothing, so aborting loses nothing.
        // Retrying as a merge is strictly cheaper to resolve: one resolution
        // against the final tree instead of one per replayed commit.
        println!("Rebase could not complete cleanly; aborting it and retrying as a merge...");
        let _ = run_git_no_editor(repo_root, &["rebase", "--abort"]);
    }

    if run_git_no_editor(repo_root, &["merge", "--no-ff", "@{upstream}"]).is_ok() {
        println!("Merged origin/{branch} into {branch}.");
        return Ok(PullOutcome::Merged);
    }

    resolve_merge_conflicts(repo_root, branch, resolver)?;
    println!("Merged origin/{branch} into {branch} after conflict resolution.");
    Ok(PullOutcome::Merged)
}

/// Hand the unmerged paths to `resolver`, verify the markers are gone, then
/// stage and commit the merge.
///
/// On any failure the conflicted merge is deliberately LEFT IN PLACE: both
/// sides remain reachable (working tree, `MERGE_HEAD`, and the backup
/// branches), so the user can finish by hand without having lost anything.
pub fn resolve_merge_conflicts(
    repo_root: &Path,
    branch: &str,
    resolver: &dyn ConflictResolver,
) -> Result<(), String> {
    let name_only = run_git_captured(repo_root, &["diff", "--name-only", "--diff-filter=U"]);
    let files = parse_unmerged_paths(&name_only.stdout);
    if files.is_empty() {
        return Err(format!(
            "git merge origin/{branch} failed without leaving unmerged paths; \
             the repository was not modified beyond the safety snapshots."
        ));
    }

    println!(
        "Merge hit conflicts in {} file(s); invoking ConflictResolver...",
        files.len()
    );
    resolver.resolve(&ConflictContext {
        repo_root: repo_root.to_path_buf(),
        branch: branch.to_string(),
        unmerged_files: files.clone(),
        kind: ConflictKind::Merge,
    })?;

    for file in &files {
        let content = std::fs::read_to_string(repo_root.join(file)).unwrap_or_default();
        if contains_conflict_markers(&content) {
            return Err(format!(
                "conflict markers still present in {file} after resolution; the merge is left in \
                 place so both sides are preserved"
            ));
        }
    }

    run_git_no_editor(repo_root, &["add", "-A"])?;
    run_git_no_editor(repo_root, &["commit", "--no-edit"])
        .map_err(|e| format!("could not commit the resolved merge:\n{e}"))?;
    Ok(())
}

/// Point `branch` at `origin/<branch>` when it has no upstream yet. This is the
/// "There is no tracking information for the current branch" case, which is
/// mechanical, not a decision the user needs to make.
fn ensure_upstream(repo_root: &Path, branch: &str) -> Result<(), String> {
    let remote_ref = format!("origin/{branch}");
    if !run_git_captured(repo_root, &["rev-parse", "--verify", "--quiet", &remote_ref]).success {
        return Err(format!(
            "branch `{branch}` has no upstream and {remote_ref} does not exist. \
             Publish it first: `git push -u origin {branch}`."
        ));
    }
    run_git_no_editor(
        repo_root,
        &["branch", &format!("--set-upstream-to={remote_ref}"), branch],
    )
    .map_err(|e| format!("could not set upstream to {remote_ref}:\n{e}"))?;
    println!("Set upstream: {branch} -> {remote_ref}.");
    Ok(())
}

// ── Pure decision helpers ────────────────────────────────────────────────────

/// Pick the ladder rung for a branch's `(ahead, behind)` divergence.
pub fn classify_pull_strategy(ahead: usize, behind: usize, has_upstream: bool) -> PullStrategy {
    if !has_upstream {
        return PullStrategy::NeedsUpstream;
    }
    if behind == 0 {
        return PullStrategy::UpToDate;
    }
    if ahead == 0 {
        return PullStrategy::FastForward;
    }
    PullStrategy::Diverged
}

/// Tracked paths with staged or unstaged modifications, from
/// `git status --porcelain`. Untracked (`??`) entries are NOT dirty: git
/// integrates cleanly around them, so refusing on them would be hostile.
pub fn dirty_tracked_paths(status_porcelain: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in status_porcelain.lines() {
        if line.len() < 4 || line.starts_with("??") {
            continue;
        }
        // `XY <path>`, with `R  old -> new` for renames.
        let path = line[3..].trim().trim_matches('"');
        let path = path.rsplit(" -> ").next().unwrap_or(path);
        if !path.is_empty() {
            paths.push(path.to_string());
        }
    }
    paths
}

/// Human-readable refusal for a pull attempted over uncommitted tracked work.
pub fn format_dirty_tree_error(branch: &str, dirty: &[String]) -> String {
    let listed: Vec<&str> = dirty.iter().take(10).map(String::as_str).collect();
    let more = dirty.len().saturating_sub(listed.len());
    let suffix = if more > 0 {
        format!("\n  ... and {more} more")
    } else {
        String::new()
    };
    format!(
        "Refusing to pull {branch}: {} tracked file(s) have uncommitted changes.\n  {}{}\n\
         Commit them first (`sgit shove` will commit and push in one step), or revert them. \
         Nothing was fetched or merged.",
        dirty.len(),
        listed.join("\n  "),
        suffix
    )
}

// ── Git helpers local to pull ────────────────────────────────────────────────

/// Run a git subprocess with a non-interactive editor, capturing output.
fn run_git_no_editor(path: &Path, args: &[&str]) -> Result<(), String> {
    let mut full_args = vec!["-C", path.to_str().unwrap_or(".")];
    full_args.extend_from_slice(args);
    let output = Command::new("git")
        .args(&full_args)
        .env("GIT_EDITOR", "true")
        .env("GIT_MERGE_AUTOEDIT", "no")
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;

    // ── Pure helpers ─────────────────────────────────────────────────────────

    #[test]
    fn classify_pull_strategy_maps_divergence() {
        assert_eq!(
            classify_pull_strategy(0, 0, false),
            PullStrategy::NeedsUpstream
        );
        assert_eq!(
            classify_pull_strategy(3, 2, false),
            PullStrategy::NeedsUpstream
        );
        assert_eq!(classify_pull_strategy(0, 0, true), PullStrategy::UpToDate);
        assert_eq!(classify_pull_strategy(4, 0, true), PullStrategy::UpToDate);
        assert_eq!(classify_pull_strategy(0, 5, true), PullStrategy::FastForward);
        assert_eq!(classify_pull_strategy(1, 1, true), PullStrategy::Diverged);
    }

    #[test]
    fn dirty_tracked_paths_ignores_untracked() {
        let status = " M src/a.rs\nM  src/b.rs\n?? scratch.txt\nA  src/c.rs\n";
        assert_eq!(
            dirty_tracked_paths(status),
            vec![
                "src/a.rs".to_string(),
                "src/b.rs".to_string(),
                "src/c.rs".to_string()
            ]
        );
        assert!(dirty_tracked_paths("?? only/untracked\n").is_empty());
        assert!(dirty_tracked_paths("").is_empty());
    }

    #[test]
    fn format_dirty_tree_error_names_the_files() {
        let msg = format_dirty_tree_error("main", &["src/a.rs".to_string()]);
        assert!(msg.contains("src/a.rs"), "{msg}");
        assert!(msg.contains("main"), "{msg}");
        // Must never suggest stashing (SC_AXIOMS git safety).
        assert!(!msg.to_lowercase().contains("stash"), "{msg}");
    }

    // ── Test resolvers ───────────────────────────────────────────────────────

    /// Records every invocation; resolves by keeping BOTH sides of each hunk.
    struct UnionResolver {
        calls: RefCell<Vec<ConflictKind>>,
    }

    impl UnionResolver {
        fn new() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl ConflictResolver for UnionResolver {
        fn resolve(&self, ctx: &ConflictContext) -> Result<(), String> {
            self.calls.borrow_mut().push(ctx.kind);
            for rel in &ctx.unmerged_files {
                let path = ctx.repo_root.join(rel);
                let content = std::fs::read_to_string(&path).unwrap();
                std::fs::write(&path, union_of_hunks(&content)).unwrap();
            }
            Ok(())
        }
    }

    /// Never resolves — proves the caller leaves the conflict in place.
    struct FailingResolver {
        calls: RefCell<usize>,
    }

    impl ConflictResolver for FailingResolver {
        fn resolve(&self, _ctx: &ConflictContext) -> Result<(), String> {
            *self.calls.borrow_mut() += 1;
            Err("resolver declined".to_string())
        }
    }

    /// Strip conflict markers, keeping ours AND theirs.
    fn union_of_hunks(content: &str) -> String {
        let mut out = String::new();
        let mut in_conflict = false;
        for line in content.lines() {
            if line.starts_with("<<<<<<<") {
                in_conflict = true;
                continue;
            }
            if in_conflict && (line.starts_with("=======") || line.starts_with(">>>>>>>")) {
                if line.starts_with(">>>>>>>") {
                    in_conflict = false;
                }
                continue;
            }
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    // ── Real-git fixture ─────────────────────────────────────────────────────

    struct Fixture {
        _tmp: tempfile::TempDir,
        local: PathBuf,
        other: PathBuf,
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let out = run_git_captured(dir, args);
        assert!(
            out.success,
            "git {args:?} in {} failed:\n{}{}",
            dir.display(),
            out.stdout,
            out.stderr
        );
    }

    fn rev(dir: &Path, spec: &str) -> String {
        let out = run_git_captured(dir, &["rev-parse", spec]);
        assert!(out.success, "rev-parse {spec}: {}", out.stderr);
        out.stdout.trim().to_string()
    }

    fn configure(dir: &Path, hooks: &Path) {
        git_ok(dir, &["config", "user.email", "t@example.com"]);
        git_ok(dir, &["config", "user.name", "Test"]);
        git_ok(dir, &["config", "commit.gpgsign", "false"]);
        // Isolate from any globally-installed hooks (pin / lock hooks).
        git_ok(dir, &["config", "core.hooksPath", hooks.to_str().unwrap()]);
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        std::fs::write(dir.join(rel), content).unwrap();
    }

    fn commit_all(dir: &Path, msg: &str) {
        git_ok(dir, &["add", "-A"]);
        git_ok(dir, &["commit", "-m", msg]);
    }

    /// A bare `origin` seeded with one commit on `main`, plus two clones:
    /// `local` (the repo under test) and `other` (stands in for a teammate).
    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let hooks = root.join("nohooks");
        std::fs::create_dir_all(&hooks).unwrap();

        let origin = root.join("origin.git");
        let out = Command::new("git")
            .args(["init", "--bare", "-b", "main"])
            .arg(&origin)
            .output()
            .unwrap();
        assert!(out.status.success(), "git init --bare failed");

        let local = root.join("local");
        let out = Command::new("git")
            .arg("clone")
            .arg(&origin)
            .arg(&local)
            .output()
            .unwrap();
        assert!(out.status.success(), "clone local failed");
        configure(&local, &hooks);

        write(&local, "file.txt", "line1\nline2\nline3\n");
        commit_all(&local, "base");
        git_ok(&local, &["push", "-u", "origin", "main"]);

        let other = root.join("other");
        let out = Command::new("git")
            .arg("clone")
            .arg(&origin)
            .arg(&other)
            .output()
            .unwrap();
        assert!(out.status.success(), "clone other failed");
        configure(&other, &hooks);

        Fixture {
            _tmp: tmp,
            local,
            other,
        }
    }

    /// Push a commit to origin from the teammate clone.
    fn remote_commit(fx: &Fixture, content: &str, msg: &str) {
        write(&fx.other, "file.txt", content);
        commit_all(&fx.other, msg);
        git_ok(&fx.other, &["push", "origin", "main"]);
    }

    fn backup_branches(dir: &Path) -> Vec<String> {
        let out = run_git_captured(
            dir,
            &[
                "for-each-ref",
                "--format=%(refname:short)",
                &format!("refs/heads/{PULL_BACKUP_PREFIX}/**"),
            ],
        );
        out.stdout
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }

    fn file(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("file.txt")).unwrap()
    }

    // ── Ladder behavior ──────────────────────────────────────────────────────

    #[test]
    fn pull_fast_forwards_when_only_behind() {
        let fx = fixture();
        remote_commit(&fx, "line1\nline2\nline3\nremote\n", "remote work");

        let resolver = UnionResolver::new();
        let outcome = pull(&fx.local, &PullOptions::default(), &resolver).unwrap();

        assert_eq!(outcome, PullOutcome::FastForwarded);
        assert_eq!(rev(&fx.local, "HEAD"), rev(&fx.local, "origin/main"));
        assert!(file(&fx.local).contains("remote"));
        assert!(resolver.calls.borrow().is_empty(), "resolver must not run");
    }

    #[test]
    fn pull_is_a_noop_when_up_to_date() {
        let fx = fixture();
        let before = rev(&fx.local, "HEAD");
        let resolver = UnionResolver::new();
        let outcome = pull(&fx.local, &PullOptions::default(), &resolver).unwrap();
        assert_eq!(outcome, PullOutcome::AlreadyUpToDate);
        assert_eq!(rev(&fx.local, "HEAD"), before);
    }

    #[test]
    fn pull_rebases_non_overlapping_divergence() {
        let fx = fixture();
        // Teammate appends at the end of the file.
        remote_commit(&fx, "line1\nline2\nline3\nremote\n", "remote work");
        // We add a whole separate file — cannot textually conflict.
        write(&fx.local, "mine.txt", "mine\n");
        commit_all(&fx.local, "local work");
        let local_msg_head = rev(&fx.local, "HEAD");

        let resolver = UnionResolver::new();
        let outcome = pull(&fx.local, &PullOptions::default(), &resolver).unwrap();

        assert_eq!(outcome, PullOutcome::Rebased);
        assert!(resolver.calls.borrow().is_empty(), "resolver must not run");
        // Both sides present, and history is linear (rebase, not merge).
        assert!(file(&fx.local).contains("remote"));
        assert!(fx.local.join("mine.txt").exists());
        let log = run_git_captured(&fx.local, &["log", "--oneline"]).stdout;
        assert!(log.contains("local work"), "{log}");
        assert!(log.contains("remote work"), "{log}");
        let parents = run_git_captured(&fx.local, &["rev-list", "--parents", "-1", "HEAD"]).stdout;
        assert_eq!(
            parents.split_whitespace().count(),
            2,
            "rebased tip must have exactly one parent: {parents}"
        );
        // Rebase rewrites, so the tip SHA must have changed.
        assert_ne!(rev(&fx.local, "HEAD"), local_msg_head);
    }

    #[test]
    fn pull_falls_back_to_merge_on_overlapping_divergence() {
        let fx = fixture();
        remote_commit(&fx, "line1\nREMOTE\nline3\n", "remote edits line2");
        write(&fx.local, "file.txt", "line1\nLOCAL\nline3\n");
        commit_all(&fx.local, "local edits line2");

        let resolver = UnionResolver::new();
        let outcome = pull(&fx.local, &PullOptions::default(), &resolver).unwrap();

        assert_eq!(outcome, PullOutcome::Merged);
        let calls = resolver.calls.borrow();
        assert_eq!(calls.len(), 1, "resolver invoked exactly once: {calls:?}");
        assert_eq!(calls[0], ConflictKind::Merge);
        drop(calls);

        // Neither side lost.
        let content = file(&fx.local);
        assert!(content.contains("LOCAL"), "{content}");
        assert!(content.contains("REMOTE"), "{content}");
        assert!(
            !contains_conflict_markers(&content),
            "markers left behind: {content}"
        );
        // A merge commit: two parents, clean tree, no merge in progress.
        let parents = run_git_captured(&fx.local, &["rev-list", "--parents", "-1", "HEAD"]).stdout;
        assert_eq!(
            parents.split_whitespace().count(),
            3,
            "merge tip must have two parents: {parents}"
        );
        assert!(git_status_porcelain(&fx.local).unwrap().trim().is_empty());
    }

    #[test]
    fn pull_leaves_conflicted_merge_and_backups_when_resolver_fails() {
        let fx = fixture();
        remote_commit(&fx, "line1\nREMOTE\nline3\n", "remote edits line2");
        write(&fx.local, "file.txt", "line1\nLOCAL\nline3\n");
        commit_all(&fx.local, "local edits line2");

        let local_before = rev(&fx.local, "HEAD");
        // The real remote tip — `local`'s `origin/main` tracking ref is still
        // stale at this point, since nothing has fetched yet.
        let remote_before = rev(&fx.other, "HEAD");

        let resolver = FailingResolver {
            calls: RefCell::new(0),
        };
        let err = pull(&fx.local, &PullOptions::default(), &resolver).unwrap_err();
        assert!(err.contains("resolver declined"), "{err}");
        assert_eq!(*resolver.calls.borrow(), 1);

        // The conflicted merge is left in place — nothing discarded.
        let git_dir = run_git_captured(&fx.local, &["rev-parse", "--git-dir"])
            .stdout
            .trim()
            .to_string();
        assert!(
            fx.local.join(&git_dir).join("MERGE_HEAD").exists(),
            "MERGE_HEAD must survive so the user can finish by hand"
        );

        // Both sides snapshotted.
        let backups = backup_branches(&fx.local);
        assert_eq!(backups.len(), 2, "expected both backups: {backups:?}");
        let shas: Vec<String> = backups.iter().map(|b| rev(&fx.local, b)).collect();
        assert!(shas.contains(&local_before), "local side not backed up");
        assert!(shas.contains(&remote_before), "remote side not backed up");
    }

    #[test]
    fn pull_sets_a_missing_upstream() {
        let fx = fixture();
        git_ok(&fx.local, &["branch", "--unset-upstream"]);
        assert!(
            !run_git_captured(&fx.local, &["rev-parse", "--abbrev-ref", "@{upstream}"]).success,
            "precondition: no upstream"
        );
        remote_commit(&fx, "line1\nline2\nline3\nremote\n", "remote work");

        let resolver = UnionResolver::new();
        let outcome = pull(&fx.local, &PullOptions::default(), &resolver).unwrap();

        assert_eq!(outcome, PullOutcome::FastForwarded);
        let up = run_git_captured(
            &fx.local,
            &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"],
        );
        assert!(up.success, "upstream must now be set");
        assert_eq!(up.stdout.trim(), "origin/main");
    }

    #[test]
    fn pull_refuses_a_dirty_working_tree_without_touching_refs() {
        let fx = fixture();
        remote_commit(&fx, "line1\nline2\nline3\nremote\n", "remote work");
        let origin_ref_before = rev(&fx.local, "origin/main");
        let head_before = rev(&fx.local, "HEAD");
        write(&fx.local, "file.txt", "line1\nline2\nline3\nDIRTY\n");

        let resolver = UnionResolver::new();
        let err = pull(&fx.local, &PullOptions::default(), &resolver).unwrap_err();

        assert!(err.contains("file.txt"), "{err}");
        // No fetch, no rebase, no merge: refs are untouched and the edit survives.
        assert_eq!(rev(&fx.local, "origin/main"), origin_ref_before);
        assert_eq!(rev(&fx.local, "HEAD"), head_before);
        assert!(file(&fx.local).contains("DIRTY"));
        assert!(resolver.calls.borrow().is_empty());
    }

    #[test]
    fn pull_ff_only_refuses_to_escalate() {
        let fx = fixture();
        remote_commit(&fx, "line1\nREMOTE\nline3\n", "remote edits line2");
        write(&fx.local, "file.txt", "line1\nLOCAL\nline3\n");
        commit_all(&fx.local, "local edits line2");
        let head_before = rev(&fx.local, "HEAD");

        let resolver = UnionResolver::new();
        let opts = PullOptions {
            ff_only: true,
            ..PullOptions::default()
        };
        let err = pull(&fx.local, &opts, &resolver).unwrap_err();

        assert!(err.contains("diverged"), "{err}");
        assert_eq!(rev(&fx.local, "HEAD"), head_before, "must not rewrite");
        assert!(resolver.calls.borrow().is_empty());
    }

    #[test]
    fn pull_no_rebase_goes_straight_to_merge() {
        let fx = fixture();
        remote_commit(&fx, "line1\nline2\nline3\nremote\n", "remote work");
        write(&fx.local, "mine.txt", "mine\n");
        commit_all(&fx.local, "local work");

        let resolver = UnionResolver::new();
        let opts = PullOptions {
            no_rebase: true,
            ..PullOptions::default()
        };
        let outcome = pull(&fx.local, &opts, &resolver).unwrap();

        assert_eq!(outcome, PullOutcome::Merged);
        let parents = run_git_captured(&fx.local, &["rev-list", "--parents", "-1", "HEAD"]).stdout;
        assert_eq!(parents.split_whitespace().count(), 3, "{parents}");
    }
}
