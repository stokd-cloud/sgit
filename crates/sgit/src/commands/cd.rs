//! `sgit cd <owner/repo | repo | task/* | project/*> [ref]`
//!
//! Prints ONLY the absolute worktree path to stdout (shell wrapper does the real
//! `cd`). Diagnostics go to stderr; failures exit non-zero with empty stdout.
//!
//! Generic repo/branch resolution is pure filesystem + local git via sgit-core.
//! Task/project refs optionally delegate to an external argv resolver
//! (`SGIT_REF_RESOLVER`, D001) — no compile-time stokd dependency.

use std::path::{Path, PathBuf};
use std::process::Command;

use sgit_core::{
    is_task_or_project_ref, load_repositories_config, resolve_worktree_path, RepositoriesConfig,
};

/// Env var naming an external resolver command (whitespace-split argv).
/// Invoked as: `<argv...> <ref>` with stdout = absolute worktree path.
pub const SGIT_REF_RESOLVER_ENV: &str = "SGIT_REF_RESOLVER";

/// Entry point for `sgit cd`.
pub fn run(target: Option<String>, git_ref: Option<String>) {
    let Some(target) = target.filter(|t| !t.trim().is_empty()) else {
        eprintln!("sgit cd: missing target; expected <owner/repo>, <repo>, task/<hash>, or project/<hash>");
        std::process::exit(1);
    };

    match resolve(&target, git_ref.as_deref()) {
        Ok(path) => println!("{}", path.display()),
        Err(error) => {
            eprintln!("sgit cd: {error}");
            std::process::exit(1);
        }
    }
}

fn resolve(target: &str, reference: Option<&str>) -> Result<PathBuf, String> {
    // task/* and project/* as the primary target always go through the external
    // resolver seam (D001). Generic owner/repo never needs it.
    if is_task_or_project_ref(target) {
        return resolve_via_external_resolver(target);
    }

    let cfg = load_cfg();
    let worktree_root = PathBuf::from(&cfg.worktree_root);

    match resolve_worktree_path(&worktree_root, target, reference) {
        Ok(path) => Ok(path),
        Err(disk_err) => {
            // Optional: if the second arg is a task/project ref that disk missed,
            // try the external resolver once before surfacing the disk error.
            if let Some(r) = reference {
                if is_task_or_project_ref(r) {
                    return resolve_via_external_resolver(r).map_err(|resolver_err| {
                        format!("{disk_err}; external resolver also failed: {resolver_err}")
                    });
                }
            }
            Err(disk_err)
        }
    }
}

fn load_cfg() -> RepositoriesConfig {
    load_repositories_config()
        .unwrap_or_else(|e| {
            eprintln!("error: failed to load config: {e}");
            std::process::exit(1);
        })
        .0
}

/// Discover and invoke the optional external ref resolver.
///
/// Discovery: `SGIT_REF_RESOLVER` env (whitespace-split argv). When unset or
/// empty, return a clear actionable error so generic repo/branch cd still works
/// for other invocations while task/project refs fail loudly.
fn resolve_via_external_resolver(reference: &str) -> Result<PathBuf, String> {
    let argv = match discover_resolver_argv() {
        Some(argv) => argv,
        None => {
            return Err(format!(
                "ref '{reference}' requires an external resolver (task/project refs are not resolved by sgit itself). \
                 Set {SGIT_REF_RESOLVER_ENV} to an executable argv that prints a worktree path \
                 (e.g. export {SGIT_REF_RESOLVER_ENV}='stokd sgit-resolve-ref'), or use \
                 `sgit cd <owner/repo> [branch]` for generic filesystem resolution."
            ));
        }
    };

    invoke_resolver(&argv, reference)
}

fn discover_resolver_argv() -> Option<Vec<String>> {
    let raw = std::env::var(SGIT_REF_RESOLVER_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parts: Vec<String> = trimmed.split_whitespace().map(|s| s.to_string()).collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts)
    }
}

fn invoke_resolver(argv: &[String], reference: &str) -> Result<PathBuf, String> {
    let (prog, args_prefix) = argv
        .split_first()
        .ok_or_else(|| format!("{SGIT_REF_RESOLVER_ENV} is empty"))?;

    let output = Command::new(prog)
        .args(args_prefix)
        .arg(reference)
        .output()
        .map_err(|e| {
            format!(
                "failed to invoke external resolver `{prog}` (from {SGIT_REF_RESOLVER_ENV}): {e}"
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else if !stdout.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            format!("exit {}", output.status.code().unwrap_or(-1))
        };
        return Err(format!(
            "external resolver failed for '{reference}': {detail}"
        ));
    }

    let path_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path_str.is_empty() {
        return Err(format!(
            "external resolver printed no path for '{reference}'"
        ));
    }
    // Resolver may emit a trailing newline only; take first non-empty line.
    let first_line = path_str.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        return Err(format!(
            "external resolver printed no path for '{reference}'"
        ));
    }
    let path = PathBuf::from(first_line);
    if !path.is_absolute() {
        // Accept relative paths by canonicalizing against cwd when possible.
        let abs = std::fs::canonicalize(&path).unwrap_or(path);
        return Ok(abs);
    }
    // Prefer real path when it exists; keep as-is otherwise so stubs can print
    // synthetic paths without requiring the directory to exist.
    Ok(if Path::new(&path).exists() {
        std::fs::canonicalize(&path).unwrap_or(path)
    } else {
        path
    })
}
