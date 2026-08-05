//! Worktree branch pinning (AX-CLI-WORKTREE-BRANCH-PIN).
//!
//! A *linked* worktree is "pinned" to the branch it was created on by writing a
//! [`PIN_MARKER_FILE`] marker into its per-worktree gitdir
//! (`<common>/worktrees/<name>/stokd-pin`), whose single line is the allowed ref
//! (e.g. `refs/heads/main`). The managed `reference-transaction` git hook
//! ([`reference_transaction_script`]) reads that marker and refuses any attempt
//! to retarget the worktree's `HEAD` to a different branch — enforced by git
//! itself, so it binds every actor (agents, tools, humans).
//!
//! Regular (non-worktree) clones have no `worktrees/<name>` gitdir, so they never
//! carry a marker and are never pinned.
//!
//! Marker name remains `stokd-pin` for on-disk continuity (behavior unchanged).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Name of the per-worktree pin marker, written into a linked worktree's own
/// gitdir (`<common>/worktrees/<name>/`). Its presence marks the worktree as
/// branch-pinned; its single line is the allowed ref (e.g. `refs/heads/main`).
pub const PIN_MARKER_FILE: &str = "stokd-pin";

/// Run `git -C <dir> <args>`, returning trimmed stdout on success (None on
/// failure or empty output).
fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let t = s.trim().to_string();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// The per-worktree gitdir for the worktree at `path`.
fn worktree_git_dir(path: &Path) -> Option<PathBuf> {
    git_out(path, &["rev-parse", "--absolute-git-dir"]).map(PathBuf::from)
}

/// True when `path` is a *linked* worktree — git's layout puts a linked
/// worktree's gitdir at `<common>/worktrees/<name>`, so its parent dir is named
/// `worktrees`.
pub fn is_linked_worktree(path: &Path) -> bool {
    worktree_git_dir(path)
        .as_deref()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .map(|n| n == "worktrees")
        .unwrap_or(false)
}

/// The pin marker path for the worktree at `path`, if it is a linked worktree.
pub fn pin_marker_path(path: &Path) -> Option<PathBuf> {
    if !is_linked_worktree(path) {
        return None;
    }
    worktree_git_dir(path).map(|gd| gd.join(PIN_MARKER_FILE))
}

/// The current branch ref (`refs/heads/<name>`) of the worktree at `path`, or
/// None when detached / outside a repo.
fn current_branch_ref(path: &Path) -> Option<String> {
    git_out(path, &["symbolic-ref", "HEAD"])
}

/// True when `ref_name` resolves to an existing ref in the repo at `path`.
fn ref_exists(path: &Path, ref_name: &str) -> bool {
    git_out(path, &["rev-parse", "--verify", "--quiet", ref_name]).is_some()
}

/// The branch ref a worktree should be pinned to. Resolved in order:
///   1. the current branch (attached HEAD), else
///   2. the branch checked out immediately before a detach (`@{-1}`), if it still
///      exists, else
///   3. the repo's configured default branch (`stokd.defaultBranch`), if it exists.
///
/// `None` only when none of these can be resolved — pinning is then a no-op.
fn intended_pin_ref(path: &Path) -> Option<String> {
    if let Some(current) = current_branch_ref(path) {
        return Some(current);
    }
    if let Some(prev) = git_out(path, &["rev-parse", "--symbolic-full-name", "@{-1}"]) {
        if prev.starts_with("refs/heads/") && ref_exists(path, &prev) {
            return Some(prev);
        }
    }
    if let Some(default) = git_out(path, &["config", "--get", "stokd.defaultBranch"]) {
        let full = format!("refs/heads/{default}");
        if ref_exists(path, &full) {
            return Some(full);
        }
    }
    None
}

