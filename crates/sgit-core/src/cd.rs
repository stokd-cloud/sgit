//! Generic `sgit cd` resolution: owner/repo (+ optional branch/ref) → worktree path.
//!
//! Filesystem + local git only — no cloud API, no stokd. Task/project-hash refs are
//! not resolved here; the CLI optional external resolver seam (D001) handles those.

use std::path::{Path, PathBuf};

use crate::workspace::find_worktree_for_branch_at;

/// Parsed cd target: fully-qualified `owner/repo`, or a bare `repo` name whose
/// owner is discovered by scanning the worktree root.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum CdTarget {
    OwnerRepo { owner: String, repo: String },
    Repo { repo: String },
}

/// True when `s` is a stokd-style task/project ref that needs the optional
/// external resolver (not generic filesystem resolution).
pub fn is_task_or_project_ref(s: &str) -> bool {
    let t = s.trim().trim_matches('/');
    t.starts_with("task/") || t.starts_with("project/")
}

/// Parse `<owner/repo | repo>`. A trailing `.git` and surrounding slashes are
/// stripped. Paths with more than one `/` (e.g. `a/b/c`) are rejected — except
/// callers should route `task/*` / `project/*` to the external resolver first.
pub fn parse_cd_target(target: &str) -> Result<CdTarget, String> {
    let trimmed = target.trim().trim_matches('/');
    if trimmed.is_empty() {
        return Err("empty target: expected <owner/repo> or <repo>".to_string());
    }
    match trimmed.split_once('/') {
        Some((owner, repo)) => {
            let owner = owner.trim();
            let repo = repo.trim().trim_end_matches(".git");
            if owner.is_empty() || repo.is_empty() || repo.contains('/') {
                return Err(format!(
                    "invalid target '{target}': expected <owner/repo> or <repo>"
                ));
            }
            Ok(CdTarget::OwnerRepo {
                owner: owner.to_string(),
                repo: repo.to_string(),
            })
        }
        None => Ok(CdTarget::Repo {
            repo: trimmed.trim_end_matches(".git").to_string(),
        }),
    }
}

/// Given owners under which `<worktree_root>/<owner>/<repo>` exists, pick the
/// single owner, or error on ambiguity/absence.
pub fn resolve_owner_from_candidates(repo: &str, owners: &[String]) -> Result<String, String> {
    match owners {
        [] => Err(format!(
            "no worktree found for repo '{repo}' under the worktree root; qualify with <owner/repo>"
        )),
        [only] => Ok(only.clone()),
        many => {
            let listed = many
                .iter()
                .map(|owner| format!("{owner}/{repo}"))
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "repo '{repo}' is ambiguous across multiple owners: {listed}; qualify with <owner/repo>"
            ))
        }
    }
}

/// Select the worktree leaf directory name given an optional explicit leaf.
/// With no explicit leaf: prefer `main`, then `master`, then lexicographically first.
pub fn select_worktree_leaf(leaves: &[String], explicit: Option<&str>) -> Result<String, String> {
    let mut sorted = leaves.to_vec();
    sorted.sort();
    match explicit {
        Some(leaf) => {
            if sorted.iter().any(|candidate| candidate == leaf) {
                Ok(leaf.to_string())
            } else {
                Err(format!(
                    "no worktree '{leaf}' found; available: {}",
                    join_or_none(&sorted)
                ))
            }
        }
        None => {
            if sorted.iter().any(|leaf| leaf == "main") {
                return Ok("main".to_string());
            }
            if sorted.iter().any(|leaf| leaf == "master") {
                return Ok("master".to_string());
            }
            sorted
                .into_iter()
                .next()
                .ok_or_else(|| "no worktrees found for repo".to_string())
        }
    }
}

/// Ordered, de-duped candidate leaf directory names for an explicit ref (disk-only
/// conventions; no API-derived hash expansion).
pub fn candidate_leaves_for_ref(reference: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |value: String| {
        if !value.is_empty() && !out.contains(&value) {
            out.push(value);
        }
    };
    push(reference.to_string());
    // Branch `task/abc` often maps to leaf `task-abc`.
    if let Some(rest) = reference.strip_prefix("task/") {
        push(format!("task-{rest}"));
    }
    if let Some(rest) = reference.strip_prefix("project/") {
        push(format!("project-{rest}"));
    }
    push(format!("project-{reference}"));
    push(format!("task-{reference}"));
    out
}

/// First candidate that exists in `leaves`, preserving candidate order.
pub fn first_present(leaves: &[String], candidates: &[String]) -> Option<String> {
    candidates
        .iter()
        .find(|c| leaves.iter().any(|l| l == *c))
        .cloned()
}

