# sgit shove Deterministic Recovery

## 0. Source Context

- **Derived From:** an observed `stokd shove` failure in `stokd-cloud/mono` on branch
  `main`, where a stale `index.lock` left by a killed sibling git process caused shove to
  report a resolved rebase, push non-fast-forward, be rejected, and strand the worktree
  mid-rebase.
- **Feature Name:** sgit shove Deterministic Recovery
- **PRD Owner:** Brian Stoker
- **Last Updated:** 2026-08-11

`sgit shove` stages, commits, and pushes, rebasing onto origin when the branch has
diverged. Content conflicts are delegated through the `ConflictResolver` seam
(`crates/sgit-core/src/shove.rs`), which `sgit` fills with an editor resolver and `stokd`
fills with an agent resolver.

Today the flow conflates three different failure classes into one boolean. `push_with_sync`
treats *any* `git pull --rebase` failure as a content conflict
(`crates/sgit-core/src/shove.rs:192`), and `resolve_rebase_conflicts` returns success when
the unmerged-path set is empty (`crates/sgit-core/src/shove.rs:252-256`). An environment
fault — a stale lock file, a permission error, a network blip — therefore becomes a silent
"conflicts resolved", followed by a push that cannot possibly fast-forward, followed by a
worktree left mid-rebase whose next `shove` fails with a different error entirely.

This project makes every non-semantic failure recover mechanically and makes the
terminal state of a shove binary: pushed, or nothing pushed and the worktree restored.
Semantic conflicts continue to escalate through the existing seam; that escalation path
(the agent resolver) is specified by a separate PRD and is explicitly out of scope here.

Governed by `AX-SGIT-SHOVE-DETERMINISTIC-RECOVERY` in `.axioms.md`.

## 1. Objectives & Constraints

### Objectives

- A shove never leaves a rebase, merge, or cherry-pick in progress on a failure path.
- An environment fault is never misreported as a resolved conflict.
- Stale git lock files are reaped mechanically, with zero model involvement.
- Two shoves in one worktree cannot corrupt each other's index.
- A rebase spanning several conflicting commits completes rather than failing on round two.
- Binary-path conflicts are resolvable, since marker scanning cannot decide them.

### Constraints

- All recovery lives in `sgit-core`, **below** the `ConflictResolver` trait, so `sgit` and
  `stokd` inherit identical behavior from one implementation.
- The `ConflictResolver` trait's existing method contract must not break `stokd`'s
  resolver; any new conflict information is additive on `ConflictContext`.
- Serialization reuses `crates/sgit-core/src/lock.rs`. No second locking scheme.
- Lock reaping must be conservative: a lock with a live holder is a hard failure naming the
  PID, never a forced removal.
- Backup branches (`sgit-shove-backup/<branch>/<stamp>-{local,remote}`) are already created
  before any rebase and must be named in every failure message.
- Decision logic must be pure and unit-testable: inputs in, classification out, with git
  subprocess calls at the edges.

### Scope Inventory

| Surface | File | Change |
|---------|------|--------|
| library | `crates/sgit-core/src/shove.rs` | lock reaping, in-progress detection, `SyncOutcome`, conflict-round loop, binary conflict class |
| library | `crates/sgit-core/src/lock.rs` | repo-scoped shove lease reused for serialization |
| library | `crates/sgit-core/src/lib.rs` | re-export new public types |
| cli | `crates/sgit/src/commands/shove.rs` | editor resolver handles the binary conflict class |

### Non-Goals

- The agent conflict resolver. It plugs into the unchanged seam and ships separately.
- Any change to staging, commit-message generation, or artifact `.gitignore` detection.
- Submodule-aware shove, multi-repo/workspace shove, and LFS-specific remediation.
- Force-push, history rewrite, or any resolution that discards a side without a decision.
- Changing what constitutes a divergence (`is_divergence_error`) or the push decision rule.

## 1.5 Required Toolchain

| Tool | Min Version | Install Command | Verify Command |
|------|-------------|-----------------|----------------|
| Rust toolchain | 1.75 | `rustup toolchain install stable` | `cargo --version` |
| git | 2.39 | `brew install git` | `git --version` |

## 2. Contract

**VAL-SHOVE-LOCK-001** — A stale git lock left by a dead process does not stop a shove.
Surface: library
Needs: none
Behavior: when a git lock file (`index.lock`, `HEAD.lock`, `shallow.lock`) exists under the
  worktree's git dir and no live process holds it, shove removes it, reports the removal,
  and proceeds; when a live process does hold it, shove refuses before mutating anything and
  names the holding PID.
