//! Plan (and eventually apply) migration of bare repos and worktrees to the
//! canonical Stokd layout:
//!
//! - bare: `<bareRoot>/<owner>/<repo>.git`
//! - main worktree: `<root>/<owner>/<repo>/<main-worktree-name>` where the leaf
//!   is rendered from the configured `repositories.main_worktree_name` pattern
//!   (default `{branch}` → `main`), matching `repo open`/`repo list`.
//! - other worktrees: `<root>/<owner>/<repo>/<existing-leaf>`
//!
//! The planner is read-only. Discovery runs four channels (`scan_roots`):
//! a walk of the configured worktree root (`repositories.root`) following
//! every `.git` marker back to its bare; a direct walk of `bareRoot` (finding
//! worktree-less bares, full clones parked there, and non-git data dirs); a
//! walk of the launch directory (cwd) adopting repos outside both configured
//! roots (skipped when the cwd is inside one); and per-bare `git worktree
//! list` enumeration (adopting worktrees registered outside the scanned
//! roots). Each bare is asked for its `origin` remote (so broken worktrees
//! still classify), and a per-repo report is emitted.
//!
//! `--plan` is read-only; `--apply`/`--interactive` perform the relocations.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::KeyCode;
use serde::{Deserialize, Serialize};

use sgit_core::{
    load_repositories_config, parse_git_remote_url, point_bare_head_to_placeholder,
    render_worktree_name_pattern,
};

// Session-history relocation is stokd-domain; sgit migrate is layout-only.
// Deliberate divergence from stokd: no Claude/Codex session path rewrites.

/// Schema version for the cached migration plan, bumped if the on-disk shape changes.
const PLAN_VERSION: u32 = 2;

const MAX_WALK_DEPTH: usize = 6;
/// Vendored/build directories the discovery walks never descend into — a real
/// repo is never nested inside one, but dependency checkouts are.
const ACTIVITY_SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".next",
    ".turbo",
    ".cache",
    "dist",
    "build",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".gradle",
    ".idea",
    ".vscode",
];

pub fn run(plan: bool, apply: bool, interactive: bool, orphan_days: u32, filter: Option<String>) {
    // `--plan` is allowed and is also the default; it carries no behavior of its
    // own beyond "do not apply".
    let _ = plan;

    // Optional positional `<owner>` / `<owner>/<repo>` scopes every path below.
    let filter = match parse_migrate_filter(filter.as_deref()) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    if interactive {
        run_interactive(apply, orphan_days, &filter);
        return;
    }
    if apply {
        run_apply(orphan_days, &filter);
        return;
    }
    run_plan(orphan_days, &filter);
}

// ─────────────────────────────────────────────────────────────────────────────
// Discovery
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Worktree {
    /// Filesystem path of the worktree's working directory.
    path: PathBuf,
    /// Path the worktree's `.git` file points at (the bare's admin dir).
    /// `None` if `.git` is a directory (regular clone, not a linked worktree).
    admin_dir: Option<PathBuf>,
    /// True if `admin_dir` does not exist on disk.
    admin_missing: bool,
    /// Branch name from `git rev-parse --abbrev-ref HEAD`, or `None` if it failed.
    branch: Option<String>,
    /// Whether the working tree has uncommitted changes (`None` if undetermined).
    dirty: Option<bool>,
    /// Whether the worktree has commits ahead of its upstream (`None` if undetermined / no upstream).
    unpushed: Option<bool>,
    /// UNIX timestamp of last detected activity. Combines the latest of:
    /// - `git log -1 --format=%ct HEAD` (when git is healthy)
    /// - max file mtime under the worktree (skipping build dirs)
    ///   `None` if neither could be determined.
    last_activity_secs: Option<u64>,
    /// True when the `.git` marker is a directory rather than a file — i.e. this
    /// is a standalone (full) clone, not a linked worktree of a bare. Such repos
    /// have no `admin_dir` and are candidates for bare reconstruction.
    git_is_dir: bool,
    /// `(owner, repo)` parsed from this clone's OWN `origin` remote, resolved only
    /// for standalone clones (`git_is_dir`). Linked worktrees resolve owner/repo
    /// via their bare instead, so this stays `None` for them.
    origin_owner_repo: Option<(String, String)>,
}

#[derive(Debug, Clone)]
struct Bare {
    /// Filesystem path of the bare repo.
    path: PathBuf,
    /// `(owner, repo)` parsed from `git -C <bare> remote get-url origin`.
    owner_repo: Option<(String, String)>,
    /// When this "bare" is really a full clone's embedded `.git` directory,
    /// the clone's working directory. Such a gitdir must never be relocated
    /// with `move_bare` (that strands the clone's files); the clone is adopted
    /// via reconstruct instead.
    attached_clone: Option<PathBuf>,
}

#[derive(Debug)]
struct RepoGroup {
    bare: Bare,
    worktrees: Vec<Worktree>,
}

/// The discovered layout: repo groups (bare + its worktrees) and the worktrees
/// whose bare could not be resolved (orphans), plus the resolved roots.
struct ScanResult {
    repo_groups: Vec<RepoGroup>,
    orphans: Vec<Worktree>,
    /// Standalone (full) clones found under `repositories.root` OR parked under
    /// `bareRoot` whose `origin` resolves to an owner/repo. These have no bare
    /// yet; the planner emits a `ReconstructBare` action to convert each into
    /// the canonical layout.
    standalone_clones: Vec<Worktree>,
    /// Non-git directories found under `bareRoot` (stranded working data,
    /// empty clutter). Report-only: migrate cannot know what is authoritative
    /// without git metadata, so these are surfaced for a human.
    data_dirs: Vec<PathBuf>,
    bare_root: PathBuf,
    worktree_root: PathBuf,
    /// The configured `repositories.main_worktree_name` pattern, used to render
    /// the canonical leaf for a repo's main worktree.
    main_worktree_name: String,
}

/// Load config and walk `root`, grouping every discovered worktree under
/// its resolved bare. Shared by `--plan`, `--apply`, and `--interactive`.
fn scan(announce: bool, filter: &MigrateFilter) -> ScanResult {
    let (cfg, _src) = match load_repositories_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to load config: {e}");
            std::process::exit(1);
        }
    };

    let bare_root = PathBuf::from(&cfg.bare_root);
    let worktree_root = PathBuf::from(&cfg.worktree_root);
    let main_worktree_name = cfg.main_worktree_name.clone();

    // The launch directory is a scan root too — unless it already lives
    // inside one of the configured roots (then its contents are covered).
    let cwd = std::env::current_dir().ok().filter(|c| {
        !path_is_under(c, &worktree_root) && !path_is_under(c, &bare_root)
    });

    if announce {
        match &cwd {
            Some(c) => println!(
                "Scanning {}, {}, and {}...",
                worktree_root.display(),
                bare_root.display(),
                c.display()
            ),
            None => println!(
                "Scanning {} and {}...",
                worktree_root.display(),
                bare_root.display()
            ),
        }
    }
    let DiscoveredRoots {
        mut repo_groups,
        orphans,
        standalone_clones,
        data_dirs,
    } = scan_roots(&bare_root, &worktree_root, cwd.as_deref());
    if announce {
        let wt_count: usize = repo_groups.iter().map(|g| g.worktrees.len()).sum();
        println!(
            "Found {} worktree(s) across {} repo(s).\n",
            wt_count,
            repo_groups.len()
        );
    }

    // Present repos grouped by owner, then repo (case-insensitive), so all of one
    // owner's repos sit together; unknown-remote bares sort last.
    sort_repo_groups(&mut repo_groups);

    // Narrow to the requested owner / owner/repo when a positional filter was
    // given. Unknown-remote orphan worktrees have no resolvable owner/repo, so
    // they never match a filter and are dropped from the filtered view.
    let repo_groups = apply_group_filter(repo_groups, filter);
    let orphans = if matches!(filter, MigrateFilter::NoFilter) {
        orphans
    } else {
        Vec::new()
    };
    // Standalone clones carry their own resolved owner/repo, so a positional
    // filter narrows them just like bare-backed groups.
    let standalone_clones = standalone_clones
        .into_iter()
        .filter(|wt| matches_filter(wt.origin_owner_repo.as_ref(), filter))
        .collect();
    // Data dirs have no resolvable owner/repo; like orphans they only appear
    // in the unfiltered view.
    let data_dirs = if matches!(filter, MigrateFilter::NoFilter) {
        data_dirs
    } else {
        Vec::new()
    };

    ScanResult {
        repo_groups,
        orphans,
        standalone_clones,
        data_dirs,
        bare_root,
        worktree_root,
        main_worktree_name,
    }
}

/// Raw discovery output for a (bareRoot, root) pair — the shared core
/// of `scan` and the hermetic test helper, so both exercise every discovery
/// channel: the worktree-root walk, the direct bareRoot walk, and per-bare
/// registered-worktree enumeration. (AX-CLI-REPO-MIGRATE-RECONSTRUCT-BARE)
struct DiscoveredRoots {
    repo_groups: Vec<RepoGroup>,
    orphans: Vec<Worktree>,
    standalone_clones: Vec<Worktree>,
    data_dirs: Vec<PathBuf>,
}

/// `extra_root` is an additional scan root (the cwd for production runs) that
/// the caller has verified is NOT inside `bare_root`/`worktree_root`. Its
/// worktrees, clones, and gitdirs are adopted (deduplicated against the
/// configured roots); git-less directories under it are NOT reported as data —
/// an arbitrary cwd is full of ordinary folders.
fn scan_roots(
    bare_root: &Path,
    worktree_root: &Path,
    extra_root: Option<&Path>,
) -> DiscoveredRoots {
    let mut worktrees = Vec::new();
    walk_for_worktrees(worktree_root, 0, &mut worktrees);

    // CWD channel: adopt repos living under the launch directory. The walk
    // only inspects children, so the enclosing repo (when the cwd IS a repo,
    // or sits inside one) is resolved explicitly via rev-parse.
    let mut extra_gitdirs: Vec<PathBuf> = Vec::new();
    if let Some(extra) = extra_root {
        let mut extra_wts = Vec::new();
        walk_for_worktrees(extra, 0, &mut extra_wts);
        if let Some(top) = run_git_capture(extra, &["rev-parse", "--show-toplevel"]) {
            let top = PathBuf::from(top.trim());
            if top.exists() && !extra_wts.iter().any(|w| same_path(&w.path, &top)) {
                extra_wts.push(inspect_worktree(&top, &top.join(".git")));
            }
        }
        for wt in extra_wts {
            if !worktrees.iter().any(|w| same_path(&w.path, &wt.path)) {
                worktrees.push(wt);
            }
        }
        // Bares parked under the cwd (clone/data findings are covered by the
        // worktree walk above / intentionally skipped).
        extra_gitdirs = walk_bare_root(extra, &[worktree_root, bare_root]).gitdirs;
    }

    // Group worktrees by their resolved bare path. A worktree with no resolvable
    // bare is either a standalone (full) clone we can reconstruct a bare for (its
    // `.git` is a directory and its own `origin` resolves to owner/repo) or a true
    // orphan (a `.git` file pointing at a dead bare, or a clone with no origin).
    let mut groups: BTreeMap<PathBuf, Vec<Worktree>> = BTreeMap::new();
    let mut orphans: Vec<Worktree> = Vec::new();
    let mut standalone_clones: Vec<Worktree> = Vec::new();
    for wt in worktrees {
        match wt
            .admin_dir
            .as_ref()
            .and_then(|admin| bare_from_admin_dir(admin))
        {
            Some(bare_path) => groups.entry(bare_path).or_default().push(wt),
            None if wt.git_is_dir && wt.origin_owner_repo.is_some() => {
                standalone_clones.push(wt)
            }
            None => orphans.push(wt),
        }
    }

    // Direct bareRoot walk: bares with no linked worktree under the scanned
    // root, full clones parked in the bare tree, and stranded non-git data
    // directories are otherwise invisible to the backlink-only discovery.
    let bare_scan = walk_bare_root(bare_root, &[worktree_root]);
    for gitdir in bare_scan.gitdirs.into_iter().chain(extra_gitdirs) {
        if !groups.keys().any(|k| same_path(k, &gitdir)) {
            groups.entry(gitdir).or_default();
        }
    }
    for clone in bare_scan.clones {
        let wt = inspect_worktree(&clone, &clone.join(".git"));
        if wt.origin_owner_repo.is_some() {
            standalone_clones.push(wt);
        } else {
            orphans.push(wt);
        }
    }
    let mut data_dirs = bare_scan.data_dirs;
    // The worktree root strands git-less data the same way (e.g. a repo dir
    // whose `.git` was lost, leaving plain files at a canonical-looking path).
    // Reuse the classifying walk, keeping only its data findings — its
    // clone/gitdir findings are the worktree walk's business.
    let root_scan = walk_bare_root(worktree_root, &[bare_root]);
    data_dirs.extend(root_scan.data_dirs);

    // Inspect each bare. A "bare" that is really a full clone's `.git`
    // directory is additionally routed to reconstruct: relocating such a gitdir
    // with `move_bare` would strand the clone's working files with no git
    // metadata, so `group_actions` suppresses the bare move for it and the
    // reconstruct adopts the whole clone instead.
    let mut repo_groups: Vec<RepoGroup> = Vec::new();
    for (bare_path, wts) in groups {
        let owner_repo = bare_remote_owner_repo(&bare_path);
        let attached_clone = clone_of_gitdir(&bare_path);
        if let Some(clone) = &attached_clone {
            if !standalone_clones.iter().any(|c| same_path(&c.path, clone)) {
                let wt = inspect_worktree(clone, &clone.join(".git"));
                if wt.origin_owner_repo.is_some() {
                    standalone_clones.push(wt);
                }
            }
        }
        repo_groups.push(RepoGroup {
            bare: Bare {
                path: bare_path,
                owner_repo,
                attached_clone,
            },
            worktrees: wts,
        });
    }

    // Adopt worktrees registered on a bare but living OUTSIDE the scanned
    // worktree root (e.g. leftovers under a previously configured root). They
    // classify like any other worktree, so an off-canonical one becomes a
    // normal (dirty-preserving) move action.
    for group in &mut repo_groups {
        for reg in registered_worktrees(&group.bare.path) {
            if !reg.exists() || same_path(&reg, &group.bare.path) {
                continue;
            }
            let already_known = group.worktrees.iter().any(|w| same_path(&w.path, &reg))
                || standalone_clones.iter().any(|c| same_path(&c.path, &reg))
                || group
                    .bare
                    .attached_clone
                    .as_ref()
                    .is_some_and(|c| same_path(c, &reg));
            if already_known {
                continue;
            }
            group.worktrees.push(inspect_worktree(&reg, &reg.join(".git")));
        }
    }

    DiscoveredRoots {
        repo_groups,
        orphans,
        standalone_clones,
        data_dirs,
    }
}

/// What a direct walk of `bareRoot` found. (AX-CLI-REPO-MIGRATE-RECONSTRUCT-BARE)
#[derive(Default)]
struct BareRootScan {
    /// Git directories (bare or not): `HEAD` + `objects/` + `refs/` present.
    gitdirs: Vec<PathBuf>,
    /// Full clones (a `.git` entry inside a working directory).
    clones: Vec<PathBuf>,
    /// Directories with no git anything beneath them — stranded working data
    /// or clutter that has no business under `bareRoot`.
    data_dirs: Vec<PathBuf>,
}

/// A directory that IS a git directory (works for bare repos and detached
/// gitdirs alike) — cheap filesystem probe, no git subprocess.
fn is_git_dir(p: &Path) -> bool {
    p.join("HEAD").is_file() && p.join("objects").is_dir() && p.join("refs").is_dir()
}

/// Whether `p` lives under (or is) `root`, tolerant of symlinked ancestors.
fn path_is_under(p: &Path, root: &Path) -> bool {
    match (p.canonicalize(), root.canonicalize()) {
        (Ok(a), Ok(b)) => a.starts_with(&b),
        _ => p.starts_with(root),
    }
}

/// `<clone>/.git` → `<clone>` when the gitdir is a full clone's embedded `.git`
/// directory (the parent owns it as its working tree). `None` for true bares
/// and standalone gitdirs.
fn clone_of_gitdir(gitdir: &Path) -> Option<PathBuf> {
    if gitdir.file_name().and_then(|n| n.to_str()) != Some(".git") {
        return None;
    }
    let parent = gitdir.parent()?;
    if parent.join(".git").is_dir() {
        Some(parent.to_path_buf())
    } else {
        None
    }
}

const MAX_BARE_WALK_DEPTH: usize = 3;

/// Walk a root classifying every subtree, skipping the `exclude` subtrees
/// (other scan roots whose contents are their own walk's business).
fn walk_bare_root(root: &Path, exclude: &[&Path]) -> BareRootScan {
    let mut scan = BareRootScan::default();
    if !root.is_dir() {
        return scan;
    }
    walk_bare_dir(root, exclude, 0, &mut scan);
    scan
}

/// Returns true when this subtree contains anything git-shaped. Git-less
/// child directories are reported as data ONLY by a parent that itself has
/// git content (they are stranded among repos) or by the scan root — so an
/// entirely git-less tree is reported once at its topmost directory, not once
/// per descendant.
fn walk_bare_dir(
    dir: &Path,
    exclude: &[&Path],
    depth: usize,
    scan: &mut BareRootScan,
) -> bool {
    if depth > MAX_BARE_WALK_DEPTH {
        return false;
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return false,
    };
    let mut has_git = false;
    let mut gitless_children: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || exclude.iter().any(|x| same_path(&path, x)) {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if ACTIVITY_SKIP_DIRS.contains(&name) || name == ".stokd-migrate-trash" {
                continue;
            }
        }
        if is_git_dir(&path) {
            scan.gitdirs.push(path);
            has_git = true;
            continue;
        }
        if path.join(".git").exists() {
            scan.clones.push(path);
            has_git = true;
            continue;
        }
        if walk_bare_dir(&path, exclude, depth + 1, scan) {
            has_git = true;
        } else {
            gitless_children.push(path);
        }
    }
    if has_git || depth == 0 {
        scan.data_dirs.extend(gitless_children);
    }
    has_git
}

/// Worktree paths registered on a bare (`git worktree list --porcelain`),
/// excluding the bare's own entry.
fn registered_worktrees(bare: &Path) -> Vec<PathBuf> {
    let Some(out) = run_git_capture(bare, &["worktree", "list", "--porcelain"]) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    let mut current: Option<PathBuf> = None;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            if let Some(p) = current.take() {
                result.push(p);
            }
            current = Some(PathBuf::from(rest.trim()));
        } else if line.trim() == "bare" {
            // The bare repo's own entry — not a working tree.
            current = None;
        }
    }
    if let Some(p) = current.take() {
        result.push(p);
    }
    result
}

/// Order repo groups by (owner, repo) case-insensitively, with unknown-remote
/// bares (no resolvable owner/repo) sorted last. Shared by the report and the
/// migration plan so every surface lists repos in the same owner-grouped order.
fn sort_repo_groups(groups: &mut [RepoGroup]) {
    groups.sort_by(|a, b| repo_group_sort_key(&a.bare).cmp(&repo_group_sort_key(&b.bare)));
}

