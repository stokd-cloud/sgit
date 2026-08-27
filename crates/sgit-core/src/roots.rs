//! Default repository roots (AX-REPO-REPO-ROOTS-PRIVILEGE-FREE-AND-PARITY).
//!
//! The bare-repo root and the worktree root used to be the hardcoded literals
//! `/opt/dev` and `/opt/worktrees`, declared independently by `sgit-core`, the
//! `stokd` CLI, and the API. `/opt` is `root:wheel` on a stock macOS or Linux
//! box and no installer ever provisioned it, so a fresh non-root install died
//! at the first clone with `cannot create bare directory parent: Permission
//! denied` — while a developer machine whose `/opt` subdirectories had been
//! hand-made years earlier looked perfectly healthy.
//!
//! This module is the single Rust source of those defaults. `sgit-core` owns it
//! and the `stokd` CLI consumes it, so the two Rust surfaces cannot drift; the
//! API mirrors the same precedence in TypeScript.
//!
//! Precedence, highest first:
//!
//! 1. An explicit environment override.
//! 2. An existing, writable legacy `/opt` layout — so machines already
//!    provisioned that way keep working and nothing on disk is ever migrated.
//! 3. The home-derived default (`$HOME/dev`, `$HOME/worktrees`; on Windows
//!    `%USERPROFILE%\dev` and `%USERPROFILE%\worktrees`), which the installing
//!    user can always create without privilege escalation.
//! 4. The legacy `/opt` paths, only when no home directory can be resolved at
//!    all, so behavior is never worse than it was before this module existed.

use std::path::{Path, PathBuf};

/// Legacy bare-repo root. Still honored when it is already provisioned.
pub const LEGACY_BARE_ROOT: &str = "/opt/dev";
/// Legacy worktree root. Still honored when it is already provisioned.
pub const LEGACY_WORKTREE_ROOT: &str = "/opt/worktrees";

/// Directory name appended to the home directory for bare repositories.
pub const HOME_BARE_DIRNAME: &str = "dev";
/// Directory name appended to the home directory for checked-out worktrees.
pub const HOME_WORKTREE_DIRNAME: &str = "worktrees";

/// Environment variables that pin the bare root, highest precedence first.
/// `STOKD_BARE_ROOT` is canonical; `STOKD_BARE_REPOS_ROOT` is the spelling the
/// API already shipped and stays accepted so existing deployments keep working.
pub const BARE_ROOT_ENV_VARS: [&str; 2] = ["STOKD_BARE_ROOT", "STOKD_BARE_REPOS_ROOT"];

/// Environment variables that pin the worktree root, highest precedence first.
/// `STOKD_WORKTREE_ROOT` is canonical; `STOKD_WORKTREES_ROOT` (API) and
/// `SGIT_WORKTREE_ROOT` (the shell completion snippet) stay accepted.
pub const WORKTREE_ROOT_ENV_VARS: [&str; 3] = [
    "STOKD_WORKTREE_ROOT",
    "STOKD_WORKTREES_ROOT",
    "SGIT_WORKTREE_ROOT",
];

/// The resolved pair of roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootDefaults {
    pub bare_root: String,
    pub worktree_root: String,
}

/// The probed environment [`resolve_root_defaults`] decides from.
///
/// Kept as plain data with no I/O of its own so the precedence rules are
/// testable without touching the filesystem, the real `$HOME`, or process env
/// — which on macOS and Windows `dirs::home_dir()` ignores anyway.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootEnv {
    /// Explicit bare-root pin from the environment, already trimmed.
    pub bare_override: Option<String>,
    /// Explicit worktree-root pin from the environment, already trimmed.
    pub worktree_override: Option<String>,
    /// True when both legacy `/opt` roots exist and are writable by this user.
    pub legacy_layout_usable: bool,
    /// The user's home directory, when one can be resolved.
    pub home: Option<PathBuf>,
}

/// Resolve the default roots from an already-probed environment.
pub fn resolve_root_defaults(env: &RootEnv) -> RootDefaults {
    RootDefaults {
        bare_root: resolve_one(
            env.bare_override.as_deref(),
            env.home.as_deref(),
            HOME_BARE_DIRNAME,
            LEGACY_BARE_ROOT,
            env.legacy_layout_usable,
        ),
        worktree_root: resolve_one(
            env.worktree_override.as_deref(),
            env.home.as_deref(),
            HOME_WORKTREE_DIRNAME,
            LEGACY_WORKTREE_ROOT,
            env.legacy_layout_usable,
        ),
    }
}

/// One root's precedence chain. Each root resolves independently so pinning
/// only the bare root still gives a sane home-derived worktree root.
fn resolve_one(
    override_value: Option<&str>,
    home: Option<&Path>,
    home_dirname: &str,
    legacy: &str,
    legacy_usable: bool,
) -> String {
    if let Some(value) = override_value.map(str::trim).filter(|v| !v.is_empty()) {
        return value.to_string();
    }
    if legacy_usable {
        return legacy.to_string();
    }
    match home {
        Some(home) => home.join(home_dirname).to_string_lossy().into_owned(),
        // No home at all (a stripped service account, a broken container): keep
        // the historical paths so we are never worse than before.
        None => legacy.to_string(),
    }
}

