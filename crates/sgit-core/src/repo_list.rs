//! Bare-repo listing used by `sgit repo list`.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::layout::{render_worktree_name_pattern, resolve_default_branch, resolve_origin_url};
use crate::RepositoriesConfig;

/// One locally bare-cloned repo under `<bareRoot>/<owner>/<repo>.git`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BareRepoEntry {
    pub owner: String,
    pub repo: String,
    #[serde(rename = "bareRepoPath")]
    pub bare_repo_path: String,
    #[serde(rename = "originUrl")]
    pub origin_url: Option<String>,
    #[serde(rename = "defaultBranch")]
    pub default_branch: String,
    #[serde(rename = "mainWorktreePath")]
    pub main_worktree_path: String,
    #[serde(rename = "worktreeExists")]
    pub worktree_exists: bool,
}

/// Scan `<cfg.bare_root>` for the canonical `<owner>/<repo>.git` layout.
pub fn list_bare_repos(cfg: &RepositoriesConfig) -> Vec<BareRepoEntry> {
    let bare_root = PathBuf::from(&cfg.bare_root);
    scan_bare_repos(cfg, &bare_root)
}

fn scan_bare_repos(cfg: &RepositoriesConfig, bare_root: &Path) -> Vec<BareRepoEntry> {
    let mut entries = Vec::new();

    let owner_dirs = match fs::read_dir(bare_root) {
        Ok(d) => d,
        Err(_) => return entries,
    };

    for owner_entry in owner_dirs.flatten() {
        let owner_path = owner_entry.path();
        if !owner_path.is_dir() {
            continue;
        }
        let owner = match owner_path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };

        let repo_dirs = match fs::read_dir(&owner_path) {
            Ok(d) => d,
            Err(_) => continue,
        };

        for repo_entry in repo_dirs.flatten() {
            let repo_path = repo_entry.path();
            if !repo_path.is_dir() {
                continue;
            }
            let leaf = match repo_path.file_name().and_then(|n| n.to_str()) {
                Some(name) => name,
                None => continue,
            };
            let repo_name = match leaf.strip_suffix(".git") {
                Some(name) if !name.is_empty() => name.to_string(),
                _ => continue,
            };

            let default_branch = resolve_default_branch(&repo_path);
            let leaf_name = render_worktree_name_pattern(
                &cfg.main_worktree_name,
                &owner,
                &repo_name,
                &default_branch,
            );
            let worktree_dir = PathBuf::from(&cfg.worktree_root)
                .join(&owner)
                .join(&repo_name)
                .join(leaf_name);
            entries.push(BareRepoEntry {
                owner: owner.clone(),
                repo: repo_name,
                bare_repo_path: repo_path.to_string_lossy().to_string(),
                origin_url: resolve_origin_url(&repo_path),
                default_branch,
                main_worktree_path: worktree_dir.to_string_lossy().to_string(),
                worktree_exists: worktree_dir.exists(),
            });
        }
    }

    entries.sort_by(|a, b| a.owner.cmp(&b.owner).then_with(|| a.repo.cmp(&b.repo)));
    entries
}