fn repo_group_sort_key(bare: &Bare) -> (u8, String, String) {
    match &bare.owner_repo {
        Some((o, r)) => (0, o.to_lowercase(), r.to_lowercase()),
        None => (1, String::new(), String::new()),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Optional positional filter: `sgit repo migrate <owner>` /
// `sgit repo migrate <owner>/<repo>`
// ─────────────────────────────────────────────────────────────────────────────

/// Restricts which discovered repo groups are processed. `NoFilter` (no
/// positional argument) processes every group, matching the historical
/// behavior. A non-`NoFilter` value matches a group only when its resolved
/// `(owner, repo)` matches case-insensitively; groups with no resolvable
/// owner/repo (unknown-remote) never match under a filter.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MigrateFilter {
    NoFilter,
    Owner(String),
    OwnerRepo(String, String),
}

/// Parse the optional positional argument into a `MigrateFilter`. Accepts
/// `<owner>` or `<owner>/<repo>` (a trailing `.git` on the repo is tolerated,
/// since resolved origins store the repo without it). An empty value, an empty
/// segment (leading/trailing slash), or more than one `/` is a usage error.
fn parse_migrate_filter(arg: Option<&str>) -> Result<MigrateFilter, String> {
    let raw = match arg {
        None => return Ok(MigrateFilter::NoFilter),
        Some(s) => s.trim(),
    };
    if raw.is_empty() {
        return Err("filter must be <owner> or <owner>/<repo>".to_string());
    }
    let parts: Vec<&str> = raw.split('/').collect();
    match parts.as_slice() {
        [owner] if !owner.is_empty() => Ok(MigrateFilter::Owner((*owner).to_string())),
        [owner, repo] if !owner.is_empty() && !repo.is_empty() => {
            let repo = repo.strip_suffix(".git").unwrap_or(repo);
            if repo.is_empty() {
                return Err(format!(
                    "invalid filter '{raw}': expected <owner> or <owner>/<repo>"
                ));
            }
            Ok(MigrateFilter::OwnerRepo(
                (*owner).to_string(),
                repo.to_string(),
            ))
        }
        _ => Err(format!(
            "invalid filter '{raw}': expected <owner> or <owner>/<repo>"
        )),
    }
}

/// Whether a group's resolved `(owner, repo)` passes the filter.
fn matches_filter(owner_repo: Option<&(String, String)>, filter: &MigrateFilter) -> bool {
    match filter {
        MigrateFilter::NoFilter => true,
        MigrateFilter::Owner(o) => owner_repo
            .map(|(go, _)| go.eq_ignore_ascii_case(o))
            .unwrap_or(false),
        MigrateFilter::OwnerRepo(o, r) => owner_repo
            .map(|(go, gr)| go.eq_ignore_ascii_case(o) && gr.eq_ignore_ascii_case(r))
            .unwrap_or(false),
    }
}

/// Retain only the repo groups that pass the filter. `NoFilter` is identity.
fn apply_group_filter(groups: Vec<RepoGroup>, filter: &MigrateFilter) -> Vec<RepoGroup> {
    if matches!(filter, MigrateFilter::NoFilter) {
        return groups;
    }
    groups
        .into_iter()
        .filter(|g| matches_filter(g.bare.owner_repo.as_ref(), filter))
        .collect()
}

fn run_plan(orphan_days: u32, filter: &MigrateFilter) {
    let ScanResult {
        repo_groups,
        orphans,
        standalone_clones,
        data_dirs,
        bare_root,
        worktree_root,
        main_worktree_name,
    } = scan(true, filter);

    // Build and cache the plan so `--interactive`/`--apply` can reuse it.
    let plan = build_migration_plan(
        &repo_groups,
        &standalone_clones,
        &data_dirs,
        &bare_root,
        &worktree_root,
        &main_worktree_name,
        orphan_days,
    );
    if let Err(e) = save_cached_plan(&plan) {
        eprintln!("warning: could not cache migration plan: {e}");
    }

    // ─── Emit report ──────────────────────────────────────────────────────────
    let mut summary = Summary::default();
    for group in &repo_groups {
        emit_group(
            group,
            &bare_root,
            &worktree_root,
            &main_worktree_name,
            orphan_days,
            &mut summary,
        );
    }

    // ─── Orphan section ───────────────────────────────────────────────────────
    let mut orphan_rows: Vec<OrphanRow> = Vec::new();
    for group in &repo_groups {
        let owner_repo_str = group
            .bare
            .owner_repo
            .as_ref()
            .map(|(o, r)| format!("{o}/{r}"))
            .unwrap_or_else(|| "<unknown>".to_string());
        for wt in &group.worktrees {
            if !is_orphan(wt, orphan_days) {
                continue;
            }
            orphan_rows.push(OrphanRow {
                repo: owner_repo_str.clone(),
                path: wt.path.clone(),
                branch: wt.branch.clone(),
                dirty: wt.dirty.unwrap_or(false),
                unpushed: wt.unpushed.unwrap_or(false),
                broken: wt.admin_missing,
                days: wt.last_activity_secs.and_then(days_since),
            });
        }
    }
    // Sort: oldest first
    orphan_rows.sort_by(|a, b| b.days.unwrap_or(u64::MAX).cmp(&a.days.unwrap_or(u64::MAX)));

    if !orphan_rows.is_empty() {
        println!("─── Orphan candidates (>{orphan_days} days inactive) ──────────────────",);
        for row in &orphan_rows {
            let days_str = row
                .days
                .map(|d| format!("{d}d"))
                .unwrap_or_else(|| "?".to_string());
            let mut tags: Vec<&'static str> = Vec::new();
            if row.broken {
                tags.push("broken");
            }
            if row.dirty {
                tags.push("dirty");
            }
            if row.unpushed {
                tags.push("unpushed");
            }
            let tag_str = if tags.is_empty() {
                "clean".to_string()
            } else {
                tags.join(",")
            };
            println!(
                "  [{days_str:>5}]  {repo:<35}  {branch:<30}  {tags}",
                repo = truncate(&row.repo, 35),
                branch = truncate(row.branch.as_deref().unwrap_or("?"), 30),
                tags = tag_str,
            );
            println!("           {}", row.path.display());
        }
        println!();
    }

    if !orphans.is_empty() {
        println!("─── Worktrees with no resolvable bare ────────────────────────────────");
        for o in &orphans {
            println!("  {}", o.path.display());
            if let Some(adm) = &o.admin_dir {
                println!("    .git → {} (could not derive bare path)", adm.display());
            } else {
                println!("    .git is a directory (regular clone, not a linked worktree)");
            }
            summary.unrecoverable += 1;
        }
        println!();
    }

    // Duplicate-bare clusters (same repo backed by multiple bares).
    let dup_section = render_duplicate_bares_section(&plan.duplicate_clusters);
    if !dup_section.is_empty() {
        print!("{dup_section}");
        println!();
    }

    // Non-git directories parked under bareRoot: stranded working data or
    // clutter. Report-only — without git metadata migrate cannot know what is
    // authoritative, so a human must reconcile or remove them.
    if !plan.data_dirs.is_empty() {
        println!("─── Non-git directories under scanned roots (manual review) ──────────");
        for d in &plan.data_dirs {
            println!("  {}", d.display());
        }
        println!("  These contain no git metadata. If one holds real working data,");
        println!("  reconcile it into the repo's canonical worktree by hand, then");
        println!("  remove the stranded directory.");
        println!();
    }

    // Standalone clones with no bare — slated for bare reconstruction.
    if !plan.reconstruct.is_empty() {
        println!("─── Standard clones to convert (no bare) ─────────────────────────────");
        for r in &plan.reconstruct {
            let mut tags: Vec<&str> = Vec::new();
            if r.dirty {
                tags.push("dirty");
            }
            if r.unpushed {
                tags.push("unpushed");
            }
            let tag_str = if tags.is_empty() {
                "clean".to_string()
            } else {
                tags.join(",")
            };
            println!(
                "  {}/{}    branch: {}    state: {tag_str}",
                r.owner,
                r.repo,
                r.branch.as_deref().unwrap_or("?"),
            );
            println!("      clone:    {}", r.clone.display());
            println!("      → bare:     {}", r.bare.display());
            println!("      → worktree: {}", r.worktree.display());
        }
        println!();
    }

    println!("─── Summary ──────────────────────────────────────────────────────────");
    println!("  repos discovered:     {}", repo_groups.len());
    println!("  worktrees ok:         {}", summary.ok);
    println!("  worktrees move:       {}", summary.movable);
    println!("  worktrees dirty:      {}", summary.dirty);
    println!("  worktrees unpushed:   {}", summary.unpushed);
    println!("  worktrees broken:     {}", summary.broken);
    println!("  worktrees unrecoverable: {}", summary.unrecoverable);
    println!(
        "  worktrees orphan ({}d+):  {}",
        orphan_days,
        orphan_rows.len()
    );
    println!("  bares ok:             {}", summary.bare_ok);
    println!("  bares to move:        {}", summary.bare_move);
    println!("  bares unknown remote: {}", summary.bare_unknown);
    println!("  clone gitdirs (adopt): {}", summary.bare_clone_gitdir);
    println!("  main worktrees to create: {}", summary.main_missing);
    println!("  clones to reconstruct: {}", plan.reconstruct.len());
    println!("  data dirs (no git):   {}", plan.data_dirs.len());
    println!();
    println!("This was a dry-run. No filesystem changes were made.");
    println!("Next steps:");
    println!("  sgit repo migrate --interactive   # pick which moves to apply");
    println!("  sgit repo migrate --apply         # apply all safe moves non-interactively");
}

#[derive(Default)]
struct Summary {
    ok: usize,
    movable: usize,
    dirty: usize,
    unpushed: usize,
    broken: usize,
    unrecoverable: usize,
    bare_ok: usize,
    bare_move: usize,
    bare_unknown: usize,
    /// "Bares" that are really a full clone's gitdir (adopted via reconstruct).
    bare_clone_gitdir: usize,
    /// Repos with no worktree on their main branch anywhere.
    main_missing: usize,
}

struct OrphanRow {
    repo: String,
    path: PathBuf,
    branch: Option<String>,
    dirty: bool,
    unpushed: bool,
    broken: bool,
    days: Option<u64>,
}

fn is_orphan(wt: &Worktree, orphan_days: u32) -> bool {
    match wt.last_activity_secs.and_then(days_since) {
        Some(d) => d >= orphan_days as u64,
        None => true, // unknown activity → treat as orphan candidate (likely broken)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}

fn emit_group(
    group: &RepoGroup,
    bare_root: &Path,
    worktree_root: &Path,
    main_worktree_name: &str,
    orphan_days: u32,
    summary: &mut Summary,
) {
    let header = match &group.bare.owner_repo {
        Some((o, r)) => format!("{o}/{r}"),
        None => format!("<unknown remote>  ({})", group.bare.path.display()),
    };
    println!("─── {header} ───────────────────────────────────────────────────");
    println!("  bare: {}", group.bare.path.display());

    let canonical_bare = group
        .bare
        .owner_repo
        .as_ref()
        .map(|(o, r)| bare_root.join(o).join(format!("{r}.git")));
    match (&canonical_bare, &group.bare.owner_repo) {
        (Some(canon), Some(_)) => {
            if group.bare.attached_clone.is_some() {
                println!(
                    "    canonical: NO — this is a full clone's gitdir; the clone is adopted via reconstruct"
                );
                summary.bare_clone_gitdir += 1;
            } else if &group.bare.path == canon {
                println!("    canonical: yes");
                summary.bare_ok += 1;
            } else {
                println!("    canonical: NO  →  {}", canon.display());
                summary.bare_move += 1;
            }
        }
        _ => {
            println!("    canonical: UNKNOWN (no remote on bare)");
            summary.bare_unknown += 1;
        }
    }

    if let Some(m) = plan_materialize_main(group, worktree_root, main_worktree_name) {
        println!(
            "    main worktree: MISSING  →  will create {} ({})",
            m.worktree.display(),
            m.branch
        );
        summary.main_missing += 1;
    }

    for wt in &group.worktrees {
        emit_worktree(
            wt,
            &group.bare,
            worktree_root,
            main_worktree_name,
            orphan_days,
            summary,
        );
    }
    println!();
}

fn emit_worktree(
    wt: &Worktree,
    bare: &Bare,
    worktree_root: &Path,
    main_worktree_name: &str,
    orphan_days: u32,
    summary: &mut Summary,
) {
    let mut tags: Vec<String> = Vec::new();
    if wt.admin_missing {
        tags.push("broken-admin-dir".to_string());
    }
    if wt.dirty == Some(true) {
        tags.push("dirty".to_string());
    }
    if wt.unpushed == Some(true) {
        tags.push("unpushed".to_string());
    }
    let days = wt.last_activity_secs.and_then(days_since);
    let age_str = match days {
        Some(d) => format!("{d}d"),
        None => "?".to_string(),
    };
    if is_orphan(wt, orphan_days) {
        tags.push(format!("orphan({age_str})"));
    } else {
        tags.push(format!("active({age_str})"));
    }
    let branch_str = wt.branch.as_deref().unwrap_or("?");

    let canonical = canonical_worktree_path(wt, bare, worktree_root, main_worktree_name);
    let canonical_str = canonical
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "?".to_string());
    let at_canonical = canonical
        .as_ref()
        .is_some_and(|c| c.as_path() == wt.path.as_path());

    let class = classify(wt, at_canonical);
    match class {
        Classification::Ok => summary.ok += 1,
        Classification::Move => summary.movable += 1,
        Classification::Broken => summary.broken += 1,
    }
    // Dirty/unpushed are informational and orthogonal to movability now, so
    // count them from the worktree state rather than the position class.
    if wt.dirty == Some(true) {
        summary.dirty += 1;
    }
    if wt.unpushed == Some(true) {
        summary.unpushed += 1;
    }

    println!("  • {}", wt.path.display());
    println!(
        "      branch: {branch_str}    state: {tag_str}    class: {class}",
        tag_str = tags.join(","),
        class = class.as_str(),
    );
    if at_canonical {
        println!("      canonical: yes");
    } else {
        println!("      canonical: NO  →  {canonical_str}");
    }
}

fn canonical_worktree_path(
    wt: &Worktree,
    bare: &Bare,
    worktree_root: &Path,
    main_worktree_name: &str,
) -> Option<PathBuf> {
    let (owner, repo) = bare.owner_repo.as_ref()?;
    let leaf = pick_canonical_leaf(wt, owner, repo, main_worktree_name);
    Some(worktree_root.join(owner).join(repo).join(leaf))
}

fn pick_canonical_leaf(wt: &Worktree, owner: &str, repo: &str, main_worktree_name: &str) -> String {
    let branch = wt.branch.as_deref().unwrap_or("");
    if branch == "main" || branch == "master" {
        // The main worktree's leaf follows the configured naming pattern
        // (default `{branch}` → `main`), matching how `repo open`/`repo list`
        // materialize the canonical main worktree. A hardcoded `{repo}-main`
        // would falsely treat a `<repo>-main` worktree as already-canonical.
        return render_worktree_name_pattern(main_worktree_name, owner, repo, branch);
    }
    // Preserve existing leaf — it usually already encodes branch/task identity.
    wt.path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(branch)
        .to_string()
}

/// A worktree's *position* relative to its canonical path. Dirty/unpushed state
/// is tracked separately (and shown as informational tags) — it does NOT gate
/// movability, because `git worktree move` relocates the working tree including
/// uncommitted changes, and unpushed commits live on the branch that moves with
/// it. Only a broken admin link genuinely cannot be relocated by git.
#[derive(Debug, Clone, Copy)]
enum Classification {
    Ok,
    Move,
    Broken,
}

impl Classification {
    fn as_str(self) -> &'static str {
        match self {
            Classification::Ok => "ok",
            Classification::Move => "move",
            Classification::Broken => "repair-then-move",
        }
    }
}

fn classify(wt: &Worktree, at_canonical: bool) -> Classification {
    if wt.admin_missing {
        // A missing admin dir cannot be relocated via `git worktree move`.
        return Classification::Broken;
    }
    if at_canonical {
        Classification::Ok
    } else {
        // Off-canonical and movable even when dirty/unpushed — the move is
        // non-destructive (verified: `git worktree move` preserves uncommitted
        // changes and untracked files; unpushed commits ride along on the branch).
        Classification::Move
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Filesystem walk
// ─────────────────────────────────────────────────────────────────────────────

fn walk_for_worktrees(dir: &Path, depth: usize, out: &mut Vec<Worktree>) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Never treat a vendored/build directory as a repo to migrate, nor
        // descend into one. A real worktree or clone is never nested inside
        // `node_modules`/`dist`/`target`/… but dependency checkouts (e.g. npm git
        // deps) are, and they must NOT be surfaced as orphans or reconstructed.
        // (AX-CLI-REPO-MIGRATE-RECONSTRUCT-BARE)
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if ACTIVITY_SKIP_DIRS.contains(&name) {
                continue;
            }
        }
        let git_marker = path.join(".git");
        if git_marker.exists() {
            out.push(inspect_worktree(&path, &git_marker));
            // Do not descend into a worktree.
            continue;
        }
        walk_for_worktrees(&path, depth + 1, out);
    }
}

fn inspect_worktree(path: &Path, git_marker: &Path) -> Worktree {
    let git_is_dir = git_marker.is_dir();
    let admin_dir = if git_marker.is_file() {
        read_gitdir_file(git_marker)
    } else {
        None
    };
    let admin_missing = admin_dir.as_ref().is_some_and(|p| !p.exists());

    // A standalone (full) clone carries its own `origin`; resolve owner/repo from
    // it so the planner can reconstruct a canonical bare. Linked worktrees share
    // their bare's config, so we only probe this for `.git`-directory clones.
    let origin_owner_repo = if git_is_dir {
        bare_remote_owner_repo(path)
    } else {
        None
    };

    // Trim the trailing newline so branch comparisons (e.g. "main"/"master")
    // and the rendered canonical leaf match — `git` output ends in `\n`.
    let branch = run_git_capture(path, &["rev-parse", "--abbrev-ref", "HEAD"])
        .map(|s| s.trim().to_string());
    let dirty = run_git_capture(path, &["status", "--porcelain"]).map(|s| !s.trim().is_empty());
    let unpushed = if branch.is_some() {
        // log @{u}.. fails if there's no upstream; we treat that as "unknown".
        run_git_capture(path, &["log", "@{u}..", "--oneline"]).map(|s| !s.trim().is_empty())
    } else {
        None
    };

    let last_activity_secs = detect_last_activity(path);

    Worktree {
        path: path.to_path_buf(),
        admin_dir,
        admin_missing,
        branch,
        dirty,
        unpushed,
        last_activity_secs,
        git_is_dir,
        origin_owner_repo,
    }
}

/// Last detected activity WITHOUT walking the repo's contents — discovery
/// must never descend past a repo's root. Combines the committed time from
/// `git log` with the mtimes of the per-worktree git metadata (`index` and
/// `HEAD`, which git touches on add/status/checkout) and the worktree root
/// directory itself. O(1) per repo instead of a capped file-tree walk.
fn detect_last_activity(worktree: &Path) -> Option<u64> {
    let mut best: Option<u64> = None;
    let mut consider = |ts: Option<u64>| {
        if let Some(t) = ts {
            best = Some(best.map_or(t, |b| b.max(t)));
        }
    };

    consider(
        run_git_capture(worktree, &["log", "-1", "--format=%ct", "HEAD"])
            .and_then(|out| out.trim().parse::<u64>().ok()),
    );

    // The worktree's own gitdir: `.git` itself for a full clone, or the
    // per-worktree admin dir a `.git` pointer file names for a linked worktree.
    let marker = worktree.join(".git");
    let gitdir = if marker.is_dir() {
        Some(marker)
    } else {
        read_gitdir_file(&marker)
    };
    if let Some(gd) = gitdir {
        consider(mtime_secs(&gd.join("index")));
        consider(mtime_secs(&gd.join("HEAD")));
    }
    consider(mtime_secs(worktree));

    best
}

fn mtime_secs(p: &Path) -> Option<u64> {
    fs::metadata(p)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

fn days_since(secs: u64) -> Option<u64> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    if now <= secs {
        return Some(0);
    }
    Some((now - secs) / 86_400)
}

fn read_gitdir_file(p: &Path) -> Option<PathBuf> {
    let s = fs::read_to_string(p).ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("gitdir:") {
            return Some(PathBuf::from(rest.trim()));
        }
    }
    None
}

/// `<bare>/worktrees/<name>` → `<bare>`
fn bare_from_admin_dir(admin: &Path) -> Option<PathBuf> {
    let parent = admin.parent()?; // `.../worktrees`
    if parent.file_name().and_then(|s| s.to_str()) != Some("worktrees") {
        return None;
    }
    parent.parent().map(PathBuf::from)
}

fn bare_remote_owner_repo(bare: &Path) -> Option<(String, String)> {
    let out = run_git_capture(bare, &["remote", "get-url", "origin"])?;
    parse_git_remote_url(out.trim())
}

