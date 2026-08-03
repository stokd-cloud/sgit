//! `sgit worktree lock`, `sgit repo lock`, and the hidden `sgit lock enforce`
//! plumbing invoked by the managed hooks (AX-SGIT-LOCK-BIOMETRIC-GATE).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use sgit_core::{
    add_lock, default_registry_path, effective_locks, gated_refs_for_hook, install_lock_hooks,
    load_repositories_config, refs_from_pre_push, refs_from_reference_transaction, remove_lock,
    require_biometric, resolve_common_git_dir, resolve_repo_layout, LockHook, LOCK_WILDCARD,
};

fn exit_with(message: &str) -> ! {
    eprintln!("sgit lock: {message}");
    std::process::exit(1);
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|error| exit_with(&format!("cannot resolve cwd: {error}")))
}

fn registry_path() -> PathBuf {
    default_registry_path()
        .unwrap_or_else(|| exit_with("cannot resolve home directory for the lock registry"))
}

/// Resolve the target repo's common git dir: an explicit `--repo owner/name`
/// resolves through the configured bare layout; otherwise the repo containing
/// the current directory.
fn resolve_target_common(repo: Option<&str>) -> PathBuf {
    match repo {
        Some(slug) => {
            let (owner, name) = slug
                .split_once('/')
                .unwrap_or_else(|| exit_with(&format!("--repo expects owner/repo, got '{slug}'")));
            let (cfg, _source) = load_repositories_config()
                .unwrap_or_else(|error| exit_with(&format!("failed to load config: {error}")));
            let layout = resolve_repo_layout(&cfg, owner, name);
            if !layout.bare_dir.is_dir() {
                exit_with(&format!(
                    "no local bare repo for {slug} at {} — clone it first (`sgit clone {slug}`)",
                    layout.bare_dir.display()
                ));
            }
            resolve_common_git_dir(&layout.bare_dir)
                .unwrap_or_else(|| exit_with(&format!("not a git repository: {}", layout.bare_dir.display())))
        }
        None => resolve_common_git_dir(&cwd())
            .unwrap_or_else(|| exit_with("not inside a git repository (or pass --repo owner/name)")),
    }
}

/// The current branch of the working directory, for the no-flag default.
fn current_branch() -> Option<String> {
    let out = Command::new("git")
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// Normalize a branch argument to a full ref.
fn branch_to_ref(branch: &str) -> String {
    if branch.starts_with("refs/") {
        branch.to_string()
    } else {
        format!("refs/heads/{branch}")
    }
}

fn report(common: &Path, entry: &str, off: bool, json: bool) {
    let effective = effective_locks(common, &registry_path());
    if json {
        let payload = serde_json::json!({
            "repo": common.display().to_string(),
            "entry": entry,
            "locked": !off,
            "effective_locks": effective.entries().collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
        return;
    }
    let what = if entry == LOCK_WILDCARD {
        "repo (all refs)".to_string()
    } else {
        format!("'{entry}'")
    };
    if off {
        println!("Unlocked {what} in {}", common.display());
    } else {
        println!(
            "Locked {what} in {} — commits and pushes to it now require a biometric approval.",
            common.display()
        );
    }
}

/// `sgit worktree lock [--off] [--repo o/r] [--branch b] [--json]`
pub fn run_worktree_lock(off: bool, repo: Option<String>, branch: Option<String>, json: bool) {
    let common = resolve_target_common(repo.as_deref());
    let branch = branch
        .or_else(|| {
            if repo.is_some() {
                None // an explicit --repo needs an explicit --branch
            } else {
                current_branch()
            }
        })
        .unwrap_or_else(|| {
            exit_with("cannot infer a branch — pass --branch <name> (required with --repo)")
        });
    let entry = branch_to_ref(&branch);
    apply(&common, &entry, off);
    report(&common, &entry, off, json);
}

/// `sgit repo lock [--off] [--repo o/r] [--json]` — repo-wide lock.
pub fn run_repo_lock(off: bool, repo: Option<String>, json: bool) {
    let common = resolve_target_common(repo.as_deref());
    apply(&common, LOCK_WILDCARD, off);
    report(&common, LOCK_WILDCARD, off, json);
}

fn apply(common: &Path, entry: &str, off: bool) {
    if off {
        // Unlocking is the dangerous direction: biometric-gated, fail closed.
        remove_lock(common, &registry_path(), entry, &|reason| {
            require_biometric(reason)
        })
        .unwrap_or_else(|error| exit_with(&error));
    } else {
        add_lock(common, &registry_path(), entry).unwrap_or_else(|error| exit_with(&error));
        install_lock_hooks(common).unwrap_or_else(|error| exit_with(&error));
    }
}

/// `sgit lock list [--json]`
pub fn run_list(json: bool) {
    let common = resolve_target_common(None);
    let effective = effective_locks(&common, &registry_path());
    if json {
        let payload = serde_json::json!({
            "repo": common.display().to_string(),
            "effective_locks": effective.entries().collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
        return;
    }
    if effective.is_empty() {
        println!("No locks for {}", common.display());
        return;
    }
    println!("Locks for {}:", common.display());
    for entry in effective.entries() {
        if entry == LOCK_WILDCARD {
            println!("  * (repo-wide — every ref)");
        } else {
            println!("  {entry}");
        }
    }
}

/// `sgit lock enforce <hook>` — hidden plumbing called by the managed hooks
/// with the hook's stdin. Exit 0 allows the operation; anything else denies.
/// FAIL CLOSED on every unresolvable state.
pub fn run_enforce(hook: &str) {
    let Some(hook) = LockHook::parse(hook) else {
        exit_with(&format!("unknown hook '{hook}'"));
    };

    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        exit_with("failed to read hook input — denying");
    }

    let Some(common) = resolve_common_git_dir(&cwd()) else {
        exit_with("cannot resolve the repository — denying");
    };

    let locks = effective_locks(&common, &registry_path());
    if locks.is_empty() {
        return; // nothing locked here — allow
    }

    let refs = match hook {
        LockHook::ReferenceTransaction => refs_from_reference_transaction(&input),
        LockHook::PrePush => refs_from_pre_push(&input),
    };
    let gated = gated_refs_for_hook(hook, &refs, &locks);
    if gated.is_empty() {
        return; // no locked ref touched — allow
    }

    let action = match hook {
        LockHook::ReferenceTransaction => "update locked ref(s)",
        LockHook::PrePush => "push to locked ref(s)",
    };
    let reason = format!("{action} {}", gated.join(", "));
    if let Err(error) = require_biometric(&reason) {
        exit_with(&error);
    }
}
