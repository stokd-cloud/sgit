//! Repo-argument parsing and owner resolution for the top-level `sgit
//! clone|open|create` verbs.
//!
//! A repo argument is either fully qualified (`owner/repo`) or a bare name
//! (`repo`). A bare name is resolved to a single owner by walking an ordered
//! **chain** and accepting the answer only when it is unambiguous:
//!
//! 1. **Local layout** — owners that already have the repo bare-cloned under
//!    `bareRoot`, or a worktree under `root`. Purely offline.
//! 2. **Remote owner chain** — the owners you can reach on GitHub (your login
//!    plus the orgs you belong to). Consulted only when the local layout knows
//!    nothing, so an offline clone of an already-provisioned repo never touches
//!    the network.
//!
//! Zero candidates is "not found"; two or more is ambiguous and the caller must
//! qualify with `owner/repo`. The chain walk itself
//! ([`resolve_owner_chain`]) is pure so it is testable without a filesystem or
//! a GitHub token.

use std::path::Path;

use crate::cd::{owners_with_repo, parse_cd_target, CdTarget};
use crate::repo_list::list_bare_repos;
use crate::RepositoriesConfig;

/// A repo argument as typed by the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoSpec {
    /// Fully qualified `owner/repo` — no resolution needed.
    OwnerRepo { owner: String, repo: String },
    /// Bare `repo` name whose owner must be resolved through the chain.
    BareName { repo: String },
}

/// Which link of the chain produced a resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerSource {
    /// Local bare clones / worktrees.
    Local,
    /// GitHub owner chain (your login + your orgs).
    Remote,
}

impl OwnerSource {
    /// Human label used in resolution messages.
    pub fn label(self) -> &'static str {
        match self {
            OwnerSource::Local => "local clones",
            OwnerSource::Remote => "your GitHub owners",
        }
    }
}

/// Outcome of resolving a bare repo name against the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerResolution {
    /// Exactly one owner matched.
    Resolved { owner: String, source: OwnerSource },
    /// No link of the chain knows this repo.
    NotFound,
    /// A link matched more than one owner; the user must qualify.
    Ambiguous {
        candidates: Vec<String>,
        source: OwnerSource,
    },
}

/// Parse `<owner/repo>` or `<repo>`. A trailing `.git` and surrounding slashes
/// are stripped; more than one `/` is rejected.
pub fn parse_repo_spec(spec: &str) -> Result<RepoSpec, String> {
    match parse_cd_target(spec).map_err(|_| invalid_spec_message(spec))? {
        CdTarget::OwnerRepo { owner, repo } => Ok(RepoSpec::OwnerRepo { owner, repo }),
        CdTarget::Repo { repo } => Ok(RepoSpec::BareName { repo }),
    }
}

fn invalid_spec_message(spec: &str) -> String {
    format!("invalid repo '{spec}': expected <owner/repo> or <repo>")
}

/// Walk the chain: local candidates first, then remote. The first link with any
/// candidate decides the outcome — a locally-known repo never falls through to
/// a network lookup, and an ambiguous local match is reported rather than being
/// silently overridden by a remote guess.
pub fn resolve_owner_chain(local: &[String], remote: &[String]) -> OwnerResolution {
    for (candidates, source) in [(local, OwnerSource::Local), (remote, OwnerSource::Remote)] {
        let candidates = dedup_sorted(candidates);
        match candidates.len() {
            0 => continue,
            1 => {
                return OwnerResolution::Resolved {
                    owner: candidates.into_iter().next().expect("len == 1"),
                    source,
                }
            }
            _ => return OwnerResolution::Ambiguous { candidates, source },
        }
    }
    OwnerResolution::NotFound
}

