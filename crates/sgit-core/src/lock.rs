//! Biometric branch/repo locks (AX-SGIT-LOCK-BIOMETRIC-GATE).
//!
//! A *lock* makes a physical biometric approval (Touch ID / Windows Hello)
//! mandatory before git may update a locked ref — locally (commit, merge,
//! reset, `update-ref`, …) via the managed `reference-transaction` hook, which
//! git runs on EVERY ref transaction and which `--no-verify` cannot skip — and
//! remotely via the managed `pre-push` hook. All enforcement decisions live in
//! this module (the hook scripts are dumb pipes into `sgit lock enforce`), so
//! the shell layer can never drift from the policy.
//!
//! ## Lock stores (union)
//!
//! 1. **Repo file** — `<git-common-dir>/sgit-locks`: one entry per line, either
//!    a full ref (`refs/heads/main`) or the repo-wide wildcard `*`.
//! 2. **Machine registry** — `~/.stokd/guard/locks.yaml`: the same entries
//!    keyed by the repo's canonical common git dir. The registry survives
//!    deletion of the repo-local file, so wiping `sgit-locks` does NOT drop the
//!    lock. Both stores are protected artifacts under stokd governance.
//!
//! The effective lock set is the UNION of both stores. Locking writes both;
//! unlocking requires a biometric approval and removes from both.
//!
//! ## Fail closed
//!
//! Every path that cannot obtain a biometric verdict (headless, unsupported
//! OS, missing binary while a repo lock file exists) DENIES the operation.
//! There is deliberately no non-biometric override flag.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Name of the per-repo lock file inside the common git dir.
pub const REPO_LOCKS_FILE: &str = "sgit-locks";

/// The repo-wide lock entry: every ref update / push in the repo is gated.
pub const LOCK_WILDCARD: &str = "*";

/// Machine registry path relative to `$HOME`.
pub const REGISTRY_RELATIVE_PATH: &str = ".stokd/guard/locks.yaml";

/// The machine lock registry (`~/.stokd/guard/locks.yaml`).
pub fn default_registry_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(REGISTRY_RELATIVE_PATH))
}

/// A set of lock entries for one repo: full refs (`refs/heads/<name>`) and/or
/// the [`LOCK_WILDCARD`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockSet {
    entries: BTreeSet<String>,
}

impl LockSet {
    pub fn from_entries<I: IntoIterator<Item = String>>(entries: I) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|entry| entry.trim().to_string())
                .filter(|entry| !entry.is_empty())
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(String::as_str)
    }

    pub fn insert(&mut self, entry: &str) -> bool {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return false;
        }
        self.entries.insert(trimmed.to_string())
    }

    pub fn remove(&mut self, entry: &str) -> bool {
        self.entries.remove(entry.trim())
    }

    pub fn union(&self, other: &LockSet) -> LockSet {
        LockSet {
            entries: self.entries.union(&other.entries).cloned().collect(),
        }
    }

    /// Whether an update to `ref_name` is gated by this lock set.
    ///
    /// * An exact entry gates exactly that ref.
    /// * The wildcard gates every real ref (`refs/…`) — repo-wide lock.
    /// * Symbolic pseudo-refs (`HEAD`, `ORIG_HEAD`, …) are never gated: the
    ///   branch ref update in the same transaction is the gate, and gating
    ///   `HEAD` would break detaches/rebases that touch no locked branch.
    pub fn gates_ref(&self, ref_name: &str) -> bool {
        let name = ref_name.trim();
        if name.is_empty() || !name.starts_with("refs/") {
            return false;
        }
        self.entries.contains(name)
            || (self.entries.contains(LOCK_WILDCARD))
    }
}

/// Read the repo-local lock file (missing file ⇒ empty set).
pub fn read_repo_locks(common_git_dir: &Path) -> LockSet {
    let raw = std::fs::read_to_string(common_git_dir.join(REPO_LOCKS_FILE)).unwrap_or_default();
    LockSet::from_entries(raw.lines().map(str::to_string))
}

