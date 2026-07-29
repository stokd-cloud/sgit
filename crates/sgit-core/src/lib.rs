//! Generic git / repository / worktree primitives for `sgit` and `stokd`.
//!
//! Pure local git layout helpers only — no HTTP client, no cloud SDK, and no
//! orchestration-domain imports. Config discovery is path-based only (D002).

mod cd;
mod checkout;
mod config;
pub mod layout;
pub mod migrate_ops;
mod repo_list;
mod repo_ref;
pub mod shove;
pub mod submodule_checkout;
pub mod worktree_clean;
pub mod worktree_lease;
pub mod worktree_pin;
pub mod workspace;

pub use cd::{
    candidate_leaves_for_ref, first_present, is_task_or_project_ref, leaves_under,
    owners_with_repo, parse_cd_target, resolve_owner_from_candidates, resolve_worktree_path,
    select_worktree_leaf, CdTarget,
};
pub use checkout::{
    branch_worktree_leaf, ensure_branch_worktree, normalize_branch_name,
    preferred_branch_worktree_path, EnsureBranchWorktree,
};
pub use config::{
    load_repositories_config, resolve_config_path, ConfigSource, RepositoriesConfig,
};
pub use submodule_checkout::{
    apply_submodule_checkout, apply_submodule_checkout_for_repo, normalize_repo_key,
    normalize_repo_slug, resolve_submodule_checkout, SubmoduleCheckoutConfig,
    SubmoduleCheckoutMode,
};
pub use layout::{
    bare_clone, bare_clone_from_url, bare_placeholder_branch, create_worktree,
    list_linked_worktrees, main_worktree_leaf, normalize_path, parse_git_remote_url,
    point_bare_head_to_placeholder, render_worktree_name_pattern, resolve_default_branch,
    resolve_origin_url, resolve_repo_layout, run_git_dir, same_path, worktree_dir_for_branch,
    BARE_PLACEHOLDER_HEAD, RepoLayout,
};
pub use migrate_ops::{
    materialize_main_worktree, move_bare, move_worktree, ApplyStatus, UnsafeReason,
    WorktreeRepairTarget,
};
pub use repo_list::{list_bare_repos, BareRepoEntry};
pub use repo_ref::{
    describe_owner_resolution_failure, local_owners_for_repo, parse_repo_spec, resolve_owner_chain,
    OwnerResolution, OwnerSource, RepoSpec,
};
pub use shove::{
    shove, shove_backup_branch_names, CapturedGit, CommitOutcome, ConflictContext, ConflictKind,
    ConflictResolver, PushDecision, ShoveOptions,
};
pub use worktree_clean::{remove_worktree, run_clean_at, CleanSummary};
pub use worktree_lease::{
    find_local_repo_id, git_common_dir, legacy_path_repo_bucket, read_repo_id_file, repo_id_bucket,
    sanitize_repo, stable_repo_bucket,
};
pub use worktree_pin::{
    discover_bare_repos, ensure_pin_and_hook, install_reference_transaction_hook,
    is_linked_worktree, pin_marker_path, reconcile, reference_transaction_script,
    remove_pin_marker, resolve_common_git_dir, write_pin_marker, PIN_HOOKS_VERSION,
    PIN_MARKER_FILE, SGIT_HOOKS_SUBDIR, ReconcileResult,
};
pub use workspace::{
    build_in_progress_at, build_on_land_command, detect_repo_root_at, find_worktree_for_branch_at,
    resolve_default_branch_at, sync_branch, worktree_has_uncommitted_changes_at, worktree_is_clean,
};
