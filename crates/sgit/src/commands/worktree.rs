//! `sgit worktree clean` and `sgit worktree pin` CLI surfaces over sgit-core.

use std::path::PathBuf;

use sgit_core::{
    discover_bare_repos, ensure_pin_and_hook, load_repositories_config, run_clean_at,
};
use sgit_core::worktree_pin::{pin_status, PinState};

/// Anchors to operate on: every bare repo under bareRoot with `--all`, else cwd.
fn pin_anchors(all: bool) -> Vec<PathBuf> {
    if all {
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
    }
}

/// `sgit worktree pin --status [--all] [--json]` — read-only audit; exits 1
/// when any worktree is off its pin (mismatch, healable, or stuck).
pub fn run_pin_status(all: bool, json: bool) {
    let mut drift = false;
    let mut out_rows: Vec<serde_json::Value> = Vec::new();
    for anchor in pin_anchors(all) {
        let rows = pin_status(&anchor);
        if rows.is_empty() {
            continue;
        }
        if !json {
            println!("{}", anchor.display());
        }
        for row in rows {
            let (label, is_drift) = match &row.state {
                PinState::AttachedOk => ("ok".to_string(), false),
                PinState::AttachedMismatch { on } => (format!("MISMATCH (on {on})"), true),
                PinState::DetachedBusy => ("detached: operation in progress".to_string(), false),
                PinState::DetachedHealable => {
                    ("DETACHED at pin tip (run `sgit worktree pin` to reattach)".to_string(), true)
                }
                PinState::DetachedStuck => {
                    ("DETACHED away from pin (needs manual review)".to_string(), true)
                }
                PinState::Unpinned => ("unpinned".to_string(), false),
            };
            drift = drift || is_drift;
            if json {
                out_rows.push(serde_json::json!({
                    "repo": anchor.display().to_string(),
                    "path": row.path.display().to_string(),
                    "pin": row.marker,
                    "state": label,
                    "drift": is_drift,
                }));
            } else {
                println!(
                    "  {:<60} pin={:<40} {}",
                    row.path.display(),
                    row.marker.as_deref().unwrap_or("—"),
                    label
                );
            }
        }
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&out_rows).unwrap_or_default()
        );
    }
    if drift {
        std::process::exit(1);
    }
}

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
    let anchors: Vec<PathBuf> = pin_anchors(all);

    let mut results: Vec<(PathBuf, sgit_core::ReconcileResult)> = Vec::new();
    for anchor in &anchors {
        match ensure_pin_and_hook(anchor, off) {
            Ok(r) => results.push((anchor.clone(), r)),
            Err(e) => {
                eprintln!(
                    "sgit worktree pin: failed for {}: {e}",
                    anchor.display()
                );
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
                    "reattached": r.reattached.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
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
        for wt in &r.reattached {
            println!("Reattached: {}", wt.display());
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