/// Probe the real environment: env vars, the legacy `/opt` layout, and `$HOME`.
pub fn probe_root_env() -> RootEnv {
    RootEnv {
        bare_override: first_env(&BARE_ROOT_ENV_VARS),
        worktree_override: first_env(&WORKTREE_ROOT_ENV_VARS),
        legacy_layout_usable: legacy_layout_usable(),
        home: dirs::home_dir(),
    }
}

/// First non-empty value among `names`, trimmed.
fn first_env(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

/// True when this machine already carries a usable `/opt` layout. Both roots
/// must be present and writable: adopting half a layout would put bare repos
/// and their worktrees in inconsistent places.
fn legacy_layout_usable() -> bool {
    if cfg!(windows) {
        return false;
    }
    is_usable_root(Path::new(LEGACY_BARE_ROOT)) && is_usable_root(Path::new(LEGACY_WORKTREE_ROOT))
}

/// The default bare-repo root for this machine.
pub fn default_bare_root() -> String {
    resolve_root_defaults(&probe_root_env()).bare_root
}

/// The default worktree root for this machine.
pub fn default_worktree_root() -> String {
    resolve_root_defaults(&probe_root_env()).worktree_root
}

/// True when `dir` exists, is a directory, and this process can create entries
/// inside it. A read-only `/opt/dev` is as useless to us as a missing one.
pub fn is_usable_root(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    can_write_into(dir)
}

/// Write-permission probe that makes no filesystem changes.
///
/// On Unix this asks the kernel (`access(2)` with `W_OK | X_OK`), so ACLs and
/// group membership are honored — a naive owner/mode comparison gets both
/// wrong. `X_OK` matters as much as `W_OK`: creating `<root>/<owner>` requires
/// search permission on `<root>`.
#[cfg(unix)]
fn can_write_into(dir: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Ok(path) = CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a valid NUL-terminated C string that outlives the call.
    unsafe { libc::access(path.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

/// Non-Unix hosts never adopt the legacy `/opt` layout, so directory existence
/// is the only signal this probe is asked for.
#[cfg(not(unix))]
fn can_write_into(_dir: &Path) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with_home(home: &Path) -> RootEnv {
        RootEnv {
            home: Some(home.to_path_buf()),
            ..RootEnv::default()
        }
    }

    #[test]
    fn home_default_is_privilege_free_and_never_under_opt() {
        let home = PathBuf::from("/home/fresh");
        let roots = resolve_root_defaults(&env_with_home(&home));

        assert_eq!(roots.bare_root, home.join("dev").to_string_lossy());
        assert_eq!(roots.worktree_root, home.join("worktrees").to_string_lossy());
        assert!(
            !roots.bare_root.starts_with("/opt"),
            "a fresh install must not default into root-owned /opt: {}",
            roots.bare_root
        );
        assert!(
            !roots.worktree_root.starts_with("/opt"),
            "a fresh install must not default into root-owned /opt: {}",
            roots.worktree_root
        );
    }

    #[test]
    fn existing_legacy_opt_layout_is_preserved() {
        let env = RootEnv {
            legacy_layout_usable: true,
            ..env_with_home(&PathBuf::from("/home/veteran"))
        };
        let roots = resolve_root_defaults(&env);

        assert_eq!(roots.bare_root, LEGACY_BARE_ROOT);
        assert_eq!(roots.worktree_root, LEGACY_WORKTREE_ROOT);
    }

    #[test]
    fn env_override_outranks_legacy_layout_and_home() {
        let env = RootEnv {
            bare_override: Some("/mnt/bare".to_string()),
            worktree_override: Some("/mnt/wt".to_string()),
            legacy_layout_usable: true,
            home: Some(PathBuf::from("/home/fresh")),
        };
        let roots = resolve_root_defaults(&env);

        assert_eq!(roots.bare_root, "/mnt/bare");
        assert_eq!(roots.worktree_root, "/mnt/wt");
    }

    #[test]
    fn each_root_override_is_independent() {
        let env = RootEnv {
            bare_override: Some("/mnt/bare".to_string()),
            ..env_with_home(&PathBuf::from("/home/fresh"))
        };
        let roots = resolve_root_defaults(&env);

        assert_eq!(roots.bare_root, "/mnt/bare");
        assert_eq!(
            roots.worktree_root,
            PathBuf::from("/home/fresh")
                .join("worktrees")
                .to_string_lossy()
        );
    }

    #[test]
    fn falls_back_to_legacy_paths_when_no_home_resolves() {
        let roots = resolve_root_defaults(&RootEnv::default());

        assert_eq!(roots.bare_root, LEGACY_BARE_ROOT);
        assert_eq!(roots.worktree_root, LEGACY_WORKTREE_ROOT);
    }

    #[test]
    fn is_usable_root_requires_an_existing_writable_directory() {
        let tmp = std::env::temp_dir().join(format!("sgit-roots-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        assert!(is_usable_root(&tmp));
        assert!(!is_usable_root(&tmp.join("definitely-absent")));

        let file = tmp.join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        assert!(!is_usable_root(&file), "a regular file is not a usable root");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn repositories_default_is_wired_to_the_resolver() {
        let home = PathBuf::from("/home/fresh");
        let env = env_with_home(&home);
        let cfg = crate::config::RepositoriesConfig::default_with_root_env(&env);

        assert_eq!(cfg.bare_root, home.join("dev").to_string_lossy());
        assert_eq!(cfg.worktree_root, home.join("worktrees").to_string_lossy());
    }
}
