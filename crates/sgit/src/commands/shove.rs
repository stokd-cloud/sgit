//! `sgit shove` — stage/commit/push via sgit-core with a shell/`$EDITOR`
//! [`ConflictResolver`] (VAL-SGIT-SHOVE-003 / VAL-SGIT-SHOVE-NOOP-005).
//!
//! The resolver itself lives in [`crate::commands::resolver`] so `sgit pull`
//! resolves conflicts exactly the same way.

use sgit_core::{detect_repo_root_at, shove as run_shove, ShoveOptions};

use crate::commands::resolver::EditorConflictResolver;

/// Run `sgit shove` in the current working directory's git repository.
pub fn run(message: Option<String>) {
    let cwd = std::env::current_dir().unwrap_or_else(|e| {
        eprintln!("error: cannot resolve cwd: {e}");
        std::process::exit(1);
    });
    let repo_root = detect_repo_root_at(&cwd).unwrap_or_else(|| {
        eprintln!("error: not inside a git repository: {}", cwd.display());
        std::process::exit(1);
    });
    let opts = ShoveOptions {
        message,
        verbose_skip: true,
    };
    let resolver = EditorConflictResolver;
    if let Err(e) = run_shove(&repo_root, &opts, &resolver) {
        eprintln!("sgit shove: {e}");
        std::process::exit(1);
    }
}
