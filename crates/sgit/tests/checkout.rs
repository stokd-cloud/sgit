use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sgit_core::RepositoriesConfig;
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    repo: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("primary");
        std::fs::create_dir_all(&repo).expect("create primary worktree");
        git(&repo, &["init", "-q", "-b", "main"]);
        git(
            &repo,
            &["config", "user.email", "checkout-test@example.com"],
        );
        git(&repo, &["config", "user.name", "Checkout Test"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);

        let worktree_root = tmp.path().join("worktrees");
        let bare_root = tmp.path().join("bare");
        // Make repository-name resolution deterministic under the pre-fix
        // behavior: the target is locally ambiguous, so no GitHub lookup runs.
        for owner in ["owner-a", "owner-b"] {
            std::fs::create_dir_all(worktree_root.join(owner).join("testing123"))
                .expect("create ambiguous local repo candidate");
        }

        let config = tmp.path().join("config.yaml");
        let cfg = RepositoriesConfig {
            bare_root: bare_root.to_string_lossy().into_owned(),
            worktree_root: worktree_root.to_string_lossy().into_owned(),
            main_worktree_name: "{branch}".to_string(),
            track_non_git_workspaces: false,
            submodule_checkout: Default::default(),
        };
        std::fs::write(
            &config,
            serde_yaml::to_string(&cfg).expect("serialize config"),
        )
        .expect("write config");

        Self {
            _tmp: tmp,
            repo,
            config,
        }
    }

    fn checkout_from(&self, cwd: &Path, target: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sgit"))
            .args(["checkout", target])
            .current_dir(cwd)
            .env("SGIT_CONFIG", &self.config)
            .output()
            .expect("run sgit checkout")
    }
}

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {} failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stdout_path(output: &Output) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
}

#[test]
fn checkout_inside_repo_treats_target_as_branch_without_repo_resolution() {
    let fixture = Fixture::new();

    let output = fixture.checkout_from(&fixture.repo, "testing123");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "checkout failed:\n{stderr}");
    assert!(
        !stderr.contains("repo resolve failed"),
        "checkout must not attempt repository resolution:\n{stderr}"
    );
    let destination = stdout_path(&output);
    assert!(
        destination.is_dir(),
        "destination must exist: {}",
        destination.display()
    );

    let invoking_branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&fixture.repo)
        .output()
        .expect("read invoking branch");
    assert_eq!(
        String::from_utf8_lossy(&invoking_branch.stdout).trim(),
        "main"
    );
}

#[test]
fn checkout_outside_repo_reports_context_error_and_no_path() {
    let fixture = Fixture::new();
    let outside = fixture._tmp.path().join("outside");
    std::fs::create_dir(&outside).expect("create outside directory");

    let output = fixture.checkout_from(&outside, "testing123");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "checkout unexpectedly succeeded");
    assert!(
        stderr.contains("not inside a git repository"),
        "expected repository-context error, got:\n{stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "failed checkout must not print a path"
    );
}

#[test]
fn checkout_recreates_a_manually_deleted_registered_worktree() {
    let fixture = Fixture::new();
    let first = fixture.checkout_from(&fixture.repo, "testing123");
    assert!(
        first.status.success(),
        "initial checkout failed:\n{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let destination = stdout_path(&first);
    assert!(destination.is_dir(), "initial destination must exist");

    std::fs::remove_dir_all(&destination).expect("remove worktree directory out-of-band");
    assert!(!destination.exists(), "test setup must remove destination");

    let second = fixture.checkout_from(&fixture.repo, "testing123");
    assert!(
        second.status.success(),
        "replacement checkout failed:\n{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(stdout_path(&second), destination);
    assert!(
        destination.is_dir(),
        "checkout must recreate the missing worktree: {}",
        destination.display()
    );
}