Evidence: `cargo test -p sgit-core shove` covering the pure holder/staleness classifier for
  both a no-holder lock and a held lock, asserting reap-and-proceed versus refuse-with-PID.
Fail: rules out a killed sibling git process permanently blocking every later shove.
Rigor: R2
Why: removing a lock file is destructive if misjudged, so an independent validator must
  confirm the live-holder branch actually refuses.

**VAL-SHOVE-SERIAL-002** — Two shoves in one worktree cannot interleave their git mutations.
Surface: library
Needs: none
Behavior: shove acquires a repo-scoped lease for the duration of its stage/commit/push flow;
  a second concurrent shove on the same worktree waits or exits with a message naming the
  holder rather than running git concurrently.
Evidence: `cargo test -p sgit-core shove` asserting the second acquisition of the shove
  lease on one repo path does not succeed concurrently with the first.
Fail: rules out the concurrent-index corruption that produces the stale lock in the first
  place.
Rigor: R2
Why: this is the root cause of the observed failure, so its evidence must be signed off by
  someone other than the implementer.

**VAL-SHOVE-STATE-003** — Shove never runs on top of an unfinished git operation.
Surface: library
Needs: VAL-SHOVE-LOCK-001
Behavior: when `rebase-merge/`, `rebase-apply/`, `MERGE_HEAD`, or `CHERRY_PICK_HEAD` is
  present on entry, shove classifies the state and either adopts it deliberately or aborts
  it after backups exist, reporting which it chose; it never proceeds silently over it.
Evidence: `cargo test -p sgit-core shove` covering the pure entry-state classifier for each
  of the four markers plus the clean case.
Fail: rules out the sticky second failure where a retry reports "rebase in progress" instead
  of the original cause.
Rigor: R2
Why: aborting an in-progress operation is destructive without the backups, so the ordering
  guarantee needs independent confirmation.

**VAL-SHOVE-SYNC-004** — A failed sync is classified, and an empty conflict set is never success.
Surface: library
Needs: VAL-SHOVE-STATE-003
Behavior: a failed `git pull --rebase` resolves to exactly one of `Completed`, `Conflicts`,
  `Blocked`, or `Fatal`; only `Conflicts` dispatches the `ConflictResolver`, `Blocked`
  triggers bounded remediation and retry, `Fatal` aborts and restores — and a rebase failure
  with zero unmerged paths is never reported as a resolved conflict.
Evidence: `cargo test -p sgit-core shove` asserting the classifier maps a lock-file failure
  to `Blocked`, real unmerged paths to `Conflicts`, a clean rebase to `Completed`, an
  unrecognized fatal to `Fatal`, and that an empty unmerged set with a failed rebase does not
  classify as success.
Fail: rules out the exact silent no-op resolution observed in `stokd-cloud/mono`.
Rigor: R2
Why: this assertion is the defect itself; its evidence must be independently reviewed.

**VAL-SHOVE-ROUNDS-005** — A rebase with several conflicting commits completes.
Surface: library
Needs: VAL-SHOVE-SYNC-004
Behavior: after each resolved conflict round, shove re-checks whether a rebase is still in
  progress and resolves the next round, looping under a bounded round cap until the rebase is
  finished or a round makes no progress.
Evidence: `cargo test -p sgit-core shove` asserting the loop-continuation decision returns
  continue while a rebase directory is present, stop when it is gone, and stop with an error
  when a round resolves nothing.
Rigor: R1
Why: pure control-flow decision fully covered by unit tests; no destructive step of its own.

**VAL-SHOVE-BINARY-006** — Binary conflicts are decidable.
Surface: library
Needs: VAL-SHOVE-SYNC-004
Behavior: an unmerged path detected as binary is surfaced to the resolver as a distinct
  binary conflict carrying an ours/theirs/newest side choice, and is never scanned for
  conflict markers nor reported as "markers still present".
Evidence: `cargo test -p sgit-core shove` asserting a binary unmerged path classifies as the
  binary conflict class and is excluded from marker verification.
Fail: rules out an unresolvable shove in a media-heavy repo where every conflict is a
  `.mp4`.
Rigor: R1
Why: classification and exclusion are fully determined by unit tests over synthetic content.

**VAL-SHOVE-TERMINAL-007** — Every shove ends pushed or cleanly restored.
Surface: cli
Needs: VAL-SHOVE-SYNC-004, VAL-SHOVE-ROUNDS-005
Behavior: `sgit shove` exits either having pushed the branch, or non-zero with nothing
  pushed, no rebase/merge/cherry-pick in progress, and both safety backup branch names plus
  an exact resume command in the error text.