fn run_git_capture(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether `git -C <path> <args...>` exits 0 (used for existence probes like
/// `rev-parse --verify` / `cat-file -e` where only the status matters).
fn git_ok_status(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// How an existing bare occupying a reconstruct's canonical destination should be
/// treated on a re-run, so a half-finished migration can be finished, salvaged, or
/// safely refused instead of being permanently skipped as `DestinationExists`.
/// (AX-CLI-REPO-MIGRATE-RECONSTRUCT-RESUMABLE)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExistingBare {
    /// A complete bare whose origin matches the clone and which already contains
    /// the clone's HEAD commit — a prior run built the bare but never attached the
    /// worktree. Finish by adding the missing worktree.
    Resumable,
    /// A leftover from a failed prior run: not a usable bare (partial copy, empty
    /// dir), or a same/unknown-origin bare missing the clone's commits. The clone
    /// is the authoritative copy, so this is safe to retire to trash and rebuild.
    Salvage,
    /// A valid bare belonging to a DIFFERENT repository — a real collision that
    /// must never be touched.
    Foreign,
}

/// Classify a bare already sitting at a reconstruct's destination against the
/// standalone `clone` we intend to convert. Drives both preflight (safe vs.
/// reject) and [`reconstruct_bare`] (resume vs. salvage-and-rebuild vs. fail).
fn classify_existing_bare(clone: &Path, bare: &Path) -> ExistingBare {
    // A real bare answers `--is-bare-repository` with `true`; a partial copy or a
    // random directory does not. Anything that is not a usable bare is salvage.
    let is_bare = run_git_capture(bare, &["rev-parse", "--is-bare-repository"])
        .map(|s| s.trim() == "true")
        .unwrap_or(false);
    if !is_bare {
        return ExistingBare::Salvage;
    }
    // A valid bare whose origin differs from the clone's is someone else's repo.
    if let (Some(c), Some(b)) = (
        bare_remote_owner_repo(clone),
        bare_remote_owner_repo(bare),
    ) {
        if c != b {
            return ExistingBare::Foreign;
        }
    }
    // Same or unresolvable origin: resumable only if the bare already holds the
    // clone's HEAD commit; otherwise it is an incomplete build to rebuild.
    let holds_head = run_git_capture(clone, &["rev-parse", "HEAD"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|sha| git_ok_status(bare, &["cat-file", "-e", &format!("{sha}^{{commit}}")]))
        .unwrap_or(false);
    if holds_head {
        ExistingBare::Resumable
    } else {
        ExistingBare::Salvage
    }
}

/// Recoverable trash slot for a salvaged partial bare, mirroring the
/// consolidation convention (`<bareRoot>/.stokd-migrate-trash/<owner>/<repo>.git`)
/// and suffixing `.partial-N` so repeated salvages never collide.
/// (AX-CLI-REPO-MIGRATE-RECONSTRUCT-RESUMABLE)
fn partial_bare_trash_path(bare: &Path) -> PathBuf {
    let file = bare.file_name().map(std::ffi::OsString::from).unwrap_or_default();
    // Canonical layout is <bareRoot>/<owner>/<repo>.git; fall back to a sibling
    // trash dir when the bare is not two levels deep.
    let base = match (bare.parent().and_then(Path::parent), bare.parent().and_then(Path::file_name)) {
        (Some(root), Some(owner)) => root.join(".stokd-migrate-trash").join(owner),
        _ => bare
            .parent()
            .map(|p| p.join(".stokd-migrate-trash"))
            .unwrap_or_else(|| PathBuf::from(".stokd-migrate-trash")),
    };
    let mut candidate = base.join(&file);
    let mut n = 1;
    while candidate.exists() {
        let mut name = file.clone();
        name.push(format!(".partial-{n}"));
        candidate = base.join(&name);
        n += 1;
    }
    candidate
}

/// Retire a salvageable partial bare to recoverable trash so the reconstruct can
/// rebuild from the intact clone. Never deletes outright (trash over rm).
fn salvage_partial_bare(bare: &Path) -> std::io::Result<PathBuf> {
    let trash = partial_bare_trash_path(bare);
    if let Some(parent) = trash.parent() {
        fs::create_dir_all(parent)?;
    }
    move_path(bare, &trash)?;
    Ok(trash)
}

/// Finish a reconstruct whose bare was already built by a prior run but whose
/// worktree attach never completed: re-assert the canonical bare config
/// (idempotent) and add the missing worktree. The intact clone is left in place
/// (a warning), never auto-deleted. (AX-CLI-REPO-MIGRATE-RECONSTRUCT-RESUMABLE)
fn resume_reconstruct(clone: &Path, bare: &Path, worktree: &Path) -> ApplyStatus {
    let _ = Command::new("git")
        .arg("-C")
        .arg(bare)
        .args(["config", "core.bare", "true"])
        .output();
    let _ = Command::new("git")
        .arg("-C")
        .arg(bare)
        .args(["config", "--unset", "core.worktree"])
        .output();
    let _ = point_bare_head_to_placeholder(bare);

    // Prefer the clone's current branch when the bare still carries it; otherwise
    // fall back to the bare's local main/master.
    let branch = run_git_capture(clone, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .filter(|b| !b.is_empty() && git_ok_status(bare, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{b}")]))
        .or_else(|| bare_local_main_branch(bare));
    let Some(branch) = branch else {
        return ApplyStatus::Failed(format!(
            "resume: could not determine a branch to check out for {}",
            worktree.display()
        ));
    };

    match materialize_main_worktree(bare, worktree, &branch) {
        ApplyStatus::Moved if clone.join(".git").exists() => {
            ApplyStatus::MovedWithWarnings(vec![format!(
                "resumed a partially-migrated bare; the original clone remains at {} — \
                 remove it once you've confirmed the new worktree",
                clone.display()
            )])
        }
        other => other,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Serializable migration plan (cached so `--interactive --apply` can reuse it)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanWorktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub dirty: bool,
    pub unpushed: bool,
    pub broken: bool,
    /// One of the `Classification::as_str` values.
    pub class: String,
    pub canonical_path: Option<PathBuf>,
    pub at_canonical: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanBare {
    pub path: PathBuf,
    pub owner: Option<String>,
    pub repo: Option<String>,
    pub canonical_path: Option<PathBuf>,
    pub at_canonical: bool,
    /// The owning full clone when this "bare" is a clone's embedded `.git`
    /// directory; suppresses the bare move (the reconstruct adopts the clone).
    /// `#[serde(default)]` for cache back-compat.
    #[serde(default)]
    pub attached_clone: Option<PathBuf>,
}

/// A missing canonical main worktree slated for materialization from the bare
/// (`git worktree add`), planned only when the bare has a local `main`/`master`
/// branch and no worktree anywhere is already on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanMaterialize {
    pub worktree: PathBuf,
    pub branch: String,
}

/// A group's role within a duplicate-bare consolidation. `Standalone` is the
/// default (no consolidation in play, or a divergent cluster left to the legacy
/// suffix relocation). `Winner` keeps the canonical bare path and absorbs the
/// redundant bares' branches. `Redundant` is retired to recoverable trash after
/// its (clean) worktrees are dropped and its branches are mirrored into the
/// winner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ConsolidationRole {
    #[default]
    Standalone,
    Winner,
    Redundant {
        winner_bare: PathBuf,
        trash: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanGroup {
    pub bare: PlanBare,
    pub worktrees: Vec<PlanWorktree>,
    /// Consolidation role; defaults to `Standalone` for plans/caches that
    /// predate duplicate-bare consolidation.
    #[serde(default)]
    pub role: ConsolidationRole,
    /// Set when the repo has no worktree on its main branch anywhere: the
    /// canonical main worktree to create. `#[serde(default)]` for cache
    /// back-compat.
    #[serde(default)]
    pub materialize_main: Option<PlanMaterialize>,
}

/// A standalone (full) clone with no bare, slated for conversion to the canonical
/// bare+worktree layout. (AX-CLI-REPO-MIGRATE-RECONSTRUCT-BARE)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanReconstruct {
    /// Current path of the standalone clone (under `root`).
    pub clone: PathBuf,
    pub owner: String,
    pub repo: String,
    /// The clone's current branch, or `None` when detached/undetermined.
    pub branch: Option<String>,
    /// Canonical bare to create at `<bareRoot>/<owner>/<repo>.git`.
    pub bare: PathBuf,
    /// Canonical worktree to re-attach the working tree at.
    pub worktree: PathBuf,
    pub dirty: bool,
    pub unpushed: bool,
}

/// How a detected duplicate-bare cluster was resolved, for the plan report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DuplicateResolution {
    /// One bare is a clean superset of the others; the redundant bares are
    /// auto-retired to recoverable trash.
    Consolidate,
    /// The bares diverge (unique commits, or uncommitted/unpushed work); left to
    /// the non-destructive suffix relocation and flagged for a human.
    NeedsHuman,
}

/// A detected cluster of 2+ bares resolving to the same owner/repo, recorded on
/// the plan so the report and cache can render the consolidation outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuplicateCluster {
    pub owner_repo: String,
    pub resolution: DuplicateResolution,
    pub members: Vec<PathBuf>,
    pub winner: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationPlan {
    pub version: u32,
    pub bare_root: PathBuf,
    pub worktree_root: PathBuf,
    pub orphan_days: u32,
    pub groups: Vec<PlanGroup>,
    /// Duplicate-bare clusters discovered during planning; empty when no repo
    /// has more than one bare. `#[serde(default)]` for cache back-compat.
    #[serde(default)]
    pub duplicate_clusters: Vec<DuplicateCluster>,
    /// Standalone (full) clones with no bare, slated for conversion to the
    /// canonical layout. `#[serde(default)]` for cache back-compat.
    /// (AX-CLI-REPO-MIGRATE-RECONSTRUCT-BARE)
    #[serde(default)]
    pub reconstruct: Vec<PlanReconstruct>,
    /// Non-git directories found under `bareRoot` — report-only findings for a
    /// human. `#[serde(default)]` for cache back-compat.
    #[serde(default)]
    pub data_dirs: Vec<PathBuf>,
}

/// Compare two paths tolerant of symlinked ancestors (e.g. macOS `/var` →
/// `/private/var`): canonicalize both when they exist on disk, otherwise fall
/// back to a literal comparison. Git records canonicalized absolute paths in its
/// gitdir files, while config-derived canonical targets may not be canonical, so
/// a raw `==` would spuriously flag an already-canonical repo as needing a move.
fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

fn plan_worktree(
    wt: &Worktree,
    bare: &Bare,
    worktree_root: &Path,
    main_worktree_name: &str,
) -> PlanWorktree {
    let canonical = canonical_worktree_path(wt, bare, worktree_root, main_worktree_name);
    let at_canonical = canonical.as_ref().is_some_and(|c| same_path(c, &wt.path));
    let class = classify(wt, at_canonical);
    PlanWorktree {
        path: wt.path.clone(),
        branch: wt.branch.clone(),
        dirty: wt.dirty.unwrap_or(false),
        unpushed: wt.unpushed.unwrap_or(false),
        broken: wt.admin_missing,
        class: class.as_str().to_string(),
        canonical_path: canonical,
        at_canonical,
    }
}

fn plan_bare(bare: &Bare, bare_root: &Path) -> PlanBare {
    let canonical = bare
        .owner_repo
        .as_ref()
        .map(|(o, r)| bare_root.join(o).join(format!("{r}.git")));
    let at_canonical = canonical.as_ref().is_some_and(|c| same_path(c, &bare.path));
    PlanBare {
        path: bare.path.clone(),
        owner: bare.owner_repo.as_ref().map(|(o, _)| o.clone()),
        repo: bare.owner_repo.as_ref().map(|(_, r)| r.clone()),
        canonical_path: canonical,
        at_canonical,
        attached_clone: bare.attached_clone.clone(),
    }
}

/// The bare's local main-ish branch (`main`, then `master`), or `None` when
/// neither exists — materialization is only planned for a branch the bare
/// already has, so no network is ever needed.
fn bare_local_main_branch(bare: &Path) -> Option<String> {
    for b in ["main", "master"] {
        let refname = format!("refs/heads/{b}");
        if run_git_capture(bare, &["show-ref", "--verify", "--quiet", &refname]).is_some() {
            return Some(b.to_string());
        }
    }
    None
}

/// Compute the materialization spec for a group: `Some` only when the repo has
/// a resolved owner/repo, no worktree anywhere on `main`/`master` (an existing
/// off-canonical main worktree is handled by a move, never duplicated), no
/// attached clone (reconstruct will produce the main worktree), and the bare
/// itself has a local main-ish branch to check out.
fn plan_materialize_main(
    group: &RepoGroup,
    worktree_root: &Path,
    main_worktree_name: &str,
) -> Option<PlanMaterialize> {
    let (owner, repo) = group.bare.owner_repo.as_ref()?;
    if group.bare.attached_clone.is_some() {
        return None;
    }
    let has_main_worktree = group.worktrees.iter().any(|wt| {
        matches!(wt.branch.as_deref(), Some("main") | Some("master"))
    });
    if has_main_worktree {
        return None;
    }
    let branch = bare_local_main_branch(&group.bare.path)?;
    let leaf = render_worktree_name_pattern(main_worktree_name, owner, repo, &branch);
    let worktree = worktree_root.join(owner).join(repo).join(leaf);
    // The canonical path may already be occupied — e.g. a worktree named
    // `main` sitting on a feature branch. Never plan a dead-on-arrival create;
    // the report still shows that worktree's branch for the human to judge.
    let occupied = worktree.exists()
        || group
            .worktrees
            .iter()
            .any(|wt| same_path(&wt.path, &worktree));
    if occupied {
        return None;
    }
    Some(PlanMaterialize { worktree, branch })
}

/// Render the canonical worktree leaf for a reconstructed standalone clone. A
/// clone on `main`/`master` (or detached/undetermined) uses the configured
/// `main_worktree_name` pattern (matching `repo open`/`repo list`); any other
/// branch keeps its own name, with `/` flattened to `-` so the leaf is a single
/// directory under `<root>/<owner>/<repo>/`.
fn reconstruct_leaf(branch: Option<&str>, owner: &str, repo: &str, main_worktree_name: &str) -> String {
    let b = branch.map(str::trim).unwrap_or("");
    if b.is_empty() || b == "HEAD" || b == "main" || b == "master" {
        let effective = if b == "main" || b == "master" { b } else { "main" };
        render_worktree_name_pattern(main_worktree_name, owner, repo, effective)
    } else {
        b.replace('/', "-")
    }
}

/// Build a `PlanReconstruct` for a standalone clone, computing its canonical bare
/// and worktree destinations. (AX-CLI-REPO-MIGRATE-RECONSTRUCT-BARE)
fn plan_reconstruct(
    wt: &Worktree,
    bare_root: &Path,
    worktree_root: &Path,
    main_worktree_name: &str,
) -> Option<PlanReconstruct> {
    let (owner, repo) = wt.origin_owner_repo.clone()?;
    let bare = bare_root.join(&owner).join(format!("{repo}.git"));
    let leaf = reconstruct_leaf(wt.branch.as_deref(), &owner, &repo, main_worktree_name);
    let worktree = worktree_root.join(&owner).join(&repo).join(leaf);
    Some(PlanReconstruct {
        clone: wt.path.clone(),
        owner,
        repo,
        branch: wt.branch.clone(),
        bare,
        worktree,
        dirty: wt.dirty.unwrap_or(false),
        unpushed: wt.unpushed.unwrap_or(false),
    })
}

fn build_migration_plan(
    repo_groups: &[RepoGroup],
    standalone_clones: &[Worktree],
    data_dirs: &[PathBuf],
    bare_root: &Path,
    worktree_root: &Path,
    main_worktree_name: &str,
    orphan_days: u32,
) -> MigrationPlan {
    // Sort by (owner, repo) so the plan — and every consumer that renders it —
    // groups each owner's repos together regardless of bare-path discovery order.
    let mut sorted: Vec<&RepoGroup> = repo_groups.iter().collect();
    sorted.sort_by(|a, b| repo_group_sort_key(&a.bare).cmp(&repo_group_sort_key(&b.bare)));
    let groups = sorted
        .into_iter()
        .map(|g| PlanGroup {
            bare: plan_bare(&g.bare, bare_root),
            worktrees: g
                .worktrees
                .iter()
                .map(|wt| plan_worktree(wt, &g.bare, worktree_root, main_worktree_name))
                .collect(),
            role: ConsolidationRole::Standalone,
            materialize_main: plan_materialize_main(g, worktree_root, main_worktree_name),
        })
        .collect();

    // Standalone clones, sorted by (owner, repo) then path for stable output.
    let mut reconstruct: Vec<PlanReconstruct> = standalone_clones
        .iter()
        .filter_map(|wt| plan_reconstruct(wt, bare_root, worktree_root, main_worktree_name))
        .collect();
    reconstruct.sort_by(|a, b| {
        (a.owner.to_lowercase(), a.repo.to_lowercase(), a.clone.clone()).cmp(&(
            b.owner.to_lowercase(),
            b.repo.to_lowercase(),
            b.clone.clone(),
        ))
    });

    let mut plan = MigrationPlan {
        version: PLAN_VERSION,
        bare_root: bare_root.to_path_buf(),
        worktree_root: worktree_root.to_path_buf(),
        orphan_days,
        groups,
        duplicate_clusters: Vec::new(),
        reconstruct,
        data_dirs: data_dirs.to_vec(),
    };
    // Resolve duplicate-bare collisions before any consumer derives actions: a
    // clean-subset cluster is consolidated (winner kept, redundant bares retired
    // to trash), and any other collision falls back to the safe suffix
    // relocation — so the UI, `--apply`, and the cached plan all agree.
    resolve_bare_collisions(&mut plan);
    plan
}

/// Derive the `ReconstructBare` actions from a plan's standalone clones. Kept
/// separate from `group_actions` (which derives bare/worktree moves) because
/// reconstruct candidates have no `PlanGroup` — they have no bare yet.
/// (AX-CLI-REPO-MIGRATE-RECONSTRUCT-BARE)
pub fn reconstruct_actions(plan: &MigrationPlan) -> Vec<MigrateAction> {
    plan.reconstruct
        .iter()
        .map(|r| MigrateAction::ReconstructBare {
            clone: r.clone.clone(),
            bare: r.bare.clone(),
            worktree: r.worktree.clone(),
            branch: r.branch.clone(),
        })
        .collect()
}

fn cached_plan_path() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or_else(|| "cannot resolve home directory".to_string())?;
    // Prefer sgit XDG state; fall back to ~/.sgit then ~/.stokd continuity.
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local").join("state"));
    let sgit_state = base.join("sgit");
    Ok(sgit_state.join("repo-migrate-plan.json"))
}

fn save_cached_plan(plan: &MigrationPlan) -> Result<(), String> {
    let path = cached_plan_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(plan).map_err(|e| e.to_string())?;
    fs::write(&path, json).map_err(|e| e.to_string())
}

fn load_cached_plan() -> Option<MigrationPlan> {
    let path = cached_plan_path().ok()?;
    let data = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}

// ─────────────────────────────────────────────────────────────────────────────
// Apply actions
// ─────────────────────────────────────────────────────────────────────────────

/// A single safe, non-destructive relocation toward the canonical layout.
/// (AX-CLI-REPO-MIGRATE-APPLY-SAFE) Off-canonical worktree moves (including
/// dirty/unpushed ones — `git worktree move` preserves their state) and bare
/// relocations are produced; broken (admin-dir-missing), already-canonical, and
/// unknown-remote items never become an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrateAction {
    MoveWorktree { from: PathBuf, to: PathBuf },
    MoveBare { from: PathBuf, to: PathBuf },
    /// Remove a redundant bare's clean worktree (reproducible from the winner).
    /// (AX-CLI-REPO-MIGRATE-CONSOLIDATE-DUPLICATE-BARES)
    DropWorktree { worktree: PathBuf, bare: PathBuf },
    /// Mirror a redundant bare's branches into the winner, then move the
    /// redundant bare to recoverable trash.
    /// (AX-CLI-REPO-MIGRATE-CONSOLIDATE-DUPLICATE-BARES)
    RetireBare {
        redundant: PathBuf,
        winner: PathBuf,
        trash: PathBuf,
    },
    /// Convert a standalone (full) clone into the canonical bare+worktree layout:
    /// move its `.git` to `<bareRoot>/<owner>/<repo>.git`, point the bare HEAD at
    /// the placeholder, and re-attach the existing working tree as a linked
    /// worktree at `<root>/<owner>/<repo>/<leaf>`. Non-destructive — every
    /// file (tracked, modified, untracked, and gitignored) is preserved.
    /// (AX-CLI-REPO-MIGRATE-RECONSTRUCT-BARE)
    ReconstructBare {
        clone: PathBuf,
        bare: PathBuf,
        worktree: PathBuf,
        branch: Option<String>,
    },
    /// Create the missing canonical main worktree from the bare
    /// (`git worktree add <worktree> <branch>`). Additive and non-destructive:
    /// planned only when no worktree anywhere is on the main branch and the
    /// branch exists locally in the bare. `bare` is the CURRENT bare path; the
    /// apply phase uses the bare's post-move location.
    MaterializeMainWorktree {
        bare: PathBuf,
        worktree: PathBuf,
        branch: String,
    },
}

/// Derive the applyable actions for one repo group. Worktree moves are listed
/// before the bare move so sequential application moves worktrees while the bare
/// is still at its old path, then relocates the bare and repairs links.
pub fn group_actions(group: &PlanGroup) -> Vec<MigrateAction> {
    // A redundant group never relocates: every worktree is dropped (its content
    // is reproducible from the winner), then the bare itself is retired to trash.
    if let ConsolidationRole::Redundant { winner_bare, trash } = &group.role {
        let mut actions = Vec::new();
        for wt in &group.worktrees {
            actions.push(MigrateAction::DropWorktree {
                worktree: wt.path.clone(),
                bare: group.bare.path.clone(),
            });
        }
        actions.push(MigrateAction::RetireBare {
            redundant: group.bare.path.clone(),
            winner: winner_bare.clone(),
            trash: trash.clone(),
        });
        return actions;
    }

    let mut actions = Vec::new();
    for wt in &group.worktrees {
        // The "move" class is any linked, off-canonical worktree (dirty/unpushed
        // included — the move preserves their state). Broken and already-canonical
        // worktrees never move.
        if wt.class == Classification::Move.as_str() {
            if let Some(to) = &wt.canonical_path {
                actions.push(MigrateAction::MoveWorktree {
                    from: wt.path.clone(),
                    to: to.clone(),
                });
            }
        }
    }
    // A clone's embedded gitdir must never be relocated as a bare — moving it
    // strands the clone's working files with no git metadata. The clone is
    // adopted whole by its ReconstructBare entry instead.
    if !group.bare.at_canonical && group.bare.attached_clone.is_none() {
        if let (Some(to), Some(_), Some(_)) =
            (&group.bare.canonical_path, &group.bare.owner, &group.bare.repo)
        {
            actions.push(MigrateAction::MoveBare {
                from: group.bare.path.clone(),
                to: to.clone(),
            });
        }
    }
    if let Some(m) = &group.materialize_main {
        actions.push(MigrateAction::MaterializeMainWorktree {
            bare: group.bare.path.clone(),
            worktree: m.worktree.clone(),
            branch: m.branch.clone(),
        });
    }
    actions
}

fn action_label(action: &MigrateAction) -> String {
    match action {
        MigrateAction::MoveWorktree { from, to } => {
            format!("move worktree  {} → {}", from.display(), to.display())
        }
        MigrateAction::MoveBare { from, to } => {
            format!("move bare      {} → {}", from.display(), to.display())
        }
        MigrateAction::DropWorktree { worktree, .. } => {
            format!(
                "drop worktree  {} (redundant; reproducible from winner)",
                worktree.display()
            )
        }
        MigrateAction::RetireBare {
            redundant, trash, ..
        } => {
            format!(
                "retire bare    {} → {} (recoverable)",
                redundant.display(),
                trash.display()
            )
        }
        MigrateAction::ReconstructBare {
            clone,
            bare,
            worktree,
            ..
        } => {
            format!(
                "reconstruct    {} → bare {} + worktree {}",
                clone.display(),
                bare.display(),
                worktree.display()
            )
        }
        MigrateAction::MaterializeMainWorktree {
            worktree, branch, ..
        } => {
            format!(
                "add worktree   {} ({branch}, missing main worktree)",
                worktree.display()
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApplyStatus {
    Moved,
    /// The move succeeded but some non-fatal follow-up (e.g. repair of an
    /// already-broken worktree) could not complete. Each string is one warning.
    MovedWithWarnings(Vec<String>),
    /// The action was rejected by pre-flight validation and never executed.
    Skipped(UnsafeReason),
    Failed(String),
}

/// Why a selected action was rejected by `preflight` and never run. Each reason
/// describes a way an unchecked move could destroy or intermix repositories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsafeReason {
    /// The destination path already exists on disk.
    DestinationExists,
    /// Two selected actions resolve to the same destination.
    DuplicateDestination,
    /// The destination is nested inside the source (or vice versa) and the move
    /// mechanism (`git worktree move`) cannot stage through it.
    NestedWithSource,
    /// The source path no longer exists.
    SourceMissing,
}

impl UnsafeReason {
    fn as_str(self) -> &'static str {
        match self {
            UnsafeReason::DestinationExists => "destination already exists",
            UnsafeReason::DuplicateDestination => "two actions target the same destination",
            UnsafeReason::NestedWithSource => "destination is nested inside the source",
            UnsafeReason::SourceMissing => "source no longer exists",
        }
    }
}

/// A worktree to repair after a bare relocation, carrying whether it was already
/// broken (admin dir missing) at plan time — broken worktrees cannot be repaired
/// by `git worktree repair` and are reported as warnings rather than failing the
/// whole bare move.
#[derive(Debug, Clone)]
pub(crate) struct WorktreeRepairTarget {
    pub(crate) path: PathBuf,
    pub(crate) broken: bool,
}

impl WorktreeRepairTarget {
    #[allow(dead_code)]
    pub(crate) fn healthy(path: PathBuf) -> Self {
        Self { path, broken: false }
    }
}

struct ApplyResult {
    action: MigrateAction,
    status: ApplyStatus,
}

fn action_from(action: &MigrateAction) -> &Path {
    match action {
        MigrateAction::MoveWorktree { from, .. } | MigrateAction::MoveBare { from, .. } => from,
        MigrateAction::DropWorktree { worktree, .. } => worktree,
        MigrateAction::RetireBare { redundant, .. } => redundant,
        MigrateAction::ReconstructBare { clone, .. } => clone,
        // The "source" a materialization needs on disk is the bare it checks
        // out from (at its pre-move path — preflight runs before any move).
        MigrateAction::MaterializeMainWorktree { bare, .. } => bare,
    }
}

/// The destination an action moves something to, or `None` for a destructive
/// action that has no destination (a worktree drop just removes the worktree).
fn action_dest(action: &MigrateAction) -> Option<&Path> {
    match action {
        MigrateAction::MoveWorktree { to, .. } | MigrateAction::MoveBare { to, .. } => Some(to),
        MigrateAction::RetireBare { trash, .. } => Some(trash),
        MigrateAction::DropWorktree { .. } => None,
        // A reconstruct has two destinations (bare + worktree); preflight checks
        // both explicitly, so the generic single-dest accessor reports the bare.
        MigrateAction::ReconstructBare { bare, .. } => Some(bare),
        MigrateAction::MaterializeMainWorktree { worktree, .. } => Some(worktree),
    }
}

/// Validate a set of selected actions BEFORE any filesystem mutation, splitting
/// them into the ones that are safe to run and the ones that must be skipped.
/// (AX-CLI-REPO-MIGRATE-APPLY-SAFE) This is the guard that prevents the migrator
/// from overwriting an existing repo, collapsing two repos into one destination,
/// or moving a directory into its own subtree.
fn preflight(actions: &[MigrateAction]) -> (Vec<MigrateAction>, Vec<(MigrateAction, UnsafeReason)>) {
    // Count how many actions target each destination so duplicates can be caught.
    // Destructive worktree drops have no destination and never participate.
    let mut dest_counts: BTreeMap<PathBuf, usize> = BTreeMap::new();
    for a in actions {
        if let Some(dest) = action_dest(a) {
            *dest_counts.entry(dest.to_path_buf()).or_insert(0) += 1;
        }
        // A reconstruct also claims a worktree destination beyond its bare, so
        // count it too — two reconstructs must never target the same worktree.
        if let MigrateAction::ReconstructBare { worktree, .. } = a {
            *dest_counts.entry(worktree.to_path_buf()).or_insert(0) += 1;
        }
    }

    // Paths an earlier consolidation phase vacates before any relocation runs:
    // a RetireBare moves its redundant bare to trash, and a DropWorktree removes
    // its worktree. Apply runs drop → retire → move, so a move INTO one of these
    // paths is safe even though the path still exists at preflight time. Without
    // this a winner relocating into the canonical path currently held by the
    // redundant bare (the mac-mixer shape) is wrongly rejected as
    // DestinationExists. (AX-CLI-REPO-MIGRATE-CONSOLIDATE-DUPLICATE-BARES)
    let mut vacated: Vec<PathBuf> = Vec::new();
    for a in actions {
        match a {
            MigrateAction::RetireBare { redundant, .. } => vacated.push(redundant.clone()),
            MigrateAction::DropWorktree { worktree, .. } => vacated.push(worktree.clone()),
            _ => {}
        }
    }
    let will_be_vacated = |to: &Path| vacated.iter().any(|v| same_path(v, to));

    let mut safe = Vec::new();
    let mut rejected = Vec::new();
    for action in actions {
        let reason = match action {
            // A worktree drop only needs its source to still exist; it has no
            // destination, so the destination-exists check must NOT fire (the
            // worktree directory existing IS the thing we are removing).
            MigrateAction::DropWorktree { worktree, .. } => {
                if !worktree.exists() {
                    Some(UnsafeReason::SourceMissing)
                } else {
                    None
                }
            }
            // A reconstruct creates BOTH a bare and a worktree from a standalone
            // clone. Reject if the source is gone, if either destination already
            // exists or collides with another action, or if a destination nests
            // inside the source (we move the clone's files into the worktree, so
            // the worktree must not live within the clone).
            MigrateAction::ReconstructBare {
                clone,
                bare,
                worktree,
                ..
            } => {
                let dup = dest_counts.get(bare).copied().unwrap_or(0) > 1
                    || dest_counts.get(worktree).copied().unwrap_or(0) > 1;
                let nested = bare.starts_with(clone)
                    || clone.starts_with(bare)
                    || worktree.starts_with(clone)
                    || clone.starts_with(worktree);
                if !clone.exists() {
                    Some(UnsafeReason::SourceMissing)
                } else if dup {
                    Some(UnsafeReason::DuplicateDestination)
                } else if worktree.exists() {
                    // An occupied worktree is a hard collision — nothing to resume.
                    Some(UnsafeReason::DestinationExists)
                } else if bare.exists()
                    && classify_existing_bare(clone, bare) == ExistingBare::Foreign
                {
                    // A DIFFERENT repository already owns the bare path; refuse.
                    // A resumable or salvageable leftover from a half-finished prior
                    // run is safe — `reconstruct_bare` finishes or rebuilds it.
                    // (AX-CLI-REPO-MIGRATE-RECONSTRUCT-RESUMABLE)
                    Some(UnsafeReason::DestinationExists)
                } else if nested {
                    Some(UnsafeReason::NestedWithSource)
                } else {
                    None
                }
            }
            _ => {
                let from = action_from(action);
                let to = action_dest(action).expect("non-drop actions have a destination");
                let nested = from.starts_with(to) || to.starts_with(from);
                // Bare moves can stage through a nested destination; worktree moves cannot.
                let nested_blocks = nested && matches!(action, MigrateAction::MoveWorktree { .. });

                if !from.exists() {
                    Some(UnsafeReason::SourceMissing)
                } else if dest_counts.get(to).copied().unwrap_or(0) > 1 {
                    Some(UnsafeReason::DuplicateDestination)
                } else if to.exists() && !will_be_vacated(to) {
                    Some(UnsafeReason::DestinationExists)
                } else if nested_blocks {
                    Some(UnsafeReason::NestedWithSource)
                } else {
                    None
                }
            }
        };

        match reason {
            Some(r) => rejected.push((action.clone(), r)),
            None => safe.push(action.clone()),
        }
    }
    (safe, rejected)
}

// ─────────────────────────────────────────────────────────────────────────────
// Duplicate-bare consolidation: classification
// (AX-CLI-REPO-MIGRATE-CONSOLIDATE-DUPLICATE-BARES)
// ─────────────────────────────────────────────────────────────────────────────

/// Per-bare facts for one collision cluster, consumed by the pure classifier.
/// Git-derived (`collect_cluster_facts`) but modeled as plain data so the
/// decision logic is unit-testable without real repos.
#[derive(Debug, Clone)]
struct MemberFacts {
    /// Whether this bare already sits at its canonical path.
    at_canonical: bool,
    /// Whether any of this bare's worktrees has uncommitted or unpushed work
    /// that dropping the worktree would destroy.
    has_unique_work: bool,
    /// `contains[j]` is true when this bare's object graph fully contains every
    /// branch tip of cluster member `j` (i.e. member `j`'s commits are all
    /// reachable here). `contains[self]` is always true.
    contains: Vec<bool>,
}

#[derive(Debug, Clone)]
struct ClusterFacts {
    members: Vec<MemberFacts>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClusterResolution {
    /// One bare (the winner) contains every other member and the others are
    /// clean — auto-consolidate, retiring the redundant members.
    Consolidate { winner: usize, redundant: Vec<usize> },
    /// The bares cannot be losslessly collapsed automatically.
    Divergent,
}

/// Decide how a duplicate-bare cluster resolves. A member is winner-eligible
/// only when it contains every other member's commits AND every other member is
/// clean (no uncommitted/unpushed work that dropping would lose). Among eligible
/// winners the already-canonical one is preferred, then the lowest index. If no
/// member is eligible the cluster is `Divergent` and left to the safe suffix
/// relocation.
fn classify_bare_cluster(facts: &ClusterFacts) -> ClusterResolution {
    let n = facts.members.len();
    if n < 2 {
        return ClusterResolution::Divergent;
    }
    let mut best: Option<usize> = None;
    for i in 0..n {
        let contains_all_others = (0..n)
            .filter(|&j| j != i)
            .all(|j| facts.members[i].contains.get(j).copied().unwrap_or(false));
        let others_clean = (0..n)
            .filter(|&j| j != i)
            .all(|j| !facts.members[j].has_unique_work);
        if !(contains_all_others && others_clean) {
            continue;
        }
        best = Some(match best {
            None => i,
            Some(prev) => {
                // Prefer the already-canonical candidate; else keep the earlier.
                if facts.members[i].at_canonical && !facts.members[prev].at_canonical {
                    i
                } else {
                    prev
                }
            }
        });
    }
    match best {
        Some(winner) => ClusterResolution::Consolidate {
            winner,
            redundant: (0..n).filter(|&j| j != winner).collect(),
        },
        None => ClusterResolution::Divergent,
    }
}

/// Render the "Duplicate bares" report section, labeling each cluster as
/// auto-consolidate or needs-human. Empty input renders nothing.
fn render_duplicate_bares_section(clusters: &[DuplicateCluster]) -> String {
    if clusters.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str("─── Duplicate bares (same repo, multiple bares) ──────────────────────\n");
    for c in clusters {
        let label = match c.resolution {
            DuplicateResolution::Consolidate => "auto-consolidate",
            DuplicateResolution::NeedsHuman => "needs human",
        };
        out.push_str(&format!(
            "  {} — {label} ({} bares)\n",
            c.owner_repo,
            c.members.len()
        ));
        for m in &c.members {
            let tag = if c.winner.as_ref() == Some(m) {
                "  [winner]"
            } else {
                ""
            };
            out.push_str(&format!("      {}{tag}\n", m.display()));
        }
        match c.resolution {
            DuplicateResolution::Consolidate => out.push_str(
                "      → redundant bares retire to <bareRoot>/.stokd-migrate-trash/ (recoverable)\n",
            ),
            DuplicateResolution::NeedsHuman => out.push_str(
                "      → kept with -N suffix; resolve manually (rerun with --interactive)\n",
            ),
        }
    }
    out
}

/// Read a bare's local branch tip commit hashes. `None` when git itself fails
/// (e.g. the bare path no longer exists), `Some(empty)` for a bare with no
/// branches — so containment of a synthetic/unreadable bare degrades to
/// "unknown" and the caller treats the cluster as divergent (safe).
fn bare_branch_heads(bare: &Path) -> Option<Vec<String>> {
    let out = run_git_capture(bare, &["for-each-ref", "--format=%(objectname)", "refs/heads"])?;
    Some(
        out.lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// Whether `bare`'s object graph contains `sha` AND it is reachable from one of
/// `bare`'s own branch tips (`bare_heads`). Presence alone is not enough — an
/// object can linger unreferenced — so we require ancestry from a live ref.
fn bare_reachable(bare: &Path, bare_heads: &[String], sha: &str) -> bool {
    let present = run_git_capture(bare, &["cat-file", "-e", &format!("{sha}^{{commit}}")]).is_some();
    if !present {
        return false;
    }
    bare_heads.iter().any(|h| {
        run_git_capture(bare, &["merge-base", "--is-ancestor", sha, h]).is_some()
    })
}

/// Gather the per-bare facts for one collision cluster via git. Returns `None`
/// if any member's branch heads cannot be read (the cluster is then treated as
/// divergent), so synthetic plans in unit tests never trigger consolidation.
fn collect_cluster_facts(idxs: &[usize], plan: &MigrationPlan) -> Option<ClusterFacts> {
    let mut heads: Vec<Vec<String>> = Vec::with_capacity(idxs.len());
    let mut paths: Vec<PathBuf> = Vec::with_capacity(idxs.len());
    let mut at_canon: Vec<bool> = Vec::with_capacity(idxs.len());
    let mut unique_work: Vec<bool> = Vec::with_capacity(idxs.len());
    for &gi in idxs {
        let g = &plan.groups[gi];
        heads.push(bare_branch_heads(&g.bare.path)?);
        paths.push(g.bare.path.clone());
        at_canon.push(g.bare.at_canonical);
        unique_work.push(g.worktrees.iter().any(|w| w.dirty || w.unpushed));
    }
    let n = idxs.len();
    let members = (0..n)
        .map(|i| {
            let contains = (0..n)
                .map(|j| heads[j].iter().all(|s| bare_reachable(&paths[i], &heads[i], s)))
                .collect();
            MemberFacts {
                at_canonical: at_canon[i],
                has_unique_work: unique_work[i],
                contains,
            }
        })
        .collect();
    Some(ClusterFacts { members })
}

/// Resolve duplicate-bare collisions. Clusters of 2+ bares sharing a canonical
/// destination are classified: a clean-subset cluster is consolidated (winner
/// marked `Winner`, redundant members marked `Redundant` with a trash target),
/// and everything else is left to the legacy suffix `dedupe_bare_destinations`.
/// (AX-CLI-REPO-MIGRATE-CONSOLIDATE-DUPLICATE-BARES)
fn resolve_bare_collisions(plan: &mut MigrationPlan) {
    let mut clusters: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
    for (i, g) in plan.groups.iter().enumerate() {
        if let (Some(canon), Some(_), Some(_)) =
            (&g.bare.canonical_path, &g.bare.owner, &g.bare.repo)
        {
            clusters.entry(canon.clone()).or_default().push(i);
        }
    }

    for (_canon, idxs) in clusters {
        if idxs.len() < 2 {
            continue;
        }
        let owner_repo = {
            let b = &plan.groups[idxs[0]].bare;
            format!(
                "{}/{}",
                b.owner.clone().unwrap_or_default(),
                b.repo.clone().unwrap_or_default()
            )
        };
        let member_paths: Vec<PathBuf> =
            idxs.iter().map(|&gi| plan.groups[gi].bare.path.clone()).collect();

        let resolution = collect_cluster_facts(&idxs, plan)
            .map(|f| classify_bare_cluster(&f))
            .unwrap_or(ClusterResolution::Divergent);

        match resolution {
            ClusterResolution::Consolidate { winner, redundant } => {
                let winner_gi = idxs[winner];
                let winner_bare = plan.groups[winner_gi].bare.path.clone();
                let owner = plan.groups[winner_gi].bare.owner.clone().unwrap_or_default();
                let repo = plan.groups[winner_gi].bare.repo.clone().unwrap_or_default();
                plan.groups[winner_gi].role = ConsolidationRole::Winner;
                for (k, &ri) in redundant.iter().enumerate() {
                    let gi = idxs[ri];
                    let trash = plan
                        .bare_root
                        .join(".stokd-migrate-trash")
                        .join(&owner)
                        .join(format!("{repo}-{}.git", k + 2));
                    plan.groups[gi].role = ConsolidationRole::Redundant {
                        winner_bare: winner_bare.clone(),
                        trash,
                    };
                }
                plan.duplicate_clusters.push(DuplicateCluster {
                    owner_repo,
                    resolution: DuplicateResolution::Consolidate,
                    members: member_paths,
                    winner: Some(winner_bare),
                });
            }
            ClusterResolution::Divergent => {
                plan.duplicate_clusters.push(DuplicateCluster {
                    owner_repo,
                    resolution: DuplicateResolution::NeedsHuman,
                    members: member_paths,
                    winner: None,
                });
            }
        }
    }

    // Winners, divergent members, and singletons still flow through the legacy
    // suffix dedupe; Redundant groups are skipped there (they retire to trash).
    dedupe_bare_destinations(plan);
}

/// When two or more discovered bares resolve to the same canonical destination
/// (e.g. two clones of the same origin living in different folders), keep the
/// first claimant — preferring one already sitting at canonical — at the canonical
/// path and reassign every other to a suffixed sibling (`<repo>-2.git`,
/// `<repo>-3.git`, …), relocating ITS worktrees under the matching suffixed
/// worktree directory. This makes worktrees always follow their true backing bare
/// and guarantees no two destinations collide. Groups already marked `Redundant`
/// by consolidation are skipped — they retire to trash, not to a suffix.
/// (AX-CLI-REPO-MIGRATE-APPLY-SAFE)
fn dedupe_bare_destinations(plan: &mut MigrationPlan) {
    use std::collections::HashSet;
    let bare_root = plan.bare_root.clone();
    let worktree_root = plan.worktree_root.clone();

    // Pass 1: bares already sitting at their canonical path claim it outright.
    let mut claimed: HashSet<PathBuf> = HashSet::new();
    for g in &plan.groups {
        if matches!(g.role, ConsolidationRole::Redundant { .. }) {
            continue;
        }
        if g.bare.at_canonical {
            if let Some(c) = &g.bare.canonical_path {
                claimed.insert(c.clone());
            }
        }
    }

    // Pass 2: resolve every remaining group against the claimed set.
    for group in &mut plan.groups {
        if matches!(group.role, ConsolidationRole::Redundant { .. }) {
            continue;
        }
        if group.bare.at_canonical {
            continue;
        }
        let (owner, repo, canon) = match (
            group.bare.owner.clone(),
            group.bare.repo.clone(),
            group.bare.canonical_path.clone(),
        ) {
            (Some(o), Some(r), Some(c)) => (o, r, c),
            _ => continue,
        };

        if claimed.insert(canon) {
            continue; // canonical destination was free.
        }

        // Collision: find the next free `<repo>-N` destination.
        let mut n = 2usize;
        let (new_repo, new_bare) = loop {
            let candidate_repo = format!("{repo}-{n}");
            let candidate = bare_root.join(&owner).join(format!("{candidate_repo}.git"));
            if claimed.insert(candidate.clone()) {
                break (candidate_repo, candidate);
            }
            n += 1;
        };

        group.bare.at_canonical = same_path(&new_bare, &group.bare.path);
        group.bare.canonical_path = Some(new_bare);

        // Relocate this group's worktrees under the suffixed worktree directory,
        // preserving each leaf name.
        for wt in &mut group.worktrees {
            let leaf = wt
                .canonical_path
                .as_ref()
                .and_then(|c| c.file_name())
                .and_then(|n| n.to_str())
                .map(|s| s.to_string());
            if let Some(leaf) = leaf {
                let new_wt = worktree_root.join(&owner).join(&new_repo).join(&leaf);
                let was_move = wt.class == Classification::Move.as_str();
                wt.at_canonical = same_path(&new_wt, &wt.path);
                wt.canonical_path = Some(new_wt);
                if wt.at_canonical && was_move {
                    wt.class = Classification::Ok.as_str().to_string();
                }
            }
        }
    }
}

/// Apply the selected actions. For each group, selected worktree moves run first
/// (`git worktree move`), then a selected bare move relocates the bare and runs
/// `git worktree repair` over the group's final worktree paths so every link is
/// fixed — including worktrees that were not themselves moved.
fn apply_plan(
    plan: &MigrationPlan,
    selected: &[MigrateAction],
) -> Vec<ApplyResult> {
    // Pre-flight: never mutate the filesystem for an unsafe action. Rejected
    // actions are reported as Skipped and excluded from execution.
    let (safe, rejected) = preflight(selected);
    let mut results: Vec<ApplyResult> = rejected
        .into_iter()
        .map(|(action, reason)| ApplyResult {
            action,
            status: ApplyStatus::Skipped(reason),
        })
        .collect();

    // Phase 0: drop redundant worktrees first, freeing the canonical worktree
    // paths before any winner relocation claims them.
    for group in &plan.groups {
        for action in group_actions(group) {
            if let MigrateAction::DropWorktree { worktree, bare } = &action {
                if !safe.contains(&action) {
                    continue;
                }
                let status = drop_worktree(bare, worktree);
                results.push(ApplyResult {
                    action: action.clone(),
                    status,
                });
            }
        }
    }

    // Phase 1: retire redundant bares. Done before any winner bare move so the
    // RetireBare's recorded winner path is still valid (the winner may itself
    // relocate to canonical in the move loop below).
    for group in &plan.groups {
        for action in group_actions(group) {
            if let MigrateAction::RetireBare {
                redundant,
                winner,
                trash,
            } = &action
            {
                if !safe.contains(&action) {
                    continue;
                }
                let status = retire_bare(redundant, winner, trash);
                results.push(ApplyResult {
                    action: action.clone(),
                    status,
                });
            }
        }
    }

    for group in &plan.groups {
        let actions = group_actions(group);
        // Where the bare ends up after this group's actions run — moves update
        // it so a following materialization targets the bare's real location.
        let mut bare_now = group.bare.path.clone();

        for action in &actions {
            if let MigrateAction::MoveWorktree { from, to } = action {
                if !safe.contains(action) {
                    continue;
                }
                let status = move_worktree(&group.bare.path, from, to);
                // A worktree's session history is keyed by its path; follow the
                // move so native provider resume keeps working at the new path.
                if matches!(status, ApplyStatus::Moved) && to.exists() {
                    relocate_session_history(from, to);
                }
                results.push(ApplyResult {
                    action: action.clone(),
                    status,
                });
            }
        }

        for action in &actions {
            if let MigrateAction::MoveBare { from, to } = action {
                if !safe.contains(action) {
                    continue;
                }
                // Final worktree paths: canonical for worktrees whose move was
                // also selected (and safe), current path otherwise. Carry the
                // broken flag so repair can skip un-repairable worktrees.
                let targets: Vec<WorktreeRepairTarget> = group
                    .worktrees
                    .iter()
                    .map(|wt| {
                        let moved = wt.class == Classification::Move.as_str()
                            && wt.canonical_path.is_some()
                            && safe.iter().any(|s| {
                                matches!(s, MigrateAction::MoveWorktree { from: f, .. } if f == &wt.path)
                            });
                        let path = match (moved, &wt.canonical_path) {
                            (true, Some(c)) => c.clone(),
                            _ => wt.path.clone(),
                        };
                        WorktreeRepairTarget {
                            path,
                            broken: wt.broken,
                        }
                    })
                    .collect();
                let status = move_bare(from, to, &targets);
                if matches!(
                    status,
                    ApplyStatus::Moved | ApplyStatus::MovedWithWarnings(_)
                ) {
                    bare_now = to.clone();
                }
                results.push(ApplyResult {
                    action: action.clone(),
                    status,
                });
            }
        }

        // Materialize the missing main worktree last, from the bare's final
        // location, so the fresh checkout lands on a fully relocated repo.
        for action in &actions {
            if let MigrateAction::MaterializeMainWorktree {
                worktree, branch, ..
            } = action
            {
                if !safe.contains(action) {
                    continue;
                }
                let status = materialize_main_worktree(&bare_now, worktree, branch);
                results.push(ApplyResult {
                    action: action.clone(),
                    status,
                });
            }
        }
    }

    // Phase 3: reconstruct bares for standalone clones. These have no PlanGroup,
    // so they are driven straight off the safe action list. On success the
    // clone's session history follows the working tree to its new worktree path.
    // (AX-CLI-REPO-MIGRATE-RECONSTRUCT-BARE)
    for action in &safe {
        if let MigrateAction::ReconstructBare {
            clone,
            bare,
            worktree,
            branch,
        } = action
        {
            let status = reconstruct_bare(clone, bare, worktree, branch.as_deref());
            if matches!(status, ApplyStatus::Moved | ApplyStatus::MovedWithWarnings(_))
                && worktree.exists()
            {
                relocate_session_history(clone, worktree);
            }
            results.push(ApplyResult {
                action: action.clone(),
                status,
            });
        }
    }
    results
}

/// Relocate every provider's session history keyed to `from` so it tracks the
/// worktree move to `to`. Best-effort: a provider whose store is absent is a
/// silent skip, and a provider error is a warning that never fails the move.
fn relocate_session_history(_from: &Path, _to: &Path) {
    // Deliberate divergence: stokd rewrites Claude/Codex session paths; sgit does not.
}

/// Create the canonical main worktree from the bare: `git worktree add
/// <worktree> <branch>`. Additive — it never touches an existing directory
/// (preflight rejects an existing destination before this runs).
pub(crate) fn materialize_main_worktree(
    bare: &Path,
    worktree: &Path,
    branch: &str,
) -> ApplyStatus {
    if worktree.exists() {
        return ApplyStatus::Skipped(UnsafeReason::DestinationExists);
    }
    if let Some(parent) = worktree.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return ApplyStatus::Failed(format!("cannot create {}: {e}", parent.display()));
        }
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(bare)
        .args(["worktree", "add"])
        .arg(worktree)
        .arg(branch)
        .output();
    match output {
        Ok(o) if o.status.success() => ApplyStatus::Moved,
        Ok(o) => ApplyStatus::Failed(format!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => ApplyStatus::Failed(format!("git worktree add failed: {e}")),
    }
}

pub(crate) fn move_worktree(bare: &Path, from: &Path, to: &Path) -> ApplyStatus {
    if let Some(parent) = to.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return ApplyStatus::Failed(format!("cannot create {}: {e}", parent.display()));
        }
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(bare)
        .args(["worktree", "move"])
        .arg(from)
        .arg(to)
        .output();
    match output {
        Ok(o) if o.status.success() => ApplyStatus::Moved,
        Ok(o) => ApplyStatus::Failed(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => ApplyStatus::Failed(e.to_string()),
    }
}

/// Remove a redundant bare's worktree (consolidation). The worktree is clean
/// and its content is reproducible from the winner, so `git worktree remove
/// --force` is non-destructive to unique work. (AX-CLI-REPO-MIGRATE-CONSOLIDATE-DUPLICATE-BARES)
fn drop_worktree(bare: &Path, worktree: &Path) -> ApplyStatus {
    let output = Command::new("git")
        .arg("-C")
        .arg(bare)
        .args(["worktree", "remove", "--force"])
        .arg(worktree)
        .output();
    match output {
        Ok(o) if o.status.success() => ApplyStatus::Moved,
        Ok(o) => {
            // Fall back to a direct directory removal if git can't (e.g. the
            // admin link is already gone); the working copy is reproducible.
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            if worktree.exists() && fs::remove_dir_all(worktree).is_ok() {
                ApplyStatus::MovedWithWarnings(vec![format!(
                    "git worktree remove failed ({stderr}); removed the directory directly"
                )])
            } else {
                ApplyStatus::Failed(stderr)
            }
        }
        Err(e) => ApplyStatus::Failed(e.to_string()),
    }
}

/// Retire a redundant bare to recoverable trash. First every branch the
/// redundant bare has that the winner lacks is recreated in the winner at the
/// same commit (the winner already contains the object — the bare is a subset),
/// so no branch name or history is lost; then the bare is moved to `trash`.
/// (AX-CLI-REPO-MIGRATE-CONSOLIDATE-DUPLICATE-BARES)
fn retire_bare(redundant: &Path, winner: &Path, trash: &Path) -> ApplyStatus {
    if trash.exists() {
        return ApplyStatus::Failed(format!("trash destination already exists: {}", trash.display()));
    }
    let mut warnings = Vec::new();

    // Mirror redundant-only branch names into the winner so nothing is lost.
    let winner_branches: std::collections::HashSet<String> =
        run_git_capture(winner, &["for-each-ref", "--format=%(refname:short)", "refs/heads"])
            .map(|s| s.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
            .unwrap_or_default();
    if let Some(out) =
        run_git_capture(redundant, &["for-each-ref", "--format=%(refname:short) %(objectname)", "refs/heads"])
    {
        for line in out.lines() {
            let mut parts = line.split_whitespace();
            let (Some(name), Some(sha)) = (parts.next(), parts.next()) else {
                continue;
            };
            if winner_branches.contains(name) {
                continue; // winner already has this branch (same or ahead — it is a superset)
            }
            let created = Command::new("git")
                .arg("-C")
                .arg(winner)
                .args(["branch", name, sha])
                .output();
            match created {
                Ok(o) if o.status.success() => {}
                Ok(o) => warnings.push(format!(
                    "could not mirror branch {name} into winner: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                )),
                Err(e) => warnings.push(format!("could not mirror branch {name}: {e}")),
            }
        }
    }

    // Move the redundant bare aside to recoverable trash.
    if let Some(parent) = trash.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return ApplyStatus::Failed(format!("cannot create {}: {e}", parent.display()));
        }
    }
    if let Err(e) = fs::rename(redundant, trash) {
        return ApplyStatus::Failed(format!("could not move bare to trash: {e}"));
    }

    if warnings.is_empty() {
        ApplyStatus::Moved
    } else {
        ApplyStatus::MovedWithWarnings(warnings)
    }
}

/// Relocate a bare repo to `to`, then repair and de-bare its linked worktrees.
///
/// Robustness contract (AX-CLI-REPO-MIGRATE-APPLY-SAFE):
/// - A destination nested inside the source (`/x` → `/x/sub.git`) is moved via a
///   temp sibling so `fs::rename` never hits EINVAL.
/// - Worktrees already broken at plan time are NOT handed to `git worktree
///   repair` (it would fail on them); they are reported as warnings instead of
///   failing the whole bare move.
/// - Each healthy worktree gets a per-worktree `core.bare=false` override when the
///   relocated bare uses `extensions.worktreeConfig` with a shared `core.bare=true`
///   — otherwise the bare flag leaks in and working-tree ops exit 128.
pub(crate) fn move_bare(from: &Path, to: &Path, worktrees: &[WorktreeRepairTarget]) -> ApplyStatus {
    if to.exists() {
        return ApplyStatus::Failed(format!("destination already exists: {}", to.display()));
    }
    if let Some(parent) = to.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return ApplyStatus::Failed(format!("cannot create {}: {e}", parent.display()));
        }
    }

    // If the destination is nested inside the source, a direct rename is EINVAL
    // (moving a directory into its own subtree). Stage through a temp sibling.
    if to.starts_with(from) {
        let staged = staging_path(from);
        if staged.exists() {
            return ApplyStatus::Failed(format!(
                "staging path already exists: {}",
                staged.display()
            ));
        }
        if let Err(e) = fs::rename(from, &staged) {
            return ApplyStatus::Failed(format!("rename (stage) failed: {e}"));
        }
        if let Some(parent) = to.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                let _ = fs::rename(&staged, from); // best-effort rollback
                return ApplyStatus::Failed(format!("cannot create {}: {e}", parent.display()));
            }
        }
        if let Err(e) = fs::rename(&staged, to) {
            let _ = fs::rename(&staged, from); // best-effort rollback
            return ApplyStatus::Failed(format!("rename (final) failed: {e}"));
        }
    } else if let Err(e) = fs::rename(from, to) {
        return ApplyStatus::Failed(format!("rename failed: {e}"));
    }

    let mut warnings = Vec::new();

    // Broken worktrees cannot be repaired by `git worktree repair`; name them.
    for wt in worktrees.iter().filter(|w| w.broken) {
        warnings.push(format!(
            "could not repair already-broken worktree: {}",
            wt.path.display()
        ));
    }

    // Repair the healthy worktrees from the bare's new location.
    let healthy: Vec<&PathBuf> = worktrees
        .iter()
        .filter(|w| !w.broken)
        .map(|w| &w.path)
        .collect();
    if !healthy.is_empty() {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(to).args(["worktree", "repair"]);
        for wt in &healthy {
            cmd.arg(wt);
        }
        match cmd.output() {
            Ok(o) if o.status.success() => {}
            Ok(o) => warnings.push(format!(
                "repair reported: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            Err(e) => warnings.push(format!("repair could not run: {e}")),
        }
        // Restore the per-worktree non-bare override so working-tree ops work.
        for wt in &healthy {
            ensure_worktree_not_bare(to, wt);
        }
    }

    if warnings.is_empty() {
        ApplyStatus::Moved
    } else {
        ApplyStatus::MovedWithWarnings(warnings)
    }
}

/// Move `src` to `dst`, falling back to a recursive copy + delete when a plain
/// rename fails — most importantly when the paths sit on different volumes
/// (Windows) or filesystems (`EXDEV`). `bareRoot` and `root` may live on
/// separate drives, so moving a standalone clone's `.git` into the bare root must
/// tolerate a cross-device move.
fn move_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    if fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    // Cross-device (or otherwise rename-refused): copy IN FULL first, then remove
    // the source best-effort. The copy completes (propagating any error) before a
    // single byte of the source is deleted, so a failed or partial source removal
    // can NEVER lose data — the authoritative copy already exists at `dst`. A
    // source leftover is benign; callers remove the source tree wholesale later.
    let meta = fs::symlink_metadata(src)?;
    if meta.is_dir() {
        copy_dir_all(src, dst)?;
        let _ = fs::remove_dir_all(src);
    } else {
        fs::copy(src, dst)?;
        let _ = fs::remove_file(src);
    }
    Ok(())
}

/// Remove a file or directory tree at `p`; a missing path is success. Best-effort
/// cleanup helper used when undoing a partial overlay.
fn remove_path(p: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p),
        Ok(_) => fs::remove_file(p),
        Err(_) => Ok(()),
    }
}

/// Recursively copy `src` into `dst` (creating `dst`). Used only as the
/// cross-device fallback for [`move_path`].
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_dir_all(&from, &to)?;
        } else if ft.is_symlink() {
            // Best-effort: recreate symlinks on unix, copy the target elsewhere.
            // Git objects are never symlinks, so this only matters for the
            // overlaid working tree (rare, and the rename path covers same-volume).
            #[cfg(unix)]
            {
                if let Ok(target) = fs::read_link(&from) {
                    let _ = std::os::unix::fs::symlink(target, &to);
                }
            }
            #[cfg(not(unix))]
            {
                let _ = fs::copy(&from, &to);
            }
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// How a single top-level entry was placed into the new worktree, so a failure
/// part-way can be undone without ever destructively (partially) deleting the
/// source clone.
enum Placed {
    /// Atomically renamed out of the source — undo by renaming it back.
    Renamed(std::ffi::OsString),
    /// Copied (source left intact because rename was refused, e.g. a locked or
    /// in-use file) — undo by removing the copy at the destination.
    Copied(std::ffi::OsString),
}

/// Populate `dst` (the new worktree, which holds only its `.git` file) with the
/// clone's working files WITHOUT ever destructively deleting the source. Each
/// top-level entry is moved by an atomic `rename` when possible; if the rename is
/// refused — a locked/in-use file (the failure mode that previously corrupted a
/// repo) or a cross-device move — the entry is COPIED instead and the source copy
/// is LEFT in place. On any failure, every entry placed so far is undone
/// (renamed-back, or its copy removed) so `src` is left whole, and the offending
/// entry name + error is returned. The source clone (which may still hold copied
/// entries) is removed wholesale by the caller only after the whole reconstruct
/// succeeds, so a locked file can never cause data loss — only a benign leftover.
fn overlay_dir_contents(src: &Path, dst: &Path) -> Result<(), (String, std::io::Error)> {
    let undo = |placed: &[Placed]| {
        for p in placed.iter().rev() {
            match p {
                Placed::Renamed(name) => {
                    let _ = fs::rename(dst.join(name), src.join(name));
                }
                Placed::Copied(name) => {
                    let _ = remove_path(&dst.join(name));
                }
            }
        }
    };
    let entries = match fs::read_dir(src) {
        Ok(e) => e,
        Err(e) => return Err(("<read source>".to_string(), e)),
    };
    let mut placed: Vec<Placed> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                undo(&placed);
                return Err(("<read entry>".to_string(), e));
            }
        };
        let name = entry.file_name();
        let from = src.join(&name);
        let to = dst.join(&name);

        // Fast, atomic, and safe: rename never partially moves on failure.
        if fs::rename(&from, &to).is_ok() {
            placed.push(Placed::Renamed(name));
            continue;
        }

        // Rename refused (locked/in-use file, or cross-device): copy, leaving the
        // source untouched so nothing can be lost if a later entry fails.
        let meta = match fs::symlink_metadata(&from) {
            Ok(m) => m,
            Err(e) => {
                undo(&placed);
                return Err((name.to_string_lossy().into_owned(), e));
            }
        };
        let copied = if meta.is_dir() {
            copy_dir_all(&from, &to)
        } else {
            fs::copy(&from, &to).map(|_| ())
        };
        if let Err(e) = copied {
            let _ = remove_path(&to); // clean the partial copy at the destination
            undo(&placed);
            return Err((name.to_string_lossy().into_owned(), e));
        }
        placed.push(Placed::Copied(name));
    }
    Ok(())
}

/// Convert a standalone (full) clone into the canonical bare+worktree layout
/// without losing any work. (AX-CLI-REPO-MIGRATE-RECONSTRUCT-BARE)
///
/// 1. Move `clone/.git` to `bare`, mark it bare, and point its HEAD at the
///    placeholder branch ([[ax-cli-repo-bare-head-placeholder]]) so the clone's
///    working branch is freely checkout-able in a linked worktree.
/// 2. `git worktree add --no-checkout` registers `worktree` (admin dir, index at
///    HEAD's tree, per-worktree `core.bare=false`) without materializing files.
/// 3. The clone's real working files — tracked, modified, untracked, AND
///    gitignored (e.g. `.env`) — are moved into the new worktree, then
///    `git worktree repair` normalizes the links.
///
/// Best-effort rollback restores the original standalone clone if any step before
/// the file overlay completes fails.
fn reconstruct_bare(
    clone: &Path,
    bare: &Path,
    worktree: &Path,
    _branch: Option<&str>,
) -> ApplyStatus {
    // Refuse to reconstruct a clone the current process is sitting inside: moving
    // its `.git` out from under a live cwd corrupts the in-flight run and is the
    // common self-inflicted cause of a half-finished migration. (Files held open
    // by OTHER processes can't be detected portably — `move_path` degrades to a
    // non-destructive copy and the resumable path recovers those.)
    // (AX-CLI-REPO-MIGRATE-RECONSTRUCT-RESUMABLE)
    if let Ok(cwd) = std::env::current_dir() {
        if same_path(&cwd, clone) || cwd.starts_with(clone) {
            return ApplyStatus::Failed(format!(
                "clone is in use (current directory is inside {}); cd elsewhere and re-run",
                clone.display()
            ));
        }
    }
    if worktree.exists() {
        return ApplyStatus::Failed(format!(
            "worktree destination already exists: {}",
            worktree.display()
        ));
    }
    // A bare already at the destination is not automatically fatal: a prior run may
    // have left it half-finished. Resume it, salvage-and-rebuild it, or refuse a
    // foreign repo. (AX-CLI-REPO-MIGRATE-RECONSTRUCT-RESUMABLE)
    if bare.exists() {
        match classify_existing_bare(clone, bare) {
            ExistingBare::Resumable => return resume_reconstruct(clone, bare, worktree),
            ExistingBare::Foreign => {
                return ApplyStatus::Failed(format!(
                    "bare destination already exists (different repository): {}",
                    bare.display()
                ));
            }
            ExistingBare::Salvage => match salvage_partial_bare(bare) {
                Ok(trash) => {
                    println!(
                        "      · cleared partial bare from a prior run → {}",
                        trash.display()
                    );
                    // fall through to a clean rebuild from the intact clone
                }
                Err(e) => {
                    return ApplyStatus::Failed(format!(
                        "could not clear partial bare {}: {e}",
                        bare.display()
                    ));
                }
            },
        }
    }
    let git_dir = clone.join(".git");
    if !git_dir.is_dir() {
        return ApplyStatus::Failed(format!(
            "not a standalone clone (no .git directory): {}",
            clone.display()
        ));
    }

    // 1. Move the clone's git directory to become the bare.
    if let Some(parent) = bare.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return ApplyStatus::Failed(format!("cannot create {}: {e}", parent.display()));
        }
    }
    if let Err(e) = move_path(&git_dir, bare) {
        // A cross-device copy that fails part-way (e.g. a locked file) leaves a
        // partial bare behind; remove it so a re-run is not permanently blocked by
        // an orphaned destination. The source clone is untouched (move_path deletes
        // it only after a fully successful copy). (AX-CLI-REPO-MIGRATE-RECONSTRUCT-RESUMABLE)
        let _ = remove_path(bare);
        return ApplyStatus::Failed(format!("could not move .git to bare: {e}"));
    }

    // Snapshot the original HEAD so rollback can rebuild the standalone clone.
    let orig_head = fs::read_to_string(bare.join("HEAD")).ok();
    let git_config = |args: &[&str]| {
        let _ = Command::new("git").arg("-C").arg(bare).arg("config").args(args).output();
    };
    let restore_clone = || {
        // Undo the bare-ification and move the git dir back under the clone.
        if let Some(h) = &orig_head {
            let _ = fs::write(bare.join("HEAD"), h);
        }
        git_config(&["core.bare", "false"]);
        let _ = fs::remove_dir_all(worktree);
        let _ = move_path(bare, &git_dir);
    };

    // 2. Bare-ify config + canonical fetch refspec (matches `repo clone`).
    git_config(&["core.bare", "true"]);
    git_config(&["--unset", "core.worktree"]);
    git_config(&["remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"]);

    // 3. Resolve the clone's current ref BEFORE repointing HEAD to the placeholder.
    let head_branch = run_git_capture(bare, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let head_sha = run_git_capture(bare, &["rev-parse", "HEAD"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // 4. Free the working branch by pointing the bare HEAD at the placeholder, or
    //    `git worktree add <branch>` would be refused. (AX-CLI-REPO-BARE-HEAD-PLACEHOLDER)
    let _ = point_bare_head_to_placeholder(bare);

    // 5. Register the worktree without checking out (the real files live in the clone).
    if let Some(parent) = worktree.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            restore_clone();
            return ApplyStatus::Failed(format!("cannot create {}: {e}", parent.display()));
        }
    }
    let add = match (&head_branch, &head_sha) {
        (Some(b), _) => Command::new("git")
            .arg("-C")
            .arg(bare)
            .args(["worktree", "add", "--no-checkout"])
            .arg(worktree)
            .arg(b)
            .output(),
        (None, Some(sha)) => Command::new("git")
            .arg("-C")
            .arg(bare)
            .args(["worktree", "add", "--no-checkout", "--detach"])
            .arg(worktree)
            .arg(sha)
            .output(),
        (None, None) => {
            restore_clone();
            return ApplyStatus::Failed("could not determine the clone's HEAD".to_string());
        }
    };
    match add {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            restore_clone();
            return ApplyStatus::Failed(format!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ));
        }
        Err(e) => {
            restore_clone();
            return ApplyStatus::Failed(format!("git worktree add could not run: {e}"));
        }
    }

    // 5b. Restore the clone's ORIGINAL index into the new worktree so staged vs.
    //     unstaged state is preserved exactly. `git worktree add` reset the
    //     worktree index to HEAD's tree; the clone's real index moved into the
    //     bare with `.git` (at `<bare>/index`). Copy it over the worktree's index
    //     (its path comes from the `.git` file `worktree add` just wrote). Same
    //     repo, same HEAD commit, so the index is consistent. Best-effort: on any
    //     failure the worktree simply shows staged changes as unstaged (no work
    //     lost), which is why this is a warning, not a rollback.
    let mut warnings = Vec::new();
    if let Some(admin_dir) = read_gitdir_file(&worktree.join(".git")) {
        let src_index = bare.join("index");
        let dst_index = admin_dir.join("index");
        if src_index.is_file() {
            if let Err(e) = fs::copy(&src_index, &dst_index) {
                warnings.push(format!(
                    "could not preserve the staged index (staged changes will show as unstaged): {e}"
                ));
            }
        }
    }

    // 6. Overlay the clone's working files into the new worktree (which holds only
    //    the `.git` file written by `worktree add`). Reverse + rollback on error.
    if let Err((name, e)) = overlay_dir_contents(clone, worktree) {
        let _ = Command::new("git")
            .arg("-C")
            .arg(bare)
            .args(["worktree", "remove", "--force"])
            .arg(worktree)
            .output();
        restore_clone();
        return ApplyStatus::Failed(format!(
            "could not move working file '{name}' into worktree: {e}"
        ));
    }

    // 7. Normalize the worktree links and ensure it is not treated as bare.
    match Command::new("git")
        .arg("-C")
        .arg(bare)
        .args(["worktree", "repair"])
        .arg(worktree)
        .output()
    {
        Ok(o) if o.status.success() => {}
        Ok(o) => warnings.push(format!(
            "worktree repair reported: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => warnings.push(format!("worktree repair could not run: {e}")),
    }
    ensure_worktree_not_bare(bare, worktree);

    // 8. Remove the original clone directory now that the worktree holds every
    //    file. The overlay renamed entries out (leaving the clone empty) or, for
    //    any locked/in-use file, COPIED it (leaving the original behind) — either
    //    way the worktree is authoritative, so removing the clone is safe and a
    //    failure here only leaves a benign leftover, never lost data.
    if clone.exists() {
        if let Err(e) = fs::remove_dir_all(clone) {
            warnings.push(format!(
                "could not fully remove the old clone directory {} (its contents are safely in the new worktree): {e}",
                clone.display()
            ));
        }
    }

    if warnings.is_empty() {
        ApplyStatus::Moved
    } else {
        ApplyStatus::MovedWithWarnings(warnings)
    }
}

/// A temp sibling of `dir` used to stage a nested move. Deterministic (no
/// randomness — `Math.random`/clock are unavailable in some runtimes) and
/// collision-checked by the caller.
fn staging_path(dir: &Path) -> PathBuf {
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo");
    let parent = dir.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(".{name}.stokd-migrate-staging"))
}

