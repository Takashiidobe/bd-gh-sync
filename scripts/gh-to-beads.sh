#!/usr/bin/env bash
# gh-to-beads.sh: pull GitHub issue changes into this repo's beads.
#
# Runs inside the GitHub Action (see action.yml). It loads the repository's
# beads database, runs beads' native `bd github pull` for the issues that
# changed, and (with --publish) writes the result back to the repository
# through one of two transports:
#
#   dolt   beads' own git-backed Dolt remote (refs/dolt/data, the default
#          for `bd init` in a git repo): `bd dolt pull` / `bd dolt push`
#   jsonl  the git-tracked .beads/issues.jsonl export, committed to the
#          current branch
#
# Usage:
#   gh-to-beads.sh [--publish] [--all | NUMBER...]
#
#   NUMBER...   GitHub issue numbers to pull (e.g. from the triggering event)
#   --all       reconcile every issue in the repository
#   --publish   push the result (Dolt push or JSONL commit), retrying races
#
# Environment:
#   GITHUB_REPOSITORY   owner/repo (set by Actions)
#   GH_TOKEN            token for `gh api` reads (GITHUB_TOKEN is the fallback)
#   GITHUB_TOKEN        token bd uses for `bd github pull`
#   BEADS_TRANSPORT     auto | dolt | jsonl (default auto: dolt when origin
#                       has refs/dolt/data; an error when sync.remote is set
#                       but origin has no Dolt data yet; else jsonl)
#   BEADS_JSONL         JSONL path for the jsonl transport (default .beads/issues.jsonl)
#   ADOPT_BD_CREATED    "true" to also import issues that a bd clone created
#                       but has not published the link for yet (default false)
#   COMMIT_MESSAGE      JSONL commit message (default "beads: sync from GitHub")
#   BD, GH              override the bd / gh binaries (used by tests)
#
# Loop guards (see README "Sync loops"):
#   * this side only ever pulls; it never writes to GitHub issues
#   * publishing uses GITHUB_TOKEN, whose pushes never trigger workflows
#   * nothing is published when the pull changed nothing
#   * issues created by `bd github push` (they carry both a type:: and a
#     priority:: label) are only pulled once the bead that created them has
#     been published with its link, so a GitHub issue never gets two beads

set -euo pipefail

BD=${BD:-bd}
GH=${GH:-gh}
JSONL=${BEADS_JSONL:-.beads/issues.jsonl}
TRANSPORT=${BEADS_TRANSPORT:-auto}
ADOPT=${ADOPT_BD_CREATED:-false}
COMMIT_MESSAGE=${COMMIT_MESSAGE:-"beads: sync from GitHub"}
REPO=${GITHUB_REPOSITORY:?GITHUB_REPOSITORY must be owner/repo}
export GH_TOKEN=${GH_TOKEN:-${GITHUB_TOKEN:-}}
export GITHUB_TOKEN=${GITHUB_TOKEN:-${GH_TOKEN:-}}
export BD_NON_INTERACTIVE=1

log() { printf 'gh-to-beads: %s\n' "$*" >&2; }

publish=false
all=false
numbers=()
while [ $# -gt 0 ]; do
  case $1 in
    --publish) publish=true ;;
    --all) all=true ;;
    -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    ''|*[!0-9]*) log "not an issue number: $1"; exit 2 ;;
    *) numbers+=("$1") ;;
  esac
  shift
done

if ! $all && [ ${#numbers[@]} -eq 0 ]; then
  log "nothing to sync (pass issue numbers or --all)"
  exit 0
fi

if [ "$TRANSPORT" = auto ]; then
  if git ls-remote --exit-code origin refs/dolt/data >/dev/null 2>&1; then
    TRANSPORT=dolt
  elif [ -n "$("$BD" config get sync.remote --json 2>/dev/null | jq -r '.value // empty')" ]; then
    # The repo's beads sync through a Dolt remote, but nobody has pushed to
    # it yet. Starting one here would fork the history from the clone that
    # ran bd init, and a JSONL export would never be imported by clones
    # that use the Dolt remote, so stop and say what to do.
    log "beads here sync through a Dolt remote (sync.remote), but origin has no Dolt data yet."
    log "Run 'bd dolt push' once from your clone, or set the action's transport to jsonl."
    exit 1
  else
    TRANSPORT=jsonl
  fi
fi
case $TRANSPORT in
  dolt|jsonl) log "transport: $TRANSPORT" ;;
  *) log "unknown transport: $TRANSPORT"; exit 2 ;;
esac

# Quiet wrapper: show a command's output only when it fails.
quiet() {
  local out
  if ! out=$("$@" 2>&1); then
    printf '%s\n' "$out" >&2
    return 1
  fi
}

