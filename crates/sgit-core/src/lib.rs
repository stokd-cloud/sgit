//! Generic git / repository / worktree primitives for `sgit` and `stokd`.
//!
//! Pure local git layout helpers only — no HTTP client, no cloud SDK, and no
//! orchestration-domain imports. Config discovery is path-based only (D002).

pub mod biometric;
mod cd;
mod checkout;
mod config;
pub mod layout;
pub mod lock;
pub mod migrate_ops;
pub mod pull;
mod repo_list;
mod repo_ref;
pub mod roots;
pub mod shove;
pub mod submodule_checkout;
pub mod worktree_clean;
pub mod worktree_lease;
pub mod worktree_pin;
pub mod workspace;

pub use cd::{
    candidate_leaves_for_ref, first_present, is_task_or_project_ref, leaves_under,
    owners_with_repo, parse_cd_target, resolve_owner_from_candidates, resolve_worktree_path,
    select_worktree_leaf, unique_prefix_leaf, CdTarget,
};
pub use checkout::{
    branch_worktree_leaf, classify_checkout_target, classify_checkout_target_with_cfg,
    ensure_branch_worktree, ensure_repo_main_worktree, ensure_repo_main_worktree_from_url,
    normalize_branch_name, preferred_branch_worktree_path, CheckoutKind, EnsureBranchWorktree,
    EnsureRepoWorktree,
};
pub use config::{
    load_repositories_config, resolve_config_path, ConfigSource, RepositoriesConfig,
};
pub use roots::{
    default_bare_root, default_worktree_root, is_usable_root, probe_root_env,
    resolve_root_defaults, RootDefaults, RootEnv, HOME_BARE_DIRNAME, HOME_WORKTREE_DIRNAME,
    LEGACY_BARE_ROOT, LEGACY_WORKTREE_ROOT,
};
pub use submodule_checkout::{
    apply_submodule_checkout, apply_submodule_checkout_for_repo, normalize_repo_key,
    normalize_repo_slug, resolve_child_submodule_mode, resolve_submodule_checkout,
    ChildModeSpec, RepoSubmodulesFile, SubmoduleCheckoutConfig, SubmoduleCheckoutMode,
    REPO_SUBMODULES_REL,
};
pub use layout::{
    apply_sgit_push_defaults, bare_clone, bare_clone_from_url, bare_placeholder_branch,
    create_worktree, is_valid_linked_worktree, list_linked_worktrees, main_worktree_leaf,
    normalize_path, parse_git_remote_url, point_bare_head_to_placeholder,
    render_worktree_name_pattern, resolve_default_branch, resolve_origin_url, resolve_repo_layout,
    run_git_dir, same_path, worktree_dir_for_branch, BARE_PLACEHOLDER_HEAD, RepoLayout,
};
pub use migrate_ops::{
    materialize_main_worktree, move_bare, move_linked_worktree_preserving_state, move_worktree,
    ApplyStatus, LinkedWorktreeMove, UnsafeReason, WorktreeRepairTarget,
};
pub use repo_list::{list_bare_repos, BareRepoEntry};
pub use repo_ref::{
    describe_owner_resolution_failure, local_owners_for_repo, parse_repo_spec, resolve_owner_chain,
    OwnerResolution, OwnerSource, RepoSpec,
};
pub use pull::{
    classify_pull_strategy, dirty_tracked_paths, pull, PullOptions, PullOutcome, PullStrategy,
    PULL_BACKUP_PREFIX,
};
pub use shove::{
    backup_branch_names, create_backup_branches, prepare_artifact_exclusions, shove,
    shove_backup_branch_names, ArtifactExclusion, CapturedGit, CommitOutcome, ConflictContext,
    ConflictKind, ConflictResolver, PushDecision, ShoveOptions, SHOVE_BACKUP_PREFIX,
};
pub use biometric::require_biometric;
pub use lock::{
    add_lock, default_registry_path, effective_locks, gated_refs, gated_refs_for_hook,
    install_lock_hooks, install_pre_push_hook, pre_push_lock_fragment, read_registry,
    read_repo_locks, refs_from_pre_push, refs_from_reference_transaction, registry_key,
    remove_lock, standalone_pre_push_script, write_repo_locks, LockHook, LockSet,
    LOCK_WILDCARD, REGISTRY_RELATIVE_PATH, REPO_LOCKS_FILE,
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