/// Pin the linked worktree at `path` to its intended branch.
/// Returns Ok(false) (no-op) for a non-linked worktree or when no branch can be
/// inferred; Ok(true) when the marker is present and current afterward.
pub fn write_pin_marker(path: &Path) -> std::io::Result<bool> {
    let Some(marker) = pin_marker_path(path) else {
        return Ok(false);
    };
    let Some(branch_ref) = intended_pin_ref(path) else {
        return Ok(false);
    };
    let already = std::fs::read_to_string(&marker)
        .ok()
        .map(|s| s.trim() == branch_ref)
        .unwrap_or(false);
    if !already {
        std::fs::write(&marker, format!("{branch_ref}\n"))?;
    }
    Ok(true)
}

/// Remove the pin marker for the worktree at `path`.
pub fn remove_pin_marker(path: &Path) -> std::io::Result<bool> {
    let Some(marker) = pin_marker_path(path) else {
        return Ok(false);
    };
    match std::fs::remove_file(&marker) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// The working-tree paths of every non-bare worktree of the repo that `anchor`
/// belongs to, parsed from `git worktree list --porcelain`.
pub fn list_worktree_paths(anchor: &Path) -> Vec<PathBuf> {
    let Some(raw) = git_out(anchor, &["worktree", "list", "--porcelain"]) else {
        return vec![];
    };
    let mut out = Vec::new();
    let mut cur: Option<PathBuf> = None;
    let mut bare = false;
    let flush = |cur: &mut Option<PathBuf>, bare: &mut bool, out: &mut Vec<PathBuf>| {
        if let Some(p) = cur.take() {
            if !*bare {
                out.push(p);
            }
        }
        *bare = false;
    };
    for line in raw.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            flush(&mut cur, &mut bare, &mut out);
            cur = Some(PathBuf::from(p));
        } else if line == "bare" {
            bare = true;
        }
    }
    flush(&mut cur, &mut bare, &mut out);
    out
}

/// Outcome of reconciling one repo's worktrees.
#[derive(Debug, Default)]
pub struct ReconcileResult {
    pub pinned: Vec<PathBuf>,
    pub unpinned: Vec<PathBuf>,
    pub skipped: Vec<(PathBuf, String)>,
}

/// (Un)pin every linked worktree of the repo `anchor` belongs to.
pub fn reconcile(anchor: &Path, off: bool) -> ReconcileResult {
    let mut r = ReconcileResult::default();
    for wt in list_worktree_paths(anchor) {
        if !is_linked_worktree(&wt) {
            r.skipped.push((wt, "not a linked worktree".into()));
            continue;
        }
        if off {
            match remove_pin_marker(&wt) {
                Ok(_) => r.unpinned.push(wt),
                Err(e) => r.skipped.push((wt, e.to_string())),
            }
        } else {
            match write_pin_marker(&wt) {
                Ok(true) => r.pinned.push(wt),
                Ok(false) => r.skipped.push((wt, "no branch to infer for pinning".into())),
                Err(e) => r.skipped.push((wt, e.to_string())),
            }
        }
    }
    r
}

/// Discover bare repos (`*.git` directories) under `bare_root`.
pub fn discover_bare_repos(bare_root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_bare(bare_root, &mut out, 0);
    out
}

/// Default hooks version embedded in the pin's reference-transaction script when
/// installed by sgit (independent of stokd's full hook set version).
pub const PIN_HOOKS_VERSION: u32 = 7;

/// Subdirectory under the common git dir for sgit-managed pin hooks when no
/// existing `core.hooksPath` is configured.
pub const SGIT_HOOKS_SUBDIR: &str = "sgit-hooks";

/// Ensure the `reference-transaction` hook that enforces the branch pin is
/// installed for the repo containing `anchor`, then reconcile pin markers.
///
/// Strategy:
/// 1. Resolve the common git dir.
/// 2. Prefer writing into an already-configured `core.hooksPath` (coexist with
///    stokd/husky managed hooks).
/// 3. Otherwise create `<common>/sgit-hooks`, set `core.hooksPath` to it, and
///    write the hook there.
///
/// Returns the reconcile result for the anchor repo.
pub fn ensure_pin_and_hook(anchor: &Path, off: bool) -> Result<ReconcileResult, String> {
    if !off {
        install_reference_transaction_hook(anchor)?;
    }
    Ok(reconcile(anchor, off))
}