/// Unique prefix partial match against worktree leaf names.
///
/// Used by `sgit cd` / `scd` when no exact leaf matches: if exactly one leaf
/// starts with `prefix`, return it; if several do, error with the matches listed;
/// if none do, return `Ok(None)` so callers can try other strategies.
///
/// An exact equality match among the prefix hits always wins (so `upstream-main`
/// prefers the leaf `upstream-main` over `upstream-main-autogroup`).
/// Empty `prefix` never matches (avoids treating "" as "everything").
pub fn unique_prefix_leaf(leaves: &[String], prefix: &str) -> Result<Option<String>, String> {
    if prefix.is_empty() {
        return Ok(None);
    }
    let matches: Vec<&String> = leaves
        .iter()
        .filter(|leaf| leaf.starts_with(prefix))
        .collect();
    // Exact equality beats longer siblings that merely share the prefix.
    if let Some(exact) = matches.iter().find(|leaf| leaf.as_str() == prefix) {
        return Ok(Some((*exact).clone()));
    }
    match matches.as_slice() {
        [] => Ok(None),
        [only] => Ok(Some((*only).clone())),
        many => {
            let listed = many
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "ambiguous partial leaf '{prefix}' matches multiple worktrees: {listed}"
            ))
        }
    }
}

fn join_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".to_string()
    } else {
        items.join(", ")
    }
}

/// Owners (immediate child dirs of `worktree_root`) that contain a `<repo>`
/// subdirectory. Sorted for determinism.
pub fn owners_with_repo(worktree_root: &Path, repo: &str) -> Vec<String> {
    let mut owners = Vec::new();
    if let Ok(entries) = std::fs::read_dir(worktree_root) {
        for entry in entries.flatten() {
            let Some(owner) = entry.file_name().to_str().map(|s| s.to_string()) else {
                continue;
            };
            if worktree_root.join(&owner).join(repo).is_dir() {
                owners.push(owner);
            }
        }
    }
    owners.sort();
    owners
}

/// Worktree leaf directory names directly under `<worktree_root>/<owner>/<repo>`.
pub fn leaves_under(repo_dir: &Path) -> Vec<String> {
    let mut leaves = Vec::new();
    if let Ok(entries) = std::fs::read_dir(repo_dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    leaves.push(name.to_string());
                }
            }
        }
    }
    leaves.sort();
    leaves
}

/// Resolve `target` + optional `reference` to an absolute worktree directory.
/// Uses only `worktree_root` + local git (branch → worktree lookup). No cloud.
pub fn resolve_worktree_path(
    worktree_root: &Path,
    target: &str,
    reference: Option<&str>,
) -> Result<PathBuf, String> {
    let (owner, repo) = match parse_cd_target(target)? {
        CdTarget::OwnerRepo { owner, repo } => (owner, repo),
        CdTarget::Repo { repo } => {
            let owners = owners_with_repo(worktree_root, &repo);
            (resolve_owner_from_candidates(&repo, &owners)?, repo)
        }
    };

    let repo_dir = worktree_root.join(&owner).join(&repo);
    if !repo_dir.is_dir() {
        return Err(format!(
            "no worktrees found for {owner}/{repo} under {}",
            worktree_root.display()
        ));
    }

    let leaves = leaves_under(&repo_dir);

    let Some(reference) = reference else {
        let leaf = select_worktree_leaf(&leaves, None)?;
        return Ok(canonicalize_existing(&repo_dir.join(leaf)));
    };

    // 1) Disk-first: exact-as-typed leaf, then slug conventions. Fully offline.
    let candidates = candidate_leaves_for_ref(reference);
    if let Some(leaf) = first_present(&leaves, &candidates) {
        return Ok(canonicalize_existing(&repo_dir.join(leaf)));
    }

    // 2) Unique prefix partial: `scd gdock upstream-ag-` → `upstream-ag-brand-quad`.
    //    Exact match above always wins over longer siblings (e.g. `upstream-main`
    //    vs `upstream-main-autogroup`). Ambiguous prefixes error out.
    match unique_prefix_leaf(&leaves, reference) {
        Ok(Some(leaf)) => return Ok(canonicalize_existing(&repo_dir.join(leaf))),
        Ok(None) => {}
        Err(ambiguous) => return Err(ambiguous),
    }

    // 3) Branch fallback: ref may be a branch whose leaf name differs.
    if let Some(anchor) = leaves.first() {
        let anchor_path = repo_dir.join(anchor);
        if let Ok(path) = find_worktree_for_branch_at(&anchor_path, reference) {
            return Ok(canonicalize_existing(&path));
        }
    }

    Err(format!(
        "could not resolve '{reference}' to a worktree under {owner}/{repo}; available: {}",
        join_or_none(&leaves)
    ))
}