/// When a relocated bare uses `extensions.worktreeConfig` with a shared
/// `core.bare=true`, each linked worktree needs its own `core.bare=false` in
/// `config.worktree`, or git treats the worktree as bare and refuses working-tree
/// operations (`git diff HEAD`, `git ls-files -o` → exit 128). `git worktree
/// repair` does not restore this, so we set it explicitly. No-op for bares without
/// worktreeConfig (their worktrees are non-bare via their gitdir already).
fn ensure_worktree_not_bare(bare: &Path, worktree: &Path) {
    let worktree_config = run_git_capture(bare, &["config", "--get", "extensions.worktreeConfig"])
        .map(|s| s.trim() == "true")
        .unwrap_or(false);
    if !worktree_config {
        return;
    }
    let shared_bare = run_git_capture(bare, &["config", "--get", "core.bare"])
        .map(|s| s.trim() == "true")
        .unwrap_or(false);
    if !shared_bare {
        return;
    }
    let _ = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["config", "--worktree", "core.bare", "false"])
        .output();
}

fn report_apply_results(results: &[ApplyResult]) {
    let mut moved = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    for r in results {
        match &r.status {
            ApplyStatus::Moved => {
                moved += 1;
                println!("  ✓ {}", action_label(&r.action));
            }
            ApplyStatus::MovedWithWarnings(warnings) => {
                moved += 1;
                println!("  ✓ {}  (with warnings)", action_label(&r.action));
                for w in warnings {
                    println!("      ! {w}");
                }
            }
            ApplyStatus::Skipped(reason) => {
                skipped += 1;
                println!("  ⊘ {}", action_label(&r.action));
                println!("      skipped: {}", reason.as_str());
            }
            ApplyStatus::Failed(e) => {
                failed += 1;
                println!("  ✗ {}", action_label(&r.action));
                println!("      {e}");
            }
        }
    }
    println!("\nMigration complete: {moved} moved, {skipped} skipped (unsafe), {failed} failed.");
    if failed > 0 {
        std::process::exit(1);
    }
}