/// Install (or refresh) only the `reference-transaction` pin hook for `anchor`.
pub fn install_reference_transaction_hook(anchor: &Path) -> Result<PathBuf, String> {
    let common = resolve_common_git_dir(anchor)
        .ok_or_else(|| format!("not a git repository: {}", anchor.display()))?;

    let hooks_dir = resolve_hooks_dir_for_pin(&common)?;
    std::fs::create_dir_all(&hooks_dir).map_err(|e| {
        format!(
            "failed to create hooks dir {}: {e}",
            hooks_dir.display()
        )
    })?;

    let script_path = hooks_dir.join("reference-transaction");
    let content = reference_transaction_script(PIN_HOOKS_VERSION);
    write_hook_script(&script_path, &content)?;
    Ok(script_path)
}

pub(crate) fn resolve_hooks_dir_for_pin(common_dir: &Path) -> Result<PathBuf, String> {
    // Prefer an existing core.hooksPath so we co-install with stokd/husky.
    if let Some(configured) = git_config_get(common_dir, "core.hooksPath") {
        let candidate = PathBuf::from(&configured);
        let resolved = if candidate.is_absolute() {
            candidate
        } else {
            common_dir.join(candidate)
        };
        return Ok(resolved);
    }

    // Default git hooks location under the common dir.
    let default_hooks = common_dir.join("hooks");
    if default_hooks.is_dir() {
        return Ok(default_hooks);
    }

    // Fresh sgit-managed dir + point core.hooksPath at it.
    let sgit_hooks = common_dir.join(SGIT_HOOKS_SUBDIR);
    git_config_set(
        common_dir,
        "core.hooksPath",
        &sgit_hooks.to_string_lossy(),
    )?;
    Ok(sgit_hooks)
}

pub(crate) fn write_hook_script(path: &Path, content: &str) -> Result<(), String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        if existing == content {
            set_executable(path);
            return Ok(());
        }
    }
    std::fs::write(path, content)
        .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    set_executable(path);
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o755);
        let _ = std::fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) {}

/// Resolve `--git-common-dir` for any path inside a repo.
pub fn resolve_common_git_dir(path: &Path) -> Option<PathBuf> {
    let raw = git_out(path, &["rev-parse", "--git-common-dir"])?;
    let candidate = PathBuf::from(raw);
    let absolute = if candidate.is_absolute() {
        candidate
    } else {
        path.join(candidate)
    };
    Some(std::fs::canonicalize(&absolute).unwrap_or(absolute))
}

