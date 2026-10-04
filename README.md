# bd-gh-sync

Real-time, two-way sync between [beads](https://github.com/gastownhall/beads)
(`bd`) and GitHub issues. Requires beads 1.3+.

## What syncs

| GitHub | beads |
|---|---|
| title, body, open/closed | title, description, status |
| labels | labels, priority, type, status |
| first assignee | assignee |
| close reason (`completed`, `not_planned`, `duplicate`) | close reason |
| closed as duplicate of #N | closed bead that `supersedes` #N's bead |
| comments | comments |
| sub-issue of #N | `parent-child` dependency |
| blocked by #N | `blocks` dependency |
| `Ref: #N` mention (GitHub → beads only) | `related` dependency |
| open PR that closes the issue (GitHub → beads only) | bead goes `in_progress` |

Optionally, GitHub Projects v2 fields (status, priority, estimate, iteration)
sync too; see [`deploy/config.toml`](deploy/config.toml).

## Setup

### 1. Publish your beads

Run `bd dolt push` once from your clone so the beads exist on GitHub
(`refs/dolt/data`). Repos without a Dolt remote can instead commit
`.beads/issues.jsonl`.

### 2. GitHub → beads

Pick one.

**Webhook server** (always-on machine, any number of repositories). On a
Debian/Ubuntu box, as root, from a checkout of this repo:

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

Put [`deploy/Caddyfile`](deploy/Caddyfile) (with your domain) in
`/etc/caddy/Caddyfile` for HTTPS, then add each repository:

```sh
sudo -u bd-gh-sync sh -c 'set -a; . /etc/bd-gh-sync/env; HOME=/var/lib/bd-gh-sync exec bd-gh-sync server add owner/repo'
```

This clones the repo, syncs it once and creates the webhook.

**GitHub Action.** Copy [`.github/workflows/beads-sync.yml`](.github/workflows/beads-sync.yml)
into your repository and change `uses: ./` to `uses: Takashiidobe/bd-gh-sync@main`.

### 3. beads → GitHub

On each machine where you edit beads:

```sh
scripts/install-bd-gh-sync.sh                # or: cargo install --git https://github.com/Takashiidobe/bd-gh-sync
bd config set github.repository owner/repo
export GITHUB_TOKEN=...                      # or `gh auth login`
bd-gh-sync watch --dolt-sync 30
```

`--dolt-sync 30` also pulls what the server or Action synced from GitHub.
