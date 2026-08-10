//! `sgit worktree clean` and `sgit worktree pin` CLI surfaces over sgit-core.

use std::path::PathBuf;

use sgit_core::{discover_bare_repos, ensure_pin_and_hook, load_repositories_config, run_clean_at};

/// `sgit worktree clean [--dry-run]` — remove landed/merged linked worktrees.
pub fn run_clean(dry_run: bool) {
    let cwd = std::env::current_dir().unwrap_or_else(|e| {
        eprintln!("error: cannot resolve cwd: {e}");
        std::process::exit(1);
    });
    match run_clean_at(&cwd, dry_run) {
        Ok(_) => {}
        Err(e) => {
            eprintln!("sgit worktree clean: {e}");
            std::process::exit(1);
        }
    }
}

/// `sgit worktree pin [--all] [--off] [--json]` — install pin hook + markers.
pub fn run_pin(all: bool, off: bool, json: bool) {
    let anchors: Vec<PathBuf> = if all {
        let cfg = load_repositories_config()
            .unwrap_or_else(|e| {
                eprintln!("error: failed to load config: {e}");
                std::process::exit(1);
            })
            .0;
        let found = discover_bare_repos(std::path::Path::new(&cfg.bare_root));
        if found.is_empty() {
            eprintln!(
                "sgit worktree pin: no bare repos found under {}",
                cfg.bare_root
            );
            std::process::exit(1);
        }
        found
    } else {
        vec![std::env::current_dir().unwrap_or_else(|e| {
            eprintln!("error: cannot resolve cwd: {e}");
            std::process::exit(1);
        })]
    };

    let mut results: Vec<(PathBuf, sgit_core::ReconcileResult)> = Vec::new();
    for anchor in &anchors {
        match ensure_pin_and_hook(anchor, off) {
            Ok(r) => results.push((anchor.clone(), r)),
            Err(e) => {
                eprintln!("sgit worktree pin: failed for {}: {e}", anchor.display());
                std::process::exit(1);
            }
        }
    }

    if json {
        let arr: Vec<serde_json::Value> = results
            .iter()
            .map(|(a, r)| {
                serde_json::json!({
                    "repo": a.display().to_string(),
                    "pinned": r.pinned.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                    "unpinned": r.unpinned.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                    "skipped": r.skipped.iter().map(|(p, why)| serde_json::json!({
                        "path": p.display().to_string(),
                        "reason": why,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
        return;
    }

    let verb = if off { "Unpinned" } else { "Pinned" };
    let mut total = 0usize;
    for (_a, r) in &results {
        let done = if off { &r.unpinned } else { &r.pinned };
        for wt in done {
            println!("{verb}: {}", wt.display());
            total += 1;
        }
        for (wt, why) in &r.skipped {
            println!("Skipped {}: {why}", wt.display());
        }
    }
    println!(
        "{verb} {total} worktree(s) across {} repo(s).",
        results.len()
    );
}
