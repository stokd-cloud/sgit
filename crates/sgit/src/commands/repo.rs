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
    apply_submodule_checkout_for_repo, bare_clone, create_worktree,
    describe_owner_resolution_failure, list_bare_repos, list_linked_worktrees,
    load_repositories_config, local_owners_for_repo, move_bare, move_worktree, normalize_path,
    parse_repo_spec, render_worktree_name_pattern, resolve_default_branch, resolve_owner_chain,
    resolve_repo_layout, run_git_dir, worktree_dir_for_branch, ApplyStatus, OwnerResolution,
    RepositoriesConfig, RepoLayout, RepoSpec, WorktreeRepairTarget,
};

use crate::github::{
    authenticated_login, create_github_repo, github_owner_chain, owners_with_remote_repo,
    resolve_github_token, set_default_branch,
};

// ── shared ───────────────────────────────────────────────────────────────────

/// Strict `owner/repo` parse — used where a bare name has no sensible owner
/// (both sides of `repo rename`).
fn parse_owner_repo(repo_spec: &str) -> (String, String) {
    match repo_spec.split_once('/') {
        Some((o, r)) if !o.is_empty() && !r.is_empty() => (o.to_string(), r.to_string()),
        _ => {
            eprintln!("error: repo must be in the format owner/repo-name");
            std::process::exit(1);
        }
    }
}