/// Write (or remove, when empty) the repo-local lock file.
pub fn write_repo_locks(common_git_dir: &Path, locks: &LockSet) -> Result<(), String> {
    let path = common_git_dir.join(REPO_LOCKS_FILE);
    if locks.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("failed to remove {}: {error}", path.display())),
        }
    } else {
        let mut body = String::new();
        for entry in locks.entries() {
            body.push_str(entry);
            body.push('\n');
        }
        std::fs::write(&path, body).map_err(|error| {
            format!("failed to write {}: {error}", path.display())
        })
    }
}

/// The on-disk machine registry: repo key (canonical common git dir) → entries.
pub type LockRegistry = BTreeMap<String, BTreeSet<String>>;

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct RegistryFile {
    version: u32,
    #[serde(default)]
    repos: LockRegistry,
}

/// Canonical registry key for a repo: the canonicalized common git dir path.
pub fn registry_key(common_git_dir: &Path) -> String {
    std::fs::canonicalize(common_git_dir)
        .unwrap_or_else(|_| common_git_dir.to_path_buf())
        .to_string_lossy()
        .replace('\\', "/")
}

/// Read the machine registry (missing/corrupt file ⇒ empty registry).
pub fn read_registry(registry_path: &Path) -> LockRegistry {
    let Ok(raw) = std::fs::read_to_string(registry_path) else {
        return LockRegistry::new();
    };
    serde_yaml::from_str::<RegistryFile>(&raw)
        .map(|file| file.repos)
        .unwrap_or_default()
}

/// Persist the machine registry, creating parent directories as needed.
pub fn write_registry(registry_path: &Path, registry: &LockRegistry) -> Result<(), String> {
    if let Some(parent) = registry_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }
    let file = RegistryFile {
        version: 1,
        repos: registry.clone(),
    };
    let body = serde_yaml::to_string(&file)
        .map_err(|error| format!("failed to serialize lock registry: {error}"))?;
    std::fs::write(registry_path, body)
        .map_err(|error| format!("failed to write {}: {error}", registry_path.display()))
}

/// The effective lock set for a repo: UNION of the repo-local file and the
/// machine registry entry, so deleting either store alone never drops a lock.
pub fn effective_locks(common_git_dir: &Path, registry_path: &Path) -> LockSet {
    let repo_locks = read_repo_locks(common_git_dir);
    let registry = read_registry(registry_path);
    let registry_locks = registry
        .get(&registry_key(common_git_dir))
        .map(|entries| LockSet::from_entries(entries.iter().cloned()))
        .unwrap_or_default();
    repo_locks.union(&registry_locks)
}

/// Add a lock entry to BOTH stores. Locking is the safe direction and needs no
/// biometric.
pub fn add_lock(
    common_git_dir: &Path,
    registry_path: &Path,
    entry: &str,
) -> Result<LockSet, String> {
    let mut repo_locks = read_repo_locks(common_git_dir);
    repo_locks.insert(entry);
    write_repo_locks(common_git_dir, &repo_locks)?;

    let mut registry = read_registry(registry_path);
    registry
        .entry(registry_key(common_git_dir))
        .or_default()
        .insert(entry.trim().to_string());
    write_registry(registry_path, &registry)?;
    Ok(effective_locks(common_git_dir, registry_path))
}

/// Remove a lock entry from BOTH stores — gated behind `approve` (the
/// biometric prompt in production; injected in tests). A denied approval
/// leaves both stores untouched.
pub fn remove_lock(
    common_git_dir: &Path,
    registry_path: &Path,
    entry: &str,
    approve: &dyn Fn(&str) -> Result<(), String>,
) -> Result<LockSet, String> {
    approve(&format!("unlock '{entry}' for {}", common_git_dir.display()))?;

    let mut repo_locks = read_repo_locks(common_git_dir);
    repo_locks.remove(entry);
    write_repo_locks(common_git_dir, &repo_locks)?;

    let mut registry = read_registry(registry_path);
    let key = registry_key(common_git_dir);
    if let Some(entries) = registry.get_mut(&key) {
        entries.remove(entry.trim());
        if entries.is_empty() {
            registry.remove(&key);
        }
    }
    write_registry(registry_path, &registry)?;
    Ok(effective_locks(common_git_dir, registry_path))
}

