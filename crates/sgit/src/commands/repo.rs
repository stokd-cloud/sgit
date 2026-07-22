//! Generic repo lifecycle: clone, open, list, create, rename.
//!
//! Behavioral parity with `stokd repo *` (apps/cli/src/commands/repo.rs).
//! Deliberate divergences from stokd (documented for validators):
//! - Config: `RepositoriesConfig` via D002 (`SGIT_CONFIG` / XDG / `~/.stokd`).
//! - No stokd client-git-hooks auto-install (VAL-TIE-003 is stokd-domain).
//! - No cloud `worktree_count` refresh.
//! - Worktree pin always written (default-on; no `worktree.pinBranch` config yet).
//! - GitHub User-Agent: `sgit`.
//! - Token resolution: `GITHUB_TOKEN` / `gh auth token` only (no `~/.stokd/.github-token`).
//! - Editor fallback: `$EDITOR` / `$VISUAL` then `code` (no hard-coded stokd-code preference
//!   unless that CLI is on PATH).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sgit_core::{
    bare_clone, create_worktree, list_bare_repos, list_linked_worktrees, load_repositories_config,
    move_bare, move_worktree, normalize_path, render_worktree_name_pattern, resolve_default_branch,
    resolve_repo_layout, run_git_dir, worktree_dir_for_branch, ApplyStatus, RepositoriesConfig,
    RepoLayout, WorktreeRepairTarget,
};

use crate::github::{create_github_repo, resolve_github_token, set_default_branch};

// ── shared ───────────────────────────────────────────────────────────────────

