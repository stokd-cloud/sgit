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

## `sgit checkout <branch>`

`sgit checkout` is branch/worktree navigation for the repository containing the
current directory. It must be run from inside a Git worktree. Every target is a
branch name, including names containing `/`; checkout never performs repository
owner resolution or cloning.

It **never** switches the current worktree's branch in place (pinned worktrees
refuse that). Instead it:

1. Reuses an existing linked worktree already on `<branch>`, or
2. Creates a new sibling worktree under the configured worktree root in a folder
   named for the branch (slashes sanitized to dashes). A stale registration for
   a manually deleted worktree is pruned and recreated, then
3. Prints the absolute path (the shell `sgit()` wrapper `cd`s into it).

```bash
# From any worktree of the repo (e.g. main):
sgit checkout feature/login   # → /opt/worktrees/owner/repo/feature-login
sgit checkout main            # reuses the existing main worktree
```

Use the explicit repository commands when the target is a repository:

```bash
sgit clone stokd-cloud/sgit
sgit open sgit                 # bare repo names resolve owners here
sgit create stokd-cloud/new-repo
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

## `sgit pull` — the escalation ladder

`git pull` makes you pick a strategy up front, then punishes the wrong guess.
`sgit pull` walks the strategies in order of increasing cost, so you only pay
for the friction you actually hit:

1. **Fast-forward.** Cannot conflict, cannot rewrite, cannot lose anything.
2. **Rebase.** Linear history — but it replays each local commit separately, so
   one textual conflict can be presented once *per replayed commit*.
3. **Merge.** A merge commit, but it resolves **once** against the final tree.

When the rebase conflicts, sgit aborts it (a stopped rebase has committed
nothing, so nothing is lost) and retries as a merge — strictly less painful to
resolve. Only if the *merge* conflicts does the conflict resolver run: `$EDITOR`
in sgit, an agent in stokd.

```bash
sgit pull              # ff → rebase → merge
sgit pull --ff-only    # refuse to escalate; fail instead
sgit pull --no-rebase  # ff → merge, skipping the rebase rung
```

Safety properties:

- **Missing upstream is set for you.** No more
  `There is no tracking information for the current branch` — if
  `origin/<branch>` exists, sgit points the branch at it and continues.
- **Both sides are snapshotted** before any escalation, as
  `sgit-pull-backup/<branch>/<stamp>-local` and `…-remote`. Neither side can be
  lost even if resolution goes wrong.
- **Never stashes.** A dirty tracked working tree is a hard refusal naming the
  files, before anything is fetched or merged. Untracked files are fine.
- **Failed resolution is left in place.** The conflicted merge, `MERGE_HEAD`,
  and both backups stay put so you can finish by hand.

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
  site/                   # static public site (sgit.selfactor.io)
  terraform/              # reusable module + live/ standalone root
  package.json            # pnpm done / done:prod / done:stage / done:local / done:plan
```

Full crate export lands in a follow-on extraction; this bootstrap keeps a clean public home with MIT license and docs.

---

## Development

```bash
cargo test --workspace
cargo build -p sgit -p sgit-core
```

## Deploy (`sgit.selfactor.io`)

This repo is CLI-first. There is no web/UI crate. The first deployable is a static public site (install + docs from this README) at **https://sgit.selfactor.io**.

From this directory, after AWS credentials and DNS vars are set:

```bash
pnpm done          # terraform apply, prod (sgit.selfactor.io)
pnpm done:prod     # same as done
pnpm done:stage    # terraform apply, stage (sgit-stage.selfactor.io)
pnpm done:plan     # terraform plan (STOKD_DONE_DRY_RUN=1)
pnpm done:force    # apply -auto-approve
pnpm done:local    # serve ./site locally; no AWS apply
```

`done` / `done:prod` / `done:stage` run `terraform init` then `apply` for that environment. They do not reproduce the full mono `scripts/done.sh`.

The GitHub repository can stay private. The site is a static snapshot in `site/` and does not fetch from GitHub at request time. If `assets/logo.svg` is present (see PR #19), `scripts/build-site.sh` copies it into the site; otherwise the image is omitted.

### Required AWS / DNS vars

`sgit.selfactor.io` is a subdomain of `selfactor.io`. Use the **existing** Route53 hosted zone — do not create a second zone.

| Variable | Purpose |
|----------|---------|
| `SGIT_HOSTED_ZONE_ID` | Hosted zone ID for `selfactor.io` (already in the SST account) |
| `SGIT_AWS_REGION` | AWS region for S3 (default `us-east-1`) |
| `SGIT_AWS_ACCOUNT_ID` | Optional. Fail apply if credentials are for another account |
| `SGIT_DOMAIN` | Optional FQDN override |
| `TF_STATE_BUCKET` | S3 bucket for Terraform state |
| `TF_STATE_KEY` | State key (default `sgit/<env>/terraform.tfstate`) |
| `TF_STATE_LOCK_TABLE` | DynamoDB lock table |
| `TF_STATE_REGION` | State bucket region (default `us-east-1`) |

Alternatively copy `terraform/live/backend.hcl.example` → `terraform/live/backend.hcl` and `terraform/live/terraform.tfvars.example` → `terraform/live/terraform.tfvars` (both gitignored).

The ACM certificate is issued in **us-east-1** (CloudFront requirement) and validated via DNS records in `SGIT_HOSTED_ZONE_ID`.

Remote state is S3 + DynamoDB so it can later move into a mono workspace. This repo does not create the state bucket or lock table.

`pnpm done:local` is a no-AWS preview (`python3 -m http.server` on port 4173).

If credentials or the hosted zone are unavailable, `pnpm done:plan` still runs `terraform validate` and skips a remote plan. Do not fake an apply.

### Terraform layout (later mono roll-in)

- [`terraform/`](terraform/) is the reusable module (`variables` in, `outputs` out).
- [`terraform/live/`](terraform/live/) is the standalone root used by `pnpm done`.

When [stokd-cloud/mono](https://github.com/stokd-cloud/mono) is converted from SST to Terraform, consume this stack without a rewrite:

```hcl
module "sgit" {
  source = "./apps/sgit/terraform"

  providers = {
    aws           = aws
    aws.us_east_1 = aws.us_east_1
  }

  hosted_zone_id  = var.selfactor_hosted_zone_id
  domain_name     = "sgit.selfactor.io"
  environment     = "prod"
  site_source_dir = "${path.root}/apps/sgit/site"
}
```

Details: [`terraform/README.md`](terraform/README.md).

## License

MIT — see [LICENSE](./LICENSE).