/// Parse `reference-transaction` hook stdin (`<old> <new> <ref>` per line) into
/// the updated ref names.
pub fn refs_from_reference_transaction(input: &str) -> Vec<String> {
    input
        .lines()
        .filter_map(|line| line.split_whitespace().nth(2).map(str::to_string))
        .collect()
}

/// Parse `pre-push` hook stdin (`<local-ref> <local-oid> <remote-ref>
/// <remote-oid>` per line) into the REMOTE ref names being pushed to.
pub fn refs_from_pre_push(input: &str) -> Vec<String> {
    input
        .lines()
        .filter_map(|line| line.split_whitespace().nth(2).map(str::to_string))
        .collect()
}

/// Pure enforcement decision: which of `refs` are gated by `locks`.
pub fn gated_refs<'r>(refs: &'r [String], locks: &LockSet) -> Vec<&'r str> {
    refs.iter()
        .map(String::as_str)
        .filter(|ref_name| locks.gates_ref(ref_name))
        .collect()
}

/// Which hook is asking for an enforcement decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockHook {
    /// Local ref transaction (commit, merge, reset, update-ref, fetch, …).
    ReferenceTransaction,
    /// Push to a remote.
    PrePush,
}

impl LockHook {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "reference-transaction" => Some(Self::ReferenceTransaction),
            "pre-push" => Some(Self::PrePush),
            _ => None,
        }
    }
}

