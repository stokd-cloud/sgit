# Agentic Git Behaviors for sgit

## 0. Source Context

**Derived From:** operator request ("optional agentic behaviors for sgit; providers and models
config must be strictly the same config as stokd, derived from the same code; add a shared crate
if needed") plus the scenario catalogue in `./agent-advantage.txt` (238 lines, 18 named scenarios).
**Feature Name:** Agentic Git Behaviors for sgit
**PRD Owner:** Brian Stoker
**Last Updated:** 2026-08-11

### Summary

`sgit` today is a fixed-verb worktree/repo CLI with no model access. This project gives it an
optional agentic layer for the git operations that fail for semantic rather than textual reasons —
merge, rebase, cherry-pick, bisect, and pre-push hygiene — and grounds that layer in a **new shared
crate, `agent-core`**, which becomes the single definition of provider configuration, model
configuration, workload routing, and headless one-shot LLM invocation for **both** `sgit` and
`stokd`.

The governing economic rule is that **the model is the last resort, never the first**. Every
augmented verb runs a deterministic ladder — preflight prediction, plain git, mechanical
resolution, deterministic triage scoring — and only then spends a model call, at a tier matched to
the measured difficulty of what actually remains. When no provider is configured or available, or
when the agentic layer is off, every augmented verb degrades to plain `git` with identical argv,
streams, and exit code, and makes zero model calls.

### Charter

**Problem.** Git's merge machinery is a line-based textual diff engine with no model of syntax,
scope, or intent. Two classes of failure follow. First, *false conflicts*: git stops on changes
that do not actually clash (both branches appended an import, both appended a method before the
same closing brace, one branch reformatted while the other changed logic). Second, *silent
integration breaks*: git merges cleanly and the build is broken (a signature gained a parameter on
one branch while the other added call sites, a payload field was renamed while new fixtures were
written against the old shape, two branches introduced the same symbol in overlapping scopes).
The first class wastes developer time on conflicts a machine could settle. The second class is
worse, because nothing reports it until something downstream fails.

**Why now, and why in `sgit`.** `sgit-core` already carries the exact seam this needs:
`ConflictResolver` (`crates/sgit-core/src/shove.rs:40`) is a trait whose documented contract is
"agent dispatch in stokd, shell/`$EDITOR` in sgit". stokd already ships a mature agent-backed
implementation of it (`apps/cli/src/commands/shove.rs:60`). sgit ships an `$EDITOR` stub
(`crates/sgit/src/commands/shove.rs:18`). The capability gap between the two binaries is not
architectural; it is that sgit has no way to reach a model. Closing that gap is a config-and-
invocation problem, not a git problem.

**Why a shared crate, and in which direction.** `stokd` already depends on `sgit-core` by path
(`apps/cli/Cargo.toml:78`, `sgit-core = { path = "../sgit/crates/sgit-core" }`) because `apps/sgit`
is a git submodule of the mono (`.gitmodules`, `url = git@github.com:stokd-cloud/sgit.git`). A new
crate placed at `crates/agent-core` in the sgit repo is therefore consumable by stokd through the
mechanism already in production, with no new submodule, no publish step, and no vendoring. The
operator has confirmed this direction. The cost is accepted and stated as a constraint: the
provider/model configuration code becomes public, and stokd becomes a *consumer* of configuration
it currently owns.

**Why sharing a crate does not leak sgit's CLI into stokd.** `sgit-core` has no `clap` dependency
and defines no `Parser`/`Subcommand`; the entire command tree lives in the binary crate
`crates/sgit/src/main.rs`, which stokd does not depend on. `agent-core` is held to the same
discipline. New verbs are registered only in `crates/sgit/src/main.rs`.

**What success looks like.** A developer on a branch 180 commits behind runs `sgit merge main`,
and the false conflicts settle mechanically at zero cost, the genuine ones are resolved at a tier
proportional to their blast radius, the build is verified before anything is committed, and no
side's work is silently dropped. The same developer with no providers configured runs the same
command and gets `git merge main`, byte for byte.

### Investigation Summary

Six read-only lenses were run against `/opt/worktrees/stokd-cloud/sgit/main` (the sgit repo) and
`/opt/worktrees/stokd-cloud/mono/main` (the stokd monorepo). Findings that shaped this document:

1. **sgit CLI shape.** `crates/sgit/src/main.rs` defines a closed `clap` enum with nine verbs
   (`cd`, `checkout`, `clone`, `open`, `create`, `repo`, `worktree`, `shove`, `lock`). There is no
   `allow_external_subcommands`, no git fallthrough, and **no git-shaped verb whatsoever**. Adding
   git-shaped verbs is purely additive. Critically, three existing verbs (`checkout`, `clone`,
   `create`) have *sgit* meanings that differ from git's — `sgit checkout` ensures a sibling
   worktree and prints its path (axiom `AX-CLI-CHECKOUT-REPO-TARGET`). This forecloses making
   `sgit` a drop-in `git` alias and is recorded as a non-goal.

2. **The resolver seam already exists and is well-specified.** `sgit-core::shove` exports
   `ConflictResolver`, `ConflictContext`, `ConflictKind`, plus a rich deterministic layer that this
   project consumes rather than rebuilds: `parse_unmerged_entries` classifies index stages into
   `UnmergedKind::{BothModified, DeletedByThem, DeletedByUs, BothAdded, AddedByUs, AddedByThem}`
   (`shove.rs:333`), `conflict_round_needs_resolver` already distinguishes marker-less structural
   conflicts from ordinary ones (`shove.rs:386`), and `verify_conflict_markers_cleared` /
   `verify_conflict_staged` already gate the resolver's output (`shove.rs:432`, `shove.rs:474`).

3. **stokd's agent resolver is the reference implementation.**
   `apps/cli/src/commands/shove.rs:60` builds a prompt from the diff (truncated to 10,000 chars),
   dispatches through `AgentDiscovery`, rotates providers on failure, records cooldowns, and — the
   detail worth copying — re-fails a provider that exits zero while leaving markers on disk.

4. **Configuration is already one file, and sgit already reads it.** `sgit-core::config`
   (`crates/sgit-core/src/config.rs:1`) documents discovery precedence `SGIT_CONFIG` → XDG →
   `~/.stokd/config.yaml`, and explicitly notes "There is no code dependency on stokd; the fallback
   is only a filesystem path." The live file carries `providers:` (a polymorphic list of bare names
   and `{name, endpoint, port, apiKey}` objects) and `models:` (`mode`, `pareto`, `defaults`,
   `workloads`, `topicShiftInterval`, `providerSessionRenameSync`). `agent-core` extends the same
   precedence to the same file for the `providers:`/`models:`/`agentic:` blocks.

5. **The routing model already has the exact concepts needed.** `apps/cli/src/llm_routing.rs`
   defines `TaskClass` with canonical workload slugs (`llm_routing.rs:291`), `ModelTier::{Strong,
   Economy}` with `ECONOMY_FLOOR = 70`, `expand_workload_model_chain` for the `default` sentinel
   (`llm_routing.rs:53`), `resolved_workload_models` (`llm_routing.rs:81`), and — decisive for the
   passthrough gate — `is_workload_configured` (`llm_routing.rs:1321`) and
   `mode_provider_list`, which is documented to "never invent a Claude default … returns an empty
   vec so callers fail closed" (`llm_routing.rs:574`). Fail-closed-when-unconfigured is existing,
   intended behavior, not something this project introduces. The entanglement to break is that
   these functions take `&StokdConfig`; `agent-core` takes only the provider/model slice.

6. **A headless one-shot call path already exists and is small.**
   `governance_judge::invoke_provider_headless` (`apps/cli/src/governance_judge.rs:1077`) spawns a
   provider, feeds the prompt by argv or stdin per `CapturedPromptTransport`, enforces a deadline,
   and classifies usage-limit failures via `provider_cooldown::is_usage_limit_error`. Its
   per-provider argv construction is `build_judge_command` (`governance_judge.rs:975`), about 100
   lines covering claude/codex/gemini plus a generic `AgentFlags { raw_prompt: true, .. }` fallback.
   This — not the 5,087-line `agent.rs` interactive/PTY surface — is the correct extraction target.

7. **Prior art for triage is already written down and must not be forked.** The installed
   `conflict-resolution` skill (`~/.claude/skills/conflict-resolution/`) carries a five-phase
   protocol, a deterministic scorer (`scripts/conflict-triage.sh`, 274 lines), and published
   scoring tables (`references/triage.md`): `file_score = criticality×2 + both_sides×3 + one_sided
   + structural_penalty`; tiers T0 (score 0) / T1 (<7, `worker`) / T2 (7–24, `codeReview`) /
   T3 (≥25 or any hard escalator, `escalation`); mandatory `merge.conflictStyle=zdiff3` so the
   merge base is visible; `git merge-tree --write-tree` for non-mutating prediction; and the
   both-sides-survival gate. This project **ports that scorer into Rust and makes the skill call
   it**, so there is one scorer, not two.

8. **In-flight prerequisite.** A separate agent is, concurrently and in its own worktree,
   redesigning `shove` to always terminate cleanly: stale `index.lock` reaping, taking the sgit
   repo lock around the whole operation, adopt-or-abort of in-progress rebase/merge state,
   replacing the `.is_err()` boolean with a `SyncOutcome { Completed | Conflicts(entries) |
   Blocked(reason) | Fatal }`, looping conflict rounds until `rebase-merge/` is gone, an explicit
   conflict class for binaries (where `contains_conflict_markers` is meaningless), and a terminal
   guarantee on every exit path. Its point 5 — "escalate only what's genuinely semantic … everything
   above it is mechanical and shouldn't cost a model call" — is exactly this project's entry point.
   The operator has directed that **that work lands before this project is implemented**. This PRD
   therefore *consumes* `SyncOutcome`, the multi-round loop, and the binary conflict class, and
   defines none of them.

9. **Toolchain and baseline, measured.** `cargo 1.93.0`, `rustc 1.93.0`, `clippy 0.1.93`,
   `rustfmt 1.8.0`, `git 2.50.1`. sgit CI (`.github/workflows/ci.yml`) runs
   `cargo build -p sgit -p sgit-core` then `cargo test --workspace` under `RUSTFLAGS: -D warnings`;
   both crates also set `[lints.rust] warnings = "deny"` in-manifest. Clippy is **not** in sgit CI
   today, and — measured on 2026-08-11 —
   `cargo clippy --workspace --all-targets -- -D warnings` **currently fails** with three
   pre-existing errors: a collapsible `if` at `crates/sgit-core/src/checkout.rs:113`, elidable
   explicit lifetimes at `crates/sgit-core/src/workspace.rs:233`, and a derivable `impl` at
   `crates/sgit-core/src/submodule_checkout.rs:92`. Every clippy gate in this document therefore
   depends on work item 1.1 clearing those three first; without that, the gates would fail for
   reasons unrelated to this project. All tests are inline `#[cfg(test)]`; there are no `tests/`
   directories. Measured baseline
   on 2026-08-11: `cargo test --workspace` → **66 passed** (`sgit`) + **105 passed** (`sgit-core`),
   0 failed. stokd's `apps/cli` is a standalone crate (own `Cargo.lock`, not a workspace member)
   built with `cargo build --manifest-path Cargo.toml`, tested with
   `cargo test --manifest-path Cargo.toml`, linted with
   `cargo clippy --manifest-path Cargo.toml -- -D warnings` (`apps/cli/package.json`).

**Governance record (axiom 5.3).** The authoring task for this document was REJECTED once, with:
"Add a required `[new] AX-*` entry in Axiom Changes covering the new `agent-core` shared-agentic
behavior surface (with Why, How to apply, and runnable Acceptance Checks), or explicitly scope this
as non-durable and remove the unrelated `AX-CLI-CHECKOUT-REPO-TARGET` citation." The revision took
the second option. The consequence is carried forward here as a deliverable: work item 2.7 authors
the `[new] AX-*` entries this rejection identified.

---

## 1. Objectives & Constraints

### Objectives

- Make provider configuration, model configuration, and workload routing **one implementation**
  shared by `sgit` and `stokd`, so the two binaries cannot drift.
- Give `sgit` optional agentic behavior on the git operations where semantic context converts a
  stall into a resolution, covering as much of `agent-advantage.txt` as is safely automatable.
- Spend zero tokens on operations that succeed without help, and spend model capability in
  proportion to measured difficulty on operations that do not.
- Preserve every side's work unconditionally. A merge that loses a feature is a worse outcome than
  a merge that fails loudly.
- Degrade to plain `git` — identically, and observably — whenever the agentic layer is off,
  unconfigured, or unavailable.

### Constraints

- **Prerequisite, hard.** The in-flight "shove always terminates cleanly" work (stale-lock reaping,
  repo lock, adopt-or-abort, `SyncOutcome`, multi-round conflict looping, binary conflict class,
  terminal guarantee) MUST land on `main` of the sgit repo before Phase 1 work item 1.9 begins.
  This project consumes those seams and redefines none of them.
- `agent-core` lives at `crates/agent-core` in the **public** `stokd-cloud/sgit` repository.
  Provider/model configuration code becomes public. No secret, token, endpoint credential, or
  customer identifier may be embedded in it.
- `agent-core` is a **library**: no `clap`, no `Parser`/`Subcommand`, no CLI surface — the same
  discipline `sgit-core` holds — so no sgit verb can leak into `stokd` by dependency.
- `agent-core` MUST NOT depend on `sgit-core`, and MUST NOT import any stokd orchestration, API,
  database, telemetry, or governance module.
- `stokd`'s observable behavior must be unchanged by the extraction. Config files that load today
  must load identically; config files written today must round-trip byte-identically.
- Both crates keep `[lints.rust] warnings = "deny"`; sgit CI keeps `RUSTFLAGS: -D warnings`. Clippy
  is adopted as a gate by this project, which requires clearing the three pre-existing `sgit-core`
  clippy errors in work item 1.1 before any later clippy verification command can pass.
