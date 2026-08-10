//! `sgit shove` — stage/commit/push via sgit-core with a shell/`$EDITOR`
//! [`ConflictResolver`] (VAL-SGIT-SHOVE-003 / VAL-SGIT-SHOVE-NOOP-005).

use std::process::Command;

use sgit_core::{
    detect_repo_root_at, shove as run_shove, ConflictContext, ConflictResolver, ShoveOptions,
};

/// Generic shell/`$EDITOR` conflict resolver for standalone `sgit shove`.
///
/// Env knobs (for tests and operators):
/// - `SGIT_CONFLICT_EDITOR` — preferred over `$GIT_EDITOR` / `$EDITOR`
/// - `SGIT_RESOLVER_MARKER` — when set, path written on every `resolve()` call
///   (proves the resolver was invoked; absent on the no-conflict fast path)
pub struct EditorConflictResolver;

impl ConflictResolver for EditorConflictResolver {
    fn resolve(&self, ctx: &ConflictContext) -> Result<(), String> {
        // Invocation marker for self-check / VAL-SGIT-SHOVE-003 evidence.
        if let Ok(marker) = std::env::var("SGIT_RESOLVER_MARKER") {
            if !marker.is_empty() {
                std::fs::write(&marker, "sgit-conflict-resolver-invoked\n")
                    .map_err(|e| format!("failed to write SGIT_RESOLVER_MARKER ({marker}): {e}"))?;
            }
        }

        let editor = std::env::var("SGIT_CONFLICT_EDITOR")
            .or_else(|_| std::env::var("GIT_EDITOR"))
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".to_string());

        println!(
            "sgit shove: resolving {} conflicted file(s) via editor ({editor})...",
            ctx.unmerged_files.len()
        );

        for rel in &ctx.unmerged_files {
            let path = ctx.repo_root.join(rel);
            let status = Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "{} {}",
                    editor,
                    shell_quote(&path.to_string_lossy())
                ))
                .current_dir(&ctx.repo_root)
                .status()
                .map_err(|e| format!("failed to launch conflict editor `{editor}`: {e}"))?;
            if !status.success() {
                return Err(format!(
                    "conflict editor `{editor}` exited with status {} for {rel}",
                    status.code().unwrap_or(-1)
                ));
            }
        }
        Ok(())
    }
}

fn shell_quote(s: &str) -> String {
    // Single-quote for POSIX sh; escape embedded single quotes.
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

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