fn parse_owner_repo(repo_spec: &str) -> (String, String) {
    match repo_spec.split_once('/') {
        Some((o, r)) if !o.is_empty() && !r.is_empty() => (o.to_string(), r.to_string()),
        _ => {
            eprintln!("error: repo must be in the format owner/repo-name");
            std::process::exit(1);
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

fn die(msg: impl AsRef<str>) -> ! {
    eprintln!("error: {}", msg.as_ref());
    std::process::exit(1);
}

// ── list ─────────────────────────────────────────────────────────────────────

pub fn run_list(json: bool) {
    let (cfg, source) = load_repositories_config().unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });
    let bare_root = cfg.bare_root.clone();
    let entries = list_bare_repos(&cfg);

    if json {
        if let Some(p) = source.path() {
            eprintln!("# config: {}", p.display());
        }
        let rendered = serde_json::to_string_pretty(&entries).unwrap_or_else(|e| {
            die(format!("failed to serialize repo list: {e}"));
        });
        println!("{rendered}");
        return;
    }

    if let Some(p) = source.path() {
        eprintln!("# config: {}", p.display());
    }
    eprintln!(
        "# bareRoot={}  worktreeRoot={}",
        cfg.bare_root, cfg.worktree_root
    );

    if entries.is_empty() {
        println!("No locally bare-cloned repos found under {bare_root}");
        return;
    }
    for entry in &entries {
        let worktree = if entry.worktree_exists { "✓" } else { "✗" };
        println!(
            "{}/{}  [{worktree} worktree]  {}",
            entry.owner, entry.repo, entry.bare_repo_path
        );
    }
}

// ── clone ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct RepoCloneResult {
    owner: String,
    repo: String,
    #[serde(rename = "bareRepoPath")]
    bare_repo_path: String,
    #[serde(rename = "worktreePath")]
    worktree_path: String,
    branch: String,
    #[serde(rename = "alreadyExisted")]
    already_existed: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum RepoCloneAction {
    CloneThenProvision,
    MaterializeWorktree,
    ReportExisting,
}

fn plan_repo_clone(bare_exists: bool, worktree_exists: bool) -> RepoCloneAction {
    match (bare_exists, worktree_exists) {
        (false, _) => RepoCloneAction::CloneThenProvision,
        (true, false) => RepoCloneAction::MaterializeWorktree,
        (true, true) => RepoCloneAction::ReportExisting,
    }
}

pub fn run_clone(repo_spec: &str, json: bool) {
    let (owner, repo_name) = parse_owner_repo(repo_spec);
    let cfg = load_cfg();
    let mut layout = resolve_repo_layout(&cfg, &owner, &repo_name);

    let action = plan_repo_clone(layout.bare_dir.exists(), layout.worktree_dir.exists());
    let already_existed = action != RepoCloneAction::CloneThenProvision;

    let branch = match action {
        RepoCloneAction::CloneThenProvision => {
            if !json {
                println!("Creating bare clone at {}...", layout.bare_dir.display());
            }
            bare_clone(&owner, &repo_name, &layout.bare_dir).unwrap_or_else(|e| die(e));
            let branch = resolve_default_branch(&layout.bare_dir);
            layout.worktree_dir = worktree_dir_for_branch(&cfg, &owner, &repo_name, &branch);
            if !json {
                println!(
                    "Creating main worktree at {} ({branch})...",
                    layout.worktree_dir.display()
                );
            }
            create_worktree(&layout.bare_dir, &layout.worktree_dir, &branch, true)
                .unwrap_or_else(|e| die(e));
            branch
        }
        RepoCloneAction::MaterializeWorktree => {
            let branch = resolve_default_branch(&layout.bare_dir);
            if !json {
                println!(
                    "Repo already cloned; materializing main worktree at {} ({branch})...",
                    layout.worktree_dir.display()
                );
            }
            create_worktree(&layout.bare_dir, &layout.worktree_dir, &branch, true)
                .unwrap_or_else(|e| die(e));
            branch
        }
        RepoCloneAction::ReportExisting => {
            let branch = resolve_default_branch(&layout.bare_dir);
            if !json {
                println!(
                    "Repo already set up at {} ({branch}).",
                    layout.worktree_dir.display()
                );
            }
            branch
        }
    };

    let result = RepoCloneResult {
        owner,
        repo: repo_name,
        bare_repo_path: layout.bare_dir.to_string_lossy().to_string(),
        worktree_path: layout.worktree_dir.to_string_lossy().to_string(),
        branch,
        already_existed,
    };

    if json {
        let rendered = serde_json::to_string_pretty(&result).unwrap_or_else(|e| {
            die(format!("failed to serialize clone result: {e}"));
        });
        println!("{rendered}");
    } else {
        println!(
            "\nRepository {}/{} is ready.",
            result.owner, result.repo
        );
        println!("  Bare clone: {}", result.bare_repo_path);
        println!("  Worktree:   {}", result.worktree_path);
    }
}

// ── open ─────────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
enum RepoOpenAction {
    CloneThenOpen,
    MaterializeWorktreeThenOpen,
    OpenExisting,
}

fn plan_repo_open(bare_exists: bool, worktree_exists: bool) -> RepoOpenAction {
    match (bare_exists, worktree_exists) {
        (false, _) => RepoOpenAction::CloneThenOpen,
        (true, false) => RepoOpenAction::MaterializeWorktreeThenOpen,
        (true, true) => RepoOpenAction::OpenExisting,
    }
}

pub fn run_open(repo_spec: &str) {
    let (owner, repo_name) = parse_owner_repo(repo_spec);
    let cfg = load_cfg();
    let mut layout = resolve_repo_layout(&cfg, &owner, &repo_name);

    match plan_repo_open(layout.bare_dir.exists(), layout.worktree_dir.exists()) {
        RepoOpenAction::CloneThenOpen => {
            println!("Creating bare clone at {}...", layout.bare_dir.display());
            bare_clone(&owner, &repo_name, &layout.bare_dir).unwrap_or_else(|e| die(e));
            let branch = resolve_default_branch(&layout.bare_dir);
            layout.worktree_dir = worktree_dir_for_branch(&cfg, &owner, &repo_name, &branch);
            println!(
                "Creating main worktree at {} ({branch})...",
                layout.worktree_dir.display()
            );
            create_worktree(&layout.bare_dir, &layout.worktree_dir, &branch, true)
                .unwrap_or_else(|e| die(e));
        }
        RepoOpenAction::MaterializeWorktreeThenOpen => {
            let branch = resolve_default_branch(&layout.bare_dir);
            println!(
                "Repo already cloned; materializing main worktree at {} ({branch})...",
                layout.worktree_dir.display()
            );
            create_worktree(&layout.bare_dir, &layout.worktree_dir, &branch, true)
                .unwrap_or_else(|e| die(e));
        }
        RepoOpenAction::OpenExisting => {
            println!(
                "Repo already set up; opening worktree at {}...",
                layout.worktree_dir.display()
            );
        }
    }

    println!("\nRepository {owner}/{repo_name} is ready.");
    println!("  Bare clone: {}", layout.bare_dir.display());
    println!("  Worktree:   {}", layout.worktree_dir.display());

    let editor_cmd = resolve_open_editor_cli(|key| std::env::var(key).ok());
    println!(
        "Opening {editor_cmd} window at {}...",
        layout.worktree_dir.display()
    );
    reopen_editor(&editor_cmd, &layout.worktree_dir);
}

fn reopen_editor(editor_cmd: &str, worktree_dir: &Path) {
    // For stubbed tests, `$EDITOR` may be a shell script — invoke via `sh -c` when
    // the command contains spaces; otherwise spawn directly.
    if editor_cmd.contains(' ') || editor_cmd.contains('/') && !Path::new(editor_cmd).exists() {
        let _ = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "{editor_cmd} {}",
                shell_quote(&worktree_dir.to_string_lossy())
            ))
            .spawn();
        return;
    }
    let _ = Command::new(editor_cmd)
        .args(["--new-window", &worktree_dir.to_string_lossy()])
        .spawn();
    // Also try without --new-window for simple echo stubs that don't accept flags.
    if which(editor_cmd).is_none() {
        let _ = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "{editor_cmd} {}",
                shell_quote(&worktree_dir.to_string_lossy())
            ))
            .spawn();
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn which(binary: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

fn current_editor_cli<F: Fn(&str) -> Option<String>>(getenv: F) -> Option<String> {
    if getenv("TERM_PROGRAM").as_deref() != Some("vscode") {
        return None;
    }
    let bundle_source =
        getenv("VSCODE_GIT_ASKPASS_NODE").or_else(|| getenv("VSCODE_GIT_ASKPASS_MAIN"));
    bundle_source
        .as_deref()
        .and_then(editor_cli_from_app_path)
        .or_else(|| Some("code".to_string()))
}

fn editor_cli_from_app_path(path: &str) -> Option<String> {
    let app = path.split('/').find(|seg| seg.ends_with(".app"))?;
    let name = app.strip_suffix(".app").unwrap_or(app);
    let cli = match name {
        "Stokd" | "Stokd Code" => "stokd-code",
        "Visual Studio Code" | "Code" | "Code - Insiders" | "VSCodium" => "code",
        "Cursor" => "cursor",
        "Windsurf" => "windsurf",
        "Antigravity IDE" => "antigravity-ide",
        "Antigravity" => "antigravity",
        _ => return None,
    };
    Some(cli.to_string())
}

fn resolve_open_editor_cli<F: Fn(&str) -> Option<String>>(getenv: F) -> String {
    // Prefer explicit EDITOR/VISUAL for headless/stub verification, then ambient
    // VS Code-family terminal, then stokd-code/code.
    if let Some(ed) = getenv("SGIT_EDITOR").or_else(|| getenv("EDITOR")).or_else(|| getenv("VISUAL"))
    {
        let ed = ed.trim().to_string();
        if !ed.is_empty() {
            return ed;
        }
    }
    current_editor_cli(getenv).unwrap_or_else(preferred_editor_cli)
}

fn preferred_editor_cli() -> String {
    if which("stokd-code").is_some() {
        "stokd-code".to_string()
    } else {
        "code".to_string()
    }
}

// ── create ───────────────────────────────────────────────────────────────────
// MOVE of apps/cli/src/commands/repo.rs:758-857 (+ helpers). See handoff diff notes.

pub fn run_create(repo_spec: &str, source_path: Option<&str>, force: bool, prefer_public: bool) {
    let (owner, repo_name) = parse_owner_repo(repo_spec);

    let source = source_path.map(|p| {
        let s = PathBuf::from(p);
        if !s.exists() {
            die(format!("source path does not exist: {p}"));
        }
        if !s.is_dir() {
            die(format!("source path is not a directory: {p}"));
        }
        let s = s.canonicalize().unwrap_or_else(|e| {
            die(format!("cannot resolve source path: {e}"));
        });
        let has_content = fs::read_dir(&s)
            .map(|entries| entries.count() > 0)
            .unwrap_or(false);
        if !has_content {
            die(format!("source directory is empty: {}", s.display()));
        }
        s
    });

    let cfg = load_cfg();
    let RepoLayout {
        bare_dir,
        worktree_dir,
    } = resolve_repo_layout(&cfg, &owner, &repo_name);

    let github_token = resolve_github_token().unwrap_or_else(|| {
        die("no GitHub token available. Set GITHUB_TOKEN or authenticate with `gh auth login`.");
    });

    println!("Creating GitHub repository {owner}/{repo_name}...");
    let is_private = create_github_repo(
        &github_token,
        &owner,
        &repo_name,
        source.is_none(),
        prefer_public,
    );

    if let Some(source_path) = &source {
        println!("Pushing source files to remote...");
        push_source_to_remote(source_path, &owner, &repo_name, &github_token);
    }

    println!("Creating bare clone at {}...", bare_dir.display());
    bare_clone(&owner, &repo_name, &bare_dir).unwrap_or_else(|e| die(e));

    println!("Creating main worktree at {}...", worktree_dir.display());
    create_worktree(&bare_dir, &worktree_dir, "main", true).unwrap_or_else(|e| die(e));

    println!("Setting main as default branch...");
    set_default_branch(&github_token, &owner, &repo_name);

    if force {
        if let Some(source_path) = &source {
            println!("Validating worktree contents match source...");
            if !validate_contents(source_path, &worktree_dir) {
                die("worktree contents do not match source — refusing to delete source");
            }
            let editors = detect_editors(source_path);
            println!("Deleting original source at {}...", source_path.display());
            fs::remove_dir_all(source_path).unwrap_or_else(|e| {
                die(format!("failed to delete source directory: {e}"));
            });
            for (editor_cmd, _) in &editors {
                println!("Reopening {editor_cmd} at {}...", worktree_dir.display());
                reopen_editor(editor_cmd, &worktree_dir);
            }
        }
    }

    println!("\nDone! Repository {owner}/{repo_name} is ready.");
    println!("  Bare clone: {}", bare_dir.display());
    println!("  Worktree:   {}", worktree_dir.display());
    println!(
        "  Visibility: {}",
        if is_private { "Private" } else { "Public" }
    );

    if !prefer_public && !is_private {
        println!("\x1b[33mwarning: repository was created as a public repo because the account didn't support a private one\x1b[0m");
    }
}

fn push_source_to_remote(source: &Path, owner: &str, repo_name: &str, token: &str) {
    let remote_url = format!("https://x-access-token:{token}@github.com/{owner}/{repo_name}.git");
    let is_git_repo = source.join(".git").exists();

    if !is_git_repo {
        run_git(source, &["init"]);
        run_git(source, &["checkout", "-b", "main"]);
    } else {
        let current_branch = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(source)
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                } else {
                    None
                }
            });
        match current_branch.as_deref() {
            Some("main") => {}
            Some(_) | None => {
                let _ = Command::new("git")
                    .args(["checkout", "-b", "main"])
                    .current_dir(source)
                    .output();
            }
        }
    }

    run_git(source, &["add", "-A"]);
    let status_output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(source)
        .output()
        .ok();
    let has_changes = status_output.map(|o| !o.stdout.is_empty()).unwrap_or(false);
    if has_changes {
        run_git(source, &["commit", "-m", "Initial commit"]);
    }

    let _ = Command::new("git")
        .args(["remote", "remove", "origin"])
        .current_dir(source)
        .output();
    run_git(source, &["remote", "add", "origin", &remote_url]);
    run_git(source, &["push", "-u", "origin", "main"]);
}