- The mono consumes `agent-core` through the existing submodule path dependency. Every change
  requires advancing the `apps/sgit` gitlink and `apps/cli/Cargo.lock`.
- No new *required* configuration key. An operator with today's `~/.stokd/config.yaml` and no edits
  must get working behavior or clean passthrough, never an error.
- The forbidden git operations from the repo's global rules are never issued by any code path:
  `git stash`, `git reset --hard`, `git checkout -- .`, `git restore .`, `git clean -f`, branch
  switching inside a worktree, `-X ours` / `-X theirs` at merge level, `--no-verify` on push.
- Network egress happens only through the operator's already-configured providers. This project
  adds no new remote endpoint.

### Scope Inventory

| # | Surface | In scope |
|---|---------|----------|
| S1 | `crates/agent-core` (new): provider config types, model config types, workload policy | yes |
| S2 | `crates/agent-core`: workload → model-chain → provider-order routing | yes |
| S3 | `crates/agent-core`: provider availability, cooldown ledger, headless one-shot invocation | yes |
| S4 | `crates/agent-core`: `agentic:` config block and the capability gate | yes |
| S5 | `crates/sgit-core`: deterministic conflict triage scorer; mechanical resolution classes | yes |
| S6 | `crates/sgit/src/main.rs`: new git-shaped verbs + unknown-verb git passthrough | yes |
| S7 | `sgit merge` — preflight, ladder, resolution, survival gate, terminal guarantee | yes |
| S8 | `sgit verify` — post-operation compile/test/lint remediation loop | yes |
| S9 | `sgit rebase` / `sgit pull` — multi-round, migration renumbering, symbol relocation | yes |
| S10 | `sgit cherry-pick` — semantic backport, draft-PR output | yes |
| S11 | `sgit bisect` — repro-script authoring, bisect loop, culprit report | yes |
| S12 | `sgit commit` / `sgit push` — message generation, pre-flight sanitization proposals | yes |
| S13 | `apps/cli` (mono): repoint config/routing/invocation at `agent-core`; parity harness | yes |
| S14 | `~/.claude/skills/conflict-resolution`: delegate scoring to `sgit conflict triage` | yes |
| S15 | Axioms + README documenting the ladder and the passthrough contract | yes |
| S16 | Fixture corpus reproducing every `agent-advantage.txt` scenario | yes |

### Non-Goals

- **`sgit` does not become a drop-in `git` replacement.** `sgit checkout`, `sgit clone`, and
  `sgit create` keep their existing worktree/repo semantics per `AX-CLI-CHECKOUT-REPO-TARGET`, which
  differ from git's verbs of the same name. Aliasing `git=sgit` is explicitly unsupported, and the
  passthrough layer must not imply otherwise.
- Redesigning `shove`'s mechanical termination. That is the in-flight prerequisite work, consumed
  here, not restated.
- Extracting stokd's interactive/PTY agent surface (`AgentBackend::build_interactive_command`,
  `LaunchSpec`, `runtime::captured_session`). Only the headless one-shot path moves.
- Extracting stokd's prompt assembly (axioms, RAG, governance context). `agent-core` calls are
  raw-prompt calls; nothing splices stokd context into them.
- GitHub Actions / "markdown-driven repository automation" (`agent-advantage.txt` §E.2). That is a
  CI-platform concern, not a git-client concern, and is out of scope with no replacement planned.
- Implementing AST or LSP analysis. Difficulty is scored from git plumbing and the merge base, per
  the CONGRA finding that structural class dominates; program analysis is not portable across a
  polyglot tree.
- Automatic history rewriting. Commit squashing and sanitization are emitted as *proposals* only.
- Any change to stokd's own agent behavior, prompts, or orchestration.

---

## 1.5 Required Toolchain

| Tool | Min Version | Install Command | Verify Command |
|------|-------------|-----------------|----------------|
| Rust (cargo + rustc) | 1.93.0 | `rustup toolchain install stable` | `cargo --version && rustc --version` |
| clippy | 0.1.93 | `rustup component add clippy` | `cargo clippy --version` |
| rustfmt | 1.8.0 | `rustup component add rustfmt` | `cargo fmt --version` |
| git | 2.50.1 | `xcode-select --install` | `git --version` |
| stokd CLI | 0.2.2 | already installed at `~/.local/bin/stokd` | `stokd --version` |

`git merge-tree --write-tree` requires git ≥ 2.38; the pinned 2.50.1 satisfies it.

---

## 2. Contract

**VAL-CRATE-001** — The provider and model configuration has exactly one definition.
Surface: library
Needs: none
Behavior: a crate `agent-core` exists at `crates/agent-core` in the sgit workspace and is the sole
  definition of `ProviderEntry`, `ProvidersConfig`, `ModelsConfig`, `WorkloadPolicy`, and
  `ModePool`; `apps/cli` in the stokd mono depends on it by path and re-exports those types rather
  than declaring its own.
Evidence: `crates/agent-core/Cargo.toml` exists and is a workspace member; a repo-wide search of
  `apps/cli/src` finds zero `struct ProvidersConfig` / `struct ModelsConfig` / `enum ProviderEntry`
  / `struct WorkloadPolicy` declarations, and `apps/cli/Cargo.toml` contains
  `agent-core = { path = "../sgit/crates/agent-core" }`.
Rigor: R2
Why: a second declaration surviving anywhere silently reintroduces the drift this project exists to
  remove, and only an independent reader checking both trees can confirm its absence.
Fail: stokd and sgit each keep a private copy of the config types and diverge on the next edit.
Oracle: the two source trees themselves — zero duplicate type declarations across
  `crates/agent-core/src` and `apps/cli/src`.

**VAL-CRATE-002** — The shared crate stays consumable by both binaries.
Surface: library
Needs: VAL-CRATE-001
Behavior: `agent-core` declares no dependency on `clap`, `sgit-core`, any HTTP/cloud SDK beyond the
  one already used for provider calls, or any stokd orchestration/API/database/telemetry module,
  and defines no `Parser` or `Subcommand`.
Evidence: `crates/agent-core/Cargo.toml` dependency list, plus a search of
  `crates/agent-core/src` for `clap`, `Subcommand`, `Parser`, and `sgit_core`, all returning zero
  hits.
Rigor: R1
Why: fully decided by reading one manifest and one grep; no judgment is involved.
Fail: `agent-core` acquires a CLI or a stokd-only dependency and stops being linkable from `sgit`.
Oracle: `crates/sgit-core/Cargo.toml`, which holds the identical discipline today and is the
  precedent being matched.

**VAL-CRATE-003** — Extracting the configuration does not change how stokd behaves.
Surface: parity
Needs: VAL-CRATE-001, VAL-ROUTE-001, VAL-CALL-001
Behavior: after `apps/cli` is repointed at `agent-core`, stokd's configuration loading, workload
  routing, provider selection, and judge invocation produce results identical to the pre-extraction
  build for the same inputs.
Evidence: two independent lanes must agree. Lane A — `cargo test --manifest-path Cargo.toml` in
  `apps/cli` reports zero failures and a passing count at or above the recorded pre-extraction
  baseline. Lane B — a differential harness loads a fixed corpus of at least 12 real and synthetic
  config documents (including the operator's live `~/.stokd/config.yaml`) through the pre- and
  post-extraction code paths and asserts byte-identical parse results, byte-identical
  re-serialization, and identical `resolved_workload_models` output for all 13 `TaskClass` values.
Rigor: R5
Why: this is the single change that can break a production CLI without any user-visible signal at
  the moment of breakage; a compiling refactor proves nothing about serde behavior, so an external
  adjudicator and two lanes are the minimum honest bar.
Fail: an operator's config silently parses differently, routing changes underneath them, and the
  first symptom is a wrong model on an unrelated task days later.
Oracle: the pre-extraction `stokd` binary, built from the parent commit and retained for the
  duration of the project, run against the same 12-document corpus.

**VAL-CFG-001** — Both binaries read the same configuration file the same way.
Surface: parity
Needs: VAL-CRATE-001
Behavior: `sgit` and `stokd` resolve the provider/model configuration through one discovery
  precedence — `SGIT_CONFIG`, then `$XDG_CONFIG_HOME/sgit/config.yaml`, then
  `~/.stokd/config.yaml` — and for any single file produce identical `ProvidersConfig` and
  `ModelsConfig` values.
Evidence: a test fixture set with a document present at each precedence level; both binaries expose
  a hidden `--dump-resolved-config` flag emitting the resolved provider/model values and the
  selected source path as JSON; the two dumps are compared and must be equal for every fixture, and
  the selected source path must match the documented precedence in each case.
Rigor: R4
Why: identical config is the operator-visible promise of this whole project, and one binary reading
  a different file than the other is precisely the failure the shared crate is meant to make
  impossible — a single lane checking only one binary cannot detect it.
Fail: sgit routes on stale or different configuration than stokd and the operator has no way to
  tell which file either one used.
Oracle: `crates/sgit-core/src/config.rs:1-11`, whose documented D002 precedence is the
  specification `agent-core` must match exactly.

**VAL-CFG-002** — Every configuration form that loads today still loads.
Surface: library
Needs: VAL-CRATE-001
Behavior: `agent-core` accepts, without error, bare-string provider entries, object provider
  entries with `{name, endpoint, port, apiKey}`, legacy single-key-map provider entries, legacy
  `providers.entries` nesting, bare-sequence workload chains, object-form workload policies, the
  polymorphic title-object workload form, and the `default` sentinel inside any chain.
Evidence: a table-driven test with one case per accepted form asserting the parsed value; plus a
  parse of the operator's live `~/.stokd/config.yaml` asserting nine providers and four configured
  workloads.
Rigor: R2
Why: these forms exist in live operator files, so a regression is an immediate hard failure for a
  real user, and coverage of "every accepted form" needs a reader other than the implementer to
  confirm nothing was quietly dropped.
Fail: an operator upgrades and their existing config stops parsing.
Oracle: the enumerated accepted forms in the doc comments at `apps/cli/src/config.rs:118-126` and
  `apps/cli/src/config.rs:1815-1824`, which are the published compatibility promise.

**VAL-CFG-003** — Writing configuration preserves the file.
Surface: parity
Needs: VAL-CFG-002
Behavior: re-serializing a loaded configuration emits the target wire shape — flat polymorphic
  `providers:` list, bare-sequence workload chains where no Axis-1/2 fields are set — and does not
  emit in-memory bridge fields.
Evidence: round-trip the 12-document corpus; assert the emitted YAML is byte-identical to the
  input for every document already in target shape, and that no output contains `local_models`,
  `bedrock_models`, or a `providers.mode` key.
Rigor: R3
Why: a writer that reshapes a hand-maintained file destroys operator comments and ordering, which
  is unrecoverable and immediately visible, so the gate command and its exit status must be
  recorded.
Fail: `stokd config set` rewrites the operator's file into a different shape.
Oracle: the same 12-document corpus, with the pre-extraction binary's output as the reference.

**VAL-CFG-004** — The agentic layer is configured in the same file, and is safe when absent.
Surface: cli
Needs: VAL-CFG-001
Behavior: an optional `agentic:` block in the shared configuration controls the layer
  (`enabled`, `verify.command`, `verify.maxRounds`, `budget.maxModelCallsPerOp`,
  `preflight.enabled`, `prompt.maxBytes`); when the block is absent every key takes a documented
  default, and no augmented verb errors or warns about missing configuration.
Evidence: run `sgit merge` in a fixture repo against a configuration with no `agentic:` block and
  assert exit 0, empty stderr, and behavior identical to plain `git merge`; then assert each key's
  default via a resolved-config dump.
Rigor: R2
Why: "no new required key" is a stated constraint, and an independent validator running with an
  untouched real config is the only way to prove an operator is not forced to edit anything.
Fail: existing operators get errors or nag output after upgrading.
Oracle: the operator's live `~/.stokd/config.yaml`, which contains no `agentic:` block.

**VAL-ROUTE-001** — Workload routing is shared and unchanged.
Surface: parity
Needs: VAL-CRATE-001
Behavior: `agent-core` owns workload-chain resolution — `models.workloads.<slug>` falling back to
  `models.defaults`, with `default`-sentinel expansion — parameterized on the provider/model
  configuration slice rather than on stokd's whole configuration, and returns the same chain stokd
  returns today for the same inputs.
Evidence: for all 13 `TaskClass` slugs and the 12-document corpus, the chain produced by
  `agent-core` equals the chain produced by the pre-extraction `resolved_workload_models`.
Rigor: R4
Why: routing decides which model runs every stokd workload; a silent change here is invisible at
  the call site and expensive in aggregate, so it needs two independent lanes rather than one
  implementer-run assertion.
Fail: a workload silently routes to a different model tier and cost or quality shifts with no
  changelog entry.
Oracle: the pre-extraction `stokd` binary's `resolved_workload_models` output for the same corpus.

**VAL-ROUTE-002** — sgit's tiers use the existing canonical workload slugs.
Surface: cli
Needs: VAL-ROUTE-001
Behavior: sgit's conflict tiers map T1 to `worker`, T2 to `codeReview`, and T3 to `escalation`,
  reusing the slugs already defined in the shared routing model, and require no new key under
  `models.workloads` for an operator to get working behavior.
Evidence: assert the tier-to-slug mapping in a unit test; then run each tier against the operator's
  live configuration, which defines none of these three workloads, and assert each resolves through
  `models.defaults` without error.
Rigor: R2
Why: reusing published slugs is what makes "no config change required" true, and an independent
  check against a config that lacks those workloads is the only proof the fallback path works.
Fail: sgit demands three new workload keys before it will do anything.
Oracle: the canonical slug list at `apps/cli/src/llm_routing.rs:291-310`.

**VAL-CALL-001** — One implementation of the headless one-shot model call.
Surface: library
Needs: VAL-CRATE-001, VAL-ROUTE-001
Behavior: `agent-core` exposes a headless one-shot invocation that builds the provider argv,
  delivers the prompt by argv or stdin according to the provider's transport, enforces a deadline,
  and returns the provider's stdout or a classified error; stokd's judge path calls it instead of
  its own private implementation.
Evidence: `apps/cli/src/governance_judge.rs` contains no per-provider argv construction and
  delegates to `agent_core`; stokd's judge tests pass unchanged; an sgit integration test drives a
  stub provider binary end to end and asserts the returned text.
Rigor: R2
Why: this is the second place drift would reappear, and confirming stokd's judge really delegates —
  rather than keeping a parallel copy behind a feature flag — requires reading the diff, not just a
  green test.
Fail: sgit grows a second, subtly different provider-invocation path.
Oracle: `apps/cli/src/governance_judge.rs:975-1077`, the pre-extraction implementation whose
  behavior must be preserved.

**VAL-CALL-002** — Provider availability and cooldown are shared and respected.
Surface: library
Needs: VAL-CALL-001
Behavior: `agent-core` determines provider availability and reads and writes one cooldown ledger;
  a provider in cooldown is skipped by both binaries, and a usage-limit failure recorded by one is
  observed by the other.
Evidence: record a cooldown for a provider through `agent-core`, then assert both a stokd routing
  call and an sgit routing call skip that provider; assert both processes read the same ledger path.
Rigor: R2
Why: two ledgers means one binary burns a rate-limited provider the other already knows is
  exhausted, and only a cross-binary check catches that.
Fail: sgit repeatedly retries a provider stokd has already banked as rate-limited.
Oracle: the single ledger file path under `~/.stokd`, asserted equal from both processes.

**VAL-CALL-003** — A hung provider never hangs a git operation.
Surface: cli
Needs: VAL-CALL-001
Behavior: every model call made by an sgit verb is bounded by a deadline; on expiry the child
  process is killed, the call is recorded as a transient failure, rotation proceeds, and if no
  provider succeeds the verb exits along its declared terminal path with the repository restored or
  preserved — never mid-operation.
Evidence: a stub provider that sleeps past the deadline; assert the verb returns within the
  deadline plus a bounded margin, that no provider child process survives, and that the repository
  is in a declared terminal state.
Rigor: R3
Why: an unbounded child holding a repository mid-merge is the worst failure this project can
  produce, so the timeout gate command and its exit status must be recorded, not merely observed.
Fail: a merge stalls forever behind a wedged provider and leaves the worktree unusable.
Oracle: the measured wall-clock bound of the test run against the configured deadline.

**VAL-GATE-001** — With no model, every augmented verb is plain git.
Surface: cli
Needs: VAL-CFG-004, VAL-ROUTE-002
Behavior: when `agentic.enabled` is false, or no provider is configured, or no configured provider
  is available, each augmented verb execs the corresponding `git` command with the caller's
  arguments unchanged and reproduces its stdout, stderr, and exit code, making zero model calls.
Evidence: for each augmented verb, run the sgit form and the git form under an empty provider
  configuration and under an unavailable-provider configuration, and assert byte-identical stdout,
  byte-identical stderr, and equal exit codes. Zero-spend is proved by an **external recorder**, not
  by self-reported telemetry: a stub provider binary placed on `PATH` appends to a log on every
  invocation, and that log must be empty after each run.
Rigor: R3
Why: this is the promise that makes adoption safe, it must hold for failure exit codes and not only
  success, and its gate invocations must be sealed because a later change could weaken it
  invisibly.
Fail: a developer without providers configured gets different behavior, mangled output, or a
  wrong exit code from a verb they expected to be transparent.
Oracle: the `git` binary itself, run with the same arguments in the same fixture repository.

**VAL-GATE-002** — Success costs nothing.
Surface: cli
Needs: VAL-GATE-001
Behavior: when the underlying git operation succeeds and post-operation verification passes, the
  verb makes zero model calls.
Evidence: a fixture repository whose merge is clean and whose verification command exits 0; run
  `sgit merge` with the stub provider configured and available on `PATH`, and assert the stub's
  invocation log is empty. The log is written by the stub, not by the code under test, so an
  implementation that under-counts its own calls cannot pass this.
Rigor: R3
Why: the operator's explicit requirement is that tokens are not spent where they add nothing, and
  a regression here is a silent, recurring cost that no test failure would otherwise reveal.
Fail: every clean merge quietly bills a model call.
Oracle: the model-call counter emitted in the operation's machine-readable summary.

**VAL-GATE-003** — Model spend per operation is bounded and reported.
Surface: cli
Needs: VAL-GATE-002
Behavior: each augmented operation enforces `agentic.budget.maxModelCallsPerOp`; on exhaustion it
  stops, preserves the repository along its declared terminal path, and reports the budget as the
  reason; every operation emits a machine-readable summary containing the model-call count, the
  tier chosen, and the reason for each call.
Evidence: a fixture whose conflicts exceed the budget; assert the operation stops at exactly the
  configured count, that the exit reason names the budget, that the repository is in a declared
  terminal state, and that the summary lists one entry per call.
Rigor: R3
Why: an unbounded escalation loop on a large conflict is the realistic runaway-cost failure, and
  the stop must be sealed evidence rather than a claim.
Fail: one `sgit merge` on a badly diverged branch consumes an unbounded number of model calls.
Oracle: the configured budget value compared against the emitted summary count.

**VAL-GATE-004** — sgit does not silently become git.
Surface: cli
Needs: none
Behavior: `sgit`'s existing verbs (`cd`, `checkout`, `clone`, `open`, `create`, `repo`, `worktree`,
  `shove`, `lock`) keep their current sgit semantics unchanged; a verb sgit does not define is
  forwarded to `git` verbatim; and `sgit --help` states that sgit is not a drop-in `git`
  replacement and names the verbs whose meaning differs.