/// Resolve a `clone`/`open` argument. A bare repo name walks the owner chain:
/// local bare clones + worktrees first (offline), then the GitHub owners you can
/// reach. Ambiguity is an error rather than a guess.
fn resolve_existing_repo_arg(repo_spec: &str, cfg: &RepositoriesConfig) -> (String, String) {
    let repo = match parse_repo_spec(repo_spec).unwrap_or_else(|e| die(e)) {
        RepoSpec::OwnerRepo { owner, repo } => return (owner, repo),
        RepoSpec::BareName { repo } => repo,
    };

    let local = local_owners_for_repo(cfg, &repo);
    // Only reach for the network when the local layout knows nothing.
    let remote = if local.is_empty() {
        resolve_github_token()
            .map(|token| owners_with_remote_repo(&token, &github_owner_chain(&token), &repo))
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let resolution = resolve_owner_chain(&local, &remote);
    if let Some(message) = describe_owner_resolution_failure(&repo, &resolution) {
        die(message);
    }
    let OwnerResolution::Resolved { owner, source } = resolution else {
        unreachable!("failure described above");
    };
    eprintln!("# resolved '{repo}' to {owner}/{repo} via {}", source.label());
    (owner, repo)
}

/// Resolve a `create` argument. The repo does not exist yet, so a bare name has
/// no chain to walk — it is created under your own GitHub account, matching
/// `gh repo create <name>`.
fn resolve_new_repo_arg(repo_spec: &str) -> (String, String) {
    let repo = match parse_repo_spec(repo_spec).unwrap_or_else(|e| die(e)) {
        RepoSpec::OwnerRepo { owner, repo } => return (owner, repo),
        RepoSpec::BareName { repo } => repo,
    };

    let owner = resolve_github_token()
        .and_then(|token| authenticated_login(&token))
        .unwrap_or_else(|| {
            die(format!(
                "could not resolve a GitHub account to create '{repo}' under; qualify with <owner/repo> or authenticate with `gh auth login`"
            ))
        });
    eprintln!("# creating under your GitHub account: {owner}/{repo}");
    (owner, repo)
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

/// Best-effort submodule materialization after a superproject worktree exists.
/// Failures are warnings (never abort clone/open) so a missing bare for a heavy
/// submodule does not block the parent repo.
fn maybe_checkout_submodules(
    cfg: &RepositoriesConfig,
    worktree_dir: &Path,
    owner: &str,
    repo_name: &str,
    quiet: bool,
) {
    match apply_submodule_checkout_for_repo(worktree_dir, cfg, owner, repo_name) {
        Ok(()) => {}
        Err(e) => {
            if !quiet {
                eprintln!("warning: submodule checkout: {e}");
            }
        }
    }
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
    for row in format_repo_list(&entries) {
        println!("{row}");
    }
}

/// Render aligned rows (header first). `WT` is worktree presence (`✓`/`✗`);
/// `!` marks a `*.git` dir that is not actually a git repo.
fn format_repo_list(entries: &[sgit_core::BareRepoEntry]) -> Vec<String> {
    let name_width = entries
        .iter()
        .map(|e| e.owner.chars().count() + 1 + e.repo.chars().count())
        .max()
        .unwrap_or(0)
        .max("REPO".len());

    let mut rows = Vec::with_capacity(entries.len() + 1);
    rows.push(format!("{:<name_width$}  {}  {}", "REPO", "WT", "PATH"));
    for e in entries {
        let name = format!("{}/{}", e.owner, e.repo);
        let (mark, note) = if !e.valid {
            ('!', "  (not a git repo)")
        } else if e.worktree_exists {
            ('✓', "")
        } else {
            ('✗', "")
        };
        rows.push(format!(
            "{name:<name_width$}  {mark}   {}{note}",
            e.bare_repo_path
        ));
    }
    rows
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
    let cfg = load_cfg();
    let (owner, repo_name) = resolve_existing_repo_arg(repo_spec, &cfg);
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
            maybe_checkout_submodules(&cfg, &layout.worktree_dir, &owner, &repo_name, json);
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
            maybe_checkout_submodules(&cfg, &layout.worktree_dir, &owner, &repo_name, json);
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
    let cfg = load_cfg();
    let (owner, repo_name) = resolve_existing_repo_arg(repo_spec, &cfg);
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
            maybe_checkout_submodules(&cfg, &layout.worktree_dir, &owner, &repo_name, false);
        }
        RepoOpenAction::MaterializeWorktreeThenOpen => {
            let branch = resolve_default_branch(&layout.bare_dir);
            println!(
                "Repo already cloned; materializing main worktree at {} ({branch})...",
                layout.worktree_dir.display()
            );
            create_worktree(&layout.bare_dir, &layout.worktree_dir, &branch, true)
                .unwrap_or_else(|e| die(e));
            maybe_checkout_submodules(&cfg, &layout.worktree_dir, &owner, &repo_name, false);
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
    let (owner, repo_name) = resolve_new_repo_arg(repo_spec);

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
    /// Entries parked under the old worktree prefix that git does not know as
    /// linked worktrees of the bare (typically standalone full clones). They
    /// still name the old owner/repo after the move, so they travel too.
    stray_moves: Vec<(PathBuf, PathBuf)>,
    /// `<worktreeRoot>/<oldOwner>/<oldRepo>` — pruned when it ends up empty.
    old_worktree_prefix: PathBuf,
}

/// Repo-config keys whose value is a hook directory that may live inside the
/// bare being relocated. A dangling `core.hooksPath` is silently "no hooks" in
/// git, so both must be carried across a rename.
const HOOK_PATH_KEYS: [&str; 2] = ["core.hooksPath", "stokd.hooks.priorHooksPath"];

/// Remap an absolute path that lives inside `from` onto `to`, preserving the
/// relative suffix. `None` when `configured` is not under `from`, so a husky- or
/// third-party-owned hooks directory is never rewritten.
fn remap_under_relocated_dir(configured: &str, from: &Path, to: &Path) -> Option<String> {
    let path = Path::new(configured);
    if !path.is_absolute() {
        return None;
    }
    // Component-wise, so a sibling like `<bare>.bak` is never treated as inside.
    let rest = path.strip_prefix(from).ok()?;
    let mapped = if rest.as_os_str().is_empty() {
        to.to_path_buf()
    } else {
        to.join(rest)
    };
    Some(mapped.to_string_lossy().into_owned())
}

fn plan_repo_rename(
    cfg: &RepositoriesConfig,
    old_owner: &str,
    old_repo: &str,
    new_owner: &str,
    new_repo: &str,
    worktree_paths: &[PathBuf],
    stray_entries: &[PathBuf],
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

    // Anything parked under the old prefix that git does not know as a linked
    // worktree still names the old owner/repo after the move, so it travels too.
    let registered: Vec<PathBuf> = worktree_paths.iter().map(|p| normalize_path(p)).collect();
    let stray_moves: Vec<(PathBuf, PathBuf)> = stray_entries
        .iter()
        .filter(|entry| {
            let entry_n = normalize_path(entry);
            !registered.contains(&entry_n)
        })
        .filter_map(|from| {
            let from_n = normalize_path(from);
            from_n
                .strip_prefix(&old_prefix)
                .ok()
                .map(|rest| (from.clone(), new_prefix.join(rest)))
        })
        .collect();

    RenamePlan {
        gh_rename: new_repo != old_repo,
        gh_transfer: new_owner != old_owner,
        bare_from: old_layout.bare_dir,
        bare_to: new_layout.bare_dir,
        new_origin_url: format!("git@github.com:{new_owner}/{new_repo}.git"),
        worktree_moves,
        stray_moves,
        old_worktree_prefix: old_prefix,
    }
}

/// Direct children of `prefix` (the old `<worktreeRoot>/<owner>/<repo>` dir).
/// Missing or unreadable prefix -> empty, so a repo with no worktree tree at all
/// plans exactly as it did before.
fn list_prefix_entries(prefix: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = match fs::read_dir(prefix) {
        Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.path()).collect(),
        Err(_) => return Vec::new(),
    };
    out.sort();
    out
}

/// Read one repo-config value, or `None` when unset/empty.
fn git_config_value(dir: &Path, key: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["config", "--get", key])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
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
    let strays = list_prefix_entries(
        &normalize_path(Path::new(&cfg.worktree_root))
            .join(old_owner)
            .join(old_repo),
    );
    let plan = plan_repo_rename(
        cfg, old_owner, old_repo, new_owner, new_repo, &worktrees, &strays,
    );
    if plan.bare_to.exists() {
        die(format!(
            "destination bare clone already exists: {}",
            plan.bare_to.display()
        ));
    }
    preflight_stray_destinations(&plan);
    apply_local_rename(&plan, old_owner, old_repo, new_owner, new_repo);
}