# Issue numbers already linked to a bead in this repo, one per line.
linked_numbers() {
  "$BD" export 2>/dev/null | jq -r --arg repo "$REPO" '
    select((._type // "issue") == "issue")
    | (.external_ref // "")
    | ascii_downcase
    | (capture("github\\.com/(?<r>[^/]+/[^/]+)/issues/(?<n>[0-9]+)$")
       // capture("^(?:gh-|github:)(?<n>[0-9]+)$")
       // empty)
    | select((.r // ($repo | ascii_downcase)) == ($repo | ascii_downcase))
    | .n' | sort -u
}

# One line per issue: "NUMBER IS_PR BD_CREATED".
describe_issues() {
  local filter='"\(.number) \(.pull_request != null) \([.labels[]? | if type == "object" then .name else . end]
      | (any(startswith("type::")) and any(startswith("priority::"))))"'
  if $all; then
    "$GH" api --paginate "repos/$REPO/issues?state=all&per_page=100" --jq ".[] | $filter"
  else
    local n
    for n in "${numbers[@]}"; do
      "$GH" api "repos/$REPO/issues/$n" --jq "$filter" || log "#$n: could not fetch, skipping"
    done
  fi
}

# Fingerprint of the whole database, to tell whether the pull changed it.
db_fingerprint() { "$BD" export 2>/dev/null | cksum; }

# Pull the selected issues into the local database.
# Sets PULL_CHANGED=true when the database changed.
pull_issues() {
  local linked to_pull=() n pr bd_created before
  linked=$(linked_numbers)
  while read -r n pr bd_created; do
    if [ -z "$n" ] || [ "$pr" = true ]; then
      continue
    fi
    if grep -qx "$n" <<<"$linked"; then
      to_pull+=("$n")
    elif [ "$bd_created" = true ] && [ "$ADOPT" != true ]; then
      log "#$n was created by bd but its bead is not published yet; skipping (it syncs once that clone pushes)"
    else
      to_pull+=("$n")
    fi
  done < <(describe_issues)

  PULL_CHANGED=false
  if [ ${#to_pull[@]} -eq 0 ]; then
    log "no issues to pull"
    return
  fi
  before=$(db_fingerprint)
  log "pulling ${#to_pull[@]} issue(s): ${to_pull[*]}"
  "$BD" github pull "${to_pull[@]}"
  [ "$(db_fingerprint)" = "$before" ] || PULL_CHANGED=true
}

# Commit pending Dolt changes; a no-op when there are none.
commit_dolt() {
  local out
  if ! out=$("$BD" dolt commit -m "$1" 2>&1) && ! grep -qi 'nothing to commit' <<<"$out"; then
    printf '%s\n' "$out" >&2
    return 1
  fi
}

sync_dolt() {
  quiet "$BD" bootstrap --yes
  local attempt
  for attempt in 1 2 3 4 5; do
    quiet "$BD" dolt pull || true
    pull_issues
    if ! $publish; then return; fi
    if ! $PULL_CHANGED; then
      log "pull changed nothing; nothing to push"
      return
    fi
    # bd github pull leaves its writes in the Dolt working set; commit them
    # so the push carries them.
    commit_dolt "bd-gh-sync: pull from GitHub"
    if quiet "$BD" dolt push; then
      log "pushed beads to the Dolt remote"
      return
    fi
    log "Dolt push rejected (attempt $attempt); pulling and retrying"
    sleep $((attempt * 2))
  done
  log "giving up after repeated push races"
  return 1
}

sync_jsonl() {
  local branch attempt
  branch=$(git rev-parse --abbrev-ref HEAD)
  quiet "$BD" bootstrap --yes
  for attempt in 1 2 3 4 5; do
    pull_issues
    mkdir -p "$(dirname "$JSONL")"
    quiet "$BD" export -o "$JSONL"
    if ! $publish; then return; fi
    git add -- "$JSONL"
    if git diff --cached --quiet; then
      log "export unchanged; nothing to commit"
      return
    fi
    git commit -q -m "$COMMIT_MESSAGE" -m "[skip ci]"
    if git push -q origin "HEAD:$branch"; then
      log "pushed $(git rev-parse --short HEAD) to $branch"
      return
    fi
    log "push rejected (attempt $attempt); rebuilding on the latest $branch"
    git fetch -q origin "$branch"
    git reset -q --hard "origin/$branch"
    # Take in the newer export before pulling again; the pull re-applies
    # GitHub's state on top of it.
    quiet "$BD" import --allow-stale "$JSONL" || true
    sleep $((attempt * 2))
  done
  log "giving up after repeated push races"
  return 1
}

"sync_$TRANSPORT"
