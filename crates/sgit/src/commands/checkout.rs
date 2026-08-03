//! `sgit checkout <branch | owner/repo | reponame>` — pin-safe sibling worktree
//! or ensure-repo main worktree + path print.
//!
//! Prints ONLY the absolute worktree path to stdout (shell wrapper does the real
//! `cd`). Diagnostics go to stderr; failures exit non-zero with empty stdout.

use sgit_core::{
    classify_checkout_target_with_cfg, describe_owner_resolution_failure, detect_repo_root_at,
    ensure_branch_worktree, ensure_repo_main_worktree, load_repositories_config,
    local_owners_for_repo, parse_repo_spec, resolve_owner_chain, CheckoutKind, OwnerResolution,
    RepositoriesConfig, RepoSpec,
};

use crate::github::{github_owner_chain, owners_with_remote_repo, resolve_github_token};

/// Entry point for `sgit checkout <target>`.
pub fn run(target: &str) {
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

    let repo_root = detect_repo_root_at(&cwd);
    let kind = classify_checkout_target_with_cfg(target, repo_root.as_deref(), Some(&cfg));

    match kind {
        CheckoutKind::Branch(branch) => run_branch(&cwd, &branch, &cfg),
        CheckoutKind::RepoSpec(spec) => {
            if spec.trim().is_empty() {
                eprintln!("sgit checkout: empty target");
                std::process::exit(1);
            }
            match try_repo(&spec, &cfg) {
                Ok(()) => {}
                Err(repo_err) => {
                    // Fall back to branch create when inside a git repo so
                    // ambiguous names (bare or owner/repo-shaped) still work
                    // as sibling worktrees when the remote/layout is missing.
                    if repo_root.is_some() {
                        eprintln!(
                            "sgit checkout: repo resolve failed ({repo_err}); treating '{spec}' as a branch"
                        );
                        run_branch(&cwd, &spec, &cfg);
                    } else {
                        eprintln!("sgit checkout: {repo_err}");
                        std::process::exit(1);
                    }
                }
            }
        }
    }
}

fn run_branch(cwd: &std::path::Path, branch: &str, cfg: &RepositoriesConfig) {
    match ensure_branch_worktree(cwd, branch, cfg) {
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

fn try_repo(spec: &str, cfg: &RepositoriesConfig) -> Result<(), String> {
    let (owner, repo_name) = resolve_repo_arg(spec, cfg)?;
    let result = ensure_repo_main_worktree(&owner, &repo_name, cfg)?;
    if result.created {
        eprintln!(
            "sgit checkout: ensured {owner}/{repo_name} ({})",
            result.source
        );
    }
    println!("{}", result.path.display());
    Ok(())
}

/// Resolve `owner/repo` or bare `repo` the same way clone/open do.
fn resolve_repo_arg(repo_spec: &str, cfg: &RepositoriesConfig) -> Result<(String, String), String> {
    let repo = match parse_repo_spec(repo_spec)? {
        RepoSpec::OwnerRepo { owner, repo } => return Ok((owner, repo)),
        RepoSpec::BareName { repo } => repo,
    };

    let local = local_owners_for_repo(cfg, &repo);
    let remote = if local.is_empty() {
        resolve_github_token()
            .map(|token| owners_with_remote_repo(&token, &github_owner_chain(&token), &repo))
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let resolution = resolve_owner_chain(&local, &remote);
    if let Some(message) = describe_owner_resolution_failure(&repo, &resolution) {
        return Err(message);
    }
    let OwnerResolution::Resolved { owner, source } = resolution else {
        return Err(format!("could not resolve owner for '{repo}'"));
    };
    eprintln!(
        "# resolved '{repo}' to {owner}/{repo} via {}",
        source.label()
    );
    let _ = cfg;
    Ok((owner, repo))
}