Evidence: the existing `crates/sgit/src/main.rs` parse tests continue to pass unchanged; a
  forwarded unknown verb reproduces git's output and exit code; `sgit --help` output contains the
  non-equivalence statement and names `checkout`, `clone`, and `create`.
Rigor: R3
Why: `sgit checkout` ensuring a worktree while `git checkout` switches branches is a trap that
  destroys work if a user aliases the two, so the disclaimer and the preserved semantics must both
  be gate-sealed.
Fail: a user aliases `git=sgit`, runs `sgit checkout main`, and gets a worktree operation where they
  expected a branch switch.
Oracle: the axiom `AX-CLI-CHECKOUT-REPO-TARGET` in `.axioms.md`, which fixes the current meaning of
  `sgit checkout`.

**VAL-TRIAGE-001** — Conflict difficulty is scored deterministically and read-only.
Surface: cli
Needs: none
Behavior: `sgit conflict triage --json` reads the live conflicted index and emits, per file, the
  structural class, criticality, hunk count, both-sides-changed hunk count, auto-resolvability, and
  score, plus an aggregate score and tier; it mutates nothing in the repository.
Evidence: run against each fixture in the conflict corpus and assert the emitted JSON equals the
  recorded expectation; assert `git status --porcelain` and `git rev-parse HEAD` are unchanged
  across the run; run twice and assert byte-identical output.
Rigor: R2
Why: the whole spend model rests on this score being reproducible, and an independent validator
  re-running it on the same corpus is what makes "same state, same tier" a fact rather than a
  claim.
Fail: identical conflicts route to different tiers on different runs and model spend becomes
  indefensible.
Oracle: `~/.claude/skills/conflict-resolution/scripts/conflict-triage.sh`, whose output for the
  same corpus is the reference the Rust port must reproduce.

**VAL-TRIAGE-002** — The published scoring model is preserved exactly.
Surface: cli
Needs: VAL-TRIAGE-001
Behavior: the scorer computes `criticality×2 + both_sides×3 + one_sided + structural_penalty` per
  file and sums it, with auto-resolvable files contributing zero; it assigns T0 at score 0, T1 below
  7, T2 from 7 through 24, and T3 at 25 or above; and it forces T3 on any hard escalator —
  criticality-3 path, modify/delete, binary, submodule, five or more both-sides hunks in one file,
  or more than ten conflicted files.
Evidence: unit tests covering each weight, each tier boundary at its exact edge values, and one
  test per hard escalator asserting T3 regardless of a below-threshold score.
Rigor: R2
Why: boundary conditions are where a reimplementation of a published table silently drifts, and the
  tables are operator-facing documentation that must keep matching the code.
Fail: a mission-critical conflict scores into T1 and a cheap model rewrites an auth check.
Oracle: the scoring and tier tables in
  `~/.claude/skills/conflict-resolution/references/triage.md`.

**VAL-TRIAGE-003** — Missing merge-base information over-estimates, never under-estimates.
Surface: cli
Needs: VAL-TRIAGE-002
Behavior: `merge.conflictStyle=zdiff3` is set before any merge the layer performs; when a hunk
  carries no base section, the scorer counts it as both-sides-changed.
Evidence: a fixture with `diff3`-style markers lacking a base section; assert those hunks are
  counted as both-sides; assert the effective `merge.conflictStyle` is `zdiff3` after preflight.
Rigor: R2
Why: the base section is what separates a false conflict from a real one, and defaulting the
  unknown case toward "harder" is the only choice that cannot silently destroy work.
Fail: context-drift heuristics are applied to a genuinely contested hunk and one side is dropped.
Oracle: `~/.claude/skills/conflict-resolution/references/triage.md`, which states the
  over-estimation rule.

**VAL-MECH-001** — Recorded resolutions are reused before any model runs.
Surface: cli
Needs: VAL-TRIAGE-001
Behavior: preflight enables `rerere.enabled` and `rerere.autoupdate`; a conflict `rerere` resolves
  is verified and staged with no model call.
Evidence: produce the same conflict twice in a fixture; resolve it on the first pass; on the second
  pass assert it resolves with zero model calls and that the staged content equals the first
  resolution.
Rigor: R2
Why: on repeated rebases this is the single largest zero-cost win, and confirming the second pass
  is genuinely free requires an independent counter check.
Fail: identical conflicts are re-resolved by a model on every rebase.
Oracle: git's own `rerere` cache in the fixture repository.

**VAL-MECH-002** — Generated files are regenerated, never hand-merged.
Surface: cli
Needs: VAL-TRIAGE-001
Behavior: a conflict in a lockfile or generated artifact (`Cargo.lock`, `pnpm-lock.yaml`,
  `package-lock.json`, `yarn.lock`, `poetry.lock`, `go.sum`, `Gemfile.lock`, `composer.lock`,
  `*.generated.*`, `*.pb.go`, `*.snap`, and paths under `dist/`, `build/`, `target/`) is resolved by
  merging the high-level manifests, taking either side of the artifact, and regenerating it with the
  project's own tool; no model call is made and no conflict markers are ever left inside the
  artifact.
Evidence: a fixture where both branches add a dependency, producing a `Cargo.lock` conflict; assert
  the resolved lockfile contains both dependencies, contains no conflict markers, is byte-identical
  to a freshly regenerated lockfile, and that zero model calls were made.
Rigor: R2
Why: a hand-merged lockfile is an invalid lockfile that may still install, so the failure is
  delayed and confusing, and byte-equality against a fresh regeneration is the only sound check.
Fail: a textually merged lockfile pins an inconsistent dependency graph.
Oracle: the lockfile produced by running the project's package manager on the merged manifests.

**VAL-MECH-003** — Append-only files are unioned.
Surface: cli
Needs: VAL-TRIAGE-001
Behavior: conflicts in `CHANGELOG*`, `.gitignore`, `.dockerignore`, `AUTHORS`, and `CODEOWNERS` are
  resolved by unioning both sides, de-duplicating, and preserving order, with no model call.
Evidence: a fixture where both branches append distinct entries to each of the five classes; assert
  every entry from both sides survives, no duplicates are introduced, relative order within each
  side is preserved, and zero model calls were made.
Rigor: R2
Why: the file class makes the rule mechanical, but a dropped entry is silent work loss under this
  project's prime directive, so the "every entry from both sides survives" claim needs a validator
  other than the implementer rather than resting on a self-written assertion.
Fail: one branch's changelog entry disappears.
Oracle: the union of both sides' additions, computed independently in the test.

**VAL-MECH-004** — False conflicts are settled for free.
Surface: cli
Needs: VAL-TRIAGE-003
Behavior: for a conflicted hunk where one side equals the merge base, the layer takes the side that
  moved, with no model call; this covers the concurrent-list-and-import case and the class-boundary
  case from the scenario catalogue.
Evidence: fixtures reproducing `agent-advantage.txt` scenarios 1 and 2 — both branches adding an
  import at the same anchor, and both branches adding a method before the same closing brace;
  assert both additions survive, the file parses, and zero model calls were made.
Rigor: R2
Why: this is the highest-volume conflict class on a stale branch, so a defect here is both frequent
  and destructive, and an independent reader must confirm both sides genuinely survived rather than
  one side merely appearing to.
Fail: an import or a method added on one branch is dropped during a routine catch-up merge.
Oracle: the two named scenarios in `agent-advantage.txt`, reproduced as fixtures with both
  additions required in the result.

**VAL-MERGE-001** — Conflicts can be predicted without touching anything.
Surface: cli
Needs: VAL-TRIAGE-001
Behavior: `sgit merge --preflight <ref>` reports whether the merge is clean and, if not, which paths
  conflict and at what tier, using an in-memory merge that creates no merge state, dirties no
  worktree, and requires no checkout.
Evidence: run preflight in a fixture with known conflicts and assert the reported paths equal the
  paths a real merge produces; assert `git status --porcelain`, `git rev-parse HEAD`, and the
  absence of `MERGE_HEAD` are all unchanged after the run.
Rigor: R2
Why: this is the "check something first" case the operator asked for, it is safe to run against
  worktrees other agents are using only if it truly mutates nothing, and that property needs an
  independent check.