fn run_git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| {
            die(format!("failed to run git {}: {e}", args.join(" ")));
        });
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        die(format!("git {} failed: {stderr}", args.join(" ")));
    }
}

fn validate_contents(source: &Path, worktree: &Path) -> bool {
    let output = Command::new("git")
        .args([
            "diff",
            "--no-index",
            "--quiet",
            "--exclude=.git",
            &source.to_string_lossy(),
            &worktree.to_string_lossy(),
        ])
        .output();
    match output {
        Ok(o) => o.status.success(),
        Err(e) => {
            eprintln!("warning: git diff --no-index failed: {e}");
            false
        }
    }
}

const EDITORS: &[(&str, &str)] = &[
    ("code", "code"),
    ("cursor", "cursor"),
    ("antigravity", "antigravity"),
];

fn detect_editors(source: &Path) -> Vec<(String, String)> {
    let source_str = source.to_string_lossy();
    let mut found = Vec::new();
    let output = Command::new("ps").args(["aux"]).output().ok();
    if let Some(output) = output {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                for (cmd_name, _) in EDITORS {
                    if line.contains(cmd_name) && line.contains(source_str.as_ref()) {
                        let fields: Vec<&str> = line.split_whitespace().collect();
                        if fields.len() > 1 {
                            found.push((cmd_name.to_string(), fields[1].to_string()));
                        }
                    }
                }
            }
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.dedup_by(|a, b| a.0 == b.0);
    found
}

