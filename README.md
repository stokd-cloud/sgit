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
```

**Shell helper `scd`:** source a small function that runs `sgit cd` and `cd`s for you:

```bash
# ~/.zshrc or ~/.bashrc
scd() {
  local target
  target="$(sgit cd "$@")" || return $?
  cd "$target" || return $?
}
```

Then:

```bash
scd owner/repo
scd owner/repo feature-branch
```

`scd` is a thin shell wrapper around `sgit cd`. After the stokd hard-cut, `scd` depends on the `sgit` binary (not on `stokd`).

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