Fail: a prediction run leaves merge state behind and corrupts another agent's worktree.
Oracle: the path set produced by an actual `git merge` of the same refs in a throwaway clone.

**VAL-MERGE-002** — The semantic conflict classes are resolved.
Surface: cli
Needs: VAL-MECH-004, VAL-CALL-001, VAL-GATE-001
Behavior: `sgit merge` resolves the five semantic classes from the scenario catalogue — concurrent
  insertion collisions, class/block boundary additions, rename-propagation into new call sites,
  formatting-versus-logic divergence, and documentation/type-annotation versus implementation
  divergence — producing a result that parses and preserves both sides' intent.
Evidence: five fixture repositories, one per class, each with a recorded expected outcome; assert
  each merge completes, the result parses or compiles, and the class-specific invariant holds — both
  imports present; both methods present; the new call site using the renamed symbol; the logic
  change present on top of the formatted template; the updated documentation retained alongside the
  updated body.
Rigor: R3
Why: these are the cases the whole feature exists for, correctness is checkable only against a
  stated expected outcome rather than "no markers", and each fixture run must be gate-sealed with
  its exact invocation and exit status.
Fail: a merge reports success while having discarded one branch's contribution.
Oracle: the five worked cases in `agent-advantage.txt`, with per-fixture recorded expected outcomes
  reviewed and frozen before implementation begins.

**VAL-MERGE-003** — No side's work is silently lost.
Surface: cli
Needs: VAL-MERGE-002
Behavior: before any resolution is staged, the layer diffs the resolution against each side's
  contribution relative to the merge base and requires every symbol, branch, guard, and call
  introduced by either side to be present, or the operation stops and reports the specific absence;
  a resolution is never staged on the strength of markers being gone.
Evidence: two fixtures, and both must hold. Positive — an adversarial fixture whose
  correct-looking resolution drops one function added by the incoming side: assert the operation
  refuses to stage, exits non-zero, names the dropped symbol and the file, and leaves both sides
  recoverable. Negative — a fixture where one side's change is genuinely superseded by the other
  (the same guard, rewritten to subsume it): assert the operation does **not** refuse and completes.
  A gate that refuses everything would pass the positive case alone and make the feature unusable.
Rigor: R4
Why: this is the one gate whose failure is invisible to the developer and permanent in history, so
  it needs two independent lanes — the survival check itself and an end-to-end fixture proving the
  refusal actually fires.
Fail: a feature merged on one branch vanishes from the merge commit with no signal at all.
Oracle: `git diff <merge-base> <each side> -- <file>`, computed independently in the test, as the
  authority on what each side contributed.

**VAL-MERGE-004** — Every exit is a declared terminal state.
Surface: cli
Needs: VAL-MERGE-003, VAL-CALL-003
Behavior: `sgit merge` exits in exactly one of two states — committed, or cleanly restored — and in
  the restored case prints the backup ref names and an exact resume command; it never leaves the
  repository mid-merge without that report.
Evidence: force failure at each stage — preflight, mechanical resolution, model call, survival
  gate, verification — and additionally interrupt a run with `SIGINT` mid-resolution; assert for
  each of the six cases that the repository is either committed or clean, that the printed backup
  refs exist and resolve, and that the printed resume command runs successfully.
Rigor: R3
Why: the prerequisite shove work establishes this guarantee for the sync path and the operator has
  required it there; the agentic path must not reintroduce the mid-operation abandonment it removes,
  so each forced-failure invocation is gate-sealed.
Fail: a failed merge leaves the worktree mid-rebase with no recovery instructions.
Oracle: the terminal-guarantee requirement of the in-flight shove-termination design, item 6.

**VAL-VERIFY-001** — Silent integration breaks are caught after a clean merge.
Surface: cli
Needs: VAL-CFG-004
Behavior: after any merge, rebase, or cherry-pick the layer performs — including one that produced
  no conflict — it runs the verification command from `agentic.verify.command`, or an auto-detected
  one when that key is empty, and reports failure; auto-detection is a fixed, documented table
  (`Cargo.toml` to `cargo check --workspace`; `go.mod` to `go build ./...`; `pyproject.toml` to
  `python -m compileall .`; `package.json` to the first of `typecheck`, `check`, `build` that the
  file actually defines) and nothing else; when no command is configured and the table does not
  match, verification is skipped silently at zero cost.
Evidence: a fixture that merges cleanly but fails to compile; assert the failure is reported and
  the exit code is non-zero. A second fixture with no recognizable project type and no configured
  command; assert exit 0, no warning, and zero model calls.
Rigor: R2
Why: a clean merge that breaks the build is the failure class git cannot see at all, and the
  skip-when-unconfigured half must be independently confirmed so the layer never nags.
Fail: the interface-drift and stale-fixture scenarios merge green and break the build downstream.
Oracle: the verification command's own exit status in each fixture.

**VAL-VERIFY-002** — Verification failures are remediated in a bounded loop.
Surface: cli
Needs: VAL-VERIFY-001, VAL-CALL-001, VAL-GATE-003
Behavior: when verification fails, the layer feeds the failure output to a model, applies the
  proposed patch, and re-verifies, for at most `agentic.verify.maxRounds` rounds; on exhaustion it
  stops along its declared terminal path and reports the last failure; the loop covers the
  signature-drift, stale-fixture, and scope-shadowing scenarios.
Evidence: three fixtures, one per scenario — a required parameter added while call sites use the old
  arity; a payload field renamed while fixtures use the old shape; the same symbol introduced in
  overlapping scopes; assert each reaches a verified-green state within the configured rounds, and
  a fourth unfixable fixture stops at exactly `maxRounds` with the last failure reported.
Rigor: R3
Why: an unbounded remediation loop is the second runaway-cost path and the most likely one to loop
  on an unfixable failure, so the round cap must be sealed evidence.
Fail: a model patches, re-breaks, and re-patches indefinitely on an environment failure it cannot
  fix.
Oracle: the three named scenarios in `agent-advantage.txt` §2, reproduced as fixtures.

**VAL-VERIFY-003** — Verification never mutates history on its own.
Surface: cli
Needs: VAL-VERIFY-002
Behavior: remediation patches are applied to the working tree and staged, and are included in the
  operation's own commit; verification never creates an independent commit, never amends, and never
  pushes.
Evidence: run a remediating merge and assert the commit count increases by exactly the number the
  merge itself would produce, that no commit is authored outside the operation, and that no push
  occurred.
Rigor: R2
Why: extra or amended commits appearing from a verification pass would corrupt history in a way
  that is hard to attribute, and an independent count check is needed to prove it does not happen.
Fail: verification silently amends a commit the developer had already pushed.
Oracle: `git rev-list --count` before and after, compared against the plain-git baseline for the
  same merge.

**VAL-REBASE-001** — Rebase and pull get the same ladder, across every round.
Surface: cli
Needs: VAL-MERGE-004
Behavior: `sgit rebase` and `sgit pull` apply the same preflight, mechanical, triage, model, and
  verification ladder as `sgit merge`, and continue through every conflicting commit until the
  rebase completes or the operation reaches a declared terminal state.
Evidence: a fixture with a four-commit divergence in which two separate commits conflict; assert
  the rebase completes, that both conflicting commits were resolved, and that the ladder ran per
  round rather than only once.
Rigor: R3
Why: the single-round limitation is a known defect the prerequisite work removes, and this must
  prove the agentic layer rides the multi-round loop correctly rather than reintroducing the
  single-round assumption, so the run is gate-sealed.
Fail: a rebase resolves the first conflicting commit and fails on the second.
Oracle: the multi-round requirement of the in-flight shove-termination design, item 3.

**VAL-REBASE-002** — Migration sequence collisions are renumbered coherently.
Surface: cli
Needs: VAL-REBASE-001
Behavior: when both branches add a migration at the same sequence number, the layer renumbers the
  incoming migration to the next free number, updates the identifiers and metadata inside the file,
  and updates any snapshot or index file that references it.
Evidence: a fixture where both branches add a migration at the same index; assert both migrations
  exist at distinct consecutive numbers, that internal identifiers match their filenames, that no
  reference to the old number remains anywhere in the tree, and that the framework's own
  consistency check passes.
Rigor: R3
Why: a half-renumbered migration passes a textual check and fails at deploy time against a real
  database, so the framework's consistency check must be run and sealed rather than inferred.
Fail: two migrations share a sequence number, or a renamed migration leaves dangling references.
Oracle: the migration framework's own sequence-consistency check in the fixture project.

**VAL-REBASE-003** — Relocated symbols keep the other side's edits.
Surface: cli
Needs: VAL-REBASE-001, VAL-MERGE-003
Behavior: when one branch moves a symbol to a new file and the other edits that symbol in place,
  the layer applies the edit to the relocated definition and updates importers, rather than
  resurrecting the old location or dropping the edit.
Evidence: a fixture reproducing the move-and-edit case; assert the symbol exists only at its new
  location, that the incoming logic change is present in the relocated definition, that every
  importer resolves, and that the project compiles.
Rigor: R3
Why: git reports this as a delete/modify conflict where the intuitive resolution — keep the
  deletion — silently discards the edit, so the compile gate must be recorded as evidence.
Fail: the relocated function silently reverts to its pre-edit body.
Oracle: the cross-file symbol relocation scenario in `agent-advantage.txt` §1.C.

**VAL-PICK-001** — Backports are ported, not textually forced.
Surface: cli
Needs: VAL-MERGE-002
Behavior: `sgit cherry-pick` applies the intent of the source commit to the target branch's own
  architecture when a textual pick would conflict, and reports what it changed relative to a plain
  pick.
Evidence: a fixture with a maintenance branch whose equivalent logic has been refactored; assert a
  plain `git cherry-pick` conflicts, that `sgit cherry-pick` produces a result that compiles, that
  the fix's behavior is present, and that the report names the divergence it bridged.
Rigor: R3
Why: a wrong backport lands in a release branch that is by definition less tested than main, so the
  compile and behavior gates must be sealed.
Fail: a security fix is backported in a form that does not actually apply to the older code path.
Oracle: a behavioral test in the fixture that fails before the backport and passes after it.

**VAL-PICK-002** — Backports never land directly.
Surface: cli
Needs: VAL-PICK-001
Behavior: a backport is emitted as a branch plus a draft pull request against the target branch;
  the layer never pushes to the target branch and never merges.
Evidence: run a backport and assert the target branch's tip is unchanged, that a branch and a draft
  pull request were created, and that the operation exits non-zero if draft-PR creation is
  unavailable rather than falling back to a direct push.
Rigor: R3
Why: this is the containment that makes an agentic backport low-risk at all, and the
  no-fallback-to-push behavior must be sealed because a convenience fallback is exactly what a
  later change would add.
Fail: an unreviewed agent-authored backport lands directly on a release branch.
Oracle: the target branch's tip commit, asserted unchanged.

**VAL-BISECT-001** — Bisect is automated without touching the working branch.
Surface: cli
Needs: VAL-CALL-001, VAL-GATE-001
Behavior: `sgit bisect <good> <bad>` derives or accepts a reproduction script, validates that it
  fails at `bad` and passes at `good` before starting, runs the bisect loop, reports the culprit
  commit with the evidence for the verdict, and restores the original HEAD and working tree on
  every exit path including interruption.
Evidence: a fixture repository with a known culprit commit; assert the reported culprit equals the
  known one, that HEAD and the working tree are restored, and that a run whose script does not
  discriminate between `good` and `bad` is refused before any checkout occurs. Separately, with no
  provider configured and an operator-supplied script, assert the bisect still runs to a correct
  culprit and the stub provider's invocation log is empty — script derivation is the only part that
  needs a model.
Rigor: R3
Why: bisect checks out historical commits in the user's tree, so failure to restore is destructive,
  and the pre-flight discrimination check is what prevents a confidently wrong culprit — both must
  be gate-sealed.
Fail: bisect names a wrong commit, or leaves the developer on a detached historical checkout.
Oracle: the known culprit commit planted in the fixture repository.

**VAL-COMMIT-001** — Commit messages come from one implementation.
Surface: library
Needs: VAL-CALL-001
Behavior: commit-message generation is implemented once and used by both `sgit shove` and
  `sgit commit`; when no model is available it falls back to the existing deterministic message
  rather than failing.
Evidence: assert both call sites resolve to the same function; run `sgit commit` with no provider
  configured and assert the deterministic fallback message is used and the commit succeeds.
Rigor: R2
Why: the deterministic fallback already exists and must keep working, and confirming both call
  sites really share one implementation requires reading the code, not just observing output.
Fail: `sgit commit` fails outright when no model is configured.
Oracle: `crates/sgit-core/src/shove.rs:769`, the existing deterministic fallback whose behavior
  must be preserved.

**VAL-COMMIT-002** — History hygiene is proposed, never imposed.
Surface: cli
Needs: VAL-COMMIT-001
Behavior: `sgit push --preflight` scans the outgoing commits and reports proposals — squash
  groupings, conventional-commit rewrites, leftover debug statements, documentation the change
  implies — and applies nothing; applying requires a separate explicit invocation.
Evidence: run preflight on a branch with messy commits and a leftover debug statement; assert every
  proposal appears in the report, that `git rev-list` for the branch is unchanged afterward, and
  that no file in the working tree was modified.
Rigor: R3
Why: automatic history rewriting is irreversible for anyone who has already fetched the branch, so
  the "changed nothing" property must be sealed evidence rather than a design intention.
Fail: a preflight silently rewrites commits a colleague has already pulled.
Oracle: `git rev-list <upstream>..HEAD` before and after the run, asserted identical.

**VAL-SAFETY-001** — The forbidden git operations are never issued.
Surface: cli
Needs: none
Behavior: no code path in the agentic layer invokes `git stash`, `git reset --hard`,
  `git checkout -- .`, `git restore .`, `git clean -f`, a branch switch inside a worktree,
  `-X ours` or `-X theirs` at merge level, or `--no-verify` on push.