fn canonicalize_existing(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_owner_repo() {
        assert_eq!(
            parse_cd_target("stokd-cloud/stokd-mono"),
            Ok(CdTarget::OwnerRepo {
                owner: "stokd-cloud".to_string(),
                repo: "stokd-mono".to_string(),
            })
        );
    }

    #[test]
    fn parse_bare_repo() {
        assert_eq!(
            parse_cd_target("stokd-mono"),
            Ok(CdTarget::Repo {
                repo: "stokd-mono".to_string(),
            })
        );
    }

    #[test]
    fn parse_rejects_empty_and_nested() {
        assert!(parse_cd_target("").is_err());
        assert!(parse_cd_target("a/b/c").is_err());
    }

    #[test]
    fn select_leaf_prefers_main() {
        let leaves = vec![
            "master".to_string(),
            "task-abc".to_string(),
            "main".to_string(),
        ];
        assert_eq!(select_worktree_leaf(&leaves, None).unwrap(), "main");
    }

    #[test]
    fn is_task_or_project_ref_detects() {
        assert!(is_task_or_project_ref("task/abc123"));
        assert!(is_task_or_project_ref("project/xyz"));
        assert!(!is_task_or_project_ref("owner/repo"));
        assert!(!is_task_or_project_ref("main"));
    }

    #[test]
    fn candidate_leaves_include_task_slash_to_dash() {
        let c = candidate_leaves_for_ref("task/abc");
        assert!(c.contains(&"task/abc".to_string()));
        assert!(c.contains(&"task-abc".to_string()));
    }

    #[test]
    fn unique_prefix_leaf_resolves_single_match() {
        let leaves = vec![
            "main".to_string(),
            "upstream-ag-brand-quad".to_string(),
            "upstream-main".to_string(),
            "upstream-main-autogroup".to_string(),
        ];
        assert_eq!(
            unique_prefix_leaf(&leaves, "upstream-ag-").unwrap(),
            Some("upstream-ag-brand-quad".to_string())
        );
    }

    #[test]
    fn unique_prefix_leaf_exact_wins_over_longer_sibling() {
        let leaves = vec![
            "upstream-main".to_string(),
            "upstream-main-autogroup".to_string(),
        ];
        assert_eq!(
            unique_prefix_leaf(&leaves, "upstream-main").unwrap(),
            Some("upstream-main".to_string())
        );
    }

    #[test]
    fn unique_prefix_leaf_errors_when_ambiguous() {
        let leaves = vec![
            "upstream-main".to_string(),
            "upstream-main-autogroup".to_string(),
        ];
        let err = unique_prefix_leaf(&leaves, "upstream-m")
            .expect_err("expected ambiguity when prefix matches two leaves");
        assert!(err.contains("ambiguous"), "err={err}");
        assert!(err.contains("upstream-main"), "err={err}");
        assert!(err.contains("upstream-main-autogroup"), "err={err}");
    }

    #[test]
    fn unique_prefix_leaf_none_when_no_match() {
        let leaves = vec!["main".to_string(), "upstream-main".to_string()];
        assert_eq!(unique_prefix_leaf(&leaves, "nope").unwrap(), None);
    }

    #[test]
    fn unique_prefix_leaf_empty_prefix_is_none() {
        let leaves = vec!["main".to_string()];
        assert_eq!(unique_prefix_leaf(&leaves, "").unwrap(), None);
    }

    #[test]
    fn resolve_worktree_path_exact_beats_longer_prefix_sibling() {
        let root = tempfile_worktree_root(&[
            ("stokd-cloud", "gdock", &["upstream-main", "upstream-main-autogroup"]),
        ]);
        let path = resolve_worktree_path(&root, "gdock", Some("upstream-main")).unwrap();
        assert!(
            path.ends_with("upstream-main"),
            "exact leaf must win over longer prefix sibling; got {}",
            path.display()
        );
        assert!(
            !path.ends_with("upstream-main-autogroup"),
            "must not pick the longer sibling; got {}",
            path.display()
        );
    }

    #[test]
    fn resolve_worktree_path_unique_partial_prefix() {
        let root = tempfile_worktree_root(&[(
            "stokd-cloud",
            "gdock",
            &["main", "upstream-ag-brand-quad", "upstream-main"],
        )]);
        let path = resolve_worktree_path(&root, "gdock", Some("upstream-ag-")).unwrap();
        assert!(
            path.ends_with("upstream-ag-brand-quad"),
            "expected unique partial prefix resolution; got {}",
            path.display()
        );
    }

    #[test]
    fn resolve_worktree_path_ambiguous_partial_errors() {
        let root = tempfile_worktree_root(&[(
            "stokd-cloud",
            "gdock",
            &["upstream-main", "upstream-main-autogroup"],
        )]);
        // No exact leaf named "upstream-m"; two prefix matches → error.
        let err = resolve_worktree_path(&root, "gdock", Some("upstream-m"))
            .expect_err("expected ambiguity error");
        assert!(err.contains("ambiguous"), "err={err}");
    }

    #[test]
    fn resolve_worktree_path_no_match_still_errors() {
        let root = tempfile_worktree_root(&[("stokd-cloud", "gdock", &["main"])]);
        let err = resolve_worktree_path(&root, "gdock", Some("does-not-exist"))
            .expect_err("expected resolution failure");
        assert!(err.contains("could not resolve"), "err={err}");
    }

    /// Build `<root>/<owner>/<repo>/<leaf>` dirs for resolve_worktree_path tests.
    fn tempfile_worktree_root(entries: &[(&str, &str, &[&str])]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "sgit-cd-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        for (owner, repo, leaves) in entries {
            for leaf in *leaves {
                std::fs::create_dir_all(root.join(owner).join(repo).join(leaf))
                    .expect("create test leaf");
            }
        }
        root
    }
}