Evidence: `cargo test -p sgit shove` driving a real temporary repo through a blocked sync and
  asserting the process exits non-zero, the worktree reports no in-progress operation, and
  stderr contains both backup branch names.
Fail: rules out the "left mid-rebase, good luck" terminal state.
Rigor: R2
Why: this is the user-visible promise of the whole project and must be validated by someone
  other than the implementer, end to end rather than per-helper.

**VAL-SHOVE-SEAM-008** — The resolver seam still carries the agent implementation unchanged.
Surface: library
Needs: VAL-SHOVE-BINARY-006
Behavior: mechanical recovery is reachable without any resolver decision, and the
  `ConflictResolver` seam continues to receive text and structural conflicts (as classified
  by `UnmergedKind`) plus the new binary class, so a resolver implemented outside this crate
  keeps compiling and keeps being called only for semantic conflicts.
Evidence: `cargo test -p sgit-core shove` asserting a `Blocked` sync outcome completes with a
  resolver that panics if invoked, and that text/structural/binary conflicts do invoke it.
Fail: rules out mechanical faults costing a model call.
Rigor: R1
Why: covered by a unit test using a panicking resolver as the negative oracle.

## 3. Execution Topology

## Phase 1: Deterministic recovery below the resolver seam
**Purpose:** One unattended pass delivering the whole contract. Every step is mechanical git
behavior with unit-testable decisions, so no human decision is pending mid-flight; ordering
is encoded with `**Dependencies:**`.

### 1.1 Stale git lock classification and reaping
**Targets:** VAL-SHOVE-LOCK-001
**Dependencies:** []

**Implementation Details**
- Add a pure classifier to `crates/sgit-core/src/shove.rs`: given a lock path, whether any
  live process holds it, and its age, return `Stale` (reap), `Held { pid }` (refuse), or
  `Absent`.
- Resolve the worktree's real git dir with `git rev-parse --git-dir` so linked worktrees
  under `<bare>/worktrees/<name>/` are handled, not just `<repo>/.git`.
- Probe holders with `lsof` (fall back to `fuser`); an unavailable prober means "cannot prove
  stale", which refuses rather than reaps.
- Reap only in the `Stale` case, printing the removed path. Refuse in `Held` **before** any
  staging, naming the PID.
- Cover `index.lock`, `HEAD.lock`, and `shallow.lock`.

**Acceptance Criteria**
- AC-1.1.a: a lock file with no live holder → classifier returns the reap decision.
- AC-1.1.b: a lock file with a live holder → classifier returns refuse carrying the PID.
- AC-1.1.c: an unavailable holder prober → classifier refuses rather than reaping.
- AC-1.1.d: the git-dir resolution returns the linked-worktree git dir for a worktree of a
  bare repo, not the bare root.

**Acceptance Tests**
- Test-1.1.a maps to AC-1.1.a and AC-1.1.b — table test over the classifier.
- Test-1.1.b maps to AC-1.1.c — prober-unavailable input refuses.
- Test-1.1.c maps to AC-1.1.d — real temporary bare repo plus linked worktree.

**Verification Commands**
```bash
cargo test -p sgit-core shove
```

### 1.2 Repo-scoped shove serialization
**Targets:** VAL-SHOVE-SERIAL-002
**Dependencies:** ["1.1"]

**Implementation Details**
- Acquire a shove lease from `crates/sgit-core/src/lock.rs` keyed on the resolved git dir,
  held across the whole stage/commit/push flow and released on every exit path.
- A contended lease reports the holder rather than running git concurrently.
- Acquire the lease **before** lock reaping so reaping can never race a live sibling shove.

**Acceptance Criteria**
- AC-1.2.a: a second acquisition of the same repo's shove lease while the first is held does
  not succeed.
- AC-1.2.b: the lease is released after a failing shove, so an immediate retry can acquire it.
- AC-1.2.c: lease acquisition happens before any lock reaping or staging.

**Acceptance Tests**
- Test-1.2.a maps to AC-1.2.a — two acquisitions against one temporary repo path.
- Test-1.2.b maps to AC-1.2.b — acquire, fail, re-acquire.
- Test-1.2.c maps to AC-1.2.c — ordering asserted via a recorded step sequence.

**Verification Commands**
```bash
cargo test -p sgit-core shove
cargo test -p sgit-core lock
```