Evidence: a source-level check over the new code asserting none of these forms appears; plus a
  test harness that shims `git` to record every invocation, run across the full fixture corpus,
  asserting no recorded invocation matches any forbidden form.
Rigor: R3
Why: a source grep alone cannot see an argument assembled at runtime, so the recorded-invocation
  lane is required, and its gate command and exit status must be sealed.
Fail: an escalation path "cleans up" by discarding the developer's uncommitted work.
Oracle: the forbidden-operations list in the repository's global git-safety rules.

**VAL-SAFETY-002** — Both sides are recoverable before anything mutates.
Surface: cli
Needs: VAL-SAFETY-001
Behavior: before any mutating operation the layer records backup refs for both the local tip and
  the incoming tip, and prints them; the refs survive the operation regardless of outcome.
Evidence: for each mutating verb, assert the backup refs exist and resolve to the correct commits
  after both a successful run and a forced-failure run, and that each side's tree is recoverable
  from them.
Rigor: R3
Why: this is the last line of defense behind every other gate in this contract, and it must be
  proven on the failure path specifically, with the invocation sealed.
Fail: a failed agentic merge leaves no way back to either original tip.
Oracle: `git rev-parse` on the recorded backup refs, compared to the pre-operation tips captured
  independently by the test.

**VAL-SAFETY-003** — What leaves the machine is bounded and inspectable.
Surface: cli
Needs: VAL-CALL-001
Behavior: repository content sent to a provider is limited to the conflicted hunks, their
  surrounding context, and the verification output, and is capped at `agentic.prompt.maxBytes`
  (default 32768); a `--dry-run` flag prints exactly what would be sent without sending it; and no
  file matched by the repository's secret-ignore patterns is included.
Evidence: run `--dry-run` on a fixture containing a file with a credential-shaped name and assert
  it is excluded; assert the printed payload is at or below `agentic.prompt.maxBytes`; assert the
  stub provider's invocation log is empty for the dry run.
Rigor: R3
Why: this crate becoming public and this feature sending repository content outward together make
  disclosure the highest-consequence non-correctness risk here, so the exclusion check must be
  gate-sealed.
Fail: an environment file or key material is included in a prompt sent to a third-party provider.
Oracle: the repository's own ignore and secret-scanning patterns.

**VAL-SKILL-001** — There is one scorer, not two.
Surface: artifact
Needs: VAL-TRIAGE-002
Behavior: the installed `conflict-resolution` skill delegates scoring to `sgit conflict triage
  --json` when `sgit` is present, and its shell scorer remains only as the fallback for when it is
  not; the skill's documented tables and the Rust implementation agree.
Evidence: run both scorers over the full conflict corpus and assert identical tier assignments for
  every fixture; assert the skill's documentation cites the sgit verb as primary.
Rigor: R2
Why: two independently maintained scorers would drift within a release, and equality across the
  corpus is only meaningful if someone other than the implementer runs both.
Fail: the skill and the CLI disagree about a conflict's tier and spend differs by which entry point
  was used.
Oracle: the full conflict fixture corpus, scored by both implementations.

**VAL-DOC-001** — The contract with the operator is written down.
Surface: artifact
Needs: none
Behavior: the sgit README documents the escalation ladder, the passthrough guarantee, the
  `agentic:` configuration keys with their defaults, and the statement that `sgit` is not a drop-in
  `git` replacement.
Evidence: the README contains a section naming each ladder stage in order, each `agentic:` key, and
  the non-equivalence statement.
Scope: no-behaviour-change
Rigor: R0
Why: fully decided by reading one file; no code behavior changes, so the RED to GREEN obligation of
  axiom 5.1 does not attach.
Fail: operators cannot tell when the layer will spend a model call.
Oracle: the README file itself.

### Oracle

This contract is adjudicated by four external references, none of which is authored by the
implementation:

1. **The pre-extraction `stokd` binary.** Built from the commit immediately preceding work item
   1.4 and retained, unmodified, for the duration of the project. It is the sole authority for
   every `Surface: parity` assertion (`VAL-CRATE-003`, `VAL-CFG-001`, `VAL-CFG-003`,
   `VAL-ROUTE-001`). Its outputs over the corpus below are recorded before any extraction begins.

2. **The 12-document configuration corpus.** At minimum: the operator's live
   `~/.stokd/config.yaml`; one document per accepted legacy provider form; one per accepted
   workload form; one containing the `default` sentinel; one empty document; one with `providers:`
   absent; one with `models:` absent. Frozen and checked in before work item 1.1 begins.

3. **The conflict fixture corpus.** One fixture per scenario in `agent-advantage.txt` —
   **fourteen in scope**: five semantic merge classes (§"5 Cases"), three advanced rebase cases
   (lockfile, migration sequence, symbol relocation), three silent-integration-break cases
   (signature drift, stale fixtures, scope shadowing), and three low-risk agent cases (bisect,
   backport, pre-flight sanitization). The fourth low-risk case, markdown-driven repository
   automation, is out of scope per Non-Goals and has no fixture. Each fixture carries a recorded
   expected outcome, reviewed and frozen **before** the corresponding implementation work item
   begins, so no expectation can be back-fitted to whatever the implementation happens to produce.

4. **The published triage tables.** `~/.claude/skills/conflict-resolution/references/triage.md` and
   `scripts/conflict-triage.sh` adjudicate `VAL-TRIAGE-001`, `VAL-TRIAGE-002`, `VAL-TRIAGE-003`,
   and `VAL-SKILL-001`. The Rust port must reproduce the shell scorer's tier for every fixture.

**Fixed count.** The conflict corpus is exactly fourteen fixtures. A run that resolves fewer than
fourteen has not satisfied the catalogue, and the shortfall must be named explicitly rather than
absorbed into a passing summary.

---

## 3. Execution Topology

## Phase 1: Shared configuration and the merge vertical slice

**Purpose:** Nothing else in this project can be built honestly until the provider/model
configuration is genuinely shared and stokd is proven unchanged by the sharing. This phase
establishes `agent-core`, repoints stokd at it under a parity oracle, builds the passthrough gate,
and delivers one complete augmented verb — `sgit merge` — with the full ladder, the survival gate,
the verification loop, and cost instrumentation. That single slice is what makes the token economics
of the whole design measurable rather than theoretical.

**Stop:** Once a real `sgit merge` has run against the fourteen-fixture corpus and against my own
repositories, I will need to see the measured cost and latency per ladder stage before deciding
which verbs ship enabled by default, whether the T1/T2/T3 score thresholds are set where I want
them, and whether the per-operation call budget default is right. I cannot judge any of that from
a design document — only from what it actually costs on my own branches.

### 1.1 Create `agent-core` and move the provider/model configuration types
**Targets:** VAL-CRATE-001, VAL-CRATE-002, VAL-CFG-002
**Dependencies:** []

**Implementation Details**
- Add `crates/agent-core` to the `[workspace] members` list in the sgit repo's root `Cargo.toml`,
  mirroring `crates/sgit-core`'s manifest discipline: `[lints.rust] warnings = "deny"`, `[lib]`
  only, no `clap`.
- Move, verbatim where possible, from `apps/cli/src/config.rs`: `ProviderEntry` (`config.rs:129`),
  `ProvidersConfig` (`config.rs:414`), `ModelsConfig` (`config.rs:634`), `WorkloadPolicy`
  (`config.rs:1824`), `ModePool` and its custom `Deserialize`, `LocalModelRoles` (`config.rs:1168`),
  `LocalModelsConfig` (`config.rs:1307`), `SessionTitlesConfig`, `IqPolicy`, and
  `WORKLOAD_DEFAULT_SENTINEL`, together with their hand-written `Serialize`/`Deserialize`
  implementations and every `default_*` free function they reference.
- Inputs: YAML documents. Outputs: the typed values above. Failure modes: a serde attribute or a
  custom impl left behind changes the accepted forms — the corpus in 1.2 is the guard.
- `WorkPlanConfig` and `DefinitionOfDone` are referenced by `WorkloadPolicy` and live in stokd's
  `work_plan` module. Move the minimal type definitions they need, or gate those two fields behind
  a trait-object-free generic; do not drag the orchestration module across.
- Freeze the 12-document configuration corpus at
  `crates/agent-core/tests/fixtures/config/` before writing any code, including a sanitized copy of
  the operator's live `~/.stokd/config.yaml`.
- Clear the three pre-existing `sgit-core` clippy errors so the clippy gate this project adopts can
  actually pass: the collapsible `if` at `crates/sgit-core/src/checkout.rs:113`, the elidable
  lifetimes at `crates/sgit-core/src/workspace.rs:233`, and the derivable `impl` at
  `crates/sgit-core/src/submodule_checkout.rs:92`. These are the only pre-existing-defect fixes in
  scope for this project; each is behavior-preserving and must leave the 171-test baseline green.
- Build the stub provider binary used as the external spend recorder by every zero-model-call
  assertion: it appends its argv to a log file named by an environment variable and exits with a
  configurable code, so "zero model calls" is proved by an artifact the code under test does not
  write.

**Acceptance Criteria**
- AC-1.1.a: `crates/agent-core/Cargo.toml` exists, is listed in the root workspace members, sets
  `warnings = "deny"`, and declares neither `clap` nor `sgit-core` → code inspection.
- AC-1.1.b: `grep -rn 'Subcommand\|clap::Parser\|sgit_core' crates/agent-core/src/` → no matches.
- AC-1.1.c: every document in the frozen corpus parses without error → table-driven test.
- AC-1.1.d: `cargo test -p agent-core` → exit 0.
- AC-1.1.e: `cargo test --workspace` → exit 0 with at least the 171-test baseline still passing.
- AC-1.1.f: `cargo clippy --workspace --all-targets -- -D warnings` → exit 0, where it exits
  non-zero with three errors today.
- AC-1.1.g: the stub provider binary exists, and running it appends exactly one line to the log
  named by its environment variable → integration test.

**Acceptance Tests**
- Test-1.1.a: Unit — one case per accepted provider form (bare string, object, legacy single-key
  map, legacy `entries` nesting) asserting the parsed `ProviderEntry`.
- Test-1.1.b: Unit — one case per accepted workload form (bare sequence, object with `models`,
  polymorphic title object, chain containing the `default` sentinel).
- Test-1.1.c: Integration — parse the sanitized live config and assert nine providers and four
  configured workloads.
- Test-1.1.d: Regression — the existing 171 workspace tests still pass after the clippy fixes.
- Test-1.1.e: Integration — the stub provider records exactly one invocation per call.

**Verification Commands**
```bash
cd /opt/worktrees/stokd-cloud/sgit/main
grep -q 'crates/agent-core' Cargo.toml
! grep -rqn 'Subcommand\|clap::Parser\|sgit_core' crates/agent-core/src/
cargo test -p agent-core
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

### 1.2 Configuration discovery and the differential corpus harness
**Targets:** VAL-CFG-001, VAL-CFG-003
**Dependencies:** ["1.1"]

**Implementation Details**
- Implement discovery in `agent-core` with the precedence already documented at
  `crates/sgit-core/src/config.rs:1-11`: `SGIT_CONFIG`, then `$XDG_CONFIG_HOME/sgit/config.yaml`
  when present, then `~/.stokd/config.yaml` when present, else compiled defaults. Return the
  selected source alongside the value, exactly as `sgit-core::ConfigSource` does.
- The loader reads only the `providers:`, `models:`, and `agentic:` blocks and ignores unknown
  top-level keys, so it can read stokd's full document without owning its schema.
- Add a hidden `--dump-resolved-config` flag to the `sgit` binary emitting the resolved
  provider/model values plus the selected source path as JSON. This is the comparison surface
  VAL-CFG-001 is measured on; without it the cross-binary equality claim is not checkable.
- Build the differential harness as a test binary in `agent-core` that, for each corpus document,
  compares parse, re-serialization, and `resolved_workload_models` output for all 13 `TaskClass`
  slugs against recorded reference output produced by the pre-extraction stokd binary.
- Record the reference output **before** 1.4 repoints stokd; store it under
  `crates/agent-core/tests/fixtures/reference/`.
- Failure mode: a corpus document that only exists post-change proves nothing — the reference must
  be generated from the retained pre-extraction binary.

**Acceptance Criteria**
- AC-1.2.a: for every corpus document, the selected config source path matches the documented
  precedence for the environment under test → integration test.
- AC-1.2.e: `sgit --dump-resolved-config` emits valid JSON containing the resolved provider list,
  the resolved model chains, and the selected source path → integration test.
- AC-1.2.b: re-serializing any corpus document already in target shape produces byte-identical
  output → round-trip test.
- AC-1.2.c: no re-serialized output contains `local_models`, `bedrock_models`, or `providers.mode`
  → assertion in the same test.
- AC-1.2.d: `cargo test -p agent-core --test differential` → exit 0.

**Acceptance Tests**
- Test-1.2.a: Integration — precedence resolution with `SGIT_CONFIG` set, with XDG present, with
  only the stokd path present, and with none present.
- Test-1.2.b: Integration — byte-identical round trip over the 12-document corpus.
- Test-1.2.c: Integration — `resolved_workload_models` equality against recorded reference output
  for 13 slugs × 12 documents.

**Verification Commands**
```bash
cd /opt/worktrees/stokd-cloud/sgit/main
test -d crates/agent-core/tests/fixtures/reference
cargo test -p agent-core --test differential
cargo test --workspace
```

### 1.3 Move workload routing into `agent-core`
**Targets:** VAL-ROUTE-001, VAL-ROUTE-002
**Dependencies:** ["1.1"]

**Implementation Details**
- Move from `apps/cli/src/llm_routing.rs`: `TaskClass` and its `slug`/`required_capabilities`/
  `target_tier` impls (`llm_routing.rs:237-430`), `ModelTier` and `ECONOMY_FLOOR`,
  `expand_workload_model_chain` (`llm_routing.rs:53`), `resolved_workload_models`
  (`llm_routing.rs:81`), `LlmMode`/`LlmModeSet` (`llm_routing.rs:96-235`), `parse_model_ref`
  (`llm_routing.rs:746`), `normalize_provider_key`, `find_providers_for_model`,
  `mode_provider_list` (`llm_routing.rs:574`), `is_workload_configured` (`llm_routing.rs:1321`),
  and `no_provider_configured_error`.
- Change the signatures that take `&StokdConfig` to take a borrowed `&ProvidersConfig` and
  `&ModelsConfig` (or a small `RoutingView` holding both). stokd keeps same-named wrappers that
  take `&StokdConfig` and forward, so no stokd call site changes in this item.
- Preserve the fail-closed behavior documented at `llm_routing.rs:574-594` exactly: an empty
  configured provider list must never expand to an invented frontier list.
- Leave in stokd: everything touching `AgentDiscovery`, orchestration, IQ escalation dispatch, and
  the Axis-2 work-plan seam.
- Failure mode: accidentally widening the fail-closed path into a default would silently enable
  network calls for operators who configured nothing.

**Acceptance Criteria**
- AC-1.3.a: `agent-core` exposes `TaskClass` with all 13 slugs matching the canonical list → unit
  test comparing against the literal slug strings.
- AC-1.3.b: an empty provider list yields an empty provider order and never an invented default →
  unit test.
- AC-1.3.c: sgit's tier mapping resolves T1→`worker`, T2→`codeReview`, T3→`escalation` → unit test.
- AC-1.3.d: with the live operator config, which defines none of those three workloads, each tier
  resolves through `models.defaults` without error → integration test.
- AC-1.3.e: `cargo test -p agent-core` → exit 0.

**Acceptance Tests**
- Test-1.3.a: Unit — all 13 slugs and their tiers.
- Test-1.3.b: Unit — fail-closed on empty provider configuration, one case per `LlmMode`.
- Test-1.3.c: Unit — `default` sentinel expansion splices `models.defaults` in position.
- Test-1.3.d: Integration — tier resolution against the sanitized live config.

**Verification Commands**
```bash
cd /opt/worktrees/stokd-cloud/sgit/main
cargo test -p agent-core
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

