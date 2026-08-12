//! `sgit pull` — the fast-forward → rebase → merge escalation ladder from
//! [`sgit_core::pull`], with the editor-backed conflict resolver.

use sgit_core::{detect_repo_root_at, pull as core_pull, PullOptions, PullOutcome};

use crate::commands::resolver::EditorConflictResolver;

/// Run `sgit pull` in the current working directory's git repository.
pub fn run(ff_only: bool, no_rebase: bool) {
    let cwd = std::env::current_dir().unwrap_or_else(|e| {
        eprintln!("error: cannot resolve cwd: {e}");
        std::process::exit(1);
    });
    let repo_root = detect_repo_root_at(&cwd).unwrap_or_else(|| {
        eprintln!("error: not inside a git repository: {}", cwd.display());
        std::process::exit(1);
    });
    let opts = PullOptions { ff_only, no_rebase };
    let resolver = EditorConflictResolver;
    match core_pull(&repo_root, &opts, &resolver) {
        Ok(PullOutcome::AlreadyUpToDate) => {}
        Ok(PullOutcome::FastForwarded) => println!("sgit pull: fast-forwarded."),
        Ok(PullOutcome::Rebased) => println!("sgit pull: rebased onto the remote."),
        Ok(PullOutcome::Merged) => println!("sgit pull: merged the remote."),
        Err(e) => {
            eprintln!("sgit pull: {e}");
            std::process::exit(1);
        }
    }
}