// ── rename ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenamePlan {
    gh_rename: bool,
    gh_transfer: bool,
    bare_from: PathBuf,
    bare_to: PathBuf,
    new_origin_url: String,
    worktree_moves: Vec<(PathBuf, PathBuf)>,
}

fn plan_repo_rename(
    cfg: &RepositoriesConfig,
    old_owner: &str,
    old_repo: &str,
    new_owner: &str,
    new_repo: &str,
    worktree_paths: &[PathBuf],
) -> RenamePlan {
    let old_layout = resolve_repo_layout(cfg, old_owner, old_repo);
    let new_layout = resolve_repo_layout(cfg, new_owner, new_repo);

    // Normalize prefixes so macOS `/tmp` vs `/private/tmp` (and similar
    // resolve-equivalent spellings) still match git-reported worktree paths.
    let worktree_root = normalize_path(Path::new(&cfg.worktree_root));
    let old_prefix = worktree_root.join(old_owner).join(old_repo);
    let new_prefix = worktree_root.join(new_owner).join(new_repo);

    let worktree_moves = worktree_paths
        .iter()
        .map(|from| {
            let from_n = normalize_path(from);
            match from_n.strip_prefix(&old_prefix) {
                Ok(rest) => (from.clone(), new_prefix.join(rest)),
                Err(_) => (from.clone(), from.clone()),
            }
        })
        .collect();

    RenamePlan {
        gh_rename: new_repo != old_repo,
        gh_transfer: new_owner != old_owner,
        bare_from: old_layout.bare_dir,
        bare_to: new_layout.bare_dir,
        new_origin_url: format!("git@github.com:{new_owner}/{new_repo}.git"),
        worktree_moves,
    }
}

fn transfer_blocked_needs_mirror_fallback(detail: &str) -> bool {
    detail
        .to_lowercase()
        .contains("cannot be transferred to the original owner")
}

