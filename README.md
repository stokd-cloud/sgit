# sgit

Standalone **git / worktree CLI** with a **local knowledge graph** — no stokd account required.

**Repository:** [github.com/stokd-cloud/sgit](https://github.com/stokd-cloud/sgit)  
**License:** MIT

`sgit` is the public, installable product for repo navigation (`sgit cd` / shell `scd`) and offline graph scan/query. [stokd](https://github.com/stokd-cloud/stokd-mono) optionally **syncs** local graph data into org cloud UI; it is not required to use sgit.

---

## Install

### From source (Cargo)

```bash
git clone https://github.com/stokd-cloud/sgit.git
cd sgit
cargo install --path crates/sgit
```

Or, once published on crates.io:

```bash
cargo install sgit
```

Ensure `~/.cargo/bin` is on your `PATH`.

### Binary install

When using the stokd installer surface, `sgit` is also shipped alongside stokd tooling. A pure sgit install needs only this repo (or a release binary) — **no mono checkout and no stokd credentials**.

---

## `scd` and `sgit cd`

`sgit cd` resolves a repo (or worktree) and prints the path to `cd` into:

```bash
sgit cd owner/repo
sgit cd owner/repo main
sgit cd my-repo
sgit cd gdock upstream-main          # exact leaf
sgit cd gdock upstream-ag-           # unique prefix partial → e.g. upstream-ag-brand-quad
```

**Leaf resolution order:** exact leaf name (and task/project slug conventions) → **unique prefix partial** among leaf directories → git branch → worktree lookup. Ambiguous prefixes error with the matching candidates listed.

**Shell helper `scd`:** the stokd/sgit installer writes `~/.stokd/shell/sgit-cd.sh` (sourced from your shell rc) with `scd` / `sgit` functions that run `sgit cd` and `cd` for you, plus **zsh/bash tab completion** for repo and leaf:

```bash
scd owner/repo
scd owner/repo feature-branch
scd gdock upstream-ag-<TAB>   # completes leaves under gdock
```

`scd` is a thin shell wrapper around `sgit cd`. After the stokd hard-cut, `scd` depends on the `sgit` binary (not on `stokd`).

---

## `sgit checkout <branch | owner/repo | reponame>`

Two modes, chosen by the target shape (and existing local branches):

### Repo target — `owner/repo` or bare `reponame`

Ensures the bare + main worktree layout and prints the main worktree path
(shell `sgit()` wrapper `cd`s into it). Creates missing parent directories,
bare-clones when needed, and re-materializes a destination that exists without
a valid git connection when it is safe to do so.

```bash
sgit checkout stokd-cloud/sgit   # → /opt/worktrees/stokd-cloud/sgit/main
sgit checkout sgit               # bare name; owner resolved like clone/open
```

### Branch target — sibling worktree (never in-place)

**Never** switches the current worktree's branch in place (pinned worktrees
refuse that). Instead it:

1. Reuses an existing linked worktree already on `<branch>`, or
2. Creates a new sibling worktree under the configured worktree root in a folder
   named for the branch (slashes sanitized to dashes), then
3. Prints the absolute path (the shell `sgit()` wrapper `cd`s into it).

```bash
# From any worktree of the repo (e.g. main):
sgit checkout feature/login   # → /opt/worktrees/owner/repo/feature-login
sgit checkout main            # reuses the existing main worktree
```

Branch source when creating:

| Situation | Action |
|-----------|--------|
| Local branch exists | Check it out in the new worktree |
| Only `origin/<branch>` | Create a tracking local branch |
| Neither | Cut a new branch from `origin/<default>` (fallback: current HEAD) |

New worktrees are pin-marked so they cannot later be repointed at another branch.

Classification while inside a git repo: existing branches and names under
`feature/` / `task/` / `project/` / `fix/` / … are always branch targets.
GitHub-shaped `owner/repo` is a repo target (falls back to a sibling branch
worktree if ensure fails). Outside a git repo, every target is treated as a repo.

---

## Repo lifecycle

`clone`, `open`, and `create` are top-level verbs — no `repo` group needed:

```bash
sgit clone owner/repo        # bare clone + main worktree
sgit clone repo              # owner resolved automatically (see below)
sgit open repo               # clone if needed, then open in your editor
sgit create repo             # create on GitHub under your account + local layout
```

A **bare repo name** is resolved to a single owner by walking a chain and
accepting the answer only when it is unambiguous:

1. **Local layout** — owners that already have the repo bare-cloned under
   `bareRoot` or checked out under `root`. Fully offline.
2. **Your GitHub owners** — your login plus the orgs you belong to. Consulted
   only when the local layout knows nothing, so cloning an already-provisioned
   repo never touches the network.

If two owners match, sgit refuses to guess and asks you to qualify:

```
error: repo 'widget' is ambiguous across local clones: alpha/widget, beta/widget; qualify with <owner/repo>
```

`sgit create <name>` has no chain to walk (the repo does not exist yet), so a
bare name is created under your own GitHub account — matching `gh repo create`.

The remaining lifecycle verbs stay under the group: `sgit repo list`,
`sgit repo rename`, `sgit repo migrate`. The old `sgit repo clone|open|create`
spellings still work as hidden back-compat aliases.

---

## Local knowledge graph

sgit includes a **local-first** repo knowledge graph. Data lives under your control (default file store under `~/.sgit/graph/…`, or optional Mongo). **No stokd account** is required.

### Working commands

| Command | Purpose |
|---------|---------|
| `sgit graph scan [repo\|--all] [--dry-run] [--json]` | Scan manifests into a named graph |
| `sgit graph show <repo> [--json]` | Show components, deps, suites |
| `sgit graph suite create\|add\|remove\|list` | Manage manual suites |
| `sgit graph config` | Show effective storage backend |
| `sgit graph query <expr>` | Query the graph (second-brain style) |
| `sgit graph note …` | Manual nodes / edges |

Examples:

```bash
# Scan current repo into the default graph (file backend)
sgit graph scan --json

# Dry-run scan across configured repos
sgit graph scan --all --dry-run --json

# Inspect one repo’s components and edges
sgit graph show owner/repo --json

# Confirm storage backend (file | mongodb)
sgit graph config
```

### Config (optional)

`~/.sgit/config.yaml` (env overrides available):

```yaml
graph:
  default_name: default
  storage:
    backend: file          # or mongodb
    # file:
    #   root: ~/.sgit/graph
    # mongodb:
    #   uri: mongodb://127.0.0.1:27017
    #   database: sgit_graph
```

Env overrides: `SGIT_GRAPH_BACKEND`, `SGIT_GRAPH_NAME`, `SGIT_GRAPH_ROOT`, `SGIT_MONGO_URI`, `SGIT_MONGO_DB`.

---

## Relation to stokd

| Concern | Owner |
|---------|--------|
| `sgit` binary, `scd` / `sgit cd`, local graph scan/query | **This public repo** |
| Org graph UI, multi-tenant API, land hooks | **stokd** (optional cloud plane) |
| Mono development | stokd-mono vendors this repo as a git submodule under `apps/sgit` |

Stokd may later offer `stokd graph sync` to project a local/Mongo sgit graph into org storage. Day-to-day second-brain and offline workflows stay on **sgit alone**.

---

## Workspace layout (target)

```text
sgit/
  Cargo.toml              # workspace
  LICENSE                 # MIT
  README.md
  crates/
    sgit-core/            # library
    sgit/                 # CLI binary
    sgit-graph/           # graph engine + GraphStore trait
    sgit-graph-mongo/     # optional Mongo backend
```

Full crate export lands in a follow-on extraction; this bootstrap keeps a clean public home with MIT license and docs.

---

## Development

```bash
cargo test --workspace
cargo build -p sgit -p sgit-core
```

## License

MIT — see [LICENSE](./LICENSE).