/// Non-interactive `--apply`: scan, then apply every safe action.
fn run_apply(orphan_days: u32, filter: &MigrateFilter) {
    let ScanResult {
        repo_groups,
        standalone_clones,
        data_dirs,
        bare_root,
        worktree_root,
        main_worktree_name,
        ..
    } = scan(true, filter);
    let plan = build_migration_plan(
        &repo_groups,
        &standalone_clones,
        &data_dirs,
        &bare_root,
        &worktree_root,
        &main_worktree_name,
        orphan_days,
    );
    let _ = save_cached_plan(&plan);

    let actions: Vec<MigrateAction> = plan
        .groups
        .iter()
        .flat_map(group_actions)
        .chain(reconstruct_actions(&plan))
        .collect();
    if actions.is_empty() {
        println!("Nothing to migrate — every repo is already at its canonical layout.");
        return;
    }
    println!("Applying {} migration action(s)...\n", actions.len());
    let results = apply_plan(&plan, &actions);
    report_apply_results(&results);
}

// ─────────────────────────────────────────────────────────────────────────────
// Interactive selection (collapsible multi-select tree)
// ─────────────────────────────────────────────────────────────────────────────

const REVERSE_ON: &str = "\x1b[7m";
const REVERSE_OFF: &str = "\x1b[27m";

struct UiAction {
    action: MigrateAction,
    label: String,
    selected: bool,
}

struct UiGroup {
    header: String,
    expanded: bool,
    actions: Vec<UiAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowRef {
    Header(usize),
    Action(usize, usize),
}

/// Flatten the (collapsible) groups into the rows that are currently visible:
/// every group header, plus the actions of each expanded group.
fn visible_rows(groups: &[UiGroup]) -> Vec<RowRef> {
    let mut rows = Vec::new();
    for (gi, group) in groups.iter().enumerate() {
        rows.push(RowRef::Header(gi));
        if group.expanded {
            for ai in 0..group.actions.len() {
                rows.push(RowRef::Action(gi, ai));
            }
        }
    }
    rows
}

struct Picker {
    groups: Vec<UiGroup>,
    cursor: usize,
    /// Context lines rendered above the tree — e.g. how many repos were
    /// omitted because they are already canonical, and report-only findings
    /// (data dirs) that carry no selectable action.
    notes: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum PickerOutcome {
    Continue,
    Apply,
    Cancel,
}

impl Picker {
    fn rows(&self) -> Vec<RowRef> {
        visible_rows(&self.groups)
    }

    fn current_row(&self) -> Option<RowRef> {
        self.rows().get(self.cursor).copied()
    }

    fn move_cursor(&mut self, delta: isize) {
        let n = self.rows().len();
        if n == 0 {
            return;
        }
        let next = (self.cursor as isize + delta).rem_euclid(n as isize);
        self.cursor = next as usize;
    }

    /// Space: toggle selection. On a header, set every child action to the
    /// inverse of "all already selected"; on an action, flip that one.
    fn toggle_selection(&mut self) {
        match self.current_row() {
            Some(RowRef::Header(gi)) => {
                let all = self.groups[gi].actions.iter().all(|a| a.selected);
                for a in &mut self.groups[gi].actions {
                    a.selected = !all;
                }
            }
            Some(RowRef::Action(gi, ai)) => {
                self.groups[gi].actions[ai].selected = !self.groups[gi].actions[ai].selected;
            }
            None => {}
        }
    }

    fn expand(&mut self) {
        if let Some(RowRef::Header(gi)) = self.current_row() {
            self.groups[gi].expanded = true;
        }
    }

    fn collapse(&mut self) {
        match self.current_row() {
            Some(RowRef::Header(gi)) => self.groups[gi].expanded = false,
            Some(RowRef::Action(gi, _)) => {
                self.groups[gi].expanded = false;
                if let Some(pos) = self.rows().iter().position(|r| *r == RowRef::Header(gi)) {
                    self.cursor = pos;
                }
            }
            None => {}
        }
    }

    fn selected_actions(&self) -> Vec<MigrateAction> {
        self.groups
            .iter()
            .flat_map(|g| {
                g.actions
                    .iter()
                    .filter(|a| a.selected)
                    .map(|a| a.action.clone())
            })
            .collect()
    }
}

fn handle_key(picker: &mut Picker, key: KeyCode) -> PickerOutcome {
    match key {
        KeyCode::Up => {
            picker.move_cursor(-1);
            PickerOutcome::Continue
        }
        KeyCode::Down => {
            picker.move_cursor(1);
            PickerOutcome::Continue
        }
        KeyCode::Char(' ') => {
            picker.toggle_selection();
            PickerOutcome::Continue
        }
        KeyCode::Right => {
            picker.expand();
            PickerOutcome::Continue
        }
        KeyCode::Left => {
            picker.collapse();
            PickerOutcome::Continue
        }
        // Enter drills into a header (expand/collapse); on an action it toggles.
        KeyCode::Enter => {
            match picker.current_row() {
                Some(RowRef::Header(gi)) => {
                    picker.groups[gi].expanded = !picker.groups[gi].expanded;
                }
                Some(RowRef::Action(..)) => picker.toggle_selection(),
                None => {}
            }
            PickerOutcome::Continue
        }
        KeyCode::Char('a') => PickerOutcome::Apply,
        KeyCode::Esc => PickerOutcome::Cancel,
        _ => PickerOutcome::Continue,
    }
}

fn render_interactive_frame(picker: &Picker) -> String {
    let mut out = String::new();
    out.push_str("Select migration actions to apply\r\n");
    out.push_str(
        "↑/↓ move · →/Enter expand · ← collapse · Space toggle · a apply · Esc cancel\r\n",
    );
    for note in &picker.notes {
        out.push_str(note);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    let rows = picker.rows();
    for (i, row) in rows.iter().enumerate() {
        let line = match row {
            RowRef::Header(gi) => {
                let g = &picker.groups[*gi];
                let arrow = if g.expanded { "▼" } else { "▶" };
                let sel = g.actions.iter().filter(|a| a.selected).count();
                format!("{arrow} {} [{}/{}]", g.header, sel, g.actions.len())
            }
            RowRef::Action(gi, ai) => {
                let a = &picker.groups[*gi].actions[*ai];
                let checkbox = if a.selected { "[x]" } else { "[ ]" };
                format!("    {checkbox} {}", a.label)
            }
        };
        if i == picker.cursor {
            out.push_str(REVERSE_ON);
            out.push_str(&line);
            out.push_str(REVERSE_OFF);
        } else {
            out.push_str(&line);
        }
        out.push_str("\r\n");
    }
    out
}

/// Build the interactive groups from a plan, omitting any repo group that has no
/// applyable actions. Actions start unselected — the user marks what to apply.
/// Standalone clones slated for bare reconstruction are appended as their own
/// `owner/repo (reconstruct)` groups. (AX-CLI-REPO-MIGRATE-RECONSTRUCT-BARE)
fn build_ui_groups(plan: &MigrationPlan) -> Vec<UiGroup> {
    let mut groups: Vec<UiGroup> = plan
        .groups
        .iter()
        .filter_map(|g| {
            let actions = group_actions(g);
            if actions.is_empty() {
                return None;
            }
            let header = match (&g.bare.owner, &g.bare.repo) {
                (Some(o), Some(r)) => format!("{o}/{r}"),
                _ => g.bare.path.display().to_string(),
            };
            Some(UiGroup {
                header,
                expanded: false,
                actions: actions
                    .into_iter()
                    .map(|a| UiAction {
                        label: action_label(&a),
                        action: a,
                        selected: false,
                    })
                    .collect(),
            })
        })
        .collect();

    for r in &plan.reconstruct {
        let action = MigrateAction::ReconstructBare {
            clone: r.clone.clone(),
            bare: r.bare.clone(),
            worktree: r.worktree.clone(),
            branch: r.branch.clone(),
        };
        groups.push(UiGroup {
            header: format!("{}/{} (reconstruct bare)", r.owner, r.repo),
            expanded: false,
            actions: vec![UiAction {
                label: action_label(&action),
                action,
                selected: false,
            }],
        });
    }
    groups
}

/// Run the interactive picker, returning the selected actions (`Some`, possibly
/// empty) or `None` if the user aborted. Honors `STOKD_MIGRATE_SELECTION` for
/// non-interactive/test use: `all`, `none`, `abort`/`cancel`, or a
/// comma-separated list of flattened action indices.
fn run_interactive_picker(groups: Vec<UiGroup>, notes: Vec<String>) -> Option<Vec<MigrateAction>> {
    let mut picker = Picker {
        groups,
        cursor: 0,
        notes,
    };

    if let Ok(selection) = std::env::var("STOKD_MIGRATE_SELECTION") {
        let s = selection.trim().to_ascii_lowercase();
        if s == "abort" || s == "cancel" {
            return None;
        }
        if s == "all" {
            for g in &mut picker.groups {
                for a in &mut g.actions {
                    a.selected = true;
                }
            }
            return Some(picker.selected_actions());
        }
        if s == "none" {
            return Some(Vec::new());
        }
        let flat: Vec<MigrateAction> = picker
            .groups
            .iter()
            .flat_map(|g| g.actions.iter().map(|a| a.action.clone()))
            .collect();
        let mut chosen = Vec::new();
        for tok in s.split(',') {
            if let Ok(idx) = tok.trim().parse::<usize>() {
                if let Some(a) = flat.get(idx) {
                    chosen.push(a.clone());
                }
            }
        }
        return Some(chosen);
    }

    use crossterm::cursor;
    use crossterm::event::{self, Event, KeyEventKind};
    use crossterm::execute;
    use crossterm::terminal::{self, Clear, ClearType};
    use std::io::{self, Write};

    let mut stdout = io::stdout();
    terminal::enable_raw_mode().ok()?;
    loop {
        let frame = render_interactive_frame(&picker);
        if execute!(stdout, Clear(ClearType::All), cursor::MoveTo(0, 0)).is_err()
            || write!(stdout, "{frame}").is_err()
        {
            let _ = terminal::disable_raw_mode();
            return None;
        }
        let _ = stdout.flush();

        match event::read() {
            // Windows delivers BOTH Press and Release key events; acting on
            // both makes every toggle fire twice (select → instantly deselect).
            // Only Press drives the picker.
            Ok(Event::Key(key)) if key.kind != KeyEventKind::Press => {}
            Ok(Event::Key(key)) => match handle_key(&mut picker, key.code) {
                PickerOutcome::Continue => {}
                PickerOutcome::Apply => {
                    let _ = terminal::disable_raw_mode();
                    let _ = execute!(stdout, Clear(ClearType::All), cursor::MoveTo(0, 0));
                    return Some(picker.selected_actions());
                }
                PickerOutcome::Cancel => {
                    let _ = terminal::disable_raw_mode();
                    let _ = execute!(stdout, Clear(ClearType::All), cursor::MoveTo(0, 0));
                    return None;
                }
            },
            Ok(_) => {}
            Err(_) => {
                let _ = terminal::disable_raw_mode();
                return None;
            }
        }
    }
}

/// What `run_interactive` should do for its plan, given whether a cached plan
/// exists and whether reuse is mandatory (`--apply`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractivePlanSource {
    /// Reuse the cached plan on disk.
    UseCached,
    /// No cache and reuse is mandatory (`--apply`) — error out.
    RequireCachedError,
    /// No cache and reuse is optional — scan fresh and cache the result.
    ScanFresh,
}

/// Prefer a previously-cached plan so `sgit repo migrate --plan` followed by
/// `--interactive` reuses the analysis instead of re-scanning. `--apply`
/// (`reuse_required`) hard-requires a cache; plain `--interactive` falls back to
/// a fresh scan when none exists.
fn choose_interactive_plan_source(
    has_cached_plan: bool,
    reuse_required: bool,
) -> InteractivePlanSource {
    match (has_cached_plan, reuse_required) {
        (true, _) => InteractivePlanSource::UseCached,
        (false, true) => InteractivePlanSource::RequireCachedError,
        (false, false) => InteractivePlanSource::ScanFresh,
    }
}

/// `--interactive` (optionally with `--apply`): reuse the last cached plan when
/// one exists (re-scanning only when absent), let the user pick actions, then
/// apply the selection. Pass `--plan` first to force a fresh scan.
fn run_interactive(reuse_required: bool, orphan_days: u32, filter: &MigrateFilter) {
    let cached = load_cached_plan();
    let plan = match choose_interactive_plan_source(cached.is_some(), reuse_required) {
        InteractivePlanSource::UseCached => {
            let p = cached.expect("cache presence checked");
            println!(
                "Using cached migration plan ({} repo group(s)). Run `sgit repo migrate --plan` to re-scan.",
                p.groups.len()
            );
            p
        }
        InteractivePlanSource::RequireCachedError => {
            eprintln!(
                "error: no cached migration plan found. Run `sgit repo migrate --plan` or `sgit repo migrate --interactive` first."
            );
            std::process::exit(2);
        }
        InteractivePlanSource::ScanFresh => {
            let ScanResult {
                repo_groups,
                standalone_clones,
                data_dirs,
                bare_root,
                worktree_root,
                main_worktree_name,
                ..
            } = scan(true, filter);
            let p = build_migration_plan(
                &repo_groups,
                &standalone_clones,
                &data_dirs,
                &bare_root,
                &worktree_root,
                &main_worktree_name,
                orphan_days,
            );
            let _ = save_cached_plan(&p);
            p
        }
    };

    // Honor the positional filter even when reusing a cached (full) plan, so
    // `sgit repo migrate <owner> -i` only offers that owner's repos.
    let mut plan = plan;
    plan.groups.retain(|g| {
        let owner_repo = match (&g.bare.owner, &g.bare.repo) {
            (Some(o), Some(r)) => Some((o.clone(), r.clone())),
            _ => None,
        };
        matches_filter(owner_repo.as_ref(), filter)
    });
    plan.reconstruct
        .retain(|r| matches_filter(Some(&(r.owner.clone(), r.repo.clone())), filter));

    let groups = build_ui_groups(&plan);
    if groups.is_empty() {
        println!("Nothing to migrate — every repo is already at its canonical layout.");
        if !plan.data_dirs.is_empty() {
            println!(
                "note: {} non-git data dir(s) need manual review — run `sgit repo migrate` for details.",
                plan.data_dirs.len()
            );
        }
        return;
    }

    // Context above the tree so "only N repos shown" reads as already-canonical
    // omission, not a scan gap — and report-only findings stay visible here.
    let mut notes: Vec<String> = Vec::new();
    let canonical_omitted = plan
        .groups
        .iter()
        .filter(|g| group_actions(g).is_empty())
        .count();
    if canonical_omitted > 0 {
        notes.push(format!(
            "{canonical_omitted} repo(s) already fully canonical (not shown)"
        ));
    }
    if !plan.data_dirs.is_empty() {
        notes.push(format!(
            "{} non-git data dir(s) need manual review — run `sgit repo migrate` for the list",
            plan.data_dirs.len()
        ));
    }

    match run_interactive_picker(groups, notes) {
        Some(selected) if !selected.is_empty() => {
            println!("\nApplying {} selected action(s)...\n", selected.len());
            let results = apply_plan(&plan, &selected);
            report_apply_results(&results);
        }
        Some(_) => println!("No actions selected; nothing applied."),
        None => println!("Aborted; nothing applied."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Test helpers ────────────────────────────────────────────────────────

    fn git_ok(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Create a bare repo at `bare` (with one commit on `main`) whose origin URL
    /// is `origin`, so the scanner resolves owner/repo from it.
    fn setup_bare(tmp: &Path, bare: &Path, origin: &str) {
        fs::create_dir_all(bare.parent().unwrap()).unwrap();
        let seed = tmp.join("seed");
        fs::create_dir_all(&seed).unwrap();
        git_ok(&seed, &["init", "-b", "main"]);
        git_ok(&seed, &["config", "user.email", "t@example.com"]);
        git_ok(&seed, &["config", "user.name", "Test"]);
        fs::write(seed.join("README.md"), "hi").unwrap();
        git_ok(&seed, &["add", "-A"]);
        git_ok(&seed, &["commit", "-m", "init"]);

        let out = Command::new("git")
            .args([
                "clone",
                "--bare",
                &seed.to_string_lossy(),
                &bare.to_string_lossy(),
            ])
            .output()
            .expect("git clone --bare");
        assert!(
            out.status.success(),
            "clone bare failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let cfg = Command::new("git")
            .args([
                "--git-dir",
                &bare.to_string_lossy(),
                "config",
                "remote.origin.url",
                origin,
            ])
            .output()
            .expect("git config origin");
        assert!(cfg.status.success());
        // Clean up the seed so it isn't mistaken for a worktree.
        let _ = fs::remove_dir_all(&seed);
    }

    /// Create a standalone (full) clone at `at` with one commit on `main` and its
    /// `origin` set to `origin`, so the scanner classifies it as a reconstruct
    /// candidate (its `.git` is a directory, not a linked-worktree pointer file).
    fn setup_standalone_clone(at: &Path, origin: &str) {
        fs::create_dir_all(at).unwrap();
        git_ok(at, &["init", "-b", "main"]);
        git_ok(at, &["config", "user.email", "t@example.com"]);
        git_ok(at, &["config", "user.name", "Test"]);
        git_ok(at, &["remote", "add", "origin", origin]);
        fs::write(at.join("README.md"), "hi").unwrap();
        git_ok(at, &["add", "-A"]);
        git_ok(at, &["commit", "-m", "init"]);
    }

    /// Scan a specific (bare_root, worktree_root) pair into a plan, bypassing the
    /// global config so tests are hermetic. Runs the same `scan_roots` core as
    /// production, so every discovery channel (worktree walk, direct bareRoot
    /// walk, registered-worktree enumeration) is exercised.
    fn scan_to_plan(bare_root: &Path, worktree_root: &Path, orphan_days: u32) -> MigrationPlan {
        scan_to_plan_with_cwd(bare_root, worktree_root, None, orphan_days)
    }

    fn scan_to_plan_with_cwd(
        bare_root: &Path,
        worktree_root: &Path,
        cwd: Option<&Path>,
        orphan_days: u32,
    ) -> MigrationPlan {
        let DiscoveredRoots {
            repo_groups,
            standalone_clones,
            data_dirs,
            ..
        } = scan_roots(bare_root, worktree_root, cwd);
        // Use the default `main_worktree_name` pattern so the canonical main
        // leaf is `main` (matching the shipped config default `{branch}`).
        build_migration_plan(
            &repo_groups,
            &standalone_clones,
            &data_dirs,
            bare_root,
            worktree_root,
            "{branch}",
            orphan_days,
        )
    }

    // ── CWD discovery channel ────────────────────────────────────────────────

    #[test]
    fn cwd_channel_discovers_repos_outside_configured_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        fs::create_dir_all(&worktree_root).unwrap();
        // The "cwd": a directory unrelated to either configured root, holding
        // a standalone clone in a subdirectory and a parked bare.
        let cwd = tmp.path().join("projects");
        let clone = cwd.join("widgets");
        setup_standalone_clone(&clone, "git@github.com:acme/widgets.git");
        let parked_bare = cwd.join("gears.git");
        setup_bare(tmp.path(), &parked_bare, "git@github.com:acme/gears.git");

        let plan = scan_to_plan_with_cwd(&bare_root, &worktree_root, Some(&cwd), 60);
        assert!(
            plan.reconstruct.iter().any(|r| same_path(&r.clone, &clone)),
            "clone under the cwd must be adopted: {:?}",
            plan.reconstruct
        );
        assert!(
            plan.groups.iter().any(|g| same_path(&g.bare.path, &parked_bare)),
            "bare parked under the cwd must be discovered"
        );

        // Running FROM a repo: the cwd itself resolves to its enclosing
        // worktree top even though the walk only inspects children.
        let plan2 = scan_to_plan_with_cwd(&bare_root, &worktree_root, Some(&clone), 60);
        assert!(
            plan2.reconstruct.iter().any(|r| same_path(&r.clone, &clone)),
            "the enclosing repo of the cwd must be adopted: {:?}",
            plan2.reconstruct
        );
    }

    // ── Direct bareRoot discovery + main-worktree materialization ───────────

    #[test]
    fn bare_scan_discovers_worktreeless_bare_and_plans_main_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        fs::create_dir_all(&worktree_root).unwrap();
        let bare = bare_root.join("acme").join("widgets.git");
        setup_bare(tmp.path(), &bare, "git@github.com:acme/widgets.git");

        let plan = scan_to_plan(&bare_root, &worktree_root, 60);
        assert_eq!(
            plan.groups.len(),
            1,
            "direct bareRoot walk must find the worktree-less bare"
        );
        let group = &plan.groups[0];
        assert!(group.bare.at_canonical);
        let m = group
            .materialize_main
            .as_ref()
            .expect("missing main worktree must be planned");
        assert_eq!(m.branch, "main");
        assert_eq!(
            m.worktree,
            worktree_root.join("acme").join("widgets").join("main")
        );
        let actions = group_actions(group);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, MigrateAction::MaterializeMainWorktree { .. })),
            "actions: {actions:?}"
        );
    }

