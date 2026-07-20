//! Thin `sgit` binary over `sgit-core`.
//!
//! Surfaces: `cd`, `repo *`, `worktree clean|pin`, `shove`. `repo graph` stays
//! in stokd. No compile-time stokd dependency for task/project cd (D001 external
//! resolver seam via `SGIT_REF_RESOLVER`).

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
    /// Repository lifecycle (clone, open, list, create, rename, migrate)
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
}

#[derive(Subcommand, Debug)]
enum RepoCommands {
    /// List locally bare-cloned repos under the configured bareRoot
    List {
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Headlessly provision a repo in the bare + worktree layout (no editor)
    Clone {
        /// GitHub owner/repo-name (e.g. "myorg/my-project")
        #[arg(value_name = "OWNER/REPO")]
        repo: String,
        /// Emit the provisioning result as JSON
        #[arg(long)]
        json: bool,
    },
    /// Clone (if needed) and open the main worktree in an editor
    Open {
        /// GitHub owner/repo-name (e.g. "myorg/my-project")
        #[arg(value_name = "OWNER/REPO")]
        repo: String,
    },
    /// Create a GitHub repo and local bare/worktree layout
    Create {
        /// GitHub owner/repo-name (e.g. "myorg/my-project")
        #[arg(value_name = "OWNER/REPO")]
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
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Repo {
            command: RepoCommands::List { json },
        } => commands::repo::run_list(json),
        Commands::Repo {
            command: RepoCommands::Clone { repo, json },
        } => commands::repo::run_clone(&repo, json),
        Commands::Repo {
            command: RepoCommands::Open { repo },
        } => commands::repo::run_open(&repo),
        Commands::Repo {
            command: RepoCommands::Create {
                repo,
                path,
                force,
                public,
            },
        } => commands::repo::run_create(&repo, path.as_deref(), force, public),
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
        Commands::Worktree {
            command: WorktreeCommands::Clean { dry_run },
        } => commands::worktree::run_clean(dry_run),
        Commands::Worktree {
            command: WorktreeCommands::Pin { all, off, json },
        } => commands::worktree::run_pin(all, off, json),
        Commands::Shove { message } => commands::shove::run(message),
    }
}
