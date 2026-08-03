//! Thin `sgit` binary over `sgit-core`.
//!
//! Surfaces: `cd`, `checkout`, `clone|open|create`, `repo list|rename|migrate`,
//! `worktree clean|pin`, `shove`. `repo graph` stays in stokd. No compile-time
//! stokd dependency for task/project cd (D001 external resolver seam via
//! `SGIT_REF_RESOLVER`).
//!
//! `clone`/`open`/`create` are top-level verbs: `sgit clone <repo>` needs no
//! `repo` group, and a bare repo name resolves to an owner through the chain in
//! `sgit_core::repo_ref` when it is unambiguous. The `sgit repo clone|open|create`
//! spellings remain as hidden back-compat aliases for existing callers.

mod commands;
mod github;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "sgit",
    version,
    about = "Standalone git/repo/worktree CLI (sgit-core)",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Navigate to a worktree directory (prints path; shell integration does the cd)
    Cd {
        /// Repository slug (owner/repo), bare repo name, or task/* / project/* ref
        #[arg(value_name = "TARGET")]
        target: Option<String>,
        /// Optional branch/ref
        #[arg(value_name = "REF")]
        git_ref: Option<String>,
    },
    /// Ensure a sibling worktree for BRANCH and print its path (never switches in place)
    Checkout {
        /// Branch name to check out into a dedicated worktree folder
        #[arg(value_name = "BRANCH")]
        branch: String,
    },
    /// Headlessly provision a repo in the bare + worktree layout (no editor)
    Clone {
        /// `owner/repo-name`, or a bare `repo-name` when it is unambiguous
        #[arg(value_name = "[OWNER/]REPO")]
        repo: String,
        /// Emit the provisioning result as JSON
        #[arg(long)]
        json: bool,
    },
    /// Clone (if needed) and open the main worktree in an editor
    Open {
        /// `owner/repo-name`, or a bare `repo-name` when it is unambiguous
        #[arg(value_name = "[OWNER/]REPO")]
        repo: String,
    },
    /// Create a GitHub repo and local bare/worktree layout
    Create {
        /// `owner/repo-name`, or a bare `repo-name` (created under your account)
        #[arg(value_name = "[OWNER/]REPO")]
        repo: String,
        /// Path to the local source directory
        #[arg(value_name = "PATH")]
        path: Option<String>,
        /// After validation, delete the original source and reopen editors
        #[arg(short = 'f', long)]
        force: bool,
        /// Create a public repository (defaults to private)
        #[arg(long)]
        public: bool,
    },
    /// Repository lifecycle (list, rename, migrate)
    Repo {
        #[command(subcommand)]
        command: RepoCommands,
    },
    /// Worktree management (clean, pin; land/review stay in stokd)
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommands,
    },
    /// Stage/commit/push with conflict resolution
    Shove {
        /// Optional message
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Branch/repo lock inspection and hook plumbing
    Lock {
        #[command(subcommand)]
        command: LockCommands,
    },
}