### 1.3 Entry-state detection for unfinished git operations
**Targets:** VAL-SHOVE-STATE-003
**Dependencies:** ["1.1"]

**Implementation Details**
- Add a pure classifier returning `Clean`, `Rebase`, `Merge`, or `CherryPick` from the
  presence of `rebase-merge/`, `rebase-apply/`, `MERGE_HEAD`, `CHERRY_PICK_HEAD` in the git
  dir.
- On a non-`Clean` entry state, create the safety backup branches first, then abort the
  operation and report which state was aborted; adoption is available but never implicit.
- Never proceed into staging while a non-`Clean` state is present.

**Acceptance Criteria**
- AC-1.3.a: each of the four markers classifies to its own state; none classifies as `Clean`.
- AC-1.3.b: an empty git dir classifies as `Clean`.
- AC-1.3.c: backups are created before the abort on a non-`Clean` entry.

**Acceptance Tests**
- Test-1.3.a maps to AC-1.3.a and AC-1.3.b — table test over synthetic git dirs.
- Test-1.3.b maps to AC-1.3.c — recorded step order asserts backup-then-abort.

**Verification Commands**
```bash
cargo test -p sgit-core shove
```

### 1.4 Classified sync outcome replacing the boolean branch
**Targets:** VAL-SHOVE-SYNC-004
**Dependencies:** ["1.3"]

**Implementation Details**
- Introduce `SyncOutcome { Completed, Conflicts(Vec<UnmergedEntry>), Blocked(BlockedReason), Fatal(String) }`
  and classify `git pull --rebase` from its exit status, combined output, and the unmerged
  index — replacing the `.is_err()` branch at `crates/sgit-core/src/shove.rs:192`.
- `BlockedReason` covers at least lock-file, permission, and network failures.
- Only `Conflicts` reaches the resolver. `Blocked` runs bounded remediation (reap, refetch)
  and retries the sync with a hard attempt cap. `Fatal` aborts the rebase and restores.
- Delete the empty-unmerged-set success path in `resolve_rebase_conflicts`
  (`crates/sgit-core/src/shove.rs:252-256`): a failed rebase with no unmerged paths is
  `Blocked` or `Fatal`, never resolved.

**Acceptance Criteria**
- AC-1.4.a: output containing `index.lock: File exists` classifies as `Blocked` with the
  lock-file reason.
- AC-1.4.b: a failed rebase with real unmerged entries classifies as `Conflicts`.
- AC-1.4.c: a successful rebase classifies as `Completed`.
- AC-1.4.d: a failed rebase with an empty unmerged set never classifies as `Completed`.
- AC-1.4.e: an unrecognized failure classifies as `Fatal`.
- AC-1.4.f: `Blocked` retries are capped and the cap is reported when exhausted.

**Acceptance Tests**
- Test-1.4.a maps to AC-1.4.a through AC-1.4.e — table test over the classifier.
- Test-1.4.b maps to AC-1.4.f — remediation loop asserted to stop at the cap.

**Verification Commands**
```bash
cargo test -p sgit-core shove
```

### 1.5 Multi-round conflict resolution loop
**Targets:** VAL-SHOVE-ROUNDS-005
**Dependencies:** ["1.4"]

**Implementation Details**
- Wrap `resolve_rebase_conflicts` in a loop that re-reads the entry state after each
  `git rebase --continue` and resolves the next round while a rebase remains in progress.
- Cap the rounds; a round that resolves no entries stops with an error rather than spinning.
- Keep per-round staging path-scoped (`stage_conflict_resolution`) so unrelated working-tree
  state is never swept in.

**Acceptance Criteria**
- AC-1.5.a: rebase still in progress → the loop decision is continue.
- AC-1.5.b: rebase gone → the loop decision is stop-success.
- AC-1.5.c: a round resolving zero entries → stop-with-error, no further rounds.
- AC-1.5.d: exceeding the round cap → stop-with-error naming the cap.

**Acceptance Tests**
- Test-1.5.a maps to AC-1.5.a and AC-1.5.b — table test over the loop decision.
- Test-1.5.b maps to AC-1.5.c and AC-1.5.d — no-progress and cap-exceeded inputs.

**Verification Commands**
```bash
cargo test -p sgit-core shove
```

### 1.6 Binary conflict class
**Targets:** VAL-SHOVE-BINARY-006
**Dependencies:** ["1.4"]

**Implementation Details**
- Detect binary unmerged paths (NUL byte scan of the stage blobs, honoring git attributes)
  and carry a binary flag plus a side choice on the conflict entry passed to the resolver.