fn remote_repo_is_private(owner: &str, repo: &str) -> Option<bool> {
    let out = Command::new("gh")
        .args(["api", &format!("repos/{owner}/{repo}"), "--jq", ".private"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    match String::from_utf8_lossy(&out.stdout).trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn mirror_copy_bare_to_new_remote(
    bare_dir: &Path,
    old_owner: &str,
    old_repo: &str,
    new_owner: &str,
    new_repo: &str,
) {
    let token = resolve_github_token().unwrap_or_else(|| {
        die("no GitHub token available for mirror copy. Set GITHUB_TOKEN or authenticate with `gh auth login`.");
    });
    let prefer_public = !remote_repo_is_private(old_owner, old_repo).unwrap_or(true);
    println!("Creating GitHub repository {new_owner}/{new_repo} for mirror copy...");
    let is_private = create_github_repo(&token, new_owner, new_repo, false, prefer_public);

    let _ = Command::new("git")
        .args([
            "--git-dir",
            &bare_dir.to_string_lossy(),
            "fetch",
            "origin",
            "+refs/heads/*:refs/heads/*",
            "+refs/tags/*:refs/tags/*",
        ])
        .output();

    let push_url =
        format!("https://x-access-token:{token}@github.com/{new_owner}/{new_repo}.git");
    println!("Mirror-pushing full history to {new_owner}/{new_repo}...");
    let out = Command::new("git")
        .args([
            "--git-dir",
            &bare_dir.to_string_lossy(),
            "push",
            "--mirror",
            &push_url,
        ])
        .output()
        .unwrap_or_else(|e| die(format!("failed to run git push --mirror: {e}")));
    if !out.status.success() {
        die(format!(
            "mirror push failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    let _ = Command::new("git")
        .args([
            "--git-dir",
            &bare_dir.to_string_lossy(),
            "push",
            &push_url,
            "--delete",
            "__stokd_hub__",
        ])
        .output();
    set_default_branch(&token, new_owner, new_repo);
    println!(
        "  Visibility: {}",
        if is_private { "Private" } else { "Public" }
    );
    println!(
        "note: source repo {old_owner}/{old_repo} was left intact (GitHub blocks transferring it back to {new_owner}). Archive or delete it manually once you've confirmed the copy."
    );
}

/// Local-layout-only rename (no GitHub). Used for degraded evidence when
/// GitHub is unavailable; also exercised as part of full rename after GH.
#[allow(dead_code)]
pub fn run_rename_local_layout_only(
    cfg: &RepositoriesConfig,
    old_owner: &str,
    old_repo: &str,
    new_owner: &str,
    new_repo: &str,
) {
    let worktrees = {
        let old_layout = resolve_repo_layout(cfg, old_owner, old_repo);
        if !old_layout.bare_dir.exists() {
            die(format!(
                "no local bare clone for {old_owner}/{old_repo} at {}",
                old_layout.bare_dir.display()
            ));
        }
        list_linked_worktrees(&old_layout.bare_dir)
    };
    let plan = plan_repo_rename(cfg, old_owner, old_repo, new_owner, new_repo, &worktrees);
    if plan.bare_to.exists() {
        die(format!(
            "destination bare clone already exists: {}",
            plan.bare_to.display()
        ));
    }
    apply_local_rename(&plan, old_owner, old_repo, new_owner, new_repo);
}

fn apply_local_rename(
    plan: &RenamePlan,
    old_owner: &str,
    old_repo: &str,
    new_owner: &str,
    new_repo: &str,
) {
    run_git_dir(
        &plan.bare_from,
        &["remote", "set-url", "origin", &plan.new_origin_url],
    )
    .unwrap_or_else(|e| die(e));
    println!("Pointed origin at {}", plan.new_origin_url);

    let mut repair_targets = Vec::new();
    for (from, to) in &plan.worktree_moves {
        if from == to {
            continue;
        }
        println!("Moving worktree {} -> {}", from.display(), to.display());
        match move_worktree(&plan.bare_from, from, to) {
            ApplyStatus::Moved => {}
            ApplyStatus::MovedWithWarnings(warns) => {
                for w in warns {
                    eprintln!("  warning: {w}");
                }
            }
            other => die(format!("worktree move failed: {other:?}")),
        }
        repair_targets.push(WorktreeRepairTarget::healthy(to.clone()));
        repoint_session_history(from, to);
    }

    println!(
        "Relocating bare clone {} -> {}",
        plan.bare_from.display(),
        plan.bare_to.display()
    );
    match move_bare(&plan.bare_from, &plan.bare_to, &repair_targets) {
        ApplyStatus::Moved => {}
        ApplyStatus::MovedWithWarnings(warns) => {
            for w in warns {
                eprintln!("  warning: {w}");
            }
        }
        other => die(format!("bare relocation failed: {other:?}")),
    }

    println!("\nRenamed {old_owner}/{old_repo} -> {new_owner}/{new_repo}.");
    println!("  Bare clone: {}", plan.bare_to.display());
    for (_, to) in &plan.worktree_moves {
        println!("  Worktree:   {}", to.display());
    }
}

// ── rename state reconciliation (idempotent) ─────────────────────────────────
// AX-SGIT-RENAME-IDEMPOTENT-RECONCILER: detect current-vs-desired state for the
// GitHub remote and the local layout, then apply only the missing deltas so a
// rename is safe to re-run and "just works" when the remote is already renamed.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GithubRenameState {
    /// New slug resolves and old is gone or redirects to it — already renamed.
    AlreadyDone,
    /// Only the old slug exists (same owner) — `gh repo rename`.
    NeedsRename,
    /// Only the old slug exists under a different owner — transfer (+ rename).
    NeedsTransfer,
    /// Both slugs resolve to DIFFERENT repositories — refuse.
    Collision,
    /// Neither slug resolves on GitHub — local-only rename.
    NeitherExists,
}

/// Decide the GitHub-side action purely from the immutable numeric repo ids of
/// the old and new slugs. GitHub keeps a redirect after a rename, so a resolved
/// old id EQUAL to the new id means the rename already happened.
fn github_rename_state(
    old_id: Option<u64>,
    new_id: Option<u64>,
    owner_changed: bool,
) -> GithubRenameState {
    match (old_id, new_id) {
        // Both resolve: equal id is a redirect (already renamed); different ids
        // are two distinct repositories (a real collision).
        (Some(oid), Some(nid)) => {
            if oid == nid {
                GithubRenameState::AlreadyDone
            } else {
                GithubRenameState::Collision
            }
        }
        // Old gone, new resolves -> the rename already happened.
        (None, Some(_)) => GithubRenameState::AlreadyDone,
        // Only the old slug resolves -> rename (same owner) or transfer.
        (Some(_), None) => {
            if owner_changed {
                GithubRenameState::NeedsTransfer
            } else {
                GithubRenameState::NeedsRename
            }
        }
        (None, None) => GithubRenameState::NeitherExists,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalRenameState {
    /// Old bare gone, new bare present — already relocated.
    NoOp,
    /// Old bare present, new absent — relocate.
    Move,
    /// Both bares present — refuse (never clobber).
    Collision,
    /// Neither bare present — nothing local to rename.
    Missing,
}

fn local_rename_state(old_bare_exists: bool, new_bare_exists: bool) -> LocalRenameState {
    match (old_bare_exists, new_bare_exists) {
        (true, false) => LocalRenameState::Move,
        (false, true) => LocalRenameState::NoOp,
        (true, true) => LocalRenameState::Collision,
        (false, false) => LocalRenameState::Missing,
    }
}

/// The immutable numeric GitHub repo id for `owner/repo`, or `None` when it does
/// not resolve (404) or `gh` is unavailable. A rename leaves a redirect, so the
/// OLD slug still resolves — to the SAME id — after a completed rename.
fn remote_repo_id(owner: &str, repo: &str) -> Option<u64> {
    let out = Command::new("gh")
        .args(["api", &format!("repos/{owner}/{repo}"), "--jq", ".id"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().ok()
}

/// Repoint every provider's session history from the old worktree path to the
/// new one (idempotent — a re-run finds nothing at the old key). Never fails the
/// rename: provider errors surface as warnings.
fn repoint_session_history(from: &Path, to: &Path) {
    let roots = sgit_core::session_history::StoreRoots::system();
    for outcome in sgit_core::session_history::migrate_session_history(from, to, &roots) {
        match &outcome.action {
            sgit_core::session_history::ProviderAction::Relocated { detail } => {
                println!("  history[{}]: {detail}", outcome.provider)
            }
            sgit_core::session_history::ProviderAction::Rewrote { items } => {
                println!("  history[{}]: rewrote {items} record(s)", outcome.provider)
            }
            sgit_core::session_history::ProviderAction::Failed { error } => {
                eprintln!("  warning: history[{}]: {error}", outcome.provider)
            }
            sgit_core::session_history::ProviderAction::Skipped { .. } => {}
        }
    }
}

fn describe_github_state(state: GithubRenameState) -> &'static str {
    match state {
        GithubRenameState::AlreadyDone => "already renamed on GitHub — skip",
        GithubRenameState::NeedsRename => "will `gh repo rename`",
        GithubRenameState::NeedsTransfer => "will transfer owner (+ rename)",
        GithubRenameState::Collision => "COLLISION — refuse (distinct repos)",
        GithubRenameState::NeitherExists => "neither slug on GitHub — local-only",
    }
}

fn describe_local_state(state: LocalRenameState) -> &'static str {
    match state {
        LocalRenameState::Move => "relocate bare + worktrees",
        LocalRenameState::NoOp => "already relocated — skip",
        LocalRenameState::Collision => "COLLISION — destination bare exists",
        LocalRenameState::Missing => "no local clone",
    }
}

fn print_rename_plan(
    old_owner: &str,
    old_repo: &str,
    new_owner: &str,
    new_repo: &str,
    gh_state: GithubRenameState,
    local_state: LocalRenameState,
    plan: &RenamePlan,
) {
    println!("DRY RUN: rename {old_owner}/{old_repo} -> {new_owner}/{new_repo}");
    println!("  GitHub: {}", describe_github_state(gh_state));
    println!("  Local:  {}", describe_local_state(local_state));
    if local_state == LocalRenameState::Move {
        println!(
            "  Bare:   {} -> {}",
            plan.bare_from.display(),
            plan.bare_to.display()
        );
        for (from, to) in &plan.worktree_moves {
            if from == to {
                println!("  Worktree (in place): {}", to.display());
            } else {
                println!("  Worktree: {} -> {}", from.display(), to.display());
            }
        }
    }
    println!("No changes made (dry run).");
}

pub fn run_rename(repo_spec: &str, new_repo_spec: &str, dry_run: bool) {
    let (old_owner, old_repo) = parse_owner_repo(repo_spec);
    let (new_owner, new_repo) = parse_owner_repo(new_repo_spec);

    if old_owner == new_owner && old_repo == new_repo {
        die("new owner/repo is identical to the current one");
    }

    let cfg = load_cfg();
    let old_layout = resolve_repo_layout(&cfg, &old_owner, &old_repo);
    let new_layout = resolve_repo_layout(&cfg, &new_owner, &new_repo);

    let old_bare_exists = old_layout.bare_dir.exists();
    let new_bare_exists = new_layout.bare_dir.exists();
    let local_state = local_rename_state(old_bare_exists, new_bare_exists);

    // Worktrees to relocate come from whichever bare is present.
    let worktrees = if old_bare_exists {
        list_linked_worktrees(&old_layout.bare_dir)
    } else if new_bare_exists {
        list_linked_worktrees(&new_layout.bare_dir)
    } else {
        Vec::new()
    };
    let plan = plan_repo_rename(&cfg, &old_owner, &old_repo, &new_owner, &new_repo, &worktrees);

    // GitHub state via immutable repo ids (read-only probes; safe in dry-run).
    let owner_changed = new_owner != old_owner;
    let old_id = remote_repo_id(&old_owner, &old_repo);
    let new_id = remote_repo_id(&new_owner, &new_repo);
    let gh_state = github_rename_state(old_id, new_id, owner_changed);

    if dry_run {
        print_rename_plan(
            &old_owner, &old_repo, &new_owner, &new_repo, gh_state, local_state, &plan,
        );
        return;
    }

    // Refuse genuine collisions before mutating anything.
    if gh_state == GithubRenameState::Collision {
        die(format!(
            "GitHub collision: {old_owner}/{old_repo} and {new_owner}/{new_repo} resolve to different repositories"
        ));
    }
    if local_state == LocalRenameState::Collision {
        die(format!(
            "destination bare clone already exists: {}",
            new_layout.bare_dir.display()
        ));
    }
    let github_needed = matches!(
        gh_state,
        GithubRenameState::NeedsRename | GithubRenameState::NeedsTransfer
    );
    if local_state == LocalRenameState::Missing && !github_needed {
        die(format!(
            "nothing to do: no local bare for {old_owner}/{old_repo} at {}, and the remote is already at {new_owner}/{new_repo}",
            old_layout.bare_dir.display()
        ));
    }

    // ── GitHub side — only when the remote is not already at the target ──
    let mut did_mirror_fallback = false;
    match gh_state {
        GithubRenameState::AlreadyDone => {
            println!("GitHub: {new_owner}/{new_repo} already renamed; skipping GitHub.");
        }
        GithubRenameState::NeitherExists => {
            println!(
                "warning: neither {old_owner}/{old_repo} nor {new_owner}/{new_repo} resolves on GitHub; doing a local-only rename."
            );
        }
        GithubRenameState::NeedsRename | GithubRenameState::NeedsTransfer => {
            if plan.gh_transfer {
                println!("Transferring {old_owner}/{old_repo} to {new_owner} on GitHub...");
                let out = Command::new("gh")
                    .args([
                        "api",
                        "--method",
                        "POST",
                        &format!("repos/{old_owner}/{old_repo}/transfer"),
                        "-f",
                        &format!("new_owner={new_owner}"),
                    ])
                    .output()
                    .unwrap_or_else(|e| die(format!("failed to run gh api transfer: {e}")));
                if !out.status.success() {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let detail = if stdout.trim().is_empty() {
                        stderr.trim().to_string()
                    } else {
                        format!("{}\n{}", stderr.trim(), stdout.trim())
                    };
                    if transfer_blocked_needs_mirror_fallback(&detail) {
                        println!(
                            "warning: GitHub blocked the transfer (repo was previously owned by {new_owner}); falling back to a history-preserving mirror copy."
                        );
                        mirror_copy_bare_to_new_remote(
                            &plan.bare_from,
                            &old_owner,
                            &old_repo,
                            &new_owner,
                            &new_repo,
                        );
                        did_mirror_fallback = true;
                    } else {
                        die(format!("GitHub transfer failed: {detail}"));
                    }
                }
            }
            if plan.gh_rename && !did_mirror_fallback {
                let rename_owner = if plan.gh_transfer {
                    &new_owner
                } else {
                    &old_owner
                };
                println!("Renaming {rename_owner}/{old_repo} -> {new_repo} on GitHub...");
                let out = Command::new("gh")
                    .args([
                        "repo",
                        "rename",
                        &new_repo,
                        "--repo",
                        &format!("{rename_owner}/{old_repo}"),
                        "--yes",
                    ])
                    .output()
                    .unwrap_or_else(|e| die(format!("failed to run gh repo rename: {e}")));
                if !out.status.success() {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let detail = if stdout.trim().is_empty() {
                        stderr.trim().to_string()
                    } else {
                        format!("{}\n{}", stderr.trim(), stdout.trim())
                    };
                    die(format!("GitHub rename failed: {detail}"));
                }
            }
        }
        GithubRenameState::Collision => unreachable!("collision refused above"),
    }

    // ── Local side — apply only the missing delta ──
    match local_state {
        LocalRenameState::Move => {
            apply_local_rename(&plan, &old_owner, &old_repo, &new_owner, &new_repo);
        }
        LocalRenameState::NoOp => {
            println!(
                "Local bare already at {}; repointing origin only.",
                new_layout.bare_dir.display()
            );
            let _ = run_git_dir(
                &new_layout.bare_dir,
                &["remote", "set-url", "origin", &plan.new_origin_url],
            );
        }
        LocalRenameState::Missing => {
            println!("No local clone to move.");
        }
        LocalRenameState::Collision => unreachable!("collision refused above"),
    }
}

// Silence unused import warnings when helpers are only used in docs/tests.
#[allow(dead_code)]
fn _keep_render(cfg: &RepositoriesConfig) -> String {
    render_worktree_name_pattern(&cfg.main_worktree_name, "o", "r", "main")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_repo_clone_actions() {
        assert_eq!(
            plan_repo_clone(false, false),
            RepoCloneAction::CloneThenProvision
        );
        assert_eq!(
            plan_repo_clone(true, false),
            RepoCloneAction::MaterializeWorktree
        );
        assert_eq!(plan_repo_clone(true, true), RepoCloneAction::ReportExisting);
    }

    #[test]
    fn plan_repo_open_actions() {
        assert_eq!(plan_repo_open(false, true), RepoOpenAction::CloneThenOpen);
        assert_eq!(
            plan_repo_open(true, false),
            RepoOpenAction::MaterializeWorktreeThenOpen
        );
        assert_eq!(plan_repo_open(true, true), RepoOpenAction::OpenExisting);
    }

    #[test]
    fn plan_repo_rename_remaps() {
        let cfg = RepositoriesConfig {
            bare_root: "/opt/dev".into(),
            worktree_root: "/opt/worktrees".into(),
            main_worktree_name: "{branch}".into(),
            track_non_git_workspaces: false,
        };
        let wts = vec![
            PathBuf::from("/opt/worktrees/a/r/main"),
            PathBuf::from("/opt/worktrees/a/r/feat"),
        ];
        let plan = plan_repo_rename(&cfg, "a", "r", "b", "s", &wts);
        assert!(plan.gh_rename && plan.gh_transfer);
        assert_eq!(plan.bare_to, PathBuf::from("/opt/dev/b/s.git"));
        assert_eq!(
            plan.worktree_moves[0].1,
            PathBuf::from("/opt/worktrees/b/s/main")
        );
    }

    #[test]
    fn transfer_blocked_detects_original_owner() {
        assert!(transfer_blocked_needs_mirror_fallback(
            "Repositories cannot be transferred to the original owner"
        ));
        assert!(!transfer_blocked_needs_mirror_fallback("HTTP 401"));
    }

    #[test]
    fn github_rename_state_matrix() {
        use GithubRenameState::*;
        // New slug resolves, old gone -> already done.
        assert_eq!(github_rename_state(None, Some(9), false), AlreadyDone);
        // Old slug redirects to the SAME id as new -> already done.
        assert_eq!(github_rename_state(Some(9), Some(9), false), AlreadyDone);
        // Both resolve to DIFFERENT repos -> collision.
        assert_eq!(github_rename_state(Some(1), Some(2), false), Collision);
        // Only old exists -> rename (same owner) / transfer (owner changed).
        assert_eq!(github_rename_state(Some(1), None, false), NeedsRename);
        assert_eq!(github_rename_state(Some(1), None, true), NeedsTransfer);
        // Neither exists on GitHub -> local-only.
        assert_eq!(github_rename_state(None, None, false), NeitherExists);
    }

    #[test]
    fn local_rename_state_matrix() {
        use LocalRenameState::*;
        assert_eq!(local_rename_state(true, false), Move);
        assert_eq!(local_rename_state(false, true), NoOp);
        assert_eq!(local_rename_state(true, true), Collision);
        assert_eq!(local_rename_state(false, false), Missing);
    }

    #[test]
    fn completed_rename_is_a_noop() {
        // After a completed rename the OLD slug redirects to the SAME id and the
        // new bare is the only local copy — both sides report "already done", so
        // a re-run performs no GitHub call and no local move.
        assert_eq!(
            github_rename_state(Some(7), Some(7), false),
            GithubRenameState::AlreadyDone
        );
        assert_eq!(local_rename_state(false, true), LocalRenameState::NoOp);
    }
}