/// Fail before any mutation when a stray's destination is already occupied.
fn preflight_stray_destinations(plan: &RenamePlan) {
    for (from, to) in &plan.stray_moves {
        if to.exists() {
            die(format!(
                "cannot relocate {}: destination already exists: {}",
                from.display(),
                to.display()
            ));
        }
    }
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

    // Snapshot hook wiring while the bare is still readable at its old path.
    // git treats a dangling `core.hooksPath` as "no hooks" — silently — so the
    // pointer has to be rewritten as part of the move, not left to be noticed.
    let hook_remaps = plan_hook_remaps(&plan.bare_from, &plan.bare_from, &plan.bare_to);

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

    apply_hook_remaps(&plan.bare_to, &hook_remaps);

    for (from, to) in &plan.stray_moves {
        println!("Moving stray clone {} -> {}", from.display(), to.display());
        if let Err(e) = move_plain_dir(from, to) {
            die(format!("stray relocation failed: {e}"));
        }
        adopt_relocated_stray(to, plan, old_owner, old_repo);
    }
    prune_empty_prefix(&plan.old_worktree_prefix);

    println!("\nRenamed {old_owner}/{old_repo} -> {new_owner}/{new_repo}.");
    println!("  Bare clone: {}", plan.bare_to.display());
    for (_, to) in &plan.worktree_moves {
        println!("  Worktree:   {}", to.display());
    }
    for (_, to) in &plan.stray_moves {
        println!("  Clone:      {}", to.display());
    }
}

/// Read the hook-path config from `read_from` and remap any value that lives
/// inside `from` onto `to`. Values outside `from` (husky, another manager) are
/// left untouched by design.
fn plan_hook_remaps(read_from: &Path, from: &Path, to: &Path) -> Vec<(&'static str, String)> {
    HOOK_PATH_KEYS
        .iter()
        .filter_map(|key| {
            let current = git_config_value(read_from, key)?;
            let remapped = remap_under_relocated_dir(&current, from, to)?;
            Some((*key, remapped))
        })
        .collect()
}

