# bd-gh-sync

Real-time, two-way sync between [beads](https://github.com/gastownhall/beads)
(`bd`) and GitHub issues. Requires beads 1.3+.

## What syncs

| GitHub                                                 | beads                                   |
| ------------------------------------------------------ | --------------------------------------- |
| title, body, open/closed                               | title, description, status              |
| labels                                                 | labels, priority, type, status          |
| first assignee                                         | assignee                                |
| close reason (`completed`, `not_planned`, `duplicate`) | close reason                            |
| closed as duplicate of #N                              | closed bead that `supersedes` #N's bead |
| comments                                               | comments                                |
| sub-issue of #N                                        | `parent-child` dependency               |
| blocked by #N                                          | `blocks` dependency                     |
| `Ref: #N` mention (GitHub → beads only)                | `related` dependency                    |
| open PR that closes the issue (GitHub → beads only)    | bead goes `in_progress`                 |

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

The server is the only thing that writes to GitHub, so no machine needs a
GitHub token. On each machine where you edit beads, `watch` pushes your commits
to the Dolt remote (`refs/dolt/data`) and pulls what the server synced back:

```sh
scripts/install-bd-gh-sync.sh                # or: cargo install --git https://github.com/Takashiidobe/bd-gh-sync
bd config set github.repository owner/repo
bd-gh-sync watch
```

The server checks `refs/dolt/data` every `dolt_poll_seconds` (10 by default)
and pushes whatever changed to GitHub, creating issues for new beads and
pushing the links back. Edits reach GitHub within about 10 seconds. To skip the
wait, give `watch` the repository's webhook secret (from
`<data_dir>/.secrets/<owner>__<name>` on the server) and the server's URL, and it
nudges the server after each push:

```sh
export BD_GH_SYNC_POKE_URL=https://sync.example.com/poke
export BD_GH_SYNC_POKE_SECRET=...
```

The server's token needs Contents: read and write, because it pushes
`refs/dolt/data` as well as reading it. `bd-gh-sync watch --local` keeps the old
behaviour of pushing to GitHub from this machine; it needs `GITHUB_TOKEN`.

`watch` follows `.beads/` with inotify (FSEvents on macOS) and reacts within a
second of a change. To keep it running for a repo, run this inside that clone
(`$BDGH` is a checkout of this repo, for the files in
[`deploy/watch`](deploy/watch)):

```sh
# Linux: systemd user service
mkdir -p ~/.config/systemd/user && cp $BDGH/deploy/watch/bd-gh-sync-watch@.service ~/.config/systemd/user/
systemctl --user enable --now "bd-gh-sync-watch@$(systemd-escape --path "$PWD").service"

# macOS: launchd
n=$(basename "$PWD"); p=~/Library/LaunchAgents/bd-gh-sync.$n.plist
sed "s#@REPO@#$PWD#g; s#@HOME@#$HOME#g; s#@NAME@#$n#g" $BDGH/deploy/watch/bd-gh-sync-watch.plist > "$p"
launchctl load "$p"
```

Put `BD_GH_SYNC_POKE_*` in `~/.config/bd-gh-sync/env` for systemd if you use them.