    #[test]
    fn full_clone_under_bare_root_is_planned_for_reconstruction() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        fs::create_dir_all(&worktree_root).unwrap();
        let clone = bare_root.join("acme").join("widgets");
        setup_standalone_clone(&clone, "git@github.com:acme/widgets.git");

        let plan = scan_to_plan(&bare_root, &worktree_root, 60);
        assert_eq!(plan.reconstruct.len(), 1, "{:?}", plan.reconstruct);
        let r = &plan.reconstruct[0];
        assert!(same_path(&r.clone, &clone));
        assert_eq!(r.bare, bare_root.join("acme").join("widgets.git"));
        assert_eq!(
            r.worktree,
            worktree_root.join("acme").join("widgets").join("main")
        );
    }

    #[test]
    fn data_dir_under_bare_root_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        fs::create_dir_all(&worktree_root).unwrap();
        // Stranded working data (no git) next to a healthy bare.
        let data = bare_root.join("acme").join("stranded");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("app.csproj"), "x").unwrap();
        setup_bare(
            tmp.path(),
            &bare_root.join("acme").join("widgets.git"),
            "git@github.com:acme/widgets.git",
        );

        // A git-less tree at a canonical-looking spot under the WORKTREE root
        // (e.g. a repo dir whose `.git` was lost) must be reported too.
        let lost = worktree_root.join("acme").join("lost-template");
        fs::create_dir_all(lost.join("main")).unwrap();
        fs::write(lost.join("main").join("App.cs"), "x").unwrap();

        let plan = scan_to_plan(&bare_root, &worktree_root, 60);
        assert!(
            plan.data_dirs.iter().any(|d| same_path(d, &data)),
            "stranded dir must be reported: {:?}",
            plan.data_dirs
        );
        // Reported at its TOPMOST git-less ancestor (here the whole `acme`
        // owner dir is git-less), never once per descendant.
        assert!(
            plan.data_dirs.iter().any(|d| lost.starts_with(d)),
            "git-less tree under the worktree root must be reported: {:?}",
            plan.data_dirs
        );
        // The owner dir itself is NOT misreported — it contains a real bare.
        assert!(
            !plan
                .data_dirs
                .iter()
                .any(|d| same_path(d, &bare_root.join("acme"))),
            "{:?}",
            plan.data_dirs
        );
    }

    #[test]
    fn registered_out_of_root_worktree_gets_move_action() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        fs::create_dir_all(&worktree_root).unwrap();
        let bare = bare_root.join("acme").join("widgets.git");
        setup_bare(tmp.path(), &bare, "git@github.com:acme/widgets.git");
        // Register a worktree OUTSIDE the scanned worktree root (the old-root
        // leftover shape).
        let stray = tmp.path().join("old-root").join("widgets-feat");
        fs::create_dir_all(stray.parent().unwrap()).unwrap();
        let stray_s = stray.to_string_lossy().into_owned();
        git_ok(&bare, &["worktree", "add", "-b", "feat", &stray_s, "main"]);

        let plan = scan_to_plan(&bare_root, &worktree_root, 60);
        let group = plan
            .groups
            .iter()
            .find(|g| g.bare.repo.as_deref() == Some("widgets"))
            .expect("group for the bare");
        let wt = group
            .worktrees
            .iter()
            .find(|w| same_path(&w.path, &stray))
            .expect("registered out-of-root worktree must be adopted");
        assert!(!wt.at_canonical);
        let actions = group_actions(group);
        assert!(
            actions.iter().any(|a| matches!(
                a,
                MigrateAction::MoveWorktree { from, .. } if same_path(from, &stray)
            )),
            "actions: {actions:?}"
        );
    }

    #[test]
    fn clone_gitdir_backlink_reroutes_to_reconstruct_not_move_bare() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        fs::create_dir_all(&worktree_root).unwrap();
        // A full clone OUTSIDE both roots acting as the "bare" of a linked
        // worktree under the scanned root — the shape that used to get its
        // gitdir ripped out by a bare move, stranding the clone's files.
        let clone = tmp.path().join("elsewhere").join("widgets");
        setup_standalone_clone(&clone, "git@github.com:acme/widgets.git");
        let linked = worktree_root.join("acme").join("widgets").join("feat");
        fs::create_dir_all(linked.parent().unwrap()).unwrap();
        let linked_s = linked.to_string_lossy().into_owned();
        git_ok(&clone, &["worktree", "add", "-b", "feat", &linked_s, "main"]);

        let plan = scan_to_plan(&bare_root, &worktree_root, 60);
        let group = plan
            .groups
            .iter()
            .find(|g| g.bare.attached_clone.is_some())
            .expect("clone-gitdir group");
        assert!(same_path(
            group.bare.attached_clone.as_ref().unwrap(),
            &clone
        ));
        let actions = group_actions(group);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, MigrateAction::MoveBare { .. })),
            "a clone's gitdir must never be relocated as a bare: {actions:?}"
        );
        assert!(
            plan.reconstruct.iter().any(|r| same_path(&r.clone, &clone)),
            "the clone must be adopted via reconstruct: {:?}",
            plan.reconstruct
        );
    }

    #[test]
    fn materialize_main_worktree_creates_checkout_and_never_clobbers() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("bare").join("acme").join("widgets.git");
        setup_bare(tmp.path(), &bare, "git@github.com:acme/widgets.git");
        let dest = tmp
            .path()
            .join("wt")
            .join("acme")
            .join("widgets")
            .join("main");
        let status = materialize_main_worktree(&bare, &dest, "main");
        assert!(matches!(status, ApplyStatus::Moved), "{status:?}");
        assert!(dest.join("README.md").exists());

        // An existing destination is skipped, never clobbered.
        let again = materialize_main_worktree(&bare, &dest, "main");
        assert!(matches!(
            again,
            ApplyStatus::Skipped(UnsafeReason::DestinationExists)
        ));
    }

    fn worktree(path: &str, class: &str, dirty: bool, canonical: Option<&str>) -> PlanWorktree {
        let canonical_path = canonical.map(PathBuf::from);
        let at_canonical = canonical_path
            .as_ref()
            .is_some_and(|c| c.as_path() == Path::new(path));
        PlanWorktree {
            path: PathBuf::from(path),
            branch: Some("b".to_string()),
            dirty,
            unpushed: false,
            broken: false,
            class: class.to_string(),
            canonical_path,
            at_canonical,
        }
    }

    // ── AC1: plan caching round-trip ────────────────────────────────────────

    #[test]
    fn plan_serializes_and_roundtrips() {
        let plan = MigrationPlan {
            version: PLAN_VERSION,
            bare_root: PathBuf::from("/b"),
            worktree_root: PathBuf::from("/w"),
            orphan_days: 60,
            groups: vec![PlanGroup {
                bare: PlanBare {
                    path: PathBuf::from("/b/acme/widgets.git"),
                    owner: Some("acme".to_string()),
                    repo: Some("widgets".to_string()),
                    canonical_path: Some(PathBuf::from("/b/acme/widgets.git")),
                    at_canonical: true,
                    attached_clone: None,
                },
                worktrees: vec![worktree(
                    "/w/stray/x",
                    Classification::Move.as_str(),
                    false,
                    Some("/w/acme/widgets/x"),
                )],
                role: ConsolidationRole::Standalone,
                materialize_main: None,
            }],
            duplicate_clusters: Vec::new(),
            reconstruct: Vec::new(),
            data_dirs: Vec::new(),
        };
        let json = serde_json::to_string_pretty(&plan).unwrap();
        let back: MigrationPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan, back);
    }

    // ── AC2: action derivation moves dirty/unpushed too, excludes broken/ok ──

    #[test]
    fn classify_dirty_unpushed_offcanonical_are_movable() {
        let mk = |dirty: bool, unpushed: bool, admin_missing: bool| Worktree {
            path: PathBuf::from("/w/stray/x"),
            admin_dir: None,
            admin_missing,
            branch: Some("feat".to_string()),
            dirty: Some(dirty),
            unpushed: Some(unpushed),
            last_activity_secs: None,
            git_is_dir: false,
            origin_owner_repo: None,
        };
        // Off-canonical (at_canonical=false), dirty and/or unpushed → still Move.
        assert_eq!(classify(&mk(true, false, false), false).as_str(), "move");
        assert_eq!(classify(&mk(false, true, false), false).as_str(), "move");
        assert_eq!(classify(&mk(true, true, false), false).as_str(), "move");
        // A broken admin link can never be git-moved.
        assert_eq!(
            classify(&mk(false, false, true), false).as_str(),
            "repair-then-move"
        );
        // At canonical → no move regardless of dirty state.
        assert_eq!(classify(&mk(true, true, false), true).as_str(), "ok");
    }

    #[test]
    fn actionable_items_include_dirty_unpushed_exclude_broken_and_ok() {
        let group = PlanGroup {
            bare: PlanBare {
                path: PathBuf::from("/b/acme/widgets.git"),
                owner: Some("acme".to_string()),
                repo: Some("widgets".to_string()),
                canonical_path: Some(PathBuf::from("/b/acme/widgets.git")),
                at_canonical: true, // canonical bare -> no bare action
                attached_clone: None,
            },
            worktrees: vec![
                worktree(
                    "/w/stray/move-me",
                    Classification::Move.as_str(),
                    false,
                    Some("/w/acme/widgets/move-me"),
                ),
                worktree(
                    "/w/acme/widgets/ok",
                    Classification::Ok.as_str(),
                    false,
                    Some("/w/acme/widgets/ok"),
                ),
                // Off-canonical dirty/unpushed now classify as `move` (the
                // relocation preserves their state) and DO produce actions.
                worktree(
                    "/w/stray/dirty",
                    Classification::Move.as_str(),
                    true,
                    Some("/w/acme/widgets/dirty"),
                ),
                worktree(
                    "/w/stray/unpushed",
                    Classification::Move.as_str(),
                    false,
                    Some("/w/acme/widgets/unpushed"),
                ),
                // A broken admin link still cannot be moved.
                worktree(
                    "/w/stray/broken",
                    Classification::Broken.as_str(),
                    false,
                    Some("/w/acme/widgets/broken"),
                ),
            ],
            role: ConsolidationRole::Standalone,
            materialize_main: None,
        };

        let actions = group_actions(&group);
        assert_eq!(
            actions,
            vec![
                MigrateAction::MoveWorktree {
                    from: PathBuf::from("/w/stray/move-me"),
                    to: PathBuf::from("/w/acme/widgets/move-me"),
                },
                MigrateAction::MoveWorktree {
                    from: PathBuf::from("/w/stray/dirty"),
                    to: PathBuf::from("/w/acme/widgets/dirty"),
                },
                MigrateAction::MoveWorktree {
                    from: PathBuf::from("/w/stray/unpushed"),
                    to: PathBuf::from("/w/acme/widgets/unpushed"),
                },
            ]
        );

        // A non-canonical bare with a known owner/repo adds a bare move...
        let non_canonical_bare = PlanGroup {
            bare: PlanBare {
                path: PathBuf::from("/b/wrong/widgets.git"),
                owner: Some("acme".to_string()),
                repo: Some("widgets".to_string()),
                canonical_path: Some(PathBuf::from("/b/acme/widgets.git")),
                at_canonical: false,
                attached_clone: None,
            },
            worktrees: vec![],
            role: ConsolidationRole::Standalone,
            materialize_main: None,
        };
        assert_eq!(
            group_actions(&non_canonical_bare),
            vec![MigrateAction::MoveBare {
                from: PathBuf::from("/b/wrong/widgets.git"),
                to: PathBuf::from("/b/acme/widgets.git"),
            }]
        );

        // ...but an unknown-remote bare never becomes an action.
        let unknown_bare = PlanGroup {
            bare: PlanBare {
                path: PathBuf::from("/b/wrong/widgets.git"),
                owner: None,
                repo: None,
                canonical_path: None,
                at_canonical: false,
                attached_clone: None,
            },
            worktrees: vec![],
            role: ConsolidationRole::Standalone,
            materialize_main: None,
        };
        assert!(group_actions(&unknown_bare).is_empty());
    }

    // ── AC3: collapsible tree flattening ─────────────────────────────────────

    fn ui_action(label: &str) -> UiAction {
        UiAction {
            action: MigrateAction::MoveWorktree {
                from: PathBuf::from(format!("/from/{label}")),
                to: PathBuf::from(format!("/to/{label}")),
            },
            label: label.to_string(),
            selected: false,
        }
    }

    #[test]
    fn visible_rows_collapsed_then_expanded() {
        let mut groups = vec![
            UiGroup {
                header: "acme/a".to_string(),
                expanded: false,
                actions: vec![ui_action("a0"), ui_action("a1")],
            },
            UiGroup {
                header: "acme/b".to_string(),
                expanded: false,
                actions: vec![ui_action("b0")],
            },
        ];

        // Collapsed: only the two headers are visible.
        assert_eq!(
            visible_rows(&groups),
            vec![RowRef::Header(0), RowRef::Header(1)]
        );

        // Expand the first group: its actions appear under its header.
        groups[0].expanded = true;
        assert_eq!(
            visible_rows(&groups),
            vec![
                RowRef::Header(0),
                RowRef::Action(0, 0),
                RowRef::Action(0, 1),
                RowRef::Header(1),
            ]
        );
    }

    // ── AC4: toggling a header selects all its actions ───────────────────────

    #[test]
    fn toggle_group_header_selects_all_actions() {
        let mut picker = Picker {
            groups: vec![UiGroup {
                header: "acme/a".to_string(),
                expanded: false,
                actions: vec![ui_action("a0"), ui_action("a1"), ui_action("a2")],
            }],
            cursor: 0, // header row
            notes: Vec::new(),
        };

        // Space on the header selects every action.
        assert_eq!(handle_key(&mut picker, KeyCode::Char(' ')), PickerOutcome::Continue);
        assert!(picker.groups[0].actions.iter().all(|a| a.selected));
        assert_eq!(picker.selected_actions().len(), 3);

        // Space again clears them all.
        handle_key(&mut picker, KeyCode::Char(' '));
        assert!(picker.groups[0].actions.iter().all(|a| !a.selected));
        assert!(picker.selected_actions().is_empty());
    }

    // ── AC5: interactive frame render ────────────────────────────────────────

    #[test]
    fn render_frame_uses_crlf_and_checkboxes() {
        let picker = Picker {
            groups: vec![UiGroup {
                header: "acme/a".to_string(),
                expanded: true,
                actions: vec![
                    UiAction { selected: true, ..ui_action("sel") },
                    ui_action("unsel"),
                ],
            }],
            cursor: 0,
            notes: vec!["19 repo(s) already fully canonical (not shown)".to_string()],
        };
        let frame = render_interactive_frame(&picker);

        // Raw-mode safe: no bare \n.
        let bytes = frame.as_bytes();
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'\n' {
                assert!(
                    i > 0 && bytes[i - 1] == b'\r',
                    "bare \\n at byte {i}; raw-mode frames must use \\r\\n"
                );
            }
        }
        assert!(frame.contains("[x] sel"), "selected action shows [x]");
        assert!(frame.contains("[ ] unsel"), "unselected action shows [ ]");
        assert!(
            frame.contains("already fully canonical"),
            "notes render above the tree"
        );
    }

    // ── AC6: apply moves a clean worktree to canonical ───────────────────────

    #[test]
    fn apply_moves_clean_worktree_to_canonical() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        let bare = bare_root.join("acme").join("widgets.git");
        setup_bare(tmp.path(), &bare, "git@github.com:acme/widgets.git");

        // Worktree under a non-canonical parent on a non-main branch.
        let src = worktree_root.join("stray").join("oldname");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        git_ok(
            &bare,
            &["worktree", "add", "-b", "oldname", &src.to_string_lossy()],
        );

        let plan = scan_to_plan(&bare_root, &worktree_root, 60);
        let dest = worktree_root.join("acme").join("widgets").join("oldname");
        // The group has no main-branch worktree, so a MaterializeMainWorktree
        // rides along; the move itself is what this test pins.
        let actions: Vec<MigrateAction> = plan.groups.iter().flat_map(group_actions).collect();
        assert!(
            actions.contains(&MigrateAction::MoveWorktree {
                from: src.clone(),
                to: dest.clone(),
            }),
            "expected the clean worktree move to be offered: {actions:?}"
        );

        let results = apply_plan(&plan, &actions);
        assert!(results.iter().all(|r| r.status == ApplyStatus::Moved));
        assert!(dest.exists(), "worktree should now live at canonical path");
        assert!(!src.exists(), "old worktree path should be gone");
        // The moved worktree is still a functional git worktree.
        let status = Command::new("git")
            .arg("-C")
            .arg(&dest)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(status.status.success(), "git status must work in moved worktree");
    }

    // ── AC7: apply relocates a bare and repairs links ────────────────────────

    #[test]
    fn apply_moves_bare_to_canonical_and_repairs() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        // Bare lives under a wrong owner; origin says acme/widgets.
        let bare = bare_root.join("wrongowner").join("widgets.git");
        setup_bare(tmp.path(), &bare, "git@github.com:acme/widgets.git");

        // Worktree already at its canonical (main) leaf so only the bare moves.
        // The default `main_worktree_name` pattern (`{branch}`) renders `main`.
        let wt = worktree_root.join("acme").join("widgets").join("main");
        fs::create_dir_all(wt.parent().unwrap()).unwrap();
        git_ok(&bare, &["worktree", "add", &wt.to_string_lossy(), "main"]);

        let plan = scan_to_plan(&bare_root, &worktree_root, 60);
        let canonical_bare = bare_root.join("acme").join("widgets.git");
        let actions: Vec<MigrateAction> = plan.groups.iter().flat_map(group_actions).collect();
        // Git records the bare's canonicalized path in its gitdir files, so the
        // scanned `from` is the canonical form (e.g. /private/var on macOS).
        assert_eq!(
            actions,
            vec![MigrateAction::MoveBare {
                from: bare.canonicalize().unwrap(),
                to: canonical_bare.clone(),
            }],
            "expected exactly one bare move"
        );

        let results = apply_plan(&plan, &actions);
        assert!(
            results.iter().all(|r| r.status == ApplyStatus::Moved),
            "apply results: {:?}",
            results.iter().map(|r| &r.status).collect::<Vec<_>>()
        );
        assert!(canonical_bare.exists(), "bare should now be canonical");
        assert!(!bare.exists(), "old bare path should be gone");
        // The worktree still resolves to the relocated bare after repair.
        let status = Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "git status must work in worktree after bare relocation+repair: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    // ── AC8: apply skips a dirty worktree ────────────────────────────────────

    #[test]
    fn apply_moves_dirty_worktree_preserving_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        let bare = bare_root.join("acme").join("widgets.git");
        setup_bare(tmp.path(), &bare, "git@github.com:acme/widgets.git");

        let src = worktree_root.join("stray").join("dirtyleaf");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        git_ok(
            &bare,
            &["worktree", "add", "-b", "dirtyleaf", &src.to_string_lossy()],
        );
        // Make it dirty: an untracked file plus a modification to a tracked one.
        fs::write(src.join("dirty.txt"), "uncommitted").unwrap();
        fs::write(src.join("README.md"), "modified tracked content").unwrap();

        let plan = scan_to_plan(&bare_root, &worktree_root, 60);
        let dest = worktree_root.join("acme").join("widgets").join("dirtyleaf");
        // The group has no main-branch worktree, so a MaterializeMainWorktree
        // rides along; the move itself is what this test pins.
        let actions: Vec<MigrateAction> = plan.groups.iter().flat_map(group_actions).collect();
        assert!(
            actions.contains(&MigrateAction::MoveWorktree {
                from: src.clone(),
                to: dest.clone(),
            }),
            "a dirty off-canonical worktree must still be offered as a move: {actions:?}"
        );

        let results = apply_plan(&plan, &actions);
        assert!(
            results.iter().all(|r| r.status == ApplyStatus::Moved),
            "dirty worktree move should succeed: {:?}",
            results.iter().map(|r| &r.status).collect::<Vec<_>>()
        );
        assert!(dest.exists(), "worktree should now live at canonical path");
        assert!(!src.exists(), "old worktree path should be gone");
        // The uncommitted changes rode along with the move.
        assert_eq!(
            fs::read_to_string(dest.join("dirty.txt")).unwrap(),
            "uncommitted",
            "untracked file must be preserved at the destination"
        );
        assert_eq!(
            fs::read_to_string(dest.join("README.md")).unwrap(),
            "modified tracked content",
            "modified tracked file must be preserved at the destination"
        );
        let status = Command::new("git")
            .arg("-C")
            .arg(&dest)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "the worktree must still report dirty after the move"
        );
    }

    #[test]
    fn git_worktree_move_preserves_dirty_changes() {
        // Pins the safety assumption behind moving dirty worktrees: `git
        // worktree move` relocates the working tree including uncommitted
        // (tracked + untracked) changes.
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("bare.git");
        setup_bare(tmp.path(), &bare, "git@github.com:acme/widgets.git");
        let src = tmp.path().join("wt-old");
        git_ok(&bare, &["worktree", "add", &src.to_string_lossy(), "main"]);
        fs::write(src.join("README.md"), "changed").unwrap();
        fs::write(src.join("untracked.txt"), "new").unwrap();

        let dst = tmp.path().join("wt-new");
        let status = move_worktree(&bare, &src, &dst);
        assert_eq!(status, ApplyStatus::Moved);
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(dst.join("README.md")).unwrap(), "changed");
        assert_eq!(
            fs::read_to_string(dst.join("untracked.txt")).unwrap(),
            "new"
        );
    }

    // ── AC9: preflight rejects an existing destination ───────────────────────

    #[test]
    fn preflight_rejects_existing_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.git");
        let dst = tmp.path().join("dst.git");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap(); // destination already exists

        let action = MigrateAction::MoveBare {
            from: src.clone(),
            to: dst.clone(),
        };
        let (safe, rejected) = preflight(std::slice::from_ref(&action));
        assert!(safe.is_empty(), "an existing-dest action must not be safe");
        assert_eq!(rejected, vec![(action, UnsafeReason::DestinationExists)]);
    }

    #[test]
    fn preflight_rejects_existing_worktree_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("from/leaf");
        let dst = tmp.path().join("to/leaf");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();

        let action = MigrateAction::MoveWorktree {
            from: src.clone(),
            to: dst.clone(),
        };
        let (safe, rejected) = preflight(std::slice::from_ref(&action));
        assert!(safe.is_empty());
        assert_eq!(rejected, vec![(action, UnsafeReason::DestinationExists)]);
    }

    // ── AC10: preflight rejects two actions sharing one destination ──────────

    #[test]
    fn preflight_rejects_duplicate_destinations() {
        let tmp = tempfile::tempdir().unwrap();
        let src_a = tmp.path().join("a.git");
        let src_b = tmp.path().join("b.git");
        let dst = tmp.path().join("shared.git");
        fs::create_dir_all(&src_a).unwrap();
        fs::create_dir_all(&src_b).unwrap();
        // `dst` intentionally does NOT exist — the collision is between actions.

        let a = MigrateAction::MoveBare {
            from: src_a,
            to: dst.clone(),
        };
        let b = MigrateAction::MoveBare {
            from: src_b,
            to: dst,
        };
        let (safe, rejected) = preflight(&[a.clone(), b.clone()]);
        assert!(safe.is_empty(), "both colliding actions must be skipped");
        assert_eq!(rejected.len(), 2);
        assert!(rejected
            .iter()
            .all(|(_, r)| *r == UnsafeReason::DuplicateDestination));
    }

    #[test]
    fn preflight_passes_distinct_safe_actions() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.git");
        fs::create_dir_all(&src).unwrap();
        let action = MigrateAction::MoveBare {
            from: src,
            to: tmp.path().join("nested/dst.git"), // does not exist, not nested in src
        };
        let (safe, rejected) = preflight(std::slice::from_ref(&action));
        assert_eq!(safe, vec![action]);
        assert!(rejected.is_empty());
    }

    // ── AC11: move_bare stages a destination nested inside the source ────────

    #[test]
    fn move_bare_handles_destination_nested_in_source() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        let bare = bare_root.join("owner").join("repo.git");
        setup_bare(tmp.path(), &bare, "git@github.com:owner/repo.git");

        let wt = worktree_root.join("owner").join("repo").join("repo-main");
        fs::create_dir_all(wt.parent().unwrap()).unwrap();
        git_ok(&bare, &["worktree", "add", &wt.to_string_lossy(), "main"]);

        // Destination is a child of the source — a direct rename would EINVAL.
        let nested_dest = bare.join("repo.git");
        let status = move_bare(
            &bare,
            &nested_dest,
            &[WorktreeRepairTarget {
                path: wt.clone(),
                broken: false,
            }],
        );
        assert!(
            matches!(
                status,
                ApplyStatus::Moved | ApplyStatus::MovedWithWarnings(_)
            ),
            "nested bare move should succeed, got {status:?}"
        );
        assert!(nested_dest.exists(), "bare must now live at the nested dest");
        // The worktree still resolves to the relocated bare after repair.
        let out = Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git status must work in worktree after nested bare relocation: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // ── AC12: a pre-broken worktree warns instead of failing the bare move ───

    #[test]
    fn move_bare_reports_warnings_for_prebroken_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        let bare = bare_root.join("owner").join("repo.git");
        setup_bare(tmp.path(), &bare, "git@github.com:owner/repo.git");

        let healthy = worktree_root.join("owner").join("repo").join("repo-main");
        fs::create_dir_all(healthy.parent().unwrap()).unwrap();
        git_ok(&bare, &["worktree", "add", &healthy.to_string_lossy(), "main"]);

        let dest = bare_root.join("moved").join("repo.git");
        let broken_path = worktree_root.join("owner").join("repo").join("ghost");
        let status = move_bare(
            &bare,
            &dest,
            &[
                WorktreeRepairTarget {
                    path: healthy.clone(),
                    broken: false,
                },
                WorktreeRepairTarget {
                    path: broken_path.clone(),
                    broken: true,
                },
            ],
        );

        match status {
            ApplyStatus::MovedWithWarnings(warnings) => {
                assert!(
                    warnings.iter().any(|w| w.contains("ghost")),
                    "warning should name the un-repairable worktree, got {warnings:?}"
                );
            }
            other => panic!("expected MovedWithWarnings, got {other:?}"),
        }
        assert!(dest.exists(), "bare must move even with a broken worktree");
        let out = Command::new("git")
            .arg("-C")
            .arg(&healthy)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(out.status.success(), "healthy worktree must still work");
    }

    // ── AC13: relocation restores core.bare=false under worktreeConfig ───────

    #[test]
    fn move_bare_restores_core_bare_false_for_worktreeconfig_bare() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        let bare = bare_root.join("owner").join("repo.git");
        setup_bare(tmp.path(), &bare, "git@github.com:owner/repo.git");
        // The real-world trigger: worktreeConfig on, shared core.bare=true.
        git_ok(&bare, &["config", "extensions.worktreeConfig", "true"]);
        git_ok(&bare, &["config", "core.bare", "true"]);

        let wt = worktree_root.join("owner").join("repo").join("repo-main");
        fs::create_dir_all(wt.parent().unwrap()).unwrap();
        git_ok(&bare, &["worktree", "add", &wt.to_string_lossy(), "main"]);

        // Simulate the broken post-migration state: drop the per-worktree override.
        let admin = bare.join("worktrees");
        if let Ok(entries) = fs::read_dir(&admin) {
            for e in entries.flatten() {
                let cfg = e.path().join("config.worktree");
                let _ = fs::remove_file(cfg);
            }
        }
        // Precondition (red): the worktree now wrongly looks bare.
        let pre = Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["rev-parse", "--is-inside-work-tree"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&pre.stdout).trim(),
            "false",
            "precondition: missing config.worktree makes the worktree look bare"
        );

        let dest = bare_root.join("moved").join("repo.git");
        let status = move_bare(
            &bare,
            &dest,
            &[WorktreeRepairTarget {
                path: wt.clone(),
                broken: false,
            }],
        );
        assert!(
            matches!(
                status,
                ApplyStatus::Moved | ApplyStatus::MovedWithWarnings(_)
            ),
            "bare move should succeed, got {status:?}"
        );

        // Post (green): the override is restored, working-tree ops succeed.
        let post = Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["rev-parse", "--is-inside-work-tree"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&post.stdout).trim(),
            "true",
            "core.bare=false must be restored so the worktree is a work tree again"
        );
    }

    // ── AC14: collision dedup yields distinct destinations ───────────────────

    #[test]
    fn dedupe_assigns_distinct_destinations_on_collision() {
        // Two distinct bares that both resolve to the same canonical destination.
        let mk_group = |bare_path: &str, wt_path: &str, wt_canon: &str| PlanGroup {
            bare: PlanBare {
                path: PathBuf::from(bare_path),
                owner: Some("owner".to_string()),
                repo: Some("repo".to_string()),
                canonical_path: Some(PathBuf::from("/b/owner/repo.git")),
                at_canonical: false,
                attached_clone: None,
            },
            worktrees: vec![worktree(
                wt_path,
                Classification::Move.as_str(),
                false,
                Some(wt_canon),
            )],
            role: ConsolidationRole::Standalone,
            materialize_main: None,
        };
        let mut plan = MigrationPlan {
            version: PLAN_VERSION,
            bare_root: PathBuf::from("/b"),
            worktree_root: PathBuf::from("/w"),
            orphan_days: 60,
            groups: vec![
                mk_group("/old/a", "/w/stray/a", "/w/owner/repo/a"),
                mk_group("/old/b", "/w/stray/b", "/w/owner/repo/b"),
            ],
            duplicate_clusters: Vec::new(),
            reconstruct: Vec::new(),
            data_dirs: Vec::new(),
        };

        dedupe_bare_destinations(&mut plan);

        let dest0 = plan.groups[0].bare.canonical_path.clone().unwrap();
        let dest1 = plan.groups[1].bare.canonical_path.clone().unwrap();
        assert_ne!(dest0, dest1, "colliding bares must get distinct destinations");
        assert_eq!(dest0, PathBuf::from("/b/owner/repo.git"));
        assert_eq!(dest1, PathBuf::from("/b/owner/repo-2.git"));

        // The loser's worktree follows its bare under the suffixed worktree dir.
        let wt1 = plan.groups[1].worktrees[0]
            .canonical_path
            .clone()
            .unwrap();
        assert_eq!(wt1, PathBuf::from("/w/owner/repo-2/b"));

        // No two final destinations (bare or worktree) collide.
        let mut dests: Vec<PathBuf> = Vec::new();
        for g in &plan.groups {
            dests.push(g.bare.canonical_path.clone().unwrap());
            for wt in &g.worktrees {
                dests.push(wt.canonical_path.clone().unwrap());
            }
        }
        let mut deduped = dests.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(dests.len(), deduped.len(), "no destination may repeat");
    }

    #[test]
    fn dedupe_prefers_already_canonical_claimant() {
        // One bare already sits at canonical; the other must be the one suffixed.
        let plan_group = |bare_path: &str, at_canonical: bool| PlanGroup {
            bare: PlanBare {
                path: PathBuf::from(bare_path),
                owner: Some("owner".to_string()),
                repo: Some("repo".to_string()),
                canonical_path: Some(PathBuf::from("/b/owner/repo.git")),
                at_canonical,
                attached_clone: None,
            },
            worktrees: vec![],
            role: ConsolidationRole::Standalone,
            materialize_main: None,
        };
        let mut plan = MigrationPlan {
            version: PLAN_VERSION,
            bare_root: PathBuf::from("/b"),
            worktree_root: PathBuf::from("/w"),
            orphan_days: 60,
            groups: vec![
                plan_group("/b/owner/repo.git", true), // already canonical — keeps it
                plan_group("/old/other", false),       // must be suffixed
            ],
            duplicate_clusters: Vec::new(),
            reconstruct: Vec::new(),
            data_dirs: Vec::new(),
        };

        dedupe_bare_destinations(&mut plan);

        assert_eq!(
            plan.groups[0].bare.canonical_path,
            Some(PathBuf::from("/b/owner/repo.git")),
            "the already-canonical bare keeps the canonical destination"
        );
        assert_eq!(
            plan.groups[1].bare.canonical_path,
            Some(PathBuf::from("/b/owner/repo-2.git")),
            "the colliding bare is suffixed"
        );
    }

    // ── Issue 2: canonical main leaf follows the configured pattern ──────────

    fn main_worktree(path: &str, branch: &str) -> Worktree {
        Worktree {
            path: PathBuf::from(path),
            admin_dir: None,
            admin_missing: false,
            branch: Some(branch.to_string()),
            dirty: Some(false),
            unpushed: Some(false),
            last_activity_secs: None,
            git_is_dir: false,
            origin_owner_repo: None,
        }
    }

    #[test]
    fn pick_canonical_leaf_main_uses_configured_pattern() {
        // Default pattern `{branch}` → the main worktree's leaf is `main`,
        // NOT the legacy hardcoded `{repo}-main`.
        let wt = main_worktree("/w/brian-stoker/wallach/wallach-main", "main");
        assert_eq!(
            pick_canonical_leaf(&wt, "brian-stoker", "wallach", "{branch}"),
            "main"
        );
        // A custom pattern is honored (e.g. `{repo}-{branch}` → `wallach-main`).
        assert_eq!(
            pick_canonical_leaf(&wt, "brian-stoker", "wallach", "{repo}-{branch}"),
            "wallach-main"
        );
        // master branch also maps through the pattern.
        let wt_master = main_worktree("/w/o/r/r-main", "master");
        assert_eq!(pick_canonical_leaf(&wt_master, "o", "r", "{branch}"), "master");
        // A non-main branch preserves its existing leaf.
        let feat = main_worktree("/w/stray/task-foo", "task-foo");
        assert_eq!(pick_canonical_leaf(&feat, "o", "r", "{branch}"), "task-foo");
    }

    #[test]
    fn wallach_main_worktree_at_repo_main_is_flagged_as_move_to_main() {
        // Reproduces the reported bug: brian-stoker/wallach's main worktree
        // lives at `wallach-main` instead of the canonical `main`. The planner
        // must flag it as a move to `<owner>/<repo>/main`.
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        let bare = bare_root.join("brian-stoker").join("wallach.git");
        setup_bare(tmp.path(), &bare, "git@github.com:brian-stoker/wallach.git");

        // Main worktree on `main`, but parked at the non-canonical `wallach-main`.
        let src = worktree_root
            .join("brian-stoker")
            .join("wallach")
            .join("wallach-main");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        git_ok(&bare, &["worktree", "add", &src.to_string_lossy(), "main"]);

        let plan = scan_to_plan(&bare_root, &worktree_root, 60);
        let dest = worktree_root
            .join("brian-stoker")
            .join("wallach")
            .join("main");
        let actions: Vec<MigrateAction> = plan.groups.iter().flat_map(group_actions).collect();
        assert_eq!(
            actions,
            vec![MigrateAction::MoveWorktree {
                from: src.clone(),
                to: dest.clone(),
            }],
            "a `<repo>-main` main worktree must be flagged to move to `main`"
        );
    }

    // ── Issue 3: groups are sorted by owner/repo ─────────────────────────────

    #[test]
    fn build_migration_plan_sorts_groups_by_owner_repo() {
        let mk = |owner: &str, repo: &str| RepoGroup {
            bare: Bare {
                path: PathBuf::from(format!("/b/{owner}/{repo}.git")),
                owner_repo: Some((owner.to_string(), repo.to_string())),
                attached_clone: None,
            },
            worktrees: vec![],
        };
        // Intentionally out of order, mixed owners and case.
        let groups = vec![
            mk("stokdconsulting", "agents"),
            mk("brian-stoker", "wallach"),
            mk("brian-stoker", "alice"),
            mk("InertialG", "aw-watcher-screenshot"),
            mk("stokdconsulting", "cli"),
        ];
        let plan = build_migration_plan(
            &groups,
            &[],
            &[],
            Path::new("/b"),
            Path::new("/w"),
            "{branch}",
            60,
        );
        let order: Vec<(String, String)> = plan
            .groups
            .iter()
            .map(|g| {
                (
                    g.bare.owner.clone().unwrap_or_default(),
                    g.bare.repo.clone().unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            order,
            vec![
                ("brian-stoker".to_string(), "alice".to_string()),
                ("brian-stoker".to_string(), "wallach".to_string()),
                ("InertialG".to_string(), "aw-watcher-screenshot".to_string()),
                ("stokdconsulting".to_string(), "agents".to_string()),
                ("stokdconsulting".to_string(), "cli".to_string()),
            ],
            "groups must be ordered by (owner, repo) case-insensitively"
        );
    }

    // ── Issue 1: interactive reuses the cached plan by default ───────────────

    #[test]
    fn interactive_prefers_cached_plan_when_present() {
        // Plain `--interactive` (reuse optional) uses the cache when it exists.
        assert_eq!(
            choose_interactive_plan_source(true, false),
            InteractivePlanSource::UseCached
        );
        // `--interactive --apply` (reuse mandatory) also uses the cache.
        assert_eq!(
            choose_interactive_plan_source(true, true),
            InteractivePlanSource::UseCached
        );
        // No cache + optional reuse → scan fresh (no hard error).
        assert_eq!(
            choose_interactive_plan_source(false, false),
            InteractivePlanSource::ScanFresh
        );
        // No cache + mandatory reuse (--apply) → require-cached error.
        assert_eq!(
            choose_interactive_plan_source(false, true),
            InteractivePlanSource::RequireCachedError
        );
    }

    // ── Optional positional filter: <owner> or <owner>/<repo> ────────────────

    #[test]
    fn parse_migrate_filter_variants() {
        assert_eq!(parse_migrate_filter(None).unwrap(), MigrateFilter::NoFilter);
        assert_eq!(
            parse_migrate_filter(Some("brian-stoker")).unwrap(),
            MigrateFilter::Owner("brian-stoker".to_string())
        );
        assert_eq!(
            parse_migrate_filter(Some("brian-stoker/mac-mixer")).unwrap(),
            MigrateFilter::OwnerRepo("brian-stoker".to_string(), "mac-mixer".to_string())
        );
        // A trailing `.git` on the repo is tolerated (origins store repo without it).
        assert_eq!(
            parse_migrate_filter(Some("brian-stoker/mac-mixer.git")).unwrap(),
            MigrateFilter::OwnerRepo("brian-stoker".to_string(), "mac-mixer".to_string())
        );
        // Surrounding whitespace is trimmed.
        assert_eq!(
            parse_migrate_filter(Some("  stokd-cloud  ")).unwrap(),
            MigrateFilter::Owner("stokd-cloud".to_string())
        );
        // Errors: empty, trailing slash (empty repo), leading slash, too many segments.
        assert!(parse_migrate_filter(Some("")).is_err());
        assert!(parse_migrate_filter(Some("   ")).is_err());
        assert!(parse_migrate_filter(Some("owner/")).is_err());
        assert!(parse_migrate_filter(Some("/repo")).is_err());
        assert!(parse_migrate_filter(Some("a/b/c")).is_err());
    }

    #[test]
    fn filter_matches_owner_and_owner_repo() {
        let or = ("brian-stoker".to_string(), "mac-mixer".to_string());
        // NoFilter matches everything, including unknown-remote (None).
        assert!(matches_filter(Some(&or), &MigrateFilter::NoFilter));
        assert!(matches_filter(None, &MigrateFilter::NoFilter));
        // Owner matches case-insensitively; a non-matching owner fails.
        assert!(matches_filter(
            Some(&or),
            &MigrateFilter::Owner("Brian-Stoker".to_string())
        ));
        assert!(!matches_filter(
            Some(&or),
            &MigrateFilter::Owner("someone-else".to_string())
        ));
        // OwnerRepo requires both, case-insensitively.
        assert!(matches_filter(
            Some(&or),
            &MigrateFilter::OwnerRepo("BRIAN-STOKER".to_string(), "MAC-MIXER".to_string())
        ));
        assert!(!matches_filter(
            Some(&or),
            &MigrateFilter::OwnerRepo("brian-stoker".to_string(), "wallach".to_string())
        ));
        // Unknown-remote (None) never matches under any non-None filter.
        assert!(!matches_filter(
            None,
            &MigrateFilter::Owner("brian-stoker".to_string())
        ));
        assert!(!matches_filter(
            None,
            &MigrateFilter::OwnerRepo("brian-stoker".to_string(), "mac-mixer".to_string())
        ));
    }

    #[test]
    fn filter_narrows_scanned_groups() {
        let mk = |owner: Option<&str>, repo: Option<&str>| RepoGroup {
            bare: Bare {
                path: PathBuf::from(format!(
                    "/b/{}/{}.git",
                    owner.unwrap_or("unknown"),
                    repo.unwrap_or("unknown")
                )),
                owner_repo: match (owner, repo) {
                    (Some(o), Some(r)) => Some((o.to_string(), r.to_string())),
                    _ => None,
                },
                attached_clone: None,
            },
            worktrees: vec![],
        };
        let groups = || {
            vec![
                mk(Some("brian-stoker"), Some("mac-mixer")),
                mk(Some("brian-stoker"), Some("wallach")),
                mk(Some("stokd-cloud"), Some("cli")),
                mk(None, None), // unknown-remote
            ]
        };

        // Owner filter keeps only that owner's two groups, drops unknown-remote.
        let by_owner =
            apply_group_filter(groups(), &MigrateFilter::Owner("brian-stoker".to_string()));
        let owners: Vec<_> = by_owner
            .iter()
            .map(|g| g.bare.owner_repo.clone().unwrap())
            .collect();
        assert_eq!(
            owners,
            vec![
                ("brian-stoker".to_string(), "mac-mixer".to_string()),
                ("brian-stoker".to_string(), "wallach".to_string()),
            ]
        );

        // OwnerRepo filter keeps exactly one group.
        let one = apply_group_filter(
            groups(),
            &MigrateFilter::OwnerRepo("brian-stoker".to_string(), "wallach".to_string()),
        );
        assert_eq!(one.len(), 1);
        assert_eq!(
            one[0].bare.owner_repo.clone().unwrap(),
            ("brian-stoker".to_string(), "wallach".to_string())
        );

        // NoFilter keeps all four (including unknown-remote).
        let all = apply_group_filter(groups(), &MigrateFilter::NoFilter);
        assert_eq!(all.len(), 4);
    }

    // ── Duplicate-bare consolidation: pure classification ────────────────────

    fn member(at_canonical: bool, has_unique_work: bool, contains: Vec<bool>) -> MemberFacts {
        MemberFacts {
            at_canonical,
            has_unique_work,
            contains,
        }
    }

    #[test]
    fn classify_cluster_consolidate_when_subset_and_clean() {
        // A (idx 0) strictly contains B (idx 1); both clean; neither canonical.
        // A contains {A,B}; B contains only {B}. Winner must be A.
        let facts = ClusterFacts {
            members: vec![
                member(false, false, vec![true, true]),
                member(false, false, vec![false, true]),
            ],
        };
        assert_eq!(
            classify_bare_cluster(&facts),
            ClusterResolution::Consolidate {
                winner: 0,
                redundant: vec![1],
            }
        );

        // Identical histories (each contains the other), but member 1 is the
        // already-canonical bare → it is preferred as the winner.
        let identical = ClusterFacts {
            members: vec![
                member(false, false, vec![true, true]),
                member(true, false, vec![true, true]),
            ],
        };
        assert_eq!(
            classify_bare_cluster(&identical),
            ClusterResolution::Consolidate {
                winner: 1,
                redundant: vec![0],
            }
        );
    }

    #[test]
    fn classify_cluster_divergent_when_unique_commits() {
        // Neither bare contains the other (each has commits the other lacks).
        let facts = ClusterFacts {
            members: vec![
                member(false, false, vec![true, false]),
                member(false, false, vec![false, true]),
            ],
        };
        assert_eq!(classify_bare_cluster(&facts), ClusterResolution::Divergent);
    }

    #[test]
    fn classify_cluster_divergent_when_contained_member_dirty() {
        // A contains B's commits, but B has a dirty/unpushed worktree — its
        // uncommitted work would be destroyed by dropping it, so do NOT auto-act.
        let facts = ClusterFacts {
            members: vec![
                member(false, false, vec![true, true]),
                member(false, true, vec![false, true]),
            ],
        };
        assert_eq!(classify_bare_cluster(&facts), ClusterResolution::Divergent);
    }

    #[test]
    fn consolidate_action_derivation_drop_and_retire() {
        // A winner already at its canonical layout produces no actions.
        let winner = PlanGroup {
            bare: PlanBare {
                path: PathBuf::from("/b/owner/repo.git"),
                owner: Some("owner".to_string()),
                repo: Some("repo".to_string()),
                canonical_path: Some(PathBuf::from("/b/owner/repo.git")),
                at_canonical: true,
                attached_clone: None,
            },
            worktrees: vec![worktree(
                "/w/owner/repo/main",
                Classification::Ok.as_str(),
                false,
                Some("/w/owner/repo/main"),
            )],
            role: ConsolidationRole::Winner,
            materialize_main: None,
        };
        assert!(group_actions(&winner).is_empty());

        // A redundant group drops each of its worktrees, then retires its bare.
        let redundant = PlanGroup {
            bare: PlanBare {
                path: PathBuf::from("/b/stray/repo.git"),
                owner: Some("owner".to_string()),
                repo: Some("repo".to_string()),
                canonical_path: Some(PathBuf::from("/b/owner/repo.git")),
                at_canonical: false,
                attached_clone: None,
            },
            worktrees: vec![worktree(
                "/w/stray/repo-b",
                Classification::Move.as_str(),
                false,
                Some("/w/owner/repo/main"),
            )],
            role: ConsolidationRole::Redundant {
                winner_bare: PathBuf::from("/b/owner/repo.git"),
                trash: PathBuf::from("/b/.stokd-migrate-trash/owner/repo-2.git"),
            },
            materialize_main: None,
        };
        assert_eq!(
            group_actions(&redundant),
            vec![
                MigrateAction::DropWorktree {
                    worktree: PathBuf::from("/w/stray/repo-b"),
                    bare: PathBuf::from("/b/stray/repo.git"),
                },
                MigrateAction::RetireBare {
                    redundant: PathBuf::from("/b/stray/repo.git"),
                    winner: PathBuf::from("/b/owner/repo.git"),
                    trash: PathBuf::from("/b/.stokd-migrate-trash/owner/repo-2.git"),
                },
            ]
        );
    }

    #[test]
    fn preflight_rejects_existing_trash_and_missing_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let live_wt = tmp.path().join("live-wt");
        let missing_wt = tmp.path().join("missing-wt");
        let redundant = tmp.path().join("redundant.git");
        let existing_trash = tmp.path().join("trash/repo-2.git");
        let free_trash = tmp.path().join("trash/repo-3.git");
        fs::create_dir_all(&live_wt).unwrap();
        fs::create_dir_all(&redundant).unwrap();
        fs::create_dir_all(&existing_trash).unwrap();

        // Drop of a live worktree is safe; drop of a missing one is SourceMissing.
        let drop_ok = MigrateAction::DropWorktree {
            worktree: live_wt.clone(),
            bare: redundant.clone(),
        };
        let drop_missing = MigrateAction::DropWorktree {
            worktree: missing_wt.clone(),
            bare: redundant.clone(),
        };
        // Retire into an existing trash dir is rejected; into a free one is safe.
        let retire_blocked = MigrateAction::RetireBare {
            redundant: redundant.clone(),
            winner: tmp.path().join("winner.git"),
            trash: existing_trash.clone(),
        };
        let retire_ok = MigrateAction::RetireBare {
            redundant: redundant.clone(),
            winner: tmp.path().join("winner.git"),
            trash: free_trash.clone(),
        };

        let (safe, rejected) = preflight(&[
            drop_ok.clone(),
            drop_missing.clone(),
            retire_blocked.clone(),
            retire_ok.clone(),
        ]);
        assert!(safe.contains(&drop_ok));
        assert!(safe.contains(&retire_ok));
        assert!(rejected
            .iter()
            .any(|(a, r)| *a == drop_missing && *r == UnsafeReason::SourceMissing));
        assert!(rejected
            .iter()
            .any(|(a, r)| *a == retire_blocked && *r == UnsafeReason::DestinationExists));
    }

    #[test]
    fn duplicate_bares_report_labels_redundant_and_divergent() {
        let clusters = vec![
            DuplicateCluster {
                owner_repo: "owner/redundantrepo".to_string(),
                resolution: DuplicateResolution::Consolidate,
                members: vec![
                    PathBuf::from("/b/owner/redundantrepo.git"),
                    PathBuf::from("/b/stray/redundantrepo.git"),
                ],
                winner: Some(PathBuf::from("/b/owner/redundantrepo.git")),
            },
            DuplicateCluster {
                owner_repo: "owner/divergentrepo".to_string(),
                resolution: DuplicateResolution::NeedsHuman,
                members: vec![
                    PathBuf::from("/b/owner/divergentrepo.git"),
                    PathBuf::from("/b/stray/divergentrepo.git"),
                ],
                winner: None,
            },
        ];
        let out = render_duplicate_bares_section(&clusters);
        assert!(out.contains("Duplicate bares"), "section header present");
        assert!(out.contains("owner/redundantrepo"));
        assert!(out.contains("auto-consolidate"));
        assert!(out.contains("owner/divergentrepo"));
        assert!(out.contains("needs human"));
        // An empty cluster list renders nothing.
        assert!(render_duplicate_bares_section(&[]).is_empty());
    }

    #[test]
    fn preflight_allows_move_into_vacated_destination() {
        let tmp = tempfile::tempdir().unwrap();
        // The winner bare relocates into the canonical path the redundant holds.
        let winner_from = tmp.path().join("stray/repo.git");
        let canonical = tmp.path().join("owner/repo.git");
        let trash = tmp.path().join(".trash/owner/repo-2.git");
        fs::create_dir_all(&winner_from).unwrap();
        fs::create_dir_all(&canonical).unwrap(); // redundant currently occupies canonical

        let move_winner = MigrateAction::MoveBare {
            from: winner_from.clone(),
            to: canonical.clone(),
        };
        let retire_redundant = MigrateAction::RetireBare {
            redundant: canonical.clone(), // vacates `canonical` before the move runs
            winner: winner_from.clone(),
            trash: trash.clone(),
        };
        let (safe, rejected) = preflight(&[move_winner.clone(), retire_redundant.clone()]);
        assert!(
            safe.contains(&move_winner),
            "moving into a path a RetireBare vacates must be allowed: rejected={rejected:?}"
        );
        assert!(safe.contains(&retire_redundant));

        // A move into an existing path that NOTHING vacates is still rejected.
        let other_existing = tmp.path().join("other.git");
        fs::create_dir_all(&other_existing).unwrap();
        let collide = MigrateAction::MoveBare {
            from: winner_from.clone(),
            to: other_existing.clone(),
        };
        let (safe2, rejected2) = preflight(std::slice::from_ref(&collide));
        assert!(safe2.is_empty());
        assert_eq!(rejected2, vec![(collide, UnsafeReason::DestinationExists)]);
    }

    // ── Duplicate-bare consolidation: apply (integration with real git) ──────

    fn seed_repo(tmp: &Path) -> PathBuf {
        let seed = tmp.join("seed");
        fs::create_dir_all(&seed).unwrap();
        git_ok(&seed, &["init", "-b", "main"]);
        git_ok(&seed, &["config", "user.email", "t@example.com"]);
        git_ok(&seed, &["config", "user.name", "Test"]);
        seed
    }

    fn seed_commit(seed: &Path, content: &str, msg: &str) {
        fs::write(seed.join("README.md"), content).unwrap();
        git_ok(seed, &["add", "-A"]);
        git_ok(seed, &["commit", "-m", msg]);
    }

    /// Bare-clone `seed`'s CURRENT state to `dest` and stamp its origin, so two
    /// bares can deliberately share or diverge history (unlike `setup_bare`,
    /// which always re-seeds a fresh single commit).
    fn clone_bare(seed: &Path, dest: &Path, origin: &str) {
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        let out = Command::new("git")
            .args([
                "clone",
                "--bare",
                &seed.to_string_lossy(),
                &dest.to_string_lossy(),
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "clone bare failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        git_ok(dest, &["config", "remote.origin.url", origin]);
    }

    #[test]
    fn apply_consolidates_subset_bare_to_trash() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        let origin = "git@github.com:owner/repo.git";

        let seed = seed_repo(tmp.path());
        seed_commit(&seed, "v1", "c1");
        // B (redundant) holds only c1.
        let bare_b = bare_root.join("stray").join("repo.git");
        clone_bare(&seed, &bare_b, origin);
        seed_commit(&seed, "v2", "c2");
        // A (winner) is the canonical bare and holds c1+c2 (a superset of B).
        let bare_a = bare_root.join("owner").join("repo.git");
        clone_bare(&seed, &bare_a, origin);

        // A's main worktree already canonical; B's main worktree off-canonical.
        let wt_a = worktree_root.join("owner").join("repo").join("main");
        fs::create_dir_all(wt_a.parent().unwrap()).unwrap();
        git_ok(&bare_a, &["worktree", "add", &wt_a.to_string_lossy(), "main"]);
        let wt_b = worktree_root.join("stray").join("repo-b");
        fs::create_dir_all(wt_b.parent().unwrap()).unwrap();
        git_ok(&bare_b, &["worktree", "add", &wt_b.to_string_lossy(), "main"]);

        let plan = scan_to_plan(&bare_root, &worktree_root, 60);
        assert!(
            plan.duplicate_clusters
                .iter()
                .any(|c| c.resolution == DuplicateResolution::Consolidate),
            "a clean subset cluster must classify as consolidate: {:?}",
            plan.duplicate_clusters
        );

        let actions: Vec<MigrateAction> = plan.groups.iter().flat_map(group_actions).collect();
        let results = apply_plan(&plan, &actions);
        assert!(
            results.iter().all(|r| matches!(
                r.status,
                ApplyStatus::Moved | ApplyStatus::MovedWithWarnings(_)
            )),
            "apply statuses: {:?}",
            results.iter().map(|r| &r.status).collect::<Vec<_>>()
        );

        // Redundant bare retired to recoverable trash; no `-2` at the canonical root.
        let trash = bare_root
            .join(".stokd-migrate-trash")
            .join("owner")
            .join("repo-2.git");
        assert!(trash.exists(), "redundant bare must move to recoverable trash");
        assert!(!bare_b.exists(), "old redundant bare path must be gone");
        assert!(
            !bare_root.join("owner").join("repo-2.git").exists(),
            "no -2 suffix bare may be created at the canonical root"
        );
        assert!(!wt_b.exists(), "the redundant worktree must be dropped");

        // Winner stays canonical and still functions.
        assert!(bare_a.exists(), "winner bare must remain at canonical path");
        let st = Command::new("git")
            .arg("-C")
            .arg(&wt_a)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(st.status.success(), "winner worktree must still be functional");
    }

    #[test]
    fn apply_consolidates_when_redundant_holds_the_canonical_path() {
        // The real-world mac-mixer shape: the SUBSET bare sits at the canonical
        // path while the SUPERSET (winner) is off-canonical. The winner must
        // relocate INTO the canonical path the redundant currently occupies —
        // which only works because the redundant is retired (vacated) first.
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        let origin = "git@github.com:owner/repo.git";

        let seed = seed_repo(tmp.path());
        seed_commit(&seed, "v1", "c1");
        // Redundant subset lives AT the canonical bare path.
        let bare_sub = bare_root.join("owner").join("repo.git");
        clone_bare(&seed, &bare_sub, origin);
        seed_commit(&seed, "v2", "c2");
        // Winner superset lives off-canonical.
        let bare_super = bare_root.join("stray").join("repo.git");
        clone_bare(&seed, &bare_super, origin);

        // Redundant's worktree occupies the canonical worktree path; the winner's
        // worktree is off-canonical and must move into it.
        let wt_sub = worktree_root.join("owner").join("repo").join("main");
        fs::create_dir_all(wt_sub.parent().unwrap()).unwrap();
        git_ok(&bare_sub, &["worktree", "add", &wt_sub.to_string_lossy(), "main"]);
        let wt_super = worktree_root.join("stray").join("repo-super");
        fs::create_dir_all(wt_super.parent().unwrap()).unwrap();
        git_ok(
            &bare_super,
            &["worktree", "add", &wt_super.to_string_lossy(), "main"],
        );

        let plan = scan_to_plan(&bare_root, &worktree_root, 60);
        let actions: Vec<MigrateAction> = plan.groups.iter().flat_map(group_actions).collect();
        let results = apply_plan(&plan, &actions);
        // No action may be skipped — the winner's relocation into the (vacated)
        // canonical path must not be rejected as "destination exists".
        assert!(
            !results
                .iter()
                .any(|r| matches!(r.status, ApplyStatus::Skipped(_))),
            "no consolidation action may be skipped: {:?}",
            results
                .iter()
                .map(|r| (&r.action, &r.status))
                .collect::<Vec<_>>()
        );
        assert!(
            results.iter().all(|r| matches!(
                r.status,
                ApplyStatus::Moved | ApplyStatus::MovedWithWarnings(_)
            )),
            "apply statuses: {:?}",
            results.iter().map(|r| &r.status).collect::<Vec<_>>()
        );

        // The winner (superset, holding c2) now lives at the canonical bare path,
        // and the redundant subset is in trash.
        let canonical_bare = bare_root.join("owner").join("repo.git");
        assert!(canonical_bare.exists(), "winner must occupy canonical bare path");
        assert!(
            !bare_super.exists(),
            "winner's old off-canonical path must be gone"
        );
        assert!(
            bare_root
                .join(".stokd-migrate-trash")
                .join("owner")
                .join("repo-2.git")
                .exists(),
            "redundant subset must be retired to trash"
        );
        // The canonical bare is the SUPERSET: its main carries the winner's c2.
        let canonical_wt = worktree_root.join("owner").join("repo").join("main");
        let st = Command::new("git")
            .arg("-C")
            .arg(&canonical_wt)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(
            st.status.success(),
            "winner worktree must be functional at canonical"
        );
        let log =
            run_git_capture(&canonical_bare, &["log", "--oneline", "main"]).unwrap_or_default();
        assert!(
            log.contains("c2"),
            "canonical bare must carry the winner's c2 commit: {log}"
        );
    }

    #[test]
    fn apply_consolidation_fetches_redundant_only_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        let origin = "git@github.com:owner/repo.git";

        let seed = seed_repo(tmp.path());
        seed_commit(&seed, "v1", "c1");
        let bare_b = bare_root.join("stray").join("repo.git");
        clone_bare(&seed, &bare_b, origin);
        // A redundant-only branch whose tip (c1) is already in A's history but
        // for which A has no ref — it must be preserved into the winner.
        git_ok(&bare_b, &["branch", "feat", "main"]);
        seed_commit(&seed, "v2", "c2");
        let bare_a = bare_root.join("owner").join("repo.git");
        clone_bare(&seed, &bare_a, origin);

        let wt_a = worktree_root.join("owner").join("repo").join("main");
        fs::create_dir_all(wt_a.parent().unwrap()).unwrap();
        git_ok(&bare_a, &["worktree", "add", &wt_a.to_string_lossy(), "main"]);
        let wt_b = worktree_root.join("stray").join("repo-b");
        fs::create_dir_all(wt_b.parent().unwrap()).unwrap();
        git_ok(&bare_b, &["worktree", "add", &wt_b.to_string_lossy(), "main"]);

        // Winner has no `feat` before consolidation.
        assert!(run_git_capture(&bare_a, &["rev-parse", "--verify", "feat"]).is_none());

        let plan = scan_to_plan(&bare_root, &worktree_root, 60);
        let actions: Vec<MigrateAction> = plan.groups.iter().flat_map(group_actions).collect();
        let results = apply_plan(&plan, &actions);
        assert!(
            results.iter().all(|r| matches!(
                r.status,
                ApplyStatus::Moved | ApplyStatus::MovedWithWarnings(_)
            )),
            "apply statuses: {:?}",
            results.iter().map(|r| &r.status).collect::<Vec<_>>()
        );

        // The redundant-only branch was mirrored into the winner; B is trashed.
        assert!(
            run_git_capture(&bare_a, &["rev-parse", "--verify", "feat"]).is_some(),
            "redundant-only branch must be preserved into the winner"
        );
        assert!(
            bare_root
                .join(".stokd-migrate-trash")
                .join("owner")
                .join("repo-2.git")
                .exists(),
            "redundant bare must be retired to trash"
        );
        assert!(!bare_b.exists(), "old redundant bare path must be gone");
    }

    #[test]
    fn apply_divergent_cluster_falls_back_to_suffix() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        let origin = "git@github.com:owner/repo.git";

        let seed = seed_repo(tmp.path());
        seed_commit(&seed, "v1", "c1");
        let bare_b = bare_root.join("stray").join("repo.git");
        clone_bare(&seed, &bare_b, origin);
        seed_commit(&seed, "v2", "c2");
        let bare_a = bare_root.join("owner").join("repo.git");
        clone_bare(&seed, &bare_a, origin);

        let wt_a = worktree_root.join("owner").join("repo").join("main");
        fs::create_dir_all(wt_a.parent().unwrap()).unwrap();
        git_ok(&bare_a, &["worktree", "add", &wt_a.to_string_lossy(), "main"]);
        let wt_b = worktree_root.join("stray").join("repo-b");
        fs::create_dir_all(wt_b.parent().unwrap()).unwrap();
        git_ok(&bare_b, &["worktree", "add", &wt_b.to_string_lossy(), "main"]);
        // Diverge B with a commit A does not have (unique unpushed work).
        fs::write(wt_b.join("b-only.txt"), "unique to B").unwrap();
        git_ok(&wt_b, &["add", "-A"]);
        git_ok(&wt_b, &["commit", "-m", "cB"]);

        let plan = scan_to_plan(&bare_root, &worktree_root, 60);
        assert!(
            plan.duplicate_clusters
                .iter()
                .any(|c| c.resolution == DuplicateResolution::NeedsHuman),
            "a divergent cluster must classify as needs-human: {:?}",
            plan.duplicate_clusters
        );

        let actions: Vec<MigrateAction> = plan.groups.iter().flat_map(group_actions).collect();
        // No destructive consolidation actions are derived for a divergent cluster.
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, MigrateAction::RetireBare { .. } | MigrateAction::DropWorktree { .. })),
            "divergent cluster must not retire or drop: {actions:?}"
        );

        let results = apply_plan(&plan, &actions);
        assert!(
            results.iter().all(|r| matches!(
                r.status,
                ApplyStatus::Moved | ApplyStatus::MovedWithWarnings(_)
            )),
            "apply statuses: {:?}",
            results.iter().map(|r| &r.status).collect::<Vec<_>>()
        );

        // The divergent bare is suffix-relocated, NOT trashed.
        assert!(
            bare_root.join("owner").join("repo-2.git").exists(),
            "divergent bare must fall back to the -2 suffix relocation"
        );
        assert!(
            !bare_root.join(".stokd-migrate-trash").exists(),
            "a divergent bare must never be retired to trash"
        );
    }

    // ── Reconstruct standalone clones (AX-CLI-REPO-MIGRATE-RECONSTRUCT-BARE) ──

    #[test]
    fn reconstruct_leaf_uses_main_pattern_or_branch() {
        // main/master/detached/undetermined render through the main pattern.
        assert_eq!(reconstruct_leaf(Some("main"), "o", "r", "{branch}"), "main");
        assert_eq!(reconstruct_leaf(Some("master"), "o", "r", "{branch}"), "master");
        assert_eq!(reconstruct_leaf(None, "o", "r", "{branch}"), "main");
        assert_eq!(reconstruct_leaf(Some("HEAD"), "o", "r", "{branch}"), "main");
        // Any other branch keeps its name, with `/` flattened to a single leaf.
        assert_eq!(
            reconstruct_leaf(Some("feature/x"), "o", "r", "{branch}"),
            "feature-x"
        );
    }

    #[test]
    fn preflight_reconstruct_rejects_existing_or_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = tmp.path().join("clone");
        fs::create_dir_all(clone.join(".git")).unwrap();

        // A free bare + worktree → safe.
        let ok = MigrateAction::ReconstructBare {
            clone: clone.clone(),
            bare: tmp.path().join("free.git"),
            worktree: tmp.path().join("freewt"),
            branch: Some("main".to_string()),
        };
        let (safe, rejected) = preflight(std::slice::from_ref(&ok));
        assert_eq!(safe, vec![ok]);
        assert!(rejected.is_empty());

        // A valid bare from a DIFFERENT repository occupying the destination →
        // DestinationExists (a real collision, never touched).
        let real_clone = tmp.path().join("realclone");
        setup_standalone_clone(&real_clone, "git@github.com:acme/widgets.git");
        let foreign_bare = tmp.path().join("foreign.git");
        setup_bare(tmp.path(), &foreign_bare, "git@github.com:other/thing.git");
        let dest_taken = MigrateAction::ReconstructBare {
            clone: real_clone.clone(),
            bare: foreign_bare,
            worktree: tmp.path().join("wt2"),
            branch: None,
        };
        let (safe, rejected) = preflight(&[dest_taken]);
        assert!(safe.is_empty());
        assert_eq!(rejected[0].1, UnsafeReason::DestinationExists);

        // A salvageable leftover (an incomplete/partial bare — here an empty dir,
        // not a usable bare) is NOT rejected: reconstruct rebuilds from the intact
        // clone. (AX-CLI-REPO-MIGRATE-RECONSTRUCT-RESUMABLE)
        let partial_bare = tmp.path().join("partial.git");
        fs::create_dir_all(&partial_bare).unwrap();
        let resumable = MigrateAction::ReconstructBare {
            clone: real_clone.clone(),
            bare: partial_bare,
            worktree: tmp.path().join("wt2b"),
            branch: None,
        };
        let (safe, rejected) = preflight(std::slice::from_ref(&resumable));
        assert_eq!(safe, vec![resumable]);
        assert!(rejected.is_empty());

        // A missing source clone → SourceMissing.
        let gone = MigrateAction::ReconstructBare {
            clone: tmp.path().join("gone"),
            bare: tmp.path().join("b3.git"),
            worktree: tmp.path().join("wt3"),
            branch: None,
        };
        let (safe, rejected) = preflight(&[gone]);
        assert!(safe.is_empty());
        assert_eq!(rejected[0].1, UnsafeReason::SourceMissing);
    }

    #[test]
    fn standalone_clone_is_planned_for_reconstruction() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        // A full clone sitting loose under root, no bare anywhere.
        let clone = worktree_root.join("loose").join("widgets");
        setup_standalone_clone(&clone, "git@github.com:acme/widgets.git");

        let plan = scan_to_plan(&bare_root, &worktree_root, 60);
        assert!(
            plan.groups.is_empty(),
            "a standalone clone has no bare-backed group"
        );
        assert_eq!(plan.reconstruct.len(), 1, "one reconstruct candidate");
        let r = &plan.reconstruct[0];
        assert_eq!((r.owner.as_str(), r.repo.as_str()), ("acme", "widgets"));
        assert_eq!(r.bare, bare_root.join("acme").join("widgets.git"));
        assert_eq!(
            r.worktree,
            worktree_root.join("acme").join("widgets").join("main")
        );

        let actions = reconstruct_actions(&plan);
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            MigrateAction::ReconstructBare { .. }
        ));
    }

    #[test]
    fn overlay_preserves_source_on_failure() {
        // The overlay must NEVER lose source data when an entry can't be placed
        // (the failure mode that previously corrupted a repo). Force a failure by
        // pre-creating a destination entry that collides with a source file, then
        // assert the source still has every original file.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        for f in ["a.txt", "b.txt", "c.txt"] {
            fs::write(src.join(f), f).unwrap();
        }
        // A non-empty directory at dst/b.txt makes both rename(file→dir) and
        // copy(file→dir) fail for the "b.txt" entry.
        fs::create_dir_all(dst.join("b.txt")).unwrap();
        fs::write(dst.join("b.txt").join("inner"), "x").unwrap();

        let result = overlay_dir_contents(&src, &dst);
        assert!(result.is_err(), "overlay must report the failure");

        // No data lost: the source still has all three files (placed ones undone).
        for f in ["a.txt", "b.txt", "c.txt"] {
            assert!(
                src.join(f).is_file(),
                "source file {f} must survive a failed overlay"
            );
            assert_eq!(fs::read_to_string(src.join(f)).unwrap(), f);
        }
    }

    #[test]
    fn reconstruct_preserves_branches_stash_and_staged_state() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        let clone = worktree_root.join("acme-widgets");
        setup_standalone_clone(&clone, "git@github.com:acme/widgets.git");

        // An extra local branch with a unique commit — must survive.
        git_ok(&clone, &["checkout", "-b", "feature/keep"]);
        fs::write(clone.join("feature.txt"), "feature work").unwrap();
        git_ok(&clone, &["add", "-A"]);
        git_ok(&clone, &["commit", "-m", "feature commit"]);
        let feature_sha = run_git_capture(&clone, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        git_ok(&clone, &["checkout", "main"]);

        // An unpushed local commit on main (there is no remote — pure local history).
        fs::write(clone.join("local.txt"), "local only").unwrap();
        git_ok(&clone, &["add", "-A"]);
        git_ok(&clone, &["commit", "-m", "local unpushed"]);
        let main_sha = run_git_capture(&clone, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();

        // A stash — must survive.
        fs::write(clone.join("README.md"), "stashed change").unwrap();
        git_ok(&clone, &["stash", "push", "-qm", "wip"]);

        // Staged + unstaged + untracked + gitignored at reconstruct time.
        fs::write(clone.join(".gitignore"), "secret.env\n").unwrap();
        git_ok(&clone, &["add", ".gitignore"]); // staged add
        fs::write(clone.join("README.md"), "unstaged edit").unwrap(); // unstaged tracked edit
        fs::write(clone.join("untracked.txt"), "untracked").unwrap();
        fs::write(clone.join("secret.env"), "TOKEN=xyz").unwrap(); // gitignored

        let plan = scan_to_plan(&bare_root, &worktree_root, 60);
        let actions = reconstruct_actions(&plan);
        let results = apply_plan(&plan, &actions);
        assert!(
            results.iter().all(|r| matches!(
                r.status,
                ApplyStatus::Moved | ApplyStatus::MovedWithWarnings(_)
            )),
            "reconstruct should succeed: {:?}",
            results.iter().map(|r| &r.status).collect::<Vec<_>>()
        );

        let bare = bare_root.join("acme").join("widgets.git");
        let wt = worktree_root.join("acme").join("widgets").join("main");

        // Every branch is preserved at its exact commit (incl. the unpushed one).
        assert_eq!(
            run_git_capture(&bare, &["rev-parse", "main"]).unwrap().trim(),
            main_sha,
            "main's unpushed commit must be preserved"
        );
        assert_eq!(
            run_git_capture(&bare, &["rev-parse", "feature/keep"])
                .unwrap()
                .trim(),
            feature_sha,
            "the extra local branch must be preserved"
        );

        // The stash survives.
        let stash = run_git_capture(&wt, &["stash", "list"]).unwrap_or_default();
        assert!(stash.contains("wip"), "stash must survive: {stash:?}");

        // Working-tree contents are byte-for-byte intact.
        assert_eq!(fs::read_to_string(wt.join("README.md")).unwrap(), "unstaged edit");
        assert_eq!(fs::read_to_string(wt.join("untracked.txt")).unwrap(), "untracked");
        assert_eq!(
            fs::read_to_string(wt.join("secret.env")).unwrap(),
            "TOKEN=xyz",
            "gitignored file preserved"
        );
        assert_eq!(fs::read_to_string(wt.join("local.txt")).unwrap(), "local only");

        // Staged vs. unstaged split is preserved exactly (index restored).
        let status = run_git_capture(&wt, &["status", "--porcelain"]).unwrap();
        assert!(
            status.lines().any(|l| l.starts_with("A ") && l.contains(".gitignore")),
            ".gitignore must stay STAGED: {status:?}"
        );
        assert!(
            status.lines().any(|l| l.starts_with(" M") && l.contains("README.md")),
            "README.md must stay UNSTAGED-modified: {status:?}"
        );
    }

    // A prior run that failed part-way (e.g. a locked file during the .git copy)
    // can leave a partial/garbage bare at the destination. A re-run must clear it
    // to recoverable trash and rebuild cleanly from the still-intact clone rather
    // than skipping forever as "destination already exists".
    // (AX-CLI-REPO-MIGRATE-RECONSTRUCT-RESUMABLE)
    #[test]
    fn reconstruct_salvages_partial_bare_and_rebuilds() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        let clone = worktree_root.join("loose").join("widgets");
        setup_standalone_clone(&clone, "git@github.com:acme/widgets.git");

        let bare = bare_root.join("acme").join("widgets.git");
        let wt = worktree_root.join("acme").join("widgets").join("main");
        // Simulate the leftover of a failed prior run: an incomplete bare (an empty
        // directory, which is not a usable bare) sitting at the destination.
        fs::create_dir_all(&bare).unwrap();

        let status = reconstruct_bare(&clone, &bare, &wt, Some("main"));
        assert!(
            matches!(status, ApplyStatus::Moved | ApplyStatus::MovedWithWarnings(_)),
            "salvage-and-rebuild must succeed: {status:?}"
        );
        assert!(
            run_git_capture(&bare, &["rev-parse", "--is-bare-repository"])
                .map(|s| s.trim() == "true")
                .unwrap_or(false),
            "a real bare must now sit at the destination"
        );
        assert!(wt.join(".git").is_file(), "the worktree must be attached");
        assert!(!clone.exists(), "the intact clone is consumed by the rebuild");
        // The partial bare was retired to recoverable trash, never deleted outright.
        let trash = bare_root.join(".stokd-migrate-trash").join("acme");
        assert!(
            trash.join("widgets.git").exists(),
            "the partial bare must be moved to recoverable trash"
        );
    }

    // A prior run that built the bare but never attached the worktree (the copy
    // left the clone intact) must be RESUMED: add the missing worktree, keep the
    // clone in place, and never re-move the .git. (AX-CLI-REPO-MIGRATE-RECONSTRUCT-RESUMABLE)
    #[test]
    fn reconstruct_resumes_when_only_worktree_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        let clone = worktree_root.join("loose").join("widgets");
        setup_standalone_clone(&clone, "git@github.com:acme/widgets.git");

        // Build a VALID bare from the clone (shares the clone's HEAD commit) and
        // stamp the matching origin — the authentic "bare done, worktree not" state.
        let bare = bare_root.join("acme").join("widgets.git");
        fs::create_dir_all(bare.parent().unwrap()).unwrap();
        git_ok(
            tmp.path(),
            &["clone", "--bare", &clone.to_string_lossy(), &bare.to_string_lossy()],
        );
        git_ok(
            &bare,
            &["config", "remote.origin.url", "git@github.com:acme/widgets.git"],
        );

        let wt = worktree_root.join("acme").join("widgets").join("main");
        assert_eq!(
            classify_existing_bare(&clone, &bare),
            ExistingBare::Resumable,
            "a matching, complete bare with the clone's HEAD is resumable"
        );

        let status = reconstruct_bare(&clone, &bare, &wt, Some("main"));
        assert!(
            matches!(status, ApplyStatus::MovedWithWarnings(_)),
            "resume warns that the original clone remains: {status:?}"
        );
        assert!(wt.join(".git").is_file(), "the missing worktree is now attached");
        assert!(
            clone.exists(),
            "resume never touches the intact clone (left for the human to remove)"
        );
        // Idempotent: re-running now that the worktree exists is a hard no-op stop.
        let again = reconstruct_bare(&clone, &bare, &wt, Some("main"));
        assert!(
            matches!(again, ApplyStatus::Failed(_)),
            "a re-run with the worktree present must refuse, not clobber: {again:?}"
        );
    }

    // A valid bare belonging to a DIFFERENT repository must never be salvaged or
    // resumed — it is a real collision. (AX-CLI-REPO-MIGRATE-RECONSTRUCT-RESUMABLE)
    #[test]
    fn reconstruct_refuses_a_foreign_bare_at_the_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = tmp.path().join("clone");
        setup_standalone_clone(&clone, "git@github.com:acme/widgets.git");
        let bare = tmp.path().join("foreign.git");
        setup_bare(tmp.path(), &bare, "git@github.com:other/thing.git");
        let wt = tmp.path().join("wt");

        assert_eq!(classify_existing_bare(&clone, &bare), ExistingBare::Foreign);
        let status = reconstruct_bare(&clone, &bare, &wt, Some("main"));
        assert!(
            matches!(status, ApplyStatus::Failed(ref m) if m.contains("different repository")),
            "a foreign bare must be refused: {status:?}"
        );
        assert!(clone.exists(), "the clone is untouched on refusal");
    }

    #[test]
    fn vendored_clones_under_node_modules_are_not_migration_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        // A real top-level clone to adopt.
        let top = worktree_root.join("ConnectorAuth");
        setup_standalone_clone(&top, "git@github.com:eonesolutions/ConnectorAuth.git");
        // A dependency checkout nested in node_modules under a non-repo dir — this
        // must be ignored entirely, never reconstructed.
        let dep = worktree_root
            .join("proj")
            .join("node_modules")
            .join("eone-ui");
        setup_standalone_clone(&dep, "git@github.com:eonesolutions/eone-ui.git");

        let plan = scan_to_plan(&bare_root, &worktree_root, 60);
        let repos: Vec<&str> = plan.reconstruct.iter().map(|r| r.repo.as_str()).collect();
        assert_eq!(
            repos,
            vec!["ConnectorAuth"],
            "only the top-level clone is a candidate; node_modules deps are skipped"
        );
    }

    #[test]
    fn reconstruct_converts_standalone_clone_preserving_work() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_root = tmp.path().join("bare");
        let worktree_root = tmp.path().join("wt");
        let clone = worktree_root.join("loose").join("widgets");
        setup_standalone_clone(&clone, "git@github.com:acme/widgets.git");

        // Commit a .gitignore that ignores `.env`, then create three kinds of work
        // that MUST survive: an uncommitted tracked modification, an untracked
        // file, and a gitignored secret.
        fs::write(clone.join(".gitignore"), ".env\n").unwrap();
        git_ok(&clone, &["add", ".gitignore"]);
        git_ok(&clone, &["commit", "-m", "add gitignore"]);
        fs::write(clone.join("README.md"), "MODIFIED").unwrap();
        fs::write(clone.join("untracked.txt"), "new").unwrap();
        fs::write(clone.join(".env"), "SECRET=1").unwrap();

        let plan = scan_to_plan(&bare_root, &worktree_root, 60);
        let actions = reconstruct_actions(&plan);
        let results = apply_plan(&plan, &actions);
        assert!(
            results.iter().all(|r| matches!(
                r.status,
                ApplyStatus::Moved | ApplyStatus::MovedWithWarnings(_)
            )),
            "reconstruct should succeed: {:?}",
            results.iter().map(|r| &r.status).collect::<Vec<_>>()
        );

        let bare = bare_root.join("acme").join("widgets.git");
        let wt = worktree_root.join("acme").join("widgets").join("main");
        assert!(bare.exists(), "canonical bare created");
        assert!(
            wt.join(".git").is_file(),
            "worktree must be a linked worktree (.git is a file)"
        );
        assert!(!clone.exists(), "the standalone clone directory is removed");

        // The bare HEAD points at the placeholder, never a working branch.
        let head = Command::new("git")
            .arg("--git-dir")
            .arg(&bare)
            .args(["symbolic-ref", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&head.stdout).trim(),
            "refs/heads/__stokd_hub__",
            "bare HEAD must be the placeholder (AX-CLI-REPO-BARE-HEAD-PLACEHOLDER)"
        );

        // The worktree is on main and git operations work there.
        let branch = Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&branch.stdout).trim(), "main");

        // Every kind of work survived the conversion.
        assert_eq!(
            fs::read_to_string(wt.join("README.md")).unwrap(),
            "MODIFIED",
            "uncommitted tracked modification preserved"
        );
        assert_eq!(
            fs::read_to_string(wt.join("untracked.txt")).unwrap(),
            "new",
            "untracked file preserved"
        );
        assert_eq!(
            fs::read_to_string(wt.join(".env")).unwrap(),
            "SECRET=1",
            "gitignored file preserved"
        );

        // git sees the modification as a pending change (status works post-convert).
        let status = Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(status.status.success(), "git status must work in the worktree");
        let s = String::from_utf8_lossy(&status.stdout);
        assert!(
            s.contains("README.md"),
            "the modified tracked file shows as dirty: {s}"
        );
        assert!(
            !s.contains(".env"),
            "the gitignored file is not surfaced by status: {s}"
        );
    }
}