### 1.4 Move headless one-shot invocation, availability, and the cooldown ledger
**Targets:** VAL-CALL-001, VAL-CALL-002, VAL-CALL-003
**Dependencies:** ["1.3"]

**Implementation Details**
- Introduce a pure `CommandSpec { program, args, env_set, env_remove, cwd, prompt_transport }` in
  `agent-core`, plus `build_headless_command(provider, model, prompt, opts) -> CommandSpec`,
  generalized from `build_judge_command` (`apps/cli/src/governance_judge.rs:975-1063`) — including
  the claude branch's `--disable-slash-commands --strict-mcp-config
  --dangerously-skip-permissions --max-turns 2` isolation and its documented prohibition on
  `--bare`, the codex `exec --dangerously-bypass-approvals-and-sandbox --ephemeral -` stdin form,
  and the gemini `GEMINI_CLI_IDE_*` scrubbing.
- Port `is_available` per provider (binary-on-PATH probe plus endpoint reachability for
  OpenAI-compatible entries) and the cooldown ledger from `apps/cli/src/provider_cooldown.rs`,
  keeping the existing on-disk ledger path so both binaries share one file.
- Implement the runner: spawn the `CommandSpec`, feed the prompt by argv or stdin per transport,
  enforce a deadline, kill the child and reap it on expiry, and classify the result as success,
  usage-limit, or transient — the classification `invoke_provider_headless_impl` performs at
  `governance_judge.rs:1112`.
- Implement cooldown-aware provider rotation over the routed chain, re-failing any provider that
  exits zero without producing usable output — the guard stokd already applies at
  `apps/cli/src/commands/shove.rs:104`.
- Failure modes: an orphaned child on timeout; a ledger written to a second path; losing the
  claude `--bare` prohibition, which would break subscription-login operators.

**Acceptance Criteria**
- AC-1.4.a: `build_headless_command` output for claude, codex, gemini, and the generic path equals
  the recorded argv of the pre-extraction `build_judge_command` for the same inputs → unit test
  over recorded fixtures.
- AC-1.4.b: a stub provider that sleeps past the deadline is killed and the call returns within the
  deadline plus a bounded margin, with no surviving child process → integration test.
- AC-1.4.c: a cooldown recorded through `agent-core` is observed by a second process reading the
  same ledger path → integration test.
- AC-1.4.d: a stub provider exiting 0 with empty output is treated as a failure and rotation
  proceeds → integration test.
- AC-1.4.e: `cargo test -p agent-core` → exit 0.

**Acceptance Tests**
- Test-1.4.a: Unit — argv equality against recorded pre-extraction fixtures, one per provider
  branch.
- Test-1.4.b: Integration — timeout kill and process reaping.
- Test-1.4.c: Integration — cross-process cooldown ledger visibility.
- Test-1.4.d: Integration — zero-exit-empty-output rotation.

**Verification Commands**
```bash
cd /opt/worktrees/stokd-cloud/sgit/main
cargo test -p agent-core
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

### 1.5 Repoint stokd at `agent-core` and prove parity
**Targets:** VAL-CRATE-003
**Dependencies:** ["1.2", "1.4"]

**Implementation Details**
- Build and retain the pre-extraction `stokd` binary from the current `apps/cli` HEAD; record its
  reference output over the 12-document corpus. This must happen before the first line of this item
  is changed.
- Add `agent-core = { path = "../sgit/crates/agent-core" }` to `apps/cli/Cargo.toml` alongside the
  existing `sgit-core` path dependency, and advance the `apps/sgit` submodule gitlink plus
  `apps/cli/Cargo.lock`.
- Replace the moved declarations in `apps/cli/src/config.rs` and `apps/cli/src/llm_routing.rs` with
  `pub use agent_core::…` re-exports, following the pattern already used at
  `apps/cli/src/worktree_pin.rs:13` and `apps/cli/src/workspace.rs:15`, so existing stokd call
  sites compile unchanged.
- Repoint `governance_judge::invoke_provider_headless` and `build_judge_command` at
  `agent_core`, deleting the per-provider argv match from stokd rather than leaving it dormant.
- Add the matching hidden `--dump-resolved-config` flag to the `stokd` binary, emitting the same
  JSON shape as sgit's, so the two dumps are directly comparable.
- Record `cargo test --manifest-path Cargo.toml` output in `apps/cli` before and after; the test
  count must not drop.
- Failure mode: a re-export that changes a type's public path breaks a downstream stokd module —
  caught by the stokd build, which is the gate here.

**Acceptance Criteria**
- AC-1.5.a: `grep -c 'struct ProvidersConfig\|struct ModelsConfig\|enum ProviderEntry\|struct
  WorkloadPolicy' apps/cli/src/config.rs` → 0.
- AC-1.5.b: `apps/cli/src/governance_judge.rs` contains no per-provider argv match → code
  inspection plus grep for the literal isolation flags.
- AC-1.5.c: `cargo test --manifest-path Cargo.toml` in `apps/cli` → exit 0, with a passing test
  count greater than or equal to the recorded pre-extraction baseline.
- AC-1.5.d: `cargo clippy --manifest-path Cargo.toml -- -D warnings` in `apps/cli` → exit 0.
- AC-1.5.e: the differential harness reports zero divergences across the corpus for parse,
  re-serialization, and all 13 workload chains → exit 0.
- AC-1.5.f: `sgit --dump-resolved-config` and `stokd --dump-resolved-config` produce equal output
  for every corpus document → integration test, the measurement surface for VAL-CFG-001.

**Acceptance Tests**
- Test-1.5.a: Regression — stokd's full test suite at or above baseline count.
- Test-1.5.b: Integration — differential harness, lane B of VAL-CRATE-003.
- Test-1.5.c: Integration — byte-identical re-serialization of the operator's live config.
- Test-1.5.d: Regression — stokd's judge tests unchanged.

**Verification Commands**
```bash
cd /opt/worktrees/stokd-cloud/mono/main/apps/cli
! grep -qn 'pub struct ProvidersConfig\|pub struct ModelsConfig\|pub enum ProviderEntry' src/config.rs
cargo build --manifest-path Cargo.toml
cargo test --manifest-path Cargo.toml
cargo clippy --manifest-path Cargo.toml -- -D warnings
cd /opt/worktrees/stokd-cloud/sgit/main && cargo test -p agent-core --test differential
```

### 1.6 The `agentic:` config block and the capability gate
**Targets:** VAL-CFG-004, VAL-GATE-001
**Dependencies:** ["1.3"]

**Implementation Details**
- Add an `AgenticConfig` to `agent-core` for the `agentic:` block: `enabled` (default true),
  `verify.command` (default empty), `verify.maxRounds` (default 3),
  `budget.maxModelCallsPerOp` (default 6), `preflight.enabled` (default true). Absent block yields
  all defaults; absent keys yield their individual defaults.
- Implement `AgentCapability::resolve(&AgenticConfig, &ProvidersConfig, &ModelsConfig)` returning
  either `Available { .. }` or `Unavailable { reason }`, where `Unavailable` is produced when the
  layer is disabled, no provider is configured, or no configured provider is available. It calls
  the routing fail-closed path from 1.3 and never probes the network when the layer is disabled.
- Implement `passthrough(argv) -> !` in the sgit binary: exec `git` with the caller's arguments
  unchanged, inheriting stdio, and propagate the exit code exactly, including signal-derived codes.
- Failure mode: buffering git's streams instead of inheriting them would break interactive pagers
  and progress output — inherit, do not capture, on the passthrough path.

**Acceptance Criteria**
- AC-1.6.a: with no `agentic:` block present, every key reports its documented default → unit test.
- AC-1.6.b: with an empty `providers:` list, `AgentCapability::resolve` returns `Unavailable` and
  makes no network call → unit test with a network-denying stub.
- AC-1.6.c: `sgit merge <ref>` under `Unavailable` produces stdout, stderr, and exit code
  byte-identical to `git merge <ref>` in the same fixture → integration test, asserted for both a
  succeeding and a failing merge.
- AC-1.6.d: the model-call counter is zero for every `Unavailable` run → integration test.

**Acceptance Tests**
- Test-1.6.a: Unit — default resolution with the block absent, partially present, and fully
  present.
- Test-1.6.b: Integration — passthrough byte-equality against real `git`, success case.
- Test-1.6.c: Integration — passthrough byte-equality against real `git`, conflict/failure case
  including exit code.
- Test-1.6.d: Unit — `Unavailable` for each of the three causes, with distinct reasons.

**Verification Commands**
```bash
cd /opt/worktrees/stokd-cloud/sgit/main
cargo test -p agent-core
cargo test -p sgit
cargo test --workspace
```

### 1.7 Register git-shaped verbs and unknown-verb passthrough
**Targets:** VAL-GATE-004
**Dependencies:** ["1.6"]

**Implementation Details**
- Add `Merge`, `Rebase`, `Pull`, `CherryPick`, `Bisect`, `Commit`, `Push`, `Verify`, and
  `Conflict { Triage }` to the `Commands` enum in `crates/sgit/src/main.rs`, alongside — never
  replacing — the existing nine verbs.
- Enable `allow_external_subcommands` so a verb sgit does not define is forwarded to `git`
  verbatim through the 1.6 passthrough.
- Extend `sgit --help` with an explicit statement that `sgit` is not a drop-in `git` replacement,
  naming `checkout`, `clone`, and `create` as verbs whose meaning differs from git's.
- Every new verb accepts and forwards arbitrary trailing arguments so `sgit merge --abort` and
  `sgit rebase --continue` reach git unchanged.
- Failure mode: `allow_external_subcommands` interacting with the existing back-compat aliases
  could swallow a typo'd known verb and forward it to git; the existing parse tests are the guard
  and must not be modified.

**Acceptance Criteria**
- AC-1.7.a: the seven existing parse tests in `crates/sgit/src/main.rs` pass **unmodified** →
  `cargo test -p sgit`.
- AC-1.7.b: `sgit --help` contains the non-equivalence statement and the three named verbs → grep
  on rendered help.
- AC-1.7.c: an undefined verb reproduces git's stdout, stderr, and exit code → integration test.
- AC-1.7.d: `sgit merge --abort` reaches git with the flag intact → integration test with a `git`
  shim recording invocations.

**Acceptance Tests**
- Test-1.7.a: Regression — existing clap parse tests, unmodified.
- Test-1.7.b: Unit — rendered help contains the disclaimer.
- Test-1.7.c: Integration — unknown-verb forwarding byte-equality.
- Test-1.7.d: Integration — trailing-argument passthrough via a recording `git` shim.

**Verification Commands**
```bash
cd /opt/worktrees/stokd-cloud/sgit/main
for t in clone_is_available_without_the_repo_group open_and_create_are_available_without_the_repo_group repo_group_keeps_the_lifecycle_verbs_as_back_compat_aliases promoted_verbs_are_visible_in_top_level_help lock_verbs_parse lock_verbs_visible_in_help checkout_parses_branch_arg; do grep -q "fn $t" crates/sgit/src/main.rs || { echo "existing parse test removed: $t"; exit 1; }; done
cargo test -p sgit
cargo test --workspace
```

### 1.8 Deterministic conflict triage scorer
**Targets:** VAL-TRIAGE-001, VAL-TRIAGE-002, VAL-TRIAGE-003
**Dependencies:** ["1.7"]

**Implementation Details**
- Implement the scorer in `sgit-core` — not `agent-core`, since it needs no model — reusing
  `parse_unmerged_entries` (`crates/sgit-core/src/shove.rs:333`) for structural classification and
  `UnmergedKind` for the structural penalty mapping: content 0, add/add 1, rename 2, modify/delete
  3, binary 3, submodule 3.