fn git_config_get(common_dir: &Path, key: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(common_dir)
        .args(["config", "--local", "--get", key])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn git_config_set(common_dir: &Path, key: &str, value: &str) -> Result<(), String> {
    let status = Command::new("git")
        .arg("--git-dir")
        .arg(common_dir)
        .args(["config", "--local", key, value])
        .status()
        .map_err(|e| format!("failed to set git config {key}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("failed to set git config {key}"))
    }
}

fn collect_bare(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.extension().map(|e| e == "git").unwrap_or(false) {
            out.push(path);
        } else {
            collect_bare(&path, out, depth + 1);
        }
    }
}

/// The `reference-transaction` hook body — the worktree branch pin.
/// `hooks_version` is embedded so install/upgrade paths can version the script
/// with the rest of the managed hook set.
///
/// Behavior is deliberately narrow so normal work is untouched:
///   * commits / fetches / pulls / merges move a *branch* ref, not `HEAD`
///   * detaching `HEAD` to an oid is allowed (rebase, bisect)
///   * only `HEAD → ref:refs/heads/<other>` (branch switch) is blocked
///   * only *this* worktree's own HEAD counts, detected via `$gd/HEAD.lock`, so
///     creating a sibling worktree with `git worktree add` is never refused
///     (AX-SGIT-PIN-HEAD-LOCK-SCOPES-ENFORCEMENT)
pub fn reference_transaction_script(hooks_version: u32) -> String {
    format!(
        r#"#!/bin/sh
# stokd-managed git hook — do not edit. Managed by `stokd hooks`.
# stokd-hooks-version: {version}
#
# Worktree branch pin: refuses switching a pinned worktree's HEAD to a different
# branch. Pinned iff this worktree's gitdir holds a `{marker}` file naming the
# allowed ref. Inert on unpinned worktrees and plain clones.
state="$1"
input="$(cat)"
{lock_fragment}if [ "$state" = "prepared" ]; then
  gd="${{GIT_DIR:-}}"
  [ -z "$gd" ] && gd="$(git rev-parse --absolute-git-dir 2>/dev/null || true)"
  marker="$gd/{marker}"
  # Scope enforcement to transactions that mutate *this* worktree's HEAD. A ref
  # transaction holds `HEAD.lock` in the gitdir it actually writes, so the lock is
  # present here for an in-place `checkout` and absent for a sibling
  # `git worktree add` (whose lock lives in <common>/worktrees/<new>/HEAD.lock).
  # Without this scope the pin refuses `worktree add` run from a pinned worktree:
  # git hands us the new worktree's HEAD *creation* while the hook runs in the
  # invoking worktree's gitdir context, and stdin, cwd, GIT_DIR and the whole GIT_*
  # env are byte-identical to a real branch switch. The transaction's old value is
  # the null oid in BOTH cases, so it is not a usable discriminator either.
  # (AX-SGIT-PIN-HEAD-LOCK-SCOPES-ENFORCEMENT)
  if [ -n "$gd" ] && [ -f "$marker" ] && [ -e "$gd/HEAD.lock" ]; then
    pinned="$(head -n1 "$marker" 2>/dev/null | tr -d '[:space:]')"
    if [ -n "$pinned" ]; then
      printf '%s\n' "$input" | while IFS=' ' read -r _old new ref; do
        [ "$ref" = "HEAD" ] || continue
        case "$new" in
          ref:refs/heads/*)
            [ "$new" = "ref:$pinned" ] || exit 39
            ;;
        esac
      done
      if [ $? -eq 39 ]; then
        echo "stokd: refusing to move this worktree off '$pinned'." >&2
        echo "stokd: a worktree directory must match its branch. NEVER repoint a worktree folder at a different branch." >&2
        echo "stokd: to work on another branch, run: sgit checkout <branch>" >&2
        exit 1
      fi
    fi
  fi
fi
# Chain to a prior reference-transaction hook, replaying stdin + the phase arg.
prior="$(git config --get stokd.hooks.priorHooksPath 2>/dev/null || true)"
if [ -n "$prior" ]; then
  case "$prior" in
    /*) prior_dir="$prior" ;;
    *) prior_dir="$(git rev-parse --show-toplevel 2>/dev/null)/$prior" ;;
  esac
  if [ -x "$prior_dir/reference-transaction" ]; then
    printf '%s' "$input" | "$prior_dir/reference-transaction" "$@" || exit $?
  fi
fi
exit 0
"#,
        version = hooks_version,
        marker = PIN_MARKER_FILE,
        lock_fragment = crate::lock::reference_transaction_lock_fragment(),
    )
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
            .expect("git runs")
            .status
            .success();
        assert!(ok, "git {args:?} failed in {}", dir.display());
    }

    /// Run git without asserting success; return (succeeded, stderr).
    fn git_try(dir: &Path, args: &[&str]) -> (bool, String) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git runs");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// The branch `dir`'s HEAD points at, or `"<detached>"`.
    fn head_symref(dir: &Path) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["symbolic-ref", "HEAD"])
            .output()
            .expect("git runs");
        if out.status.success() {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            "<detached>".to_string()
        }
    }

    /// Install the real pin hook and pin the linked worktree, so the test drives
    /// the shipped script rather than a copy of it.
    fn armed_scratch_repo() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let (tmp, primary, wt) = scratch_repo();
        install_reference_transaction_hook(&primary).expect("hook installs");
        assert!(write_pin_marker(&wt).unwrap(), "linked worktree must pin");
        (tmp, primary, wt)
    }

    /// Build a repo with a linked worktree on branch `pinned`; return
    /// (tmp, primary_repo, linked_worktree).
    fn scratch_repo() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let primary = tmp.path().join("primary");
        std::fs::create_dir_all(&primary).unwrap();
        git(&primary, &["init", "-q", "-b", "main"]);
        git(&primary, &["config", "user.email", "t@t.co"]);
        git(&primary, &["config", "user.name", "t"]);
        git(&primary, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let wt = tmp.path().join("wt");
        git(
            &primary,
            &["worktree", "add", "-q", "-b", "pinned", wt.to_str().unwrap()],
        );
        (tmp, primary, wt)
    }

    #[test]
    fn linked_worktree_detected_and_plain_clone_is_not() {
        let (tmp, primary, wt) = scratch_repo();
        assert!(
            is_linked_worktree(&wt),
            "worktree add makes a linked worktree"
        );
        assert!(!is_linked_worktree(&primary));
        let clone = tmp.path().join("plain");
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                primary.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        assert!(
            !is_linked_worktree(&clone),
            "a plain clone is never a linked worktree"
        );
        assert!(pin_marker_path(&clone).is_none());
    }

    #[test]
    fn write_then_remove_marker_is_idempotent() {
        let (_tmp, _primary, wt) = scratch_repo();
        assert!(write_pin_marker(&wt).unwrap());
        let marker = pin_marker_path(&wt).unwrap();
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap().trim(),
            "refs/heads/pinned"
        );
        assert!(write_pin_marker(&wt).unwrap());
        assert!(remove_pin_marker(&wt).unwrap());
        assert!(!marker.exists());
        assert!(!remove_pin_marker(&wt).unwrap());
    }

    #[test]
    fn reconcile_pins_and_unpins_all_worktrees() {
        let (_tmp, _primary, wt) = scratch_repo();
        let r = reconcile(&wt, false);
        assert_eq!(r.pinned.len(), 1, "the single linked worktree gets pinned");
        assert!(pin_marker_path(&wt).unwrap().exists());
        let r = reconcile(&wt, true);
        assert_eq!(r.unpinned.len(), 1);
        assert!(!pin_marker_path(&wt).unwrap().exists());
    }

    #[test]
    fn write_pin_marker_pins_detached_worktree_to_last_branch() {
        let (_tmp, _primary, wt) = scratch_repo();
        git(&wt, &["checkout", "--detach", "-q"]);
        assert!(
            current_branch_ref(&wt).is_none(),
            "worktree should be on a detached HEAD for this test"
        );
        assert!(
            write_pin_marker(&wt).unwrap(),
            "a detached linked worktree must still be pinnable"
        );
        assert_eq!(
            std::fs::read_to_string(pin_marker_path(&wt).unwrap())
                .unwrap()
                .trim(),
            "refs/heads/pinned",
        );
    }

    #[test]
    fn reconcile_pins_detached_worktree() {
        let (_tmp, _primary, wt) = scratch_repo();
        git(&wt, &["checkout", "--detach", "-q"]);
        let r = reconcile(&wt, false);
        assert_eq!(
            r.pinned.len(),
            1,
            "detached linked worktree must be pinned, not skipped: {:?}",
            r.skipped
        );
        assert_eq!(
            std::fs::read_to_string(pin_marker_path(&wt).unwrap())
                .unwrap()
                .trim(),
            "refs/heads/pinned",
        );
    }

    #[test]
    fn write_pin_marker_falls_back_to_default_branch_when_last_branch_gone() {
        let (_tmp, primary, wt) = scratch_repo();
        git(&primary, &["config", "stokd.defaultBranch", "main"]);
        git(&wt, &["checkout", "--detach", "-q"]);
        git(&primary, &["branch", "-D", "pinned"]);
        assert!(
            write_pin_marker(&wt).unwrap(),
            "must pin to the default branch when the last branch is gone"
        );
        assert_eq!(
            std::fs::read_to_string(pin_marker_path(&wt).unwrap())
                .unwrap()
                .trim(),
            "refs/heads/main",
        );
    }

    /// `git worktree add` invoked from inside a PINNED worktree must succeed.
    ///
    /// The hook fires in the *invoking* worktree's gitdir context and is handed the
    /// *new* worktree's HEAD creation, which is byte-identical to an in-place
    /// `git checkout <branch>` on stdin, cwd, GIT_DIR and the whole GIT_* env — and
    /// the old value is the null oid in both cases. Enforcement is therefore scoped
    /// by `$gd/HEAD.lock`, which only exists in the gitdir the transaction actually
    /// mutates. (AX-SGIT-PIN-HEAD-LOCK-SCOPES-ENFORCEMENT)
    #[test]
    fn worktree_add_from_pinned_worktree_is_allowed() {
        let (tmp, primary, wt) = armed_scratch_repo();
        git(&primary, &["branch", "sibling"]);
        let sibling = tmp.path().join("sibling-wt");
        let (ok, stderr) = git_try(
            &wt,
            &["worktree", "add", "-q", sibling.to_str().unwrap(), "sibling"],
        );
        assert!(
            ok,
            "worktree add from a pinned worktree must not be refused: {stderr}"
        );
        assert_eq!(
            head_symref(&sibling),
            "refs/heads/sibling",
            "the new worktree lands on the requested branch"
        );
        assert_eq!(
            head_symref(&wt),
            "refs/heads/pinned",
            "the pinned worktree itself must not have moved"
        );
    }

    /// The same add, in the real deployment topology: a *bare* repo whose only
    /// checkouts are linked worktrees (`/opt/dev/<org>/<repo>.git` plus
    /// `/opt/worktrees/<org>/<repo>/<branch>`). This is the layout that actually
    /// refused `git worktree add upstream-main`.
    /// (AX-SGIT-PIN-HEAD-LOCK-SCOPES-ENFORCEMENT)
    #[test]
    fn worktree_add_from_pinned_worktree_of_bare_repo_is_allowed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare = tmp.path().join("repo.git");
        let seed = tmp.path().join("seed");

        std::fs::create_dir_all(&seed).unwrap();
        git(tmp.path(), &["init", "-q", "--bare", bare.to_str().unwrap()]);
        git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git(&seed, &["init", "-q", "-b", "main"]);
        git(&seed, &["config", "user.email", "t@t.co"]);
        git(&seed, &["config", "user.name", "t"]);
        git(&seed, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(&seed, &["branch", "upstream-main"]);
        git(
            &seed,
            &["push", "-q", bare.to_str().unwrap(), "main", "upstream-main"],
        );

        install_reference_transaction_hook(&bare).expect("hook installs");
        let wt_main = tmp.path().join("main");
        git(
            &bare,
            &["worktree", "add", "-q", wt_main.to_str().unwrap(), "main"],
        );
        assert!(
            write_pin_marker(&wt_main).unwrap(),
            "the primary linked worktree must pin to main"
        );

        let wt_upstream = tmp.path().join("upstream-main");
        let (ok, stderr) = git_try(
            &wt_main,
            &[
                "worktree",
                "add",
                "-q",
                wt_upstream.to_str().unwrap(),
                "upstream-main",
            ],
        );
        assert!(
            ok,
            "adding a sibling worktree from the pinned main worktree must not be refused: {stderr}"
        );
        assert_eq!(head_symref(&wt_upstream), "refs/heads/upstream-main");
        assert_eq!(
            head_symref(&wt_main),
            "refs/heads/main",
            "the pinned main worktree must not have moved"
        );

        // ...and the pin still holds in that same worktree. Switch to a branch that
        // is NOT checked out anywhere, so the refusal can only come from the pin —
        // `upstream-main` would now be rejected by git's own already-checked-out
        // guard, which would make this assertion pass for the wrong reason.
        git(&bare, &["branch", "spare", "main"]);
        let (switched, stderr) = git_try(&wt_main, &["checkout", "-q", "spare"]);
        assert!(!switched, "the pin must still refuse an in-place switch");
        assert!(
            stderr.contains("refusing to move this worktree off"),
            "the refusal must come from the pin, got: {stderr}"
        );
        assert_eq!(head_symref(&wt_main), "refs/heads/main");
    }

    /// The pin still refuses an in-place branch switch in the pinned worktree.
    #[test]
    fn checkout_in_pinned_linked_worktree_is_refused() {
        let (_tmp, primary, wt) = armed_scratch_repo();
        git(&primary, &["branch", "other"]);
        let (ok, stderr) = git_try(&wt, &["checkout", "-q", "other"]);
        assert!(!ok, "a pinned worktree must refuse a branch switch");
        assert!(
            stderr.contains("refusing to move this worktree off"),
            "refusal must explain itself, got: {stderr}"
        );
        assert_eq!(
            head_symref(&wt),
            "refs/heads/pinned",
            "HEAD must be unchanged after a refused switch"
        );
    }

    /// Pinning is per-worktree: a second pinned worktree also refuses its own
    /// switch, and the refusal names *its* branch, not the first worktree's.
    #[test]
    fn each_pinned_worktree_enforces_its_own_branch() {
        let (tmp, primary, wt) = armed_scratch_repo();
        let second = tmp.path().join("second");
        git(
            &wt,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "second",
                second.to_str().unwrap(),
            ],
        );
        assert!(write_pin_marker(&second).unwrap());
        git(&primary, &["branch", "other"]);
        let (ok, stderr) = git_try(&second, &["checkout", "-q", "other"]);
        assert!(!ok, "the second pinned worktree must refuse its own switch");
        assert!(
            stderr.contains("refs/heads/second"),
            "refusal must name the offending worktree's own pin, got: {stderr}"
        );
        assert_eq!(head_symref(&second), "refs/heads/second");
    }

    /// Detaching HEAD stays allowed — rebase and bisect depend on it, and the
    /// hook's scope is deliberately narrow.
    #[test]
    fn detaching_head_in_pinned_worktree_is_allowed() {
        let (_tmp, _primary, wt) = armed_scratch_repo();
        let (ok, stderr) = git_try(&wt, &["checkout", "--detach", "-q"]);
        assert!(ok, "detaching HEAD must remain allowed: {stderr}");
        assert_eq!(head_symref(&wt), "<detached>");
    }

    #[test]
    fn pin_refusal_states_rule_and_advertises_no_off_switch() {
        let s = reference_transaction_script(PIN_HOOKS_VERSION);
        for leak in ["to unpin", "pin --off", "pinBranch", "ask the human"] {
            assert!(!s.contains(leak), "pin refusal leaks an off-switch: {leak:?}");
        }
        assert!(s.contains("a worktree directory must match its branch"));
        assert!(s.contains("NEVER repoint a worktree folder at a different branch"));
        assert!(
            s.contains("sgit checkout"),
            "pin refusal must advertise sgit checkout as the safe alternative"
        );
    }
}
