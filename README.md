# bd-gh-sync

Real-time, two-way sync between [beads](https://github.com/gastownhall/beads)
(`bd`) and GitHub issues.

beads (v0.60+) already knows how to sync with GitHub, but only when someone runs
`bd github push` or `bd github pull`. This repo runs those commands for you, the
moment something changes:

| Direction | What runs | When |
|---|---|---|
| GitHub → beads | a GitHub Action ([`action.yml`](action.yml)) running `bd github pull` for every issue updated since its last sync | a minute after the last of a burst of `issues` / `issue_comment` events, every 15 minutes, and a full reconcile every 6 hours |
| beads → GitHub | a local watcher ([`bin/bd-gh-watch`](bin/bd-gh-watch)) running `bd github push <ids>` | within a second or two of a local bead change |

What syncs, both ways:

| GitHub | beads |
|---|---|
| title, body, open/closed | title, description, status |
| labels (`priority::`, `type::`, `status::` scoped) | labels, priority, type, status |
| first assignee (login) | assignee |
| comments | comments |
| sub-issue of #N | `parent-child` dependency on #N's bead |
| blocked by #N | `blocks` dependency on #N's bead |

```
 GitHub issue edited ──issues event──▶ Action: bd github pull N ──▶ bd dolt push (or commit issues.jsonl)
                                                                         │
                                                                         ▼
 GitHub issue updated ◀── bd github push <ids> ◀── bd-gh-watch ◀── local beads (bd dolt pull / git pull)
```

Tested with beads 1.3.1.

## Setup

### 1. Share the beads database through the repository

The Action needs to read and write the same beads the team uses. Two transports
are supported, and the Action picks one automatically (`transport: auto`):

- **Dolt remote (beads' default).** `bd init` in a git repo sets `sync.remote`
  to your origin, and beads stores its Dolt data under `refs/dolt/data` in the
  same GitHub repository. Run `bd dolt push` once from your clone so the data
  exists on GitHub; after that the Action uses `bd dolt pull` / `bd dolt push`.
  Clones pick up GitHub changes with `bd dolt pull` (or `bd-gh-watch --dolt-sync`).
- **JSONL export.** For repos without a Dolt remote (`bd config unset
  sync.remote`), the Action commits `.beads/issues.jsonl`. Set `import.auto: true`
  and `export.auto: true` in `.beads/config.yaml` so clones import it on
  `git pull` and export on commit.

If `sync.remote` is set but nobody has pushed Dolt data yet, the Action stops
and tells you to run `bd dolt push`, rather than starting a second, unrelated
Dolt history.

The Action keeps a little state with the beads: the time of the last sync and
the relations it saw on GitHub then (`bd kv get bd-gh-sync.since` /
`bd-gh-sync.relations` for Dolt, `.beads/github-sync.json` for JSONL).

### 2. Add the workflow

Copy [`.github/workflows/beads-sync.yml`](.github/workflows/beads-sync.yml) into
your repository and change `uses: ./` to `uses: Takashiidobe/bd-gh-sync@main`.
It needs `contents: write` (to publish the beads) and `issues: read`.

The workflow batches. Each issue or comment event starts a `debounce` job that
sleeps for a minute; a newer event cancels the waiting one (those runs show up
as cancelled), so a burst of edits ends in a single `sync` job. That job pulls
every issue updated since the last published sync (`issues: since`), so it
does not matter which event started it. Sub-issue and blocked-by changes send
no workflow event at all; every sync reconciles them for the whole repository,
and the 15-minute schedule picks them up when nothing else happens.

Action inputs:

| Input | Default | Meaning |
|---|---|---|
| `issues` | `since` for events and schedules, `all` on dispatch | issue numbers to pull, `since` (updated since the last sync), or `all` |
| `token` | `github.token` | token for reading issues and publishing |
| `transport` | `auto` | `dolt`, `jsonl` or `auto` (see above) |
| `bd-version` | `1.3.1` | beads release to install (from the GitHub release) |
| `jsonl-path` | `.beads/issues.jsonl` | export path for the JSONL transport |
| `adopt-bd-created` | `false` | also import bd-created issues whose bead isn't published (see loop guards) |
| `commit-message` | `beads: sync from GitHub` | JSONL commit message |

### 3. Run the watcher locally

```sh
bd config set github.repository owner/repo   # once per clone
export GITHUB_TOKEN=...                      # or be logged in with `gh auth login`
bin/bd-gh-watch --dolt-sync 30
```

Requirements: bash 4+, `jq`, `bd`, and `inotifywait` (Linux, `inotify-tools`) or
`fswatch` (macOS). With neither installed it polls. Options:

| Option | Default | Meaning |
|---|---|---|
| `--poll SECONDS` | `30` | safety-net poll (and the interval when polling) |
| `--debounce SECONDS` | `1` | quiet period that ends a burst of writes |
| `--backend NAME` | auto | `inotify`, `fswatch` or `poll` |
| `--dolt-sync SECONDS` | `0` (off) | `bd dolt pull` on this interval to bring in what the Action pulled, and `bd dolt push` after each GitHub push to publish new issue links |
| `--initial-push` | off | on the first run, push every bead instead of only later changes |
| `--once` | | one pass, then exit (handy in hooks or cron) |
| `--dry-run` | | show what would be pushed |

The watcher keeps its per-clone state in `.git/bd-gh-sync/`, so it catches up on
changes made while it was stopped. It talks to the GitHub REST API with `curl`
for assignees, comments and relations (`GITHUB_API_URL` for GitHub Enterprise).

## Sync loops

A change must never bounce between the two sides. The guards, and the test in
[`tests/run.sh`](tests/run.sh) that proves each one:

1. **The Action only pulls.** It never writes to GitHub issues, so it cannot
   generate issue events. It publishes with `GITHUB_TOKEN`, whose pushes never
   start workflows, and publishes nothing when the pull changed nothing.
2. **The watcher pushes only real edits.** It fingerprints the fields a push
   sends (title, description, status, priority, type, labels) and pushes only
   beads whose fingerprint changed since their last push. bd's own write-back
   after a push (`external_ref`, `updated_at`) doesn't count, and the
   filesystem events caused by the watcher's own `bd` calls are drained.
3. **beads skips no-op writes.** `bd github push` compares against GitHub (a
   stored content hash, then a field comparison) and skips the API call when
   nothing differs. So when a GitHub edit arrives locally and the watcher sees
   the bead change, the push is a read, not a write, and no new event fires.
4. **One GitHub issue, one bead.** `bd github pull` gives new beads random IDs,
   so if the Action imported an issue that a local `bd github push` had just
   created, the issue would end up with two beads. Issues created by bd carry
   both a `type::` and a `priority::` label; the Action pulls such an issue only
   once a bead linked to it has been published (`bd dolt push`, or the JSONL
   commit). Until then it logs a skip, and the 6-hourly reconcile catches up.
   `--dolt-sync` publishes links right after each push, which keeps that
   window short.

5. **Comments, assignees and relations are compared, not replayed.** The watcher
   posts bead comments with a hidden `<!-- bd-comment:ID -->` marker, and the
   Action never imports a marked comment. A comment the Action imported (same
   author and text) is already on GitHub, so the watcher only records it. The
   watcher sets an assignee only when GitHub's first assignee differs, and adds
   or removes a relation only when GitHub doesn't already match.
6. **Relations merge three ways.** The Action remembers which relations GitHub
   had at its last sync. A relation that appears on GitHub is added to the
   beads; one that disappears from GitHub is removed from them; a bead-only
   relation that the watcher hasn't pushed yet is left alone.

One thing that does write once: when a bead imported from a GitHub-created
issue is first pushed, bd adds its `type::` and `priority::` labels (and
`status::` for in-progress, blocked or deferred beads) to the issue. That fires one more `labeled` event, which the Action pulls as a
no-op, and then everything is quiet.

## Notes and limits

- beads' GitHub sync covers title, body, open/closed state, labels (beads
  status, priority and type travel as scoped labels) and, on pull, the first
  assignee. This repo adds assignee pushes, comments and relations.
- A bead's assignee must be a GitHub login to reach GitHub; the watcher logs
  and skips one that GitHub refuses. A bead holds one assignee, so setting it
  locally replaces the issue's assignees with that one login.
- Comments sync when created. Edits and deletions of existing comments don't
  sync, and comments the watcher posts appear under the token's owner.
- Only `parent-child` and `blocks` dependencies map to GitHub. GitHub allows
  one parent per issue, so a bead's second parent is refused (and logged).
  Relations to issues in other repositories are ignored.
- `bd github pull` and `bd github push` leave their writes uncommitted in the
  Dolt working set; both the Action and the watcher run `bd dolt commit` so the
  changes reach the Dolt remote.
- Deleting a bead does not close its issue, and deleting an issue does not
  delete its bead.
- When a bead and its issue are both edited before either side syncs, the
  watcher's next push of that bead wins on GitHub.

## Development

```sh
tests/run.sh   # needs bd, jq, git, curl, python3; uses a fake GitHub API, no network
```

[`tests/fake_github.py`](tests/fake_github.py) is a tiny in-memory GitHub issues
API; `bd` talks to it through `GITHUB_API_URL`.