- Exclude binary paths from `verify_conflict_markers_cleared` and from
  `any_conflict_markers_on_disk`; verify instead that the resolver checked out one side.
- Extend `sgit`'s `EditorConflictResolver` to prompt for ours/theirs/newest on binary paths
  instead of opening an editor on binary content.

**Acceptance Criteria**
- AC-1.6.a: a binary unmerged path classifies as the binary conflict class.
- AC-1.6.b: a binary path is excluded from marker verification and cannot produce a "markers
  still present" error.
- AC-1.6.c: a text path is unaffected and still marker-verified.

**Acceptance Tests**
- Test-1.6.a maps to AC-1.6.a and AC-1.6.c — synthetic binary and text blobs.
- Test-1.6.b maps to AC-1.6.b — marker verification skips the binary entry.

**Verification Commands**
```bash
cargo test -p sgit-core shove
cargo test -p sgit shove
```

### 1.7 Terminal-state guarantee and seam preservation
**Targets:** VAL-SHOVE-TERMINAL-007, VAL-SHOVE-SEAM-008
**Dependencies:** ["1.5", "1.6"]

**Implementation Details**
- Funnel every failure exit through one reporter that aborts any in-progress operation,
  confirms the entry-state classifier now reads `Clean`, and formats the error with both
  backup branch names and an exact resume command.
- Extend `format_push_failure` to include the backup branch names it currently omits.
- Assert the seam is untouched for mechanical faults: a `Blocked` sync completes with a
  resolver that panics on invocation.

**Acceptance Criteria**
- AC-1.7.a: a shove that cannot push exits non-zero with nothing pushed.
- AC-1.7.b: after such a failure the entry-state classifier reads `Clean` — no rebase,
  merge, or cherry-pick in progress.
- AC-1.7.c: the failure message contains both backup branch names and a resume command.
- AC-1.7.d: a `Blocked` sync never invokes the resolver, proven by a panicking resolver.
- AC-1.7.e: text, structural, and binary conflicts all do invoke the resolver.

**Acceptance Tests**
- Test-1.7.a maps to AC-1.7.a through AC-1.7.c — end-to-end run over a temporary repo with a
  diverged origin and a blocked sync.
- Test-1.7.b maps to AC-1.7.d and AC-1.7.e — panicking-resolver negative oracle plus positive
  dispatch cases.

**Verification Commands**
```bash
cargo test -p sgit-core shove
cargo test -p sgit shove
cargo test --workspace
```

## 4. Completion Criteria

- Every assertion in `## 2. Contract` has a passing acceptance test, each observed failing
  before its implementation and passing after (axiom 5.1).
- `cargo test --workspace` passes.
- `cargo clippy --workspace --all-targets -- -D warnings` passes.
- The three `AX-SGIT-SHOVE-DETERMINISTIC-RECOVERY` acceptance checks in `.axioms.md` pass:
  `cargo test -p sgit-core shove`, `cargo test -p sgit-core lock`, `cargo test -p sgit shove`.
- `push_with_sync` contains no boolean `.is_err()` branch on the rebase result, and
  `resolve_rebase_conflicts` contains no empty-unmerged success path.
- No public `ConflictResolver` method signature changed in a way that breaks an out-of-crate
  resolver.

## 5. Rollout & Validation

### Rollout Strategy

- Land as one change to `sgit-core` plus the `sgit` resolver update. There is no feature
  flag: the old behavior is a defect, not a mode.
- `stokd shove` picks the fix up when it rebuilds against the new `sgit-core`; no stokd-side
  change is required for the mechanical path.
- Exercise the real regression first: a worktree with a stale `index.lock` and a diverged
  origin must shove to completion.

### Post-Launch Validation

- Re-run the original scenario in `stokd-cloud/mono` on a diverged branch with a planted
  stale lock; confirm the push completes.
- Kill a shove mid-rebase, then shove again; confirm the second run reports the aborted
  state and completes rather than reporting "rebase in progress".
- Confirm no `sgit-shove-backup/*` branch is ever the only surviving copy of work: both sides
  are named in every failure message.

## 6. Open Questions

- Should a `Blocked` shove retry automatically on network failures, or only on local faults?
  Current plan retries both under one cap; splitting the caps is a later refinement.
- Should the binary conflict default be `newest` (mtime) when the resolver declines to
  choose, or should declining always be a hard failure? Current plan: hard failure.
- Should adoption of a pre-existing rebase ever be automatic when its todo list matches this
  shove's own commits? Current plan: always abort, never adopt implicitly.
