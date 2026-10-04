# bd-gh-sync

Real-time, two-way sync between [beads](https://github.com/gastownhall/beads)
(`bd`) and GitHub issues.

beads (v0.60+) already knows how to sync with GitHub, but only when someone runs
`bd github push` or `bd github pull`. This repo's `bd-gh-sync` binary runs those
commands for you, the moment something changes:

| Direction | What runs | When |
|---|---|---|
| GitHub → beads | a webhook server (`bd-gh-sync server`) on an always-on machine, for any number of repositories | a couple of seconds after the last of a burst of issue, comment, sub-issue or blocked-by changes |
| GitHub → beads | or a GitHub Action ([`action.yml`](action.yml)) running `bd github pull` for every issue updated since its last sync | a minute after the last of a burst of `issues` / `issue_comment` events, every 15 minutes, and a full reconcile every 6 hours |
| beads → GitHub | a local watcher (`bd-gh-sync watch`) running `bd github push <ids>` | within a second or two of a local bead change |

What syncs, both ways:

| GitHub | beads |
|---|---|
| title, body, open/closed | title, description, status |
| labels (`priority::`, `type::`, `status::` scoped) | labels, priority, type, status |
| first assignee (login) | assignee |
| close reason (`completed`, `not_planned`, `duplicate`) | close reason (`Completed`, `Not planned`, `Duplicate`) |
| closed as duplicate of #N | closed bead with a `supersedes` dependency on #N's bead |
| comments | comments |
| sub-issue of #N | `parent-child` dependency on #N's bead |
| blocked by #N | `blocks` dependency on #N's bead |
| mention of #N (`Ref: #N`), GitHub to beads only | `related` dependency on #N's bead |
| open pull request that closes the issue (`Fixes #N`), GitHub to beads only | bead goes `in_progress` and gets a "Linked pull request" comment |

```
 GitHub issue edited ──issues event──▶ Action: bd github pull N ──▶ bd dolt push (or commit issues.jsonl)
                                                                         │
                                                                         ▼
 GitHub issue updated ◀── bd github push <ids> ◀── bd-gh-sync watch ◀── local beads (bd dolt pull / git pull)
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
  Clones pick up GitHub changes with `bd dolt pull` (or `bd-gh-sync watch --dolt-sync`).
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

### 2. Pull GitHub changes: the Action or the server

Use the [webhook server](#webhook-server) if you have an always-on machine: it
is faster and sees sub-issue and blocked-by changes as they happen. Otherwise
add the workflow:

Copy [`.github/workflows/beads-sync.yml`](.github/workflows/beads-sync.yml) into
your repository and change `uses: ./` to `uses: Takashiidobe/bd-gh-sync@main`.
It needs `contents: write` (to publish the beads) and `issues: read`. The
Action downloads a `bd-gh-sync` release binary for the runner (Linux x86_64 or
aarch64, macOS aarch64).

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
| `version` | `latest` | `bd-gh-sync` release to run, e.g. `v0.1.0` |
| `token` | `github.token` | token for reading issues and publishing |
| `transport` | `auto` | `dolt`, `jsonl` or `auto` (see above) |
| `bd-version` | `1.3.1` | beads release to install (from the GitHub release) |
| `jsonl-path` | `.beads/issues.jsonl` | export path for the JSONL transport |
| `adopt-bd-created` | `false` | also import bd-created issues whose bead isn't published (see loop guards) |
| `commit-message` | `beads: sync from GitHub` | JSONL commit message |

### 3. Run the watcher locally

```sh
scripts/install-bd-gh-sync.sh                # or: cargo install --git https://github.com/Takashiidobe/bd-gh-sync
bd config set github.repository owner/repo   # once per clone
export GITHUB_TOKEN=...                      # or be logged in with `gh auth login`
bd-gh-sync watch --dolt-sync 30
```

It needs `bd` on the `PATH` (or in `$BD`). It watches `.beads/` with the
platform's file events (inotify, FSEvents, ...) and falls back to polling when
those are unavailable. Options:

| Option | Default | Meaning |
|---|---|---|
| `--poll SECONDS` | `30` | safety-net poll (and the interval when polling) |
| `--debounce SECONDS` | `1` | quiet period that ends a burst of writes |
| `--backend NAME` | `native` | `native` (file events) or `poll` |
| `--dolt-sync SECONDS` | `0` (off) | `bd dolt pull` on this interval to bring in what the Action pulled, and `bd dolt push` after each GitHub push to publish new issue links |
| `--initial-push` | off | on the first run, push every bead instead of only later changes |
| `--once` | | one pass, then exit (handy in hooks or cron) |
| `--dry-run` | | show what would be pushed |

The watcher keeps its per-clone state in `.git/bd-gh-sync/`, so it catches up on
changes made while it was stopped (one watcher per clone; a second one refuses
to start). It talks to the GitHub REST API directly for assignees, comments and relations (`GITHUB_API_URL` for GitHub Enterprise).

## Webhook server

One always-on machine (a small VM is plenty) can keep many repositories in
sync. Each project is a clone under `/var/lib/bd-gh-sync/<owner>/<name>`;
GitHub webhooks queue syncs, which are debounced per project (2s quiet, at most
10s) and run one at a time per project. Every project is also reconciled at
startup and hourly, so a missed webhook costs at most an hour. The server only
pulls, like the Action; local bead changes still reach GitHub through
`bd-gh-sync watch`.

### Install (Debian/Ubuntu, as root, from a checkout of this repo)

```sh
apt-get install -y git caddy
useradd --system --home-dir /var/lib/bd-gh-sync --shell /usr/sbin/nologin bd-gh-sync
scripts/install-bd.sh 1.3.1 /usr/local/bin
scripts/install-bd-gh-sync.sh latest /usr/local/bin

install -d -m 0750 -g bd-gh-sync /etc/bd-gh-sync
install -m 0640 -g bd-gh-sync deploy/env.example /etc/bd-gh-sync/env   # fill in
install -m 0644 deploy/config.toml /etc/bd-gh-sync/config.toml          # set public_url
install -m 0644 deploy/bd-gh-sync.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now bd-gh-sync
```

[`deploy/env.example`](deploy/env.example) lists the token permissions. For
HTTPS, add [`deploy/Caddyfile`](deploy/Caddyfile) (with your domain) to
`/etc/caddy/Caddyfile`, point the domain's DNS at the machine, open ports 80
and 443 (on Hetzner, in the Cloud Firewall too), and `systemctl reload caddy`.
`curl https://<domain>/healthz` should print `ok`.

### Add projects

Run admin commands as the service user, with its environment:

```sh
as-sync() { sudo -u bd-gh-sync sh -c 'set -a; . /etc/bd-gh-sync/env; HOME=/var/lib/bd-gh-sync exec bd-gh-sync "$@"' sh "$@"; }

as-sync server add owner/repo    # clone, sync once, create the repository webhook
as-sync server list
```

`server add` is safe to re-run; it updates the webhook instead of adding a
second one. The repository's beads must already be published (`bd dolt push`
once from a clone, or a committed JSONL export). Once a repository is on the
server, delete its `beads-sync.yml` workflow: the hourly reconcile replaces the
schedules. `journalctl -u bd-gh-sync -f` shows each sync.