/// Hook-aware enforcement decision.
///
/// * `reference-transaction` gates only `refs/heads/*` updates — a repo-wide
///   wildcard must not turn a plain `git fetch --tags` into a biometric prompt;
///   the branch ref is where accidental local damage lands.
/// * `pre-push` gates every real ref: a push is always a deliberate act, so a
///   repo-wide lock covers branches AND tags there.
pub fn gated_refs_for_hook<'r>(hook: LockHook, refs: &'r [String], locks: &LockSet) -> Vec<&'r str> {
    gated_refs(refs, locks)
        .into_iter()
        .filter(|ref_name| match hook {
            LockHook::ReferenceTransaction => ref_name.starts_with("refs/heads/"),
            LockHook::PrePush => true,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Hook script fragments + installation
// ---------------------------------------------------------------------------

/// Shell fragment for the `reference-transaction` script, evaluated in the
/// `prepared` phase with `$state` and `$input` in scope. Fast-paths on the
/// existence of either lock store; fails CLOSED when the repo-local lock file
/// exists but the `sgit` binary is missing.
pub fn reference_transaction_lock_fragment() -> String {
    format!(
        r#"# Branch/repo lock: locked-ref updates require a biometric approval
# (`sgit lock enforce`). No off-switch; unlocking itself is biometric-gated.
if [ "$state" = "prepared" ]; then
  lockroot="$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null || git rev-parse --git-common-dir 2>/dev/null || true)"
  if [ -n "$lockroot" ] && {{ [ -f "$lockroot/{locks}" ] || [ -f "$HOME/{registry}" ]; }}; then
    if command -v sgit >/dev/null 2>&1; then
      if ! printf '%s\n' "$input" | sgit lock enforce reference-transaction; then
        echo "sgit: ref update denied by branch/repo lock (biometric approval required)." >&2
        exit 1
      fi
    elif [ -f "$lockroot/{locks}" ]; then
      echo "sgit: this repo has sgit locks but the sgit binary is not on PATH — refusing ref update." >&2
      exit 1
    fi
  fi
fi
"#,
        locks = REPO_LOCKS_FILE,
        registry = REGISTRY_RELATIVE_PATH,
    )
}

/// Shell fragment for a `pre-push` script, evaluated with `$input` (the push
/// ref lines) in scope. Same fail-closed contract as the local fragment.
pub fn pre_push_lock_fragment() -> String {
    format!(
        r#"# Branch/repo lock: pushes to locked refs require a biometric approval
# (`sgit lock enforce`). No off-switch; unlocking itself is biometric-gated.
lockroot="$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null || git rev-parse --git-common-dir 2>/dev/null || true)"
if [ -n "$input" ] && [ -n "$lockroot" ] && {{ [ -f "$lockroot/{locks}" ] || [ -f "$HOME/{registry}" ]; }}; then
  if command -v sgit >/dev/null 2>&1; then
    if ! printf '%s\n' "$input" | sgit lock enforce pre-push; then
      echo "sgit: push denied by branch/repo lock (biometric approval required)." >&2
      exit 1
    fi
  elif [ -f "$lockroot/{locks}" ]; then
    echo "sgit: this repo has sgit locks but the sgit binary is not on PATH — refusing push." >&2
    exit 1
  fi
fi
"#,
        locks = REPO_LOCKS_FILE,
        registry = REGISTRY_RELATIVE_PATH,
    )
}

/// Standalone managed `pre-push` script installed by sgit when the repo has no
/// stokd-managed hook set. Chains to a preserved prior hook
/// (`pre-push.sgit-prior`, see [`install_pre_push_hook`]) and to any
/// `stokd.hooks.priorHooksPath` hook, replaying stdin.
pub fn standalone_pre_push_script(hooks_version: u32) -> String {
    format!(
        r#"#!/bin/sh
# stokd-managed git hook — do not edit. Managed by `stokd hooks`.
# stokd-hooks-version: {version}
input="$(cat)"
{fragment}# Chain to a prior pre-push hook preserved at install time.
self_dir="$(dirname "$0")"
if [ -x "$self_dir/pre-push.sgit-prior" ]; then
  printf '%s' "$input" | "$self_dir/pre-push.sgit-prior" "$@" || exit $?
fi
# Chain to a prior pre-push hook (e.g. husky), replaying the original stdin.
prior="$(git config --get stokd.hooks.priorHooksPath 2>/dev/null || true)"
if [ -n "$prior" ]; then
  case "$prior" in
    /*) prior_dir="$prior" ;;
    *) prior_dir="$(git rev-parse --show-toplevel 2>/dev/null)/$prior" ;;
  esac
  if [ -x "$prior_dir/pre-push" ]; then
    printf '%s' "$input" | "$prior_dir/pre-push" "$@" || exit $?
  fi
fi
exit 0
"#,
        version = hooks_version,
        fragment = pre_push_lock_fragment(),
    )
}

/// Marker line identifying a managed hook script (safe to overwrite/upgrade).
const MANAGED_HOOK_MARKER: &str = "stokd-managed git hook";

/// Install (or refresh) the managed `pre-push` lock hook for the repo
/// containing `anchor`.
///
/// Clobber safety: an existing UNMANAGED `pre-push` (no managed marker — e.g.
/// a hand-written hook in the hooks dir) is preserved by renaming it to
/// `pre-push.sgit-prior`, which the managed script chains to after the lock
/// gate. A managed script (ours or a stale version) is overwritten in place.
pub fn install_pre_push_hook(anchor: &Path) -> Result<PathBuf, String> {
    let common = crate::worktree_pin::resolve_common_git_dir(anchor)
        .ok_or_else(|| format!("not a git repository: {}", anchor.display()))?;
    let hooks_dir = crate::worktree_pin::resolve_hooks_dir_for_pin(&common)?;
    std::fs::create_dir_all(&hooks_dir)
        .map_err(|error| format!("failed to create hooks dir {}: {error}", hooks_dir.display()))?;

    let script_path = hooks_dir.join("pre-push");
    let content = standalone_pre_push_script(crate::worktree_pin::PIN_HOOKS_VERSION);

    if let Ok(existing) = std::fs::read_to_string(&script_path) {
        if existing == content {
            return Ok(script_path);
        }
        if !existing.contains(MANAGED_HOOK_MARKER) {
            let prior = hooks_dir.join("pre-push.sgit-prior");
            std::fs::rename(&script_path, &prior).map_err(|error| {
                format!(
                    "failed to preserve existing pre-push hook as {}: {error}",
                    prior.display()
                )
            })?;
        }
    }
    crate::worktree_pin::write_hook_script(&script_path, &content)?;
    Ok(script_path)
}

/// Install every hook the lock system needs for the repo containing `anchor`:
/// the shared `reference-transaction` script (which carries the lock gate) and
/// the `pre-push` lock hook.
pub fn install_lock_hooks(anchor: &Path) -> Result<(), String> {
    crate::worktree_pin::install_reference_transaction_hook(anchor)?;
    install_pre_push_hook(anchor)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn lock_set_gates_exact_ref_and_wildcard() {
        let locks = LockSet::from_entries(["refs/heads/main".to_string()]);
        assert!(locks.gates_ref("refs/heads/main"));
        assert!(!locks.gates_ref("refs/heads/feature"));
        assert!(!locks.gates_ref("HEAD"), "pseudo-refs are never gated");

        let repo_wide = LockSet::from_entries([LOCK_WILDCARD.to_string()]);
        assert!(repo_wide.gates_ref("refs/heads/anything"));
        assert!(repo_wide.gates_ref("refs/tags/v1"));
        assert!(!repo_wide.gates_ref("HEAD"));
        assert!(!repo_wide.gates_ref(""));
    }

    #[test]
    fn repo_file_roundtrip_and_empty_removes() {
        let dir = tmp();
        let mut locks = LockSet::default();
        locks.insert("refs/heads/main");
        write_repo_locks(dir.path(), &locks).unwrap();
        assert_eq!(read_repo_locks(dir.path()), locks);

        locks.remove("refs/heads/main");
        write_repo_locks(dir.path(), &locks).unwrap();
        assert!(!dir.path().join(REPO_LOCKS_FILE).exists());
        assert!(read_repo_locks(dir.path()).is_empty());
    }

    #[test]
    fn registry_entry_alone_still_gates_after_repo_file_deleted() {
        let dir = tmp();
        let registry_path = dir.path().join("guard/locks.yaml");
        add_lock(dir.path(), &registry_path, "refs/heads/main").unwrap();

        // Simulate an actor wiping the repo-local file.
        std::fs::remove_file(dir.path().join(REPO_LOCKS_FILE)).unwrap();

        let effective = effective_locks(dir.path(), &registry_path);
        assert!(
            effective.gates_ref("refs/heads/main"),
            "registry must keep the lock alive without the repo file"
        );
    }

    #[test]
    fn unlock_denied_leaves_both_stores_intact() {
        let dir = tmp();
        let registry_path = dir.path().join("guard/locks.yaml");
        add_lock(dir.path(), &registry_path, "refs/heads/main").unwrap();

        let deny = |_reason: &str| Err("biometric denied".to_string());
        let result = remove_lock(dir.path(), &registry_path, "refs/heads/main", &deny);
        assert!(result.is_err(), "denied biometric must fail the unlock");
        assert!(effective_locks(dir.path(), &registry_path).gates_ref("refs/heads/main"));
        assert!(dir.path().join(REPO_LOCKS_FILE).exists());
    }

    #[test]
    fn unlock_approved_removes_from_both_stores() {
        let dir = tmp();
        let registry_path = dir.path().join("guard/locks.yaml");
        add_lock(dir.path(), &registry_path, "refs/heads/main").unwrap();

        let approve = |_reason: &str| Ok(());
        let effective =
            remove_lock(dir.path(), &registry_path, "refs/heads/main", &approve).unwrap();
        assert!(effective.is_empty());
        assert!(!dir.path().join(REPO_LOCKS_FILE).exists());
        assert!(read_registry(&registry_path).is_empty());
    }

    #[test]
    fn stdin_parsers_extract_ref_columns() {
        let rt = "0000 1111 refs/heads/main\n2222 3333 HEAD\n";
        assert_eq!(
            refs_from_reference_transaction(rt),
            vec!["refs/heads/main".to_string(), "HEAD".to_string()]
        );

        let push = "refs/heads/feature aaaa refs/heads/main bbbb\n";
        assert_eq!(refs_from_pre_push(push), vec!["refs/heads/main".to_string()]);
    }

    #[test]
    fn hook_aware_gating_scopes_wildcard_by_hook() {
        let repo_wide = LockSet::from_entries([LOCK_WILDCARD.to_string()]);
        let refs = vec![
            "refs/heads/main".to_string(),
            "refs/tags/v1".to_string(),
            "refs/remotes/origin/main".to_string(),
        ];
        // Local transactions gate branches only — `git fetch --tags` must not
        // raise a biometric prompt under a repo-wide lock.
        assert_eq!(
            gated_refs_for_hook(LockHook::ReferenceTransaction, &refs, &repo_wide),
            vec!["refs/heads/main"]
        );
        // A push is deliberate: everything is gated.
        assert_eq!(
            gated_refs_for_hook(LockHook::PrePush, &refs, &repo_wide),
            vec!["refs/heads/main", "refs/tags/v1", "refs/remotes/origin/main"]
        );
        assert_eq!(LockHook::parse("pre-push"), Some(LockHook::PrePush));
        assert_eq!(
            LockHook::parse("reference-transaction"),
            Some(LockHook::ReferenceTransaction)
        );
        assert_eq!(LockHook::parse("post-commit"), None);
    }

    #[test]
    fn fragments_invoke_enforce_and_fail_closed_without_binary() {
        for fragment in [
            reference_transaction_lock_fragment(),
            pre_push_lock_fragment(),
        ] {
            assert!(fragment.contains("sgit lock enforce"));
            assert!(fragment.contains("command -v sgit"), "must probe for the binary");
            assert!(
                fragment.contains("not on PATH") && fragment.contains("exit 1"),
                "missing binary with a repo lock file must fail closed"
            );
            // No off-switch may be advertised (mirrors the pin refusal contract).
            for leak in ["--off", "unlock with", "to bypass"] {
                assert!(!fragment.contains(leak), "fragment leaks an off-switch: {leak}");
            }
        }
        let standalone = standalone_pre_push_script(1);
        assert!(standalone.contains(MANAGED_HOOK_MARKER));
        assert!(standalone.contains("pre-push.sgit-prior"), "must chain to a preserved prior hook");
    }

    fn scratch_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tmp();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .expect("git runs")
                .status
                .success();
            assert!(ok, "git {args:?} failed");
        };
        run(&["init", "-q", "-b", "main"]);
        (dir, repo)
    }

    #[test]
    fn install_pre_push_preserves_unmanaged_hook() {
        let (_dir, repo) = scratch_repo();
        let hooks = repo.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(hooks.join("pre-push"), "#!/bin/sh\necho custom\n").unwrap();

        let installed = install_pre_push_hook(&repo).unwrap();
        let content = std::fs::read_to_string(&installed).unwrap();
        assert!(content.contains(MANAGED_HOOK_MARKER));
        assert!(content.contains("sgit lock enforce"));
        assert_eq!(
            std::fs::read_to_string(hooks.join("pre-push.sgit-prior")).unwrap(),
            "#!/bin/sh\necho custom\n",
            "unmanaged hook must be preserved, never clobbered"
        );

        // Re-install is idempotent and does not stack another prior rename.
        install_pre_push_hook(&repo).unwrap();
        assert!(std::fs::read_to_string(hooks.join("pre-push.sgit-prior"))
            .unwrap()
            .contains("echo custom"));
    }

    /// End-to-end: with the managed hooks installed and a locked branch, a
    /// commit is denied when `sgit lock enforce` denies — INCLUDING with
    /// `--no-verify` (the reference-transaction hook is not skippable) — and
    /// allowed when enforce approves. A fake `sgit` shim on PATH stands in for
    /// the real binary so no biometric prompt is raised.
    #[cfg(unix)]
    #[test]
    fn locked_branch_commit_denied_then_allowed_end_to_end() {
        use std::os::unix::fs::PermissionsExt;

        let (dir, repo) = scratch_repo();
        let run = |args: &[&str], envs: &[(&str, &str)]| {
            let mut cmd = std::process::Command::new("git");
            cmd.arg("-C").arg(&repo).args(args);
            for (k, v) in envs {
                cmd.env(k, v);
            }
            cmd.output().expect("git runs")
        };
        assert!(run(&["config", "user.email", "t@t.co"], &[]).status.success());
        assert!(run(&["config", "user.name", "t"], &[]).status.success());
        assert!(run(&["commit", "-q", "--allow-empty", "-m", "init"], &[]).status.success());

        // Install the managed hooks and lock the branch (repo file only — the
        // scratch HOME below has no registry).
        install_lock_hooks(&repo).unwrap();
        let common = crate::worktree_pin::resolve_common_git_dir(&repo).unwrap();
        let mut locks = LockSet::default();
        locks.insert("refs/heads/main");
        write_repo_locks(&common, &locks).unwrap();

        // Fake `sgit` shim: SGIT_FAKE_VERDICT decides allow (0) or deny (1).
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let shim = bin.join("sgit");
        std::fs::write(&shim, "#!/bin/sh\ncat >/dev/null\nexit \"${SGIT_FAKE_VERDICT:-1}\"\n")
            .unwrap();
        let mut perms = std::fs::metadata(&shim).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&shim, perms).unwrap();

        let path_env = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let scratch_home = dir.path().join("home");
        std::fs::create_dir_all(&scratch_home).unwrap();
        let home_env = scratch_home.to_string_lossy().to_string();

        // Denied biometric → commit refused.
        let denied = run(
            &["commit", "-q", "--allow-empty", "-m", "locked"],
            &[("PATH", &path_env), ("HOME", &home_env), ("SGIT_FAKE_VERDICT", "1")],
        );
        assert!(!denied.status.success(), "denied enforce must block the commit");

        // `--no-verify` must NOT bypass the gate.
        let no_verify = run(
            &["commit", "-q", "--no-verify", "--allow-empty", "-m", "locked"],
            &[("PATH", &path_env), ("HOME", &home_env), ("SGIT_FAKE_VERDICT", "1")],
        );
        assert!(
            !no_verify.status.success(),
            "--no-verify must not bypass the reference-transaction lock gate"
        );

        // Approved biometric → commit succeeds.
        let approved = run(
            &["commit", "-q", "--allow-empty", "-m", "locked"],
            &[("PATH", &path_env), ("HOME", &home_env), ("SGIT_FAKE_VERDICT", "0")],
        );
        assert!(
            approved.status.success(),
            "approved enforce must allow the commit: {}",
            String::from_utf8_lossy(&approved.stderr)
        );

        // Missing binary + repo lock file → fail closed.
        let no_binary = run(
            &["commit", "-q", "--allow-empty", "-m", "locked"],
            &[("PATH", "/usr/bin:/bin"), ("HOME", &home_env)],
        );
        assert!(
            !no_binary.status.success(),
            "a repo with locks but no sgit binary must refuse ref updates"
        );
    }

    #[test]
    fn gated_refs_filters_by_lock_set() {
        let locks = LockSet::from_entries(["refs/heads/main".to_string()]);
        let refs = vec![
            "refs/heads/main".to_string(),
            "refs/heads/feature".to_string(),
            "HEAD".to_string(),
        ];
        assert_eq!(gated_refs(&refs, &locks), vec!["refs/heads/main"]);

        let repo_wide = LockSet::from_entries([LOCK_WILDCARD.to_string()]);
        assert_eq!(
            gated_refs(&refs, &repo_wide),
            vec!["refs/heads/main", "refs/heads/feature"]
        );
    }
}
