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
    /// False when the `<name>.git` directory is not actually a git dir (no
    /// HEAD) — e.g. a stray folder someone dropped under the bare root.
    pub valid: bool,
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

            // Only probe with git when the dir is a real git dir; probing junk
            // dirs wastes spawns and used to leak `fatal:` noise to the user.
            let valid = repo_path.join("HEAD").is_file();
            let default_branch = if valid {
                resolve_default_branch(&repo_path)
            } else {
                "-".to_string()
            };
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
                origin_url: if valid {
                    resolve_origin_url(&repo_path)
                } else {
                    None
                },
                default_branch,
                main_worktree_path: worktree_dir.to_string_lossy().to_string(),
                worktree_exists: worktree_dir.exists(),
                valid,
            });
        }
    }

    entries.sort_by(|a, b| a.owner.cmp(&b.owner).then_with(|| a.repo.cmp(&b.repo)));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn scan_bare_repos_flags_non_git_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare_root = tmp.path().join("dev");

        let real = bare_root.join("owner").join("real.git");
        fs::create_dir_all(&real).expect("mkdir real");
        let out = Command::new("git")
            .arg("init")
            .arg("--bare")
            .arg(&real)
            .output()
            .expect("git init --bare");
        assert!(out.status.success(), "git init --bare failed");

        let junk = bare_root.join("owner").join("junk.git");
        fs::create_dir_all(&junk).expect("mkdir junk");
        fs::write(junk.join("AGENTS.md"), "not a repo").expect("write junk file");

        let cfg = RepositoriesConfig {
            bare_root: bare_root.to_string_lossy().to_string(),
            worktree_root: tmp.path().join("worktrees").to_string_lossy().to_string(),
            main_worktree_name: "{branch}".into(),
            track_non_git_workspaces: false,
            ..Default::default()
        };

        let entries = list_bare_repos(&cfg);
        assert_eq!(entries.len(), 2, "both dirs are listed: {entries:?}");

        let junk_entry = entries
            .iter()
            .find(|e| e.repo == "junk")
            .expect("junk listed");
        assert!(
            !junk_entry.valid,
            "plain dir with .git suffix is flagged invalid"
        );

        let real_entry = entries
            .iter()
            .find(|e| e.repo == "real")
            .expect("real listed");
        assert!(real_entry.valid, "actual bare repo is valid");
    }
}