#[derive(Subcommand, Debug)]
enum LockCommands {
    /// List the effective locks for the current repo
    List {
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Hook plumbing: decide + biometric-gate a ref operation (stdin = hook input)
    #[command(hide = true)]
    Enforce {
        /// Which hook is asking (reference-transaction | pre-push)
        #[arg(value_name = "HOOK")]
        hook: String,
    },
}

#[derive(Subcommand, Debug)]
enum RepoCommands {
    /// List locally bare-cloned repos under the configured bareRoot
    List {
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Deprecated alias for `sgit clone`
    #[command(hide = true)]
    Clone {
        /// `owner/repo-name`, or a bare `repo-name` when it is unambiguous
        #[arg(value_name = "[OWNER/]REPO")]
        repo: String,
        /// Emit the provisioning result as JSON
        #[arg(long)]
        json: bool,
    },
    /// Deprecated alias for `sgit open`
    #[command(hide = true)]
    Open {
        /// `owner/repo-name`, or a bare `repo-name` when it is unambiguous
        #[arg(value_name = "[OWNER/]REPO")]
        repo: String,
    },
    /// Deprecated alias for `sgit create`
    #[command(hide = true)]
    Create {
        /// `owner/repo-name`, or a bare `repo-name` (created under your account)
        #[arg(value_name = "[OWNER/]REPO")]
        repo: String,
        /// Path to the local source directory
        #[arg(value_name = "PATH")]
        path: Option<String>,
        /// After validation, delete the original source and reopen editors
        #[arg(short = 'f', long)]
        force: bool,
        /// Create a public repository (defaults to private)
        #[arg(long)]
        public: bool,
    },
    /// Require a biometric approval to commit/push anywhere in a repo
    Lock {
        /// Remove the lock instead (biometric-gated)
        #[arg(long)]
        off: bool,
        /// Target repo as owner/repo (defaults to the repo you are in)
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Rename on GitHub and relocate local bare + worktrees
    Rename {
        /// Current owner/repo
        #[arg(value_name = "OWNER/REPO")]
        repo: String,
        /// New owner/repo
        #[arg(value_name = "NEWOWNER/NEWREPO")]
        new_repo: String,
    },
    /// Plan or apply migration to the canonical bare/worktree layout
    Migrate {
        /// Restrict to owner or owner/repo
        #[arg(value_name = "OWNER[/REPO]")]
        filter: Option<String>,
        /// Print the migration plan without changes (default)
        #[arg(long)]
        plan: bool,
        /// Apply safe moves non-interactively
        #[arg(long)]
        apply: bool,
        /// Interactive picker
        #[arg(short = 'i', long)]
        interactive: bool,
        /// Days of inactivity before orphan flag (default: 60)
        #[arg(long, value_name = "DAYS", default_value_t = 60)]
        orphan_days: u32,
    },
}

#[derive(Subcommand, Debug)]
enum WorktreeCommands {
    /// Remove landed/merged worktrees (safe git worktree remove)
    Clean {
        /// Report candidates without removing
        #[arg(long)]
        dry_run: bool,
    },
    /// Pin worktree(s) to their branch (install reference-transaction hook + markers)
    Pin {
        /// Reconcile every bare repo under configured bareRoot
        #[arg(long)]
        all: bool,
        /// Remove pin markers instead of installing
        #[arg(long)]
        off: bool,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Require a biometric approval to commit/push to a branch (default: current)
    Lock {
        /// Remove the lock instead (biometric-gated)
        #[arg(long)]
        off: bool,
        /// Target repo as owner/repo (defaults to the repo you are in)
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
        /// Branch to lock (defaults to the current branch; required with --repo)
        #[arg(long, value_name = "BRANCH")]
        branch: Option<String>,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        // Promoted top-level verbs and their `repo *` back-compat aliases share
        // one implementation each.
        Commands::Clone { repo, json }
        | Commands::Repo {
            command: RepoCommands::Clone { repo, json },
        } => commands::repo::run_clone(&repo, json),
        Commands::Open { repo }
        | Commands::Repo {
            command: RepoCommands::Open { repo },
        } => commands::repo::run_open(&repo),
        Commands::Create {
            repo,
            path,
            force,
            public,
        }
        | Commands::Repo {
            command:
                RepoCommands::Create {
                    repo,
                    path,
                    force,
                    public,
                },
        } => commands::repo::run_create(&repo, path.as_deref(), force, public),
        Commands::Repo {
            command: RepoCommands::List { json },
        } => commands::repo::run_list(json),
        Commands::Repo {
            command: RepoCommands::Rename { repo, new_repo },
        } => commands::repo::run_rename(&repo, &new_repo),
        Commands::Repo {
            command:
                RepoCommands::Migrate {
                    filter,
                    plan,
                    apply,
                    interactive,
                    orphan_days,
                },
        } => commands::repo_migrate::run(plan, apply, interactive, orphan_days, filter),
        Commands::Cd { target, git_ref } => commands::cd::run(target, git_ref),
        Commands::Checkout { branch } => commands::checkout::run(&branch),
        Commands::Worktree {
            command: WorktreeCommands::Clean { dry_run },
        } => commands::worktree::run_clean(dry_run),
        Commands::Worktree {
            command: WorktreeCommands::Pin { all, off, json },
        } => commands::worktree::run_pin(all, off, json),
        Commands::Worktree {
            command:
                WorktreeCommands::Lock {
                    off,
                    repo,
                    branch,
                    json,
                },
        } => commands::lock::run_worktree_lock(off, repo, branch, json),
        Commands::Repo {
            command: RepoCommands::Lock { off, repo, json },
        } => commands::lock::run_repo_lock(off, repo, json),
        Commands::Lock {
            command: LockCommands::List { json },
        } => commands::lock::run_list(json),
        Commands::Lock {
            command: LockCommands::Enforce { hook },
        } => commands::lock::run_enforce(&hook),
        Commands::Shove { message } => commands::shove::run(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap_or_else(|e| panic!("parse {args:?} failed: {e}"))
    }

    #[test]
    fn clone_is_available_without_the_repo_group() {
        assert!(matches!(
            parse(&["sgit", "clone", "stokd-cloud/sgit"]).command,
            Commands::Clone { ref repo, json: false } if repo == "stokd-cloud/sgit"
        ));
        // A bare repo name is accepted; the owner is resolved through the chain.
        assert!(matches!(
            parse(&["sgit", "clone", "sgit"]).command,
            Commands::Clone { ref repo, .. } if repo == "sgit"
        ));
        assert!(matches!(
            parse(&["sgit", "clone", "sgit", "--json"]).command,
            Commands::Clone { json: true, .. }
        ));
    }

    #[test]
    fn open_and_create_are_available_without_the_repo_group() {
        assert!(matches!(
            parse(&["sgit", "open", "sgit"]).command,
            Commands::Open { ref repo } if repo == "sgit"
        ));
        assert!(matches!(
            parse(&["sgit", "create", "sgit"]).command,
            Commands::Create { ref repo, path: None, force: false, public: false } if repo == "sgit"
        ));
        assert!(matches!(
            parse(&["sgit", "create", "acme/widget", "./src", "-f", "--public"]).command,
            Commands::Create {
                path: Some(_),
                force: true,
                public: true,
                ..
            }
        ));
    }

    #[test]
    fn repo_group_keeps_the_lifecycle_verbs_as_back_compat_aliases() {
        // Existing callers (`packages/repo-refs`, menubar) still shell out to
        // `sgit repo clone … --json`; the old spelling must keep working.
        assert!(matches!(
            parse(&["sgit", "repo", "clone", "acme/widget", "--json"]).command,
            Commands::Repo {
                command: RepoCommands::Clone { json: true, .. }
            }
        ));
        assert!(matches!(
            parse(&["sgit", "repo", "open", "acme/widget"]).command,
            Commands::Repo {
                command: RepoCommands::Open { .. }
            }
        ));
        assert!(matches!(
            parse(&["sgit", "repo", "create", "acme/widget"]).command,
            Commands::Repo {
                command: RepoCommands::Create { .. }
            }
        ));
        // Verbs that were not promoted stay under `repo` only.
        assert!(matches!(
            parse(&["sgit", "repo", "list"]).command,
            Commands::Repo {
                command: RepoCommands::List { .. }
            }
        ));
        assert!(Cli::try_parse_from(["sgit", "list"]).is_err());
    }

    #[test]
    fn promoted_verbs_are_visible_in_top_level_help() {
        let help = Cli::command().render_help().to_string();
        for verb in ["clone", "open", "create", "checkout"] {
            assert!(help.contains(verb), "top-level help missing '{verb}':\n{help}");
        }
    }

    #[test]
    fn lock_verbs_parse() {
        assert!(matches!(
            parse(&["sgit", "worktree", "lock"]).command,
            Commands::Worktree {
                command: WorktreeCommands::Lock { off: false, repo: None, branch: None, json: false }
            }
        ));
        assert!(matches!(
            parse(&[
                "sgit", "worktree", "lock", "--off", "--repo", "acme/widget", "--branch", "main"
            ])
            .command,
            Commands::Worktree {
                command: WorktreeCommands::Lock { off: true, repo: Some(_), branch: Some(_), .. }
            }
        ));
        assert!(matches!(
            parse(&["sgit", "repo", "lock"]).command,
            Commands::Repo {
                command: RepoCommands::Lock { off: false, repo: None, json: false }
            }
        ));
        assert!(matches!(
            parse(&["sgit", "lock", "list", "--json"]).command,
            Commands::Lock {
                command: LockCommands::List { json: true }
            }
        ));
        assert!(matches!(
            parse(&["sgit", "lock", "enforce", "pre-push"]).command,
            Commands::Lock {
                command: LockCommands::Enforce { ref hook }
            } if hook == "pre-push"
        ));
    }

    #[test]
    fn lock_verbs_visible_in_help() {
        let worktree_help = Cli::command()
            .find_subcommand_mut("worktree")
            .expect("worktree group")
            .render_help()
            .to_string();
        assert!(worktree_help.contains("lock"), "worktree help missing lock:\n{worktree_help}");
        let repo_help = Cli::command()
            .find_subcommand_mut("repo")
            .expect("repo group")
            .render_help()
            .to_string();
        assert!(repo_help.contains("lock"), "repo help missing lock:\n{repo_help}");
    }

    #[test]
    fn checkout_parses_branch_arg() {
        assert!(matches!(
            parse(&["sgit", "checkout", "feature/foo"]).command,
            Commands::Checkout { ref branch } if branch == "feature/foo"
        ));
        assert!(Cli::try_parse_from(["sgit", "checkout"]).is_err());
    }
}