fn apply_hook_remaps(dir: &Path, remaps: &[(&'static str, String)]) {
    for (key, value) in remaps {
        match run_git_dir(dir, &["config", key, value]) {
            Ok(()) => println!("Repointed {key} at {value}"),
            Err(e) => eprintln!("  warning: could not repoint {key}: {e}"),
        }
    }
}

/// Move a directory that git does not track as a worktree. Same-volume renames
/// are atomic; a cross-device refusal is reported rather than half-copied.
fn move_plain_dir(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    fs::rename(from, to)
        .map_err(|e| format!("cannot move {} -> {}: {e}", from.display(), to.display()))
}

/// Re-point a relocated standalone clone at the renamed repo: its `origin` (the
/// old URL survives only on GitHub's non-permanent redirect) and any hook dir it
/// borrowed from the old bare.
fn adopt_relocated_stray(clone_dir: &Path, plan: &RenamePlan, old_owner: &str, old_repo: &str) {
    if !clone_dir.join(".git").exists() {
        eprintln!(
            "  warning: {} is not a git clone — relocated as-is",
            clone_dir.display()
        );
        return;
    }
    if let Some(url) = git_config_value(clone_dir, "remote.origin.url") {
        if url_names_repo(&url, old_owner, old_repo) {
            match run_git_dir(
                clone_dir,
                &["remote", "set-url", "origin", &plan.new_origin_url],
            ) {
                Ok(()) => println!("  Pointed origin at {}", plan.new_origin_url),
                Err(e) => eprintln!("  warning: could not repoint origin: {e}"),
            }
        }
    }
    let remaps = plan_hook_remaps(clone_dir, &plan.bare_from, &plan.bare_to);
    apply_hook_remaps(clone_dir, &remaps);
    eprintln!(
        "  warning: {} is a standalone clone, not a linked worktree of {}. Run `sgit repo migrate` to consolidate it.",
        clone_dir.display(),
        plan.bare_to.display()
    );
}

/// True when `url` addresses `owner/repo` (ssh or https, with or without `.git`).
fn url_names_repo(url: &str, owner: &str, repo: &str) -> bool {
    let needle = format!("{owner}/{repo}").to_ascii_lowercase();
    url.trim_end_matches('/')
        .trim_end_matches(".git")
        .to_ascii_lowercase()
        .ends_with(&needle)
}

/// Remove the old `<owner>/<repo>` prefix (and a then-empty owner dir) only when
/// empty — never recursively.
fn prune_empty_prefix(prefix: &Path) {
    if fs::remove_dir(prefix).is_err() {
        if prefix.exists() {
            eprintln!(
                "  warning: {} still holds entries and was left in place",
                prefix.display()
            );
        }
        return;
    }
    if let Some(owner_dir) = prefix.parent() {
        let _ = fs::remove_dir(owner_dir);
    }
}

pub fn run_rename(repo_spec: &str, new_repo_spec: &str) {
    let (old_owner, old_repo) = parse_owner_repo(repo_spec);
    let (new_owner, new_repo) = parse_owner_repo(new_repo_spec);

    if old_owner == new_owner && old_repo == new_repo {
        die("new owner/repo is identical to the current one");
    }

    let cfg = load_cfg();
    let worktrees = {
        let old_layout = resolve_repo_layout(&cfg, &old_owner, &old_repo);
        if !old_layout.bare_dir.exists() {
            die(format!(
                "no local bare clone for {old_owner}/{old_repo} at {}",
                old_layout.bare_dir.display()
            ));
        }
        list_linked_worktrees(&old_layout.bare_dir)
    };

    let strays = list_prefix_entries(
        &normalize_path(Path::new(&cfg.worktree_root))
            .join(&old_owner)
            .join(&old_repo),
    );
    let plan = plan_repo_rename(
        &cfg, &old_owner, &old_repo, &new_owner, &new_repo, &worktrees, &strays,
    );

    if plan.bare_to.exists() {
        die(format!(
            "destination bare clone already exists: {}",
            plan.bare_to.display()
        ));
    }
    // Collisions are fatal BEFORE any GitHub or filesystem mutation.
    preflight_stray_destinations(&plan);

    let mut did_mirror_fallback = false;
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

    apply_local_rename(&plan, &old_owner, &old_repo, &new_owner, &new_repo);
}

// Silence unused import warnings when helpers are only used in docs/tests.
#[allow(dead_code)]
fn _keep_render(cfg: &RepositoriesConfig) -> String {
    render_worktree_name_pattern(&cfg.main_worktree_name, "o", "r", "main")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sgit_core::BareRepoEntry;

    fn entry(owner: &str, repo: &str, worktree_exists: bool, valid: bool) -> BareRepoEntry {
        BareRepoEntry {
            owner: owner.into(),
            repo: repo.into(),
            bare_repo_path: format!("/opt/dev/{owner}/{repo}.git"),
            origin_url: None,
            default_branch: "main".into(),
            main_worktree_path: format!("/opt/worktrees/{owner}/{repo}/main"),
            worktree_exists,
            valid,
        }
    }

    #[test]
    fn format_repo_list_aligns_columns() {
        let entries = vec![
            entry("InertialG", "aw-watcher-screenshot", true, true),
            entry("openclaw", "openclaw", false, true),
            entry("stokd-cloud", "status", true, false),
        ];
        let rows = format_repo_list(&entries);
        assert_eq!(rows.len(), entries.len() + 1, "header + one row per entry");
        assert!(rows[0].starts_with("REPO"), "header row: {}", rows[0]);

        // The path column must start at the same char offset on every data row.
        let offsets: Vec<usize> = rows[1..]
            .iter()
            .zip(&entries)
            .map(|(row, e)| {
                let byte = row
                    .find(e.bare_repo_path.as_str())
                    .unwrap_or_else(|| panic!("row missing path: {row}"));
                row[..byte].chars().count()
            })
            .collect();
        assert!(
            offsets.windows(2).all(|w| w[0] == w[1]),
            "path column misaligned: {offsets:?}\n{}",
            rows.join("\n")
        );

        // Worktree presence markers and the invalid-dir flag are visible.
        assert!(rows[1].contains('✓'), "existing worktree marked: {}", rows[1]);
        assert!(rows[2].contains('✗'), "missing worktree marked: {}", rows[2]);
        assert!(
            rows[3].contains('!') && rows[3].contains("not a git repo"),
            "invalid dir flagged: {}",
            rows[3]
        );
    }

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
            ..Default::default()
        };
        let wts = vec![
            PathBuf::from("/opt/worktrees/a/r/main"),
            PathBuf::from("/opt/worktrees/a/r/feat"),
        ];
        let plan = plan_repo_rename(&cfg, "a", "r", "b", "s", &wts, &[]);
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
    fn remap_under_relocated_dir_rewrites_paths_inside_the_old_bare() {
        let from = Path::new("/opt/dev/a/r.git");
        let to = Path::new("/opt/dev/b/s.git");
        assert_eq!(
            remap_under_relocated_dir("/opt/dev/a/r.git/stokd-hooks", from, to),
            Some("/opt/dev/b/s.git/stokd-hooks".to_string())
        );
        assert_eq!(
            remap_under_relocated_dir("/opt/dev/a/r.git", from, to),
            Some("/opt/dev/b/s.git".to_string())
        );
    }

    #[test]
    fn remap_under_relocated_dir_leaves_foreign_hook_dirs_alone() {
        let from = Path::new("/opt/dev/a/r.git");
        let to = Path::new("/opt/dev/b/s.git");
        // husky-style relative path, and another repo's absolute path.
        assert_eq!(remap_under_relocated_dir(".husky/_", from, to), None);
        assert_eq!(
            remap_under_relocated_dir("/opt/dev/a/other.git/stokd-hooks", from, to),
            None
        );
        // A sibling whose name merely starts with the same characters.
        assert_eq!(
            remap_under_relocated_dir("/opt/dev/a/r.git.bak/stokd-hooks", from, to),
            None
        );
    }

    #[test]
    fn plan_repo_rename_relocates_unregistered_strays_only() {
        let cfg = RepositoriesConfig {
            bare_root: "/opt/dev".into(),
            worktree_root: "/opt/worktrees".into(),
            main_worktree_name: "{branch}".into(),
            track_non_git_workspaces: false,
            ..Default::default()
        };
        let registered = vec![PathBuf::from("/opt/worktrees/a/r/main")];
        let entries = vec![
            PathBuf::from("/opt/worktrees/a/r/main"),
            PathBuf::from("/opt/worktrees/a/r/stray"),
        ];
        let plan = plan_repo_rename(&cfg, "a", "r", "b", "s", &registered, &entries);

        assert_eq!(
            plan.stray_moves,
            vec![(
                PathBuf::from("/opt/worktrees/a/r/stray"),
                PathBuf::from("/opt/worktrees/b/s/stray")
            )],
            "registered worktrees must not be planned as strays"
        );
        assert_eq!(
            plan.old_worktree_prefix,
            PathBuf::from("/opt/worktrees/a/r")
        );
    }

    fn git_out(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn git_must(dir: &Path, args: &[&str]) {
        let _ = git_out(dir, args);
    }

    #[test]
    fn rename_preserves_hooks_wiring_and_relocates_stray_clone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = normalize_path(tmp.path());
        let bare_root = root.join("dev");
        let worktree_root = root.join("worktrees");
        fs::create_dir_all(&bare_root).unwrap();
        fs::create_dir_all(worktree_root.join("a").join("r")).unwrap();

        // Seed repo -> bare hub at <bareRoot>/a/r.git.
        let seed = root.join("seed");
        fs::create_dir_all(&seed).unwrap();
        git_must(&root, &["init", "-q", "-b", "main", seed.to_str().unwrap()]);
        fs::write(seed.join("f.txt"), "hello\n").unwrap();
        git_must(&seed, &["add", "."]);
        git_must(
            &seed,
            &[
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "seed",
            ],
        );
        let bare = bare_root.join("a").join("r.git");
        fs::create_dir_all(bare.parent().unwrap()).unwrap();
        git_must(
            &root,
            &[
                "clone",
                "-q",
                "--bare",
                seed.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );

        // Hook wiring that lives INSIDE the bare — the thing a rename drops.
        let hooks = bare.join("stokd-hooks");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(hooks.join("commit-msg"), "#!/bin/sh\nexit 0\n").unwrap();
        git_must(
            &bare,
            &["config", "core.hooksPath", hooks.to_str().unwrap()],
        );

        // One registered linked worktree, and one standalone clone parked
        // alongside it that git knows nothing about.
        let wt = worktree_root.join("a").join("r").join("main");
        git_must(
            &bare,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "main"],
        );
        let stray = worktree_root.join("a").join("r").join("stray");
        git_must(
            &root,
            &[
                "clone",
                "-q",
                bare.to_str().unwrap(),
                stray.to_str().unwrap(),
            ],
        );
        git_must(
            &stray,
            &["remote", "set-url", "origin", "git@github.com:a/r.git"],
        );

        let cfg = RepositoriesConfig {
            bare_root: bare_root.to_string_lossy().into_owned(),
            worktree_root: worktree_root.to_string_lossy().into_owned(),
            main_worktree_name: "{branch}".into(),
            track_non_git_workspaces: false,
            ..Default::default()
        };
        run_rename_local_layout_only(&cfg, "a", "r", "b", "s");

        // 1. Hook wiring survives and resolves.
        let new_bare = bare_root.join("b").join("s.git");
        let configured =
            git_config_value(&new_bare, "core.hooksPath").expect("core.hooksPath still configured");
        assert_eq!(
            Path::new(&configured),
            new_bare.join("stokd-hooks"),
            "hooksPath must follow the bare"
        );
        assert!(
            Path::new(&configured).is_dir(),
            "hooksPath must resolve on disk: {configured}"
        );

        // 2. Nothing is left under the old owner/repo prefix.
        let moved_stray = worktree_root.join("b").join("s").join("stray");
        assert!(moved_stray.is_dir(), "stray clone must be relocated");
        assert!(
            !worktree_root.join("a").join("r").exists(),
            "old worktree prefix must be gone"
        );

        // 3. The relocated clone names the new repo.
        assert_eq!(
            git_config_value(&moved_stray, "remote.origin.url").as_deref(),
            Some("git@github.com:b/s.git")
        );
    }
}