To debug one project, stop the service and run
`as-sync server sync owner/repo [NUMBER...] [--all]`.

## Sync loops

A change must never bounce between the two sides. The guards:

1. **The Action and the server only pull.** They never write to GitHub issues,
   so they cannot generate issue events. They publish nothing when the pull
   changed nothing, and the Action publishes with `GITHUB_TOKEN`, whose pushes
   never start workflows.
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
- A bead's close reason maps to GitHub's by keyword: "duplicate" closes as
  `duplicate`; "won't", "wont", "not planned" or "invalid" close as
  `not_planned`; anything else is `completed`. On pull, a bead's reason is only
  replaced when it implies a different GitHub reason, so custom text such as
  "fixed in v2" survives.
- Cross-references become `related` links once, when first seen. They are not
  pushed back, and a pair that already has any other dependency is skipped. A
  link you delete in beads is not re-added.
- A pull request that closes an issue (`Fixes #N`, or linked in the sidebar)
  moves an `open` bead to `in_progress` and adds a "Linked pull request: URL"
  comment, once per pull request. The comment is not posted back to GitHub.
  Merging closes the issue, and the normal sync closes the bead. A closed
  unmerged pull request changes nothing. The status is not forced: if a later
  pull of the issue resets the bead and no watcher has pushed `in_progress`
  back, it stays `open`. The server reacts to `pull_request` webhook events; the
  Action workflow triggers on `pull_request` too.
- A bead that supersedes another bead (`bd supersede OLD --with NEW`) closes
  OLD's issue on GitHub as a duplicate of NEW's, and a GitHub duplicate becomes
  a `supersedes` link the other way. GitHub's REST API cannot set the target, so
  this goes through GraphQL (`closeIssue` with `duplicateIssueId`). Only
  duplicates within the repository map.
- Deleting a bead closes its issue on GitHub as `not_planned` (the watcher
  remembers each bead's issue and confirms with `bd show` that it is really
  gone). An issue deleted on GitHub closes its bead ("Issue deleted on GitHub")
  and labels it `github-deleted`; one transferred to another repository gets
  its `external_ref` pointed at the new URL and the label `github-transferred`.
  The watcher stops pushing beads with either label, since their issue is no
  longer in this repository. Only a full reconcile (`--all`, or the issue's own
  webhook event) notices a deleted or moved issue; `--since-last` cannot.
- Comments sync when created, and comments the watcher posts appear under the
  token's owner. `bd` cannot edit or delete a comment, so a GitHub edit or
  deletion arrives as a new bead comment (`Edited on GitHub: <new text>`,
  `Deleted on GitHub: a comment by <author>`) and the original stays as history.
  These notes are never posted back to GitHub, and bead comments never change
  GitHub comments. The pull tracks each GitHub comment's id and text (in the
  same state as `since`), so comments imported before this existed are treated
  as already seen.
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
cargo clippy --all-targets -- -D warnings
```

There is no test suite yet.

Releases: push a `v*` tag and [`release.yml`](.github/workflows/release.yml)
builds and publishes the binaries the Action and the install script download.
