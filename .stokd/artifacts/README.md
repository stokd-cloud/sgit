# Repository hygiene — task d2c0e46

Recovery artifacts for the sgit branch/directory cleanup of 2026-09-04.

## `sgit-deleted-branches.txt`

44 refs deleted from `sgit.git`, recorded one per line as `<sha> <ref-name>`.
34 were auto-generated `stokd-land-backup/*` / `sgit-shove-backup/*` safety nets;
10 were named refs whose patches were fully contained in `origin/main`
(`git cherry origin/main <ref>` reported 0 unique patches).

To restore one, re-create the ref in `/opt/dev/stokd-cloud/sgit.git` from the
SHA recorded on its line — `git update-ref refs/heads/NAME SHA` does it without
tripping the stokd naming gate.

Refs deliberately kept, because they carry work that is NOT in main:

| ref | commit | why |
| --- | --- | --- |
| `feature/worktree-clean-unstarted` | `ec6fce6` | main has no "never started" reap guard at all |
| `infra/sgit-rename-reconciler` | `ef3ab8e` | adds `session_history.rs` (639 lines), absent from main |
| `task/9bb2054-sgit-repo-create-produces-a-hub` | `5a21ff3` | layout/provisioning work only partly landed |
| `task/f741091-port-the-unique-uncommitted-sgit` | `ed93e33` | in-flight; carries the real sgit work |

## `preserve-f687bd2-main-dirty-source.tar.gz`

Source-only snapshot (`target/` excluded) of the orphaned worktree directory
`/opt/worktrees/stokd-cloud/sgit/preserve-f687bd2-main-dirty`, dated 2026-08-10.
Its gitdir pointer targeted a path that no longer exists, so git could not read
it; the directory held 115M, of which all but ~750K was build output.

## What was NOT touched

`mono/project-ba9d8c7-sst-to-terraform-multi-cloud` is unregistered as a git
worktree but has a live agent working in it, so it was left in place.