- Parse `zdiff3` markers to count both-sides-changed versus one-sided hunks; when a hunk has no
  base section, count it as both-sides.
- Implement the criticality table from the published reference: 3 for auth, billing, payment,
  secrets, credentials, tokens, crypto, security, permissions, migrations, schema,
  `infrastructure/`, `deployment/`, `.github/workflows/`, IAM, governance, and landing code; 2 for
  ordinary source; 1 for tests, examples, scripts, and fixtures; 0 for documentation.
- Implement the auto-resolvable classes so they contribute zero, and the hard escalators so they
  force T3 regardless of score.
- Expose `sgit conflict triage --json` with a versioned schema, and assert read-only behavior by
  capturing repository state before and after.
- Freeze the fourteen-fixture conflict corpus with recorded expected outcomes **before** writing
  the scorer.

**Acceptance Criteria**
- AC-1.8.a: each weight, each tier boundary at its exact edge (0, 6, 7, 24, 25), and each of the
  six hard escalators is covered by a test → unit tests.
- AC-1.8.b: `sgit conflict triage --json` output equals the recorded expectation for all fourteen
  fixtures → integration test.
- AC-1.8.c: `git status --porcelain` and `git rev-parse HEAD` are unchanged across a triage run →
  integration test.
- AC-1.8.d: two consecutive runs produce byte-identical output → integration test.
- AC-1.8.e: the Rust scorer's tier equals the shell scorer's tier for all fourteen fixtures →
  integration test.

**Acceptance Tests**
- Test-1.8.a: Unit — scoring weights and tier boundaries.
- Test-1.8.b: Unit — each hard escalator forces T3 from a below-threshold score.
- Test-1.8.c: Unit — a base-less hunk counts as both-sides.
- Test-1.8.d: Integration — corpus-wide output equality and determinism.
- Test-1.8.e: Integration — tier equality against `scripts/conflict-triage.sh`.

**Verification Commands**
```bash
cd /opt/worktrees/stokd-cloud/sgit/main
test "$(ls crates/sgit-core/tests/fixtures/conflicts | wc -l)" -eq 14
cargo test -p sgit-core
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

### 1.9 Mechanical zero-model resolution classes
**Targets:** VAL-MECH-001, VAL-MECH-002, VAL-MECH-003, VAL-MECH-004
**Dependencies:** ["1.8"]

**Implementation Details**
- Preflight: set `rerere.enabled`, `rerere.autoupdate`, and `merge.conflictStyle=zdiff3` for the
  operation; record and restore any prior values so operator configuration is not permanently
  changed.
- Implement the four mechanical classes in `sgit-core`, each returning "resolved" or "not my
  class", never a partial result: rerere reuse; regenerate for lockfiles and generated artifacts;
  union for append-only files; take-the-moved-side for one-sided hunks.
- The regenerate class merges the high-level manifests first, then invokes the project's own tool
  (`cargo`, `pnpm`, `npm`, `yarn`, `poetry`, `go`, `bundle`, `composer`) detected from the manifest
  present; if the tool is absent it reports "not my class" rather than hand-merging.
- Route every resolution through the existing `stage_conflict_resolution` and
  `verify_conflict_staged` helpers (`crates/sgit-core/src/shove.rs:456`, `:474`) so deletions are
  staged as deletions and never resurrected.
- Re-run triage after this stage; the score routed on is the post-mechanical score.
- Failure mode: regenerating a lockfile with a tool version different from the project's would
  produce a valid but churned file — assert byte-equality against a fresh regeneration in the same
  environment, and report rather than guess when the tool is missing.

**Acceptance Criteria**
- AC-1.9.a: a repeated identical conflict resolves from rerere with zero model calls on the second
  pass → integration test.
- AC-1.9.b: a `Cargo.lock` conflict resolves to a file byte-identical to a fresh regeneration,
  containing both dependencies and no markers, with zero model calls → integration test.
- AC-1.9.c: union classes preserve every entry from both sides with no duplicates → integration
  test across all five named file classes.
- AC-1.9.d: the import-collision and closing-brace fixtures resolve with zero model calls and both
  additions present → integration test.
- AC-1.9.e: prior `rerere` and `merge.conflictStyle` values are restored after the operation →
  integration test.

**Acceptance Tests**
- Test-1.9.a: Integration — rerere reuse, model-call counter zero.
- Test-1.9.b: Integration — lockfile regeneration byte-equality.
- Test-1.9.c: Integration — union across CHANGELOG, .gitignore, .dockerignore, AUTHORS, CODEOWNERS.
- Test-1.9.d: Integration — scenarios 1 and 2 from the corpus at zero cost.
- Test-1.9.e: Integration — git config restoration.

**Verification Commands**
```bash
cd /opt/worktrees/stokd-cloud/sgit/main
cargo test -p sgit-core
cargo test --workspace
```

### 1.10 `sgit merge` — the full ladder, survival gate, and terminal guarantee
**Targets:** VAL-MERGE-001, VAL-MERGE-002, VAL-MERGE-003, VAL-MERGE-004, VAL-GATE-002, VAL-SAFETY-001, VAL-SAFETY-002, VAL-SAFETY-003
**Dependencies:** ["1.5", "1.9", "1.11"]

**Implementation Details**
- **Gate: do not begin this item until the in-flight "shove always terminates cleanly" work has
  landed on the sgit repo's `main`.** This item consumes its `SyncOutcome`, its multi-round conflict
  loop, and its binary conflict class, and must not reimplement any of them.
- Implement `sgit merge` as the ladder: preflight via `git merge-tree --write-tree` (non-mutating);
  record backup refs for both tips; plain `git merge`; on `SyncOutcome::Conflicts`, mechanical
  resolution; re-triage; a tier-matched model call through `agent-core` for what remains, with the
  prompt bounded per VAL-SAFETY-003; the both-sides survival gate; verification via 1.11; then
  commit.
- Implement the survival gate as an independent check: for each conflicted file compute each side's
  contribution against the merge base and require every introduced symbol, branch, guard, and call
  to be present in the result, or refuse and report the specific absence.
- Implement `--dry-run` printing the exact prompt payload without sending it, and enforce the
  documented size cap and secret-pattern exclusion.
- Escalation is one-way: a tier may raise itself mid-resolution and may never lower itself.
- Every exit path is committed or cleanly restored, printing the backup refs and an exact resume
  command.
- Failure modes: a survival gate that only checks for markers; a model call issued before the
  mechanical stage; an escalation loop with no budget; a restore path that discards uncommitted
  work using a forbidden operation.

**Acceptance Criteria**
- AC-1.10.a: preflight reports the same conflicted path set as a real merge and leaves
  `git status --porcelain`, `git rev-parse HEAD`, and the absence of `MERGE_HEAD` unchanged →
  integration test.
- AC-1.10.b: all five semantic-class fixtures merge to their recorded expected outcome and compile
  or parse → integration test.
- AC-1.10.c: the adversarial drop fixture is refused, exits non-zero, and names the dropped symbol
  and file → integration test.
- AC-1.10.d: a clean merge whose verification passes makes zero model calls → integration test.
- AC-1.10.e: forced failure at each of the five stages leaves the repository committed or clean,
  with resolvable backup refs and a resume command that runs successfully → integration test.
- AC-1.10.f: across the entire fixture corpus, a recording `git` shim logs no forbidden operation →
  integration test.
- AC-1.10.g: `--dry-run` on a fixture containing a credential-named file excludes it, stays within
  the cap, and makes zero model calls → integration test.

**Acceptance Tests**
- Test-1.10.a: Integration — non-mutating preflight equivalence.
- Test-1.10.b: E2E — five semantic-class fixtures against recorded expectations.
- Test-1.10.c: E2E — adversarial both-sides-survival refusal.
- Test-1.10.d: Integration — zero-cost clean merge.
- Test-1.10.e: E2E — terminal guarantee at five forced failure points.
- Test-1.10.f: Security — forbidden-operation shim across the corpus.
- Test-1.10.g: Security — dry-run payload bounding and secret exclusion.

**Verification Commands**
```bash
cd /opt/worktrees/stokd-cloud/sgit/main
grep -rqn 'enum SyncOutcome' crates/sgit-core/src/ || { echo "PREREQUISITE NOT LANDED: shove-termination SyncOutcome absent from sgit-core"; exit 1; }
cargo test -p sgit
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

### 1.11 Post-operation verification loop and cost instrumentation
**Targets:** VAL-VERIFY-001, VAL-VERIFY-002, VAL-VERIFY-003, VAL-GATE-003
**Dependencies:** ["1.5", "1.7"]

**Implementation Details**
- Implement `sgit verify` and the in-operation verification stage: run
  `agentic.verify.command` when set; otherwise auto-detect from the manifest present
  (`Cargo.toml` → `cargo check`, `package.json` → the project's own build or typecheck script,
  `pyproject.toml`, `go.mod`); otherwise skip silently at zero cost.
- On failure, feed the failure output to a model through `agent-core`, apply the patch to the
  working tree, stage it, and re-verify, bounded by `agentic.verify.maxRounds`.
- Never create an independent commit, never amend, never push. Remediation is included in the
  operation's own commit.
- Implement the model-call counter, per-call reason, chosen tier, elapsed time, and the
  `budget.maxModelCallsPerOp` enforcement. Emit a machine-readable summary at the end of every
  augmented operation — this summary is the measurement that answers the Phase 1 stop.
- Failure mode: a verification command that is itself flaky would burn the whole budget; record
  each round's exit status in the summary so a flaky command is visible rather than mysterious.

**Acceptance Criteria**
- AC-1.11.a: a cleanly-merging but non-compiling fixture reports failure and exits non-zero →
  integration test.
- AC-1.11.b: a fixture with no recognizable project type and no configured command exits 0 with no
  warning and zero model calls → integration test.
- AC-1.11.c: the signature-drift, stale-fixture, and scope-shadowing fixtures each reach verified
  green within the configured rounds → integration test.
- AC-1.11.d: an unfixable fixture stops at exactly `maxRounds` and reports the last failure →
  integration test.
- AC-1.11.e: commit count after a remediating merge equals the plain-git baseline for the same
  merge → integration test.
- AC-1.11.f: a fixture exceeding the call budget stops at exactly the configured count, names the
  budget as the reason, and leaves a declared terminal state → integration test.
- AC-1.11.g: every augmented operation emits a summary containing model-call count, tier, and
  per-call reason → integration test.

**Acceptance Tests**
- Test-1.11.a: Integration — clean merge, broken build, reported.
- Test-1.11.b: Integration — unconfigured and undetectable, silent zero-cost skip.
- Test-1.11.c: E2E — the three silent-integration-break scenarios.
- Test-1.11.d: Integration — round cap on an unfixable failure.
- Test-1.11.e: Integration — commit-count equality.
- Test-1.11.f: Integration — budget enforcement and terminal state.
- Test-1.11.g: Integration — summary schema completeness.

