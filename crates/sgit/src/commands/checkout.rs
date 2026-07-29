//! `sgit checkout <branch>` — pin-safe sibling worktree ensure + path print.
//!
//! Prints ONLY the absolute worktree path to stdout (shell wrapper does the real
//! `cd`). Diagnostics go to stderr; failures exit non-zero with empty stdout.

use sgit_core::{ensure_branch_worktree, load_repositories_config};

/// Entry point for `sgit checkout <branch>`.
pub fn run(branch: &str) {
    let cfg = match load_repositories_config() {
        Ok((cfg, _)) => cfg,
        Err(e) => {
            eprintln!("error: failed to load config: {e}");
            std::process::exit(1);
        }
    };

    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: cannot resolve cwd: {e}");
            std::process::exit(1);
        }
    };

    match ensure_branch_worktree(&cwd, branch, &cfg) {
        Ok(result) => {
            if result.created {
                eprintln!(
                    "sgit checkout: created worktree for '{branch}' ({})",
                    result.source
                );
            }
            println!("{}", result.path.display());
        }
        Err(e) => {
            eprintln!("sgit checkout: {e}");
            std::process::exit(1);
        }
    }
}