fn dedup_sorted(owners: &[String]) -> Vec<String> {
    let mut out: Vec<String> = owners
        .iter()
        .map(|o| o.trim().to_string())
        .filter(|o| !o.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Owners that already hold `repo` locally: a bare clone under `bareRoot` or a
/// worktree directory under `root`.
pub fn local_owners_for_repo(cfg: &RepositoriesConfig, repo: &str) -> Vec<String> {
    let mut owners: Vec<String> = list_bare_repos(cfg)
        .into_iter()
        .filter(|entry| entry.repo == repo)
        .map(|entry| entry.owner)
        .collect();
    owners.extend(owners_with_repo(Path::new(&cfg.worktree_root), repo));
    dedup_sorted(&owners)
}

/// Actionable message for a resolution that did not land on a single owner.
/// Returns `None` when the resolution succeeded.
pub fn describe_owner_resolution_failure(
    repo: &str,
    resolution: &OwnerResolution,
) -> Option<String> {
    match resolution {
        OwnerResolution::Resolved { .. } => None,
        OwnerResolution::NotFound => Some(format!(
            "could not resolve '{repo}' to an owner from local clones or your GitHub owners; qualify with <owner/repo>"
        )),
        OwnerResolution::Ambiguous { candidates, source } => Some(format!(
            "repo '{repo}' is ambiguous across {}: {}; qualify with <owner/repo>",
            source.label(),
            candidates
                .iter()
                .map(|owner| format!("{owner}/{repo}"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owners(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn parse_repo_spec_splits_owner_and_bare_name() {
        assert_eq!(
            parse_repo_spec("stokd-cloud/sgit"),
            Ok(RepoSpec::OwnerRepo {
                owner: "stokd-cloud".to_string(),
                repo: "sgit".to_string(),
            })
        );
        assert_eq!(
            parse_repo_spec("sgit"),
            Ok(RepoSpec::BareName {
                repo: "sgit".to_string()
            })
        );
        // `.git` suffix and stray slashes are tolerated.
        assert_eq!(
            parse_repo_spec("/stokd-cloud/sgit.git/"),
            Ok(RepoSpec::OwnerRepo {
                owner: "stokd-cloud".to_string(),
                repo: "sgit".to_string(),
            })
        );
    }

    #[test]
    fn parse_repo_spec_rejects_empty_and_nested() {
        assert!(parse_repo_spec("").is_err());
        assert!(parse_repo_spec("a/b/c").is_err());
    }

    #[test]
    fn chain_resolves_single_local_owner_without_consulting_remote() {
        // A locally-known repo resolves offline even when the remote chain would
        // have been ambiguous.
        let resolution = resolve_owner_chain(&owners(&["stokd-cloud"]), &owners(&["a", "b"]));
        assert_eq!(
            resolution,
            OwnerResolution::Resolved {
                owner: "stokd-cloud".to_string(),
                source: OwnerSource::Local,
            }
        );
    }

    #[test]
    fn chain_falls_through_to_remote_when_local_is_empty() {
        let resolution = resolve_owner_chain(&[], &owners(&["stokd-cloud"]));
        assert_eq!(
            resolution,
            OwnerResolution::Resolved {
                owner: "stokd-cloud".to_string(),
                source: OwnerSource::Remote,
            }
        );
    }

    #[test]
    fn chain_reports_ambiguity_instead_of_guessing() {
        let resolution = resolve_owner_chain(&owners(&["beta", "alpha"]), &[]);
        assert_eq!(
            resolution,
            OwnerResolution::Ambiguous {
                candidates: owners(&["alpha", "beta"]),
                source: OwnerSource::Local,
            }
        );

        let remote = resolve_owner_chain(&[], &owners(&["beta", "alpha", "alpha"]));
        assert_eq!(
            remote,
            OwnerResolution::Ambiguous {
                candidates: owners(&["alpha", "beta"]),
                source: OwnerSource::Remote,
            }
        );
    }

    #[test]
    fn chain_reports_not_found_when_no_link_matches() {
        assert_eq!(resolve_owner_chain(&[], &[]), OwnerResolution::NotFound);
        // Blank entries are not candidates.
        assert_eq!(
            resolve_owner_chain(&owners(&["", "   "]), &[]),
            OwnerResolution::NotFound
        );
    }

    #[test]
    fn failure_messages_name_the_candidates_and_the_fix() {
        let ambiguous = OwnerResolution::Ambiguous {
            candidates: owners(&["alpha", "beta"]),
            source: OwnerSource::Local,
        };
        let msg = describe_owner_resolution_failure("sgit", &ambiguous).expect("message");
        assert!(msg.contains("alpha/sgit"), "{msg}");
        assert!(msg.contains("beta/sgit"), "{msg}");
        assert!(msg.contains("<owner/repo>"), "{msg}");

        let missing =
            describe_owner_resolution_failure("sgit", &OwnerResolution::NotFound).expect("message");
        assert!(missing.contains("sgit"), "{missing}");
        assert!(missing.contains("<owner/repo>"), "{missing}");

        assert_eq!(
            describe_owner_resolution_failure(
                "sgit",
                &OwnerResolution::Resolved {
                    owner: "stokd-cloud".to_string(),
                    source: OwnerSource::Remote,
                }
            ),
            None
        );
    }

    #[test]
    fn local_owners_span_bare_clones_and_worktrees() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare_root = tmp.path().join("dev");
        let worktree_root = tmp.path().join("worktrees");

        // Bare clone under one owner...
        let bare = bare_root.join("alpha").join("widget.git");
        std::fs::create_dir_all(&bare).expect("mkdir bare");
        std::fs::write(bare.join("HEAD"), "ref: refs/heads/main\n").expect("write HEAD");
        // ...and a worktree-only checkout under another.
        std::fs::create_dir_all(worktree_root.join("beta").join("widget").join("main"))
            .expect("mkdir worktree");
        // An unrelated repo must not leak into the candidate set.
        std::fs::create_dir_all(worktree_root.join("gamma").join("other").join("main"))
            .expect("mkdir other");

        let cfg = RepositoriesConfig {
            bare_root: bare_root.to_string_lossy().to_string(),
            worktree_root: worktree_root.to_string_lossy().to_string(),
            main_worktree_name: "{branch}".into(),
            track_non_git_workspaces: false,
            ..Default::default()
        };

        assert_eq!(
            local_owners_for_repo(&cfg, "widget"),
            vec!["alpha".to_string(), "beta".to_string()]
        );
        assert!(local_owners_for_repo(&cfg, "nope").is_empty());
    }
}