**Verification Commands**
```bash
cd /opt/worktrees/stokd-cloud/sgit/main
cargo test -p sgit
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Phase 2: The remaining verbs

**Purpose:** Continues only after the operator answers the Phase 1 stop. The ladder, the shared
crate, the passthrough gate, and the measurement are all in place; this phase applies them to the
rest of the scenario catalogue and closes the documentation and axiom obligations.

### 2.1 `sgit rebase` and `sgit pull`
**Targets:** VAL-REBASE-001
**Dependencies:** []

**Implementation Details**
- Apply the Phase 1 ladder to rebase and pull, driving it once per conflict round through the
  prerequisite work's multi-round loop rather than the single-round `resolve_rebase_conflicts`
  shape.
- Preserve `push_with_sync`'s existing backup-branch behavior (`crates/sgit-core/src/shove.rs:204`)
  and reuse `shove_backup_branch_names` rather than inventing a second naming scheme.
- Failure mode: assuming a single conflict round, which is the defect the prerequisite work removes.

**Acceptance Criteria**
- AC-2.1.a: a four-commit divergence with two conflicting commits rebases to completion → E2E test.
- AC-2.1.b: the ladder ran once per conflicting round, as recorded in the operation summary →
  assertion on the summary.
- AC-2.1.c: `sgit pull` under `Unavailable` is byte-identical to `git pull` → integration test.

**Acceptance Tests**
- Test-2.1.a: E2E — multi-round rebase completion.
- Test-2.1.b: Integration — per-round ladder invocation count.
- Test-2.1.c: Integration — passthrough equivalence for pull.

**Verification Commands**
```bash
cd /opt/worktrees/stokd-cloud/sgit/main
cargo test -p sgit
cargo test --workspace
```

### 2.2 Migration renumbering and symbol relocation resolvers
**Targets:** VAL-REBASE-002, VAL-REBASE-003
**Dependencies:** ["2.1"]

**Implementation Details**
- Add two resolvers above the model tier: migration sequence collision (renumber the incoming
  migration to the next free index, rewrite in-file identifiers and metadata, update snapshot or
  index files) and cross-file symbol relocation (apply the in-place edit to the relocated
  definition and update importers).
- Both run only when triage classifies the conflict into their class; both fall through to the
  ordinary tiered path when they cannot complete, and never leave a partial rewrite.
- Failure mode: a partially-renumbered migration that passes a textual check but fails at deploy;
  the framework's own consistency check is the gate.

**Acceptance Criteria**
- AC-2.2.a: both migrations exist at distinct consecutive numbers with matching internal
  identifiers, no dangling references anywhere in the tree, and the framework's consistency check
  passes → E2E test.
- AC-2.2.b: the relocated symbol exists only at its new location, carries the incoming logic change,
  every importer resolves, and the project compiles → E2E test.
- AC-2.2.c: a resolver that cannot complete falls through without leaving a partial rewrite →
  integration test.

**Acceptance Tests**
- Test-2.2.a: E2E — migration collision fixture.
- Test-2.2.b: E2E — symbol relocation fixture.
- Test-2.2.c: Integration — clean fall-through on partial applicability.

**Verification Commands**
```bash
cd /opt/worktrees/stokd-cloud/sgit/main
cargo test -p sgit-core
cargo test --workspace
```

### 2.3 `sgit cherry-pick` — semantic backport with draft-PR containment
**Targets:** VAL-PICK-001, VAL-PICK-002
**Dependencies:** ["2.1"]

**Implementation Details**
- When a textual pick conflicts, analyze the source commit's intent and re-express it against the
  target branch's architecture, then emit the result as a branch plus a draft pull request using
  the existing GitHub client (`crates/sgit/src/github.rs`).
- Never push to the target branch and never merge. If draft-PR creation is unavailable, exit
  non-zero rather than falling back to any direct write.
- Report what the backport changed relative to a plain pick, so a reviewer can see the bridging.
- Failure mode: a convenience fallback to a direct push, which would defeat the containment this
  assertion exists to guarantee.

**Acceptance Criteria**
- AC-2.3.a: a plain `git cherry-pick` conflicts on the fixture while `sgit cherry-pick` produces a
  compiling result → E2E test.
- AC-2.3.b: a behavioral test failing before the backport passes after it → E2E test.
- AC-2.3.c: the target branch tip is unchanged and a branch plus draft PR were created →
  integration test.
- AC-2.3.d: with draft-PR creation unavailable, the command exits non-zero and performs no write →
  integration test.

**Acceptance Tests**
- Test-2.3.a: E2E — refactored maintenance-branch backport.
- Test-2.3.b: E2E — behavioral before/after.
- Test-2.3.c: Integration — containment, target tip unchanged.
- Test-2.3.d: Security — no fallback to direct push.

**Verification Commands**
```bash
cd /opt/worktrees/stokd-cloud/sgit/main
cargo test -p sgit
cargo test --workspace
```

### 2.4 `sgit bisect`
**Targets:** VAL-BISECT-001
**Dependencies:** []

**Implementation Details**
- Accept or derive a reproduction script; validate that it fails at `bad` and passes at `good`
  before any checkout occurs, refusing the run when it does not discriminate.
- Drive `git bisect run`, then report the culprit commit with the evidence for the verdict.
- Restore the original HEAD and working tree on every exit path, including interruption, using
  `git bisect reset` — never a forbidden operation.
- Failure mode: a non-discriminating script produces a confidently wrong culprit; the pre-flight
  check is the guard.

**Acceptance Criteria**
- AC-2.4.a: the reported culprit equals the known planted commit → E2E test.
- AC-2.4.b: HEAD and the working tree are restored after success, after failure, and after
  interruption → integration test.
- AC-2.4.c: a non-discriminating script is refused before any checkout → integration test.
- AC-2.4.d: no forbidden git operation is recorded by the shim during a bisect run → security test.

**Acceptance Tests**
- Test-2.4.a: E2E — culprit identification against a planted regression.
- Test-2.4.b: Integration — restoration on all three exit paths.
- Test-2.4.c: Integration — pre-flight discrimination refusal.
- Test-2.4.d: Security — forbidden-operation shim.

**Verification Commands**
```bash
cd /opt/worktrees/stokd-cloud/sgit/main
cargo test -p sgit
cargo test --workspace
```

### 2.5 Commit message generation and pre-push sanitization proposals
**Targets:** VAL-COMMIT-001, VAL-COMMIT-002
**Dependencies:** []

**Implementation Details**
- Extract commit-message generation into one function used by both `sgit shove` and `sgit commit`,
  falling back to the existing deterministic message (`crates/sgit-core/src/shove.rs:769`) when no
  model is available.
- Implement `sgit push --preflight` as a report-only scan of the outgoing commits: squash
  groupings, conventional-commit rewrites, leftover debug statements, and documentation the change
  implies. It applies nothing; a separate explicit invocation applies a selected proposal.
- Failure mode: any code path that rewrites history during a preflight; the rev-list equality check
  is the gate.

**Acceptance Criteria**
- AC-2.5.a: both call sites resolve to the same generation function → code inspection plus grep.
- AC-2.5.b: `sgit commit` with no provider configured uses the deterministic fallback and succeeds
  → integration test.
- AC-2.5.c: preflight reports every planted proposal → integration test.
- AC-2.5.d: `git rev-list <upstream>..HEAD` and the working tree are unchanged after preflight →
  integration test.

**Acceptance Tests**
- Test-2.5.a: Unit — shared generation function.
- Test-2.5.b: Integration — deterministic fallback with no provider.
- Test-2.5.c: Integration — proposal completeness on a planted messy branch.
- Test-2.5.d: Regression — history and working tree untouched.

**Verification Commands**
```bash
cd /opt/worktrees/stokd-cloud/sgit/main
cargo test -p sgit-core
cargo test -p sgit
cargo test --workspace
```

### 2.6 Unify the conflict-resolution skill on the Rust scorer
**Targets:** VAL-SKILL-001
**Dependencies:** []

**Implementation Details**
- Change the skill's Phase 2 step to call `sgit conflict triage --json` when `sgit` is on PATH,
  keeping `scripts/conflict-triage.sh` as the documented fallback for when it is not.
- Update the skill's tables to cite the Rust implementation as primary and the shell script as the
  fallback, so the two cannot be read as independent sources of truth.
- Change the skill template in `apps/cli/templates/skills/` if the installed copy is generated from
  it, so the change survives reinstallation.
- Failure mode: editing only the installed copy, which is overwritten on the next skill deploy.

**Acceptance Criteria**
- AC-2.6.a: both scorers assign identical tiers for all fourteen fixtures → integration test.
- AC-2.6.b: the skill document names `sgit conflict triage --json` as primary → grep.
- AC-2.6.c: with `sgit` absent from PATH, the skill's documented fallback still produces a tier →
  integration test.

**Acceptance Tests**
- Test-2.6.a: Integration — cross-implementation tier equality over the corpus.
- Test-2.6.b: Structural — skill text cites the CLI as primary.
- Test-2.6.c: Integration — fallback path with `sgit` absent.

**Verification Commands**
```bash
cd /opt/worktrees/stokd-cloud/sgit/main
SKILL=""; for r in "$HOME/.claude/skills" "$HOME/.stokd/skills"; do [ -f "$r/conflict-resolution/SKILL.md" ] && SKILL="$r/conflict-resolution/SKILL.md" && break; done
test -n "$SKILL" || { echo "conflict-resolution SKILL.md not found in either skills root"; exit 1; }
grep -q 'sgit conflict triage' "$SKILL"
cargo test --workspace
```

### 2.7 Documentation and the axioms this project owes
**Targets:** VAL-DOC-001
**Dependencies:** ["2.1", "2.3", "2.4", "2.5"]

**Implementation Details**
- Document in the sgit README: the escalation ladder stage by stage, the passthrough guarantee, the
  `agentic:` keys with defaults, and the statement that `sgit` is not a drop-in `git` replacement.
- Author the `[new] AX-*` entries the authoring task's rejection identified, each with Why, How to
  apply, and runnable Acceptance Checks:
  `AX-AGENT-CORE-SINGLE-CONFIG-SOURCE` (provider/model config has one definition, consumed by both
  binaries); `AX-SGIT-AGENTIC-FAILS-CLOSED` (no model configured or available means byte-identical
  git passthrough and zero model calls); `AX-SGIT-AGENTIC-MECHANICAL-FIRST` (no model call before
  preflight, plain git, mechanical resolution, and triage have run);
  `AX-SGIT-AGENTIC-BOTH-SIDES-SURVIVE` (no resolution is staged without the survival gate passing).
- Each axiom's Acceptance Checks must be commands that exist and pass at the time of writing.

**Acceptance Criteria**
- AC-2.7.a: the README contains a section naming every ladder stage in order, every `agentic:` key,
  and the non-equivalence statement → grep.
- AC-2.7.b: `.axioms.md` contains all four new `AX-*` slugs, each with Why, How to apply, and
  Acceptance Checks sections → grep.
- AC-2.7.c: every command in every new axiom's Acceptance Checks exits 0 → executed check.

**Acceptance Tests**
- Test-2.7.a: Structural — README section completeness.
- Test-2.7.b: Structural — axiom entry completeness.
- Test-2.7.c: Integration — every axiom acceptance check executes and passes.

**Verification Commands**
```bash
cd /opt/worktrees/stokd-cloud/sgit/main
for s in AX-AGENT-CORE-SINGLE-CONFIG-SOURCE AX-SGIT-AGENTIC-FAILS-CLOSED AX-SGIT-AGENTIC-MECHANICAL-FIRST AX-SGIT-AGENTIC-BOTH-SIDES-SURVIVE; do grep -q "$s" .axioms.md || exit 1; done
grep -q 'not a drop-in' README.md
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

---

## 4. Completion Criteria

The project is complete when all of the following hold simultaneously:

- Every assertion in `## 2. Contract` has satisfied the evidence obligations of its rigor tier, with
  the parity assertions adjudicated against the retained pre-extraction `stokd` binary.
- `crates/agent-core` is the only definition of the provider and model configuration types across
  both repositories, and `apps/cli` re-exports rather than redeclares them.
- In the sgit repo: `cargo build -p sgit -p sgit-core -p agent-core`, `cargo test --workspace`, and
  `cargo clippy --workspace --all-targets -- -D warnings` all exit 0, with the test count at or
  above the 171-test baseline plus the new tests.
- In `apps/cli`: `cargo test --manifest-path Cargo.toml` and
  `cargo clippy --manifest-path Cargo.toml -- -D warnings` exit 0, with the passing test count at or
  above the recorded pre-extraction baseline.
- The differential harness reports zero divergences across the 12-document configuration corpus.
- All **fourteen** conflict-corpus fixtures reach their recorded expected outcome. A run that
  resolves fewer than fourteen names the shortfall explicitly and is not complete.
- Across the full corpus, the recording `git` shim logs zero forbidden operations.
- With no provider configured, every augmented verb is byte-identical to plain `git` on both the
  success and failure paths, with a zero model-call count.
- The four new `AX-*` axioms exist with runnable Acceptance Checks that pass.
- The `conflict-resolution` skill and the Rust scorer assign identical tiers for all fourteen
  fixtures.

---

## 5. Rollout & Validation

### Rollout Strategy

- **Order.** The prerequisite shove-termination work lands first. Phase 1 then lands as one change
  set per work item, in dependency order, each landing green.
- **The stokd repoint is the highest-risk step and lands alone.** Work item 1.5 is landed as its own
  change with no other work item in the same commit, so a parity regression has exactly one
  candidate cause. The pre-extraction binary and its recorded reference output are retained until
  the project completes.
- **Submodule coordination.** Every `agent-core` change requires advancing the `apps/sgit` gitlink
  and `apps/cli/Cargo.lock` in the mono. The two repositories are never left where the mono's pin
  references an `agent-core` the mono does not compile against.
- **Default exposure.** Phase 1 ships with the augmented verbs present but with the enablement
  decision deferred to the Phase 1 stop. Until that stop is answered, the safe posture is the one
  the gate already guarantees: unconfigured or unavailable means plain git.
- **Rollback triggers.** Any of the following reverts the change set that introduced it, without
  discussion: a divergence reported by the differential harness; a drop in stokd's passing test
  count; a forbidden git operation recorded by the shim; a survival-gate false negative, meaning
  any fixture where work was lost and the gate did not fire.
- **Rollback mechanism.** Reverting the `apps/cli` repoint commit and the submodule pin restores
  stokd to the pre-extraction code path. `agent-core` remaining in the sgit workspace is inert for
  stokd once the path dependency is removed.

### Post-Launch Validation

- The per-operation summary (model-call count, tier, per-call reason, elapsed time) is the primary
  instrument. Watch the ratio of zero-cost operations to model-spending operations; a fall in that
  ratio means the mechanical stages are regressing.
- Watch the tier distribution. A drift toward T3 means either the criticality table is
  mis-classifying paths or the corpus no longer represents real conflicts.
- Watch verification-loop round counts. A rising mean, especially runs hitting `maxRounds`, means
  the remediation prompts are not converging and the loop is burning budget.
- Watch the frequency of survival-gate refusals. Zero refusals over a long period is suspicious,
  not reassuring — it suggests the gate is not actually evaluating.
- Watch provider cooldown records attributable to sgit. A rise means sgit is competing with stokd
  for the same rate-limited providers, which is a scheduling problem the shared ledger makes
  visible.
- Re-run the fourteen-fixture corpus on every release of the sgit binary; it is the regression
  suite for the whole feature.

---

## 6. Open Questions

- **Which verbs ship enabled by default.** Deferred by design to the Phase 1 stop; the measured
  cost per ladder stage is the input, and it does not exist until Phase 1 runs.
- **Whether the T1/T2/T3 thresholds (7 and 25) are right for this operator's repositories.** They
  are inherited from the published tables and are unvalidated against this tree's actual conflict
  distribution. Also resolved at the Phase 1 stop.
- **`WorkPlanConfig` and `DefinitionOfDone`.** These are referenced by `WorkloadPolicy` but live in
  stokd's orchestration layer. Work item 1.1 must choose between moving their minimal type
  definitions into `agent-core` and generifying the two fields. The choice is bounded and local, so
  it is left to implementation rather than pre-decided here, but it is the one place where the
  extraction boundary could grow unexpectedly.
- **Whether `agent-core` should eventually absorb stokd's full `AgentBackend` surface.** Out of
  scope here — only the headless one-shot path moves. If sgit later needs interactive dispatch,
  that is a separate project with its own parity obligation.
- **Publishing.** `agent-core` is consumed by path through the submodule, so no crates.io publish
  is required. Whether it should also be published for third-party use is a product decision, not a
  technical one, and is not answered here.
