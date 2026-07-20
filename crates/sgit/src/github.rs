//! GitHub helpers for `sgit repo create` / `repo rename` (HTTP + `gh` CLI).
//!
//! Kept in the binary crate (not sgit-core) so the core library stays free of
//! HTTP clients. Token resolution mirrors stokd: `GITHUB_TOKEN` → `gh auth token`.

use std::process::Command;

/// Resolve a GitHub token: `GITHUB_TOKEN` env, then `gh auth token`.
pub fn resolve_github_token() -> Option<String> {
    if let Ok(t) = std::env::var("GITHUB_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    let output = Command::new("gh").args(["auth", "token"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// Create a GitHub repository. Returns whether the created repo is private.
///
/// Parity with stokd `create_github_repo` (apps/cli/src/commands/repo.rs:881):
/// org vs user detection, private-with-public-fallback, auto_init flag.
/// Deliberate divergence: User-Agent is `sgit` (not `stokd-cli`).
pub fn create_github_repo(
    token: &str,
    owner: &str,
    repo_name: &str,
    auto_init: bool,
    prefer_public: bool,
) -> bool {
    let client = reqwest::blocking::Client::builder()
        .use_rustls_tls()
        .build()
        .unwrap_or_else(|e| {
            eprintln!("error: failed to build HTTP client: {e}");
            std::process::exit(1);
        });

    let (org_url, user_url, is_org) = {
        let org_url = format!("https://api.github.com/orgs/{owner}/repos");
        let user_url = "https://api.github.com/user/repos".to_string();
        let org_check = client
            .get(format!("https://api.github.com/orgs/{owner}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("User-Agent", "sgit")
            .header("Accept", "application/vnd.github+json")
            .send();
        let is_org = org_check.map(|r| r.status().is_success()).unwrap_or(false);
        (org_url, user_url, is_org)
    };

    let url = if is_org { &org_url } else { &user_url };

    let mut is_private = !prefer_public;
    let mut body = serde_json::json!({
        "name": repo_name,
        "private": is_private,
        "auto_init": auto_init,
    });

    let mut response = client
        .post(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", "sgit")
        .header("Accept", "application/vnd.github+json")
        .json(&body)
        .send()
        .unwrap_or_else(|e| {
            eprintln!("error: failed to create repository: {e}");
            std::process::exit(1);
        });

    let mut status = response.status();

    if !prefer_public && !status.is_success() {
        let body_text = response.text().unwrap_or_default();
        if status == reqwest::StatusCode::UNPROCESSABLE_ENTITY && body_text.contains("private") {
            println!("warning: failed to create private repository, trying public...");
            is_private = false;
            body = serde_json::json!({
                "name": repo_name,
                "private": is_private,
                "auto_init": auto_init,
            });
            response = client
                .post(url)
                .header("Authorization", format!("Bearer {token}"))
                .header("User-Agent", "sgit")
                .header("Accept", "application/vnd.github+json")
                .json(&body)
                .send()
                .unwrap_or_else(|e| {
                    eprintln!("error: failed to create repository (fallback): {e}");
                    std::process::exit(1);
                });
            status = response.status();
        } else {
            if status == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
                if body_text.contains("already exists") {
                    eprintln!("error: repository {owner}/{repo_name} already exists on GitHub");
                    std::process::exit(1);
                }
                eprintln!("error: GitHub API returned 422: {body_text}");
                std::process::exit(1);
            }
            eprintln!("error: GitHub API returned HTTP {status}: {body_text}");
            std::process::exit(1);
        }
    }

    if !status.is_success() {
        let body_text = response.text().unwrap_or_default();
        eprintln!("error: GitHub API returned HTTP {status}: {body_text}");
        std::process::exit(1);
    }

    is_private
}

pub fn set_default_branch(token: &str, owner: &str, repo_name: &str) {
    let client = reqwest::blocking::Client::builder()
        .use_rustls_tls()
        .build()
        .unwrap_or_else(|e| {
            eprintln!("error: failed to build HTTP client: {e}");
            std::process::exit(1);
        });

    let url = format!("https://api.github.com/repos/{owner}/{repo_name}");
    let body = serde_json::json!({ "default_branch": "main" });

    let response = match client
        .patch(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", "sgit")
        .header("Accept", "application/vnd.github+json")
        .json(&body)
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("warning: failed to set default branch: {e}");
            return;
        }
    };

    if !response.status().is_success() {
        eprintln!(
            "warning: failed to set default branch (HTTP {})",
            response.status()
        );
    }
}
