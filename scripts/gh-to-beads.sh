#!/usr/bin/env bash
# gh-to-beads.sh: pull GitHub issue changes into this repo's beads.
#
# Runs inside the GitHub Action (see action.yml). It loads the repository's
# beads database and brings in what changed on GitHub:
#
#   issues     title, body, state, labels and assignee, via beads' native
#              `bd github pull`
#   comments   GitHub comments that no bead comment matches yet
#   relations  sub-issues as parent-child dependencies and "blocked by" links
#              as blocks dependencies, added and removed
#
# and (with --publish) writes the result back to the repository through one
# of two transports:
#
#   dolt   beads' own git-backed Dolt remote (refs/dolt/data, the default
#          for `bd init` in a git repo): `bd dolt pull` / `bd dolt push`
#   jsonl  the git-tracked .beads/issues.jsonl export, committed to the
#          current branch
#
# Usage:
#   gh-to-beads.sh [--publish] [--since-last | --all | NUMBER...]
#
#   NUMBER...     GitHub issue numbers to pull (e.g. from the triggering event)
#   --since-last  pull every issue updated since the last published sync
#                 (every issue on the first run), so one run covers any
#                 number of events
#   --all         reconcile every issue in the repository
#   --publish     push the result (Dolt push or JSONL commit), retrying races
#
# Relations are reconciled for the whole repository on every run: GitHub
# sends no workflow event when they change.
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
# Sync state (the --since-last watermark and the relations seen on GitHub at
# the last sync) is kept in `bd kv` for the dolt transport, and in
# github-sync.json next to the export for the jsonl transport.
#
# Loop guards (see README "Sync loops"):
#   * this side only ever pulls; it never writes to GitHub issues
#   * publishing uses GITHUB_TOKEN, whose pushes never trigger workflows
#   * nothing is published when the pull changed nothing
#   * issues created by `bd github push` (they carry both a type:: and a
#     priority:: label) are only pulled once the bead that created them has
#     been published with its link, so a GitHub issue never gets two beads
#   * comments posted by bd-gh-watch carry a bd-comment marker and are
#     never imported back

set -euo pipefail

BD=${BD:-bd}
GH=${GH:-gh}
JSONL=${BEADS_JSONL:-.beads/issues.jsonl}
STATE_FILE=$(dirname "$JSONL")/github-sync.json
TRANSPORT=${BEADS_TRANSPORT:-auto}
ADOPT=${ADOPT_BD_CREATED:-false}
COMMIT_MESSAGE=${COMMIT_MESSAGE:-"beads: sync from GitHub"}
REPO=${GITHUB_REPOSITORY:?GITHUB_REPOSITORY must be owner/repo}
export GH_TOKEN=${GH_TOKEN:-${GITHUB_TOKEN:-}}
export GITHUB_TOKEN=${GITHUB_TOKEN:-${GH_TOKEN:-}}
export BD_NON_INTERACTIVE=1 BD_NO_DEP_TYPE_WARNING=1

log() { printf 'gh-to-beads: %s\n' "$*" >&2; }

mode=numbers
publish=false
numbers=()
while [ $# -gt 0 ]; do
  case $1 in
    --publish) publish=true ;;
    --all) mode=all ;;
    --since-last) mode=since ;;
    -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    ''|*[!0-9]*) log "not an issue number: $1"; exit 2 ;;
    *) numbers+=("$1") ;;
  esac
  shift
done

if [ "$mode" = numbers ] && [ ${#numbers[@]} -eq 0 ]; then
  log "nothing to sync (pass issue numbers, --since-last or --all)"
  exit 0
fi

# The next watermark, taken before anything is read from GitHub. The two
# minutes of overlap cover clock skew between the runner and GitHub; an
# issue pulled twice is a no-op.
NEXT_SINCE=$(jq -nr 'now - 120 | strftime("%Y-%m-%dT%H:%M:%SZ")')

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

state_get() {
  case $TRANSPORT in
    # bd kv get exits 1 for a missing key
    dolt) { "$BD" kv get "bd-gh-sync.$1" --json 2>/dev/null || true; } | jq -r 'if .found then .value else empty end' ;;
    jsonl) if [ -f "$STATE_FILE" ]; then jq -r --arg k "$1" '.[$k] // empty' "$STATE_FILE"; fi ;;
  esac
}

state_set() {
  case $TRANSPORT in
    dolt) quiet "$BD" kv set "bd-gh-sync.$1" "$2" ;;
    jsonl)
      local cur='{}'
      [ -f "$STATE_FILE" ] && cur=$(cat "$STATE_FILE")
      jq --arg k "$1" --arg v "$2" '.[$k] = $v' <<<"$cur" >"$STATE_FILE.tmp"
      mv "$STATE_FILE.tmp" "$STATE_FILE" ;;
  esac
}

# {"NUMBER": "BEAD_ID"} for the beads linked to this repo's issues.
linked_beads() {
  "$BD" export 2>/dev/null | jq -cn --arg repo "$REPO" '
    [inputs
     | select((._type // "issue") == "issue")
     | {id, ref: ((.external_ref // "") | ascii_downcase)}
     | (.ref
        | capture("github\\.com/(?<r>[^/]+/[^/]+)/issues/(?<n>[0-9]+)$")
          // capture("^(?:gh-|github:)(?<n>[0-9]+)$")
          // null) as $m
     | select($m != null and (($m.r // ($repo | ascii_downcase)) == ($repo | ascii_downcase)))
     | {key: $m.n, value: .id}]
    | from_entries'
}

# One line per issue: "NUMBER IS_PR BD_CREATED COMMENTS".
describe_issues() {
  local filter='"\(.number) \(.pull_request != null) \([.labels[]? | if type == "object" then .name else . end]
      | (any(startswith("type::")) and any(startswith("priority::")))) \(.comments // 0)"'
  case $mode in
    all) "$GH" api --paginate "repos/$REPO/issues?state=all&per_page=100" --jq ".[] | $filter" ;;
    since) "$GH" api --paginate "repos/$REPO/issues?state=all&per_page=100&since=$SINCE" --jq ".[] | $filter" ;;
    numbers)
      local n
      for n in "${numbers[@]}"; do
        "$GH" api "repos/$REPO/issues/$n" --jq "$filter" || log "#$n: could not fetch, skipping"
      done ;;
  esac
}

# Fingerprint of the whole database, to tell whether the sync changed it.
db_fingerprint() { "$BD" export 2>/dev/null | cksum; }

# Pull the selected issues into the local database. Sets COMMENTED to the
# pulled issues that have comments.
pull_issues() {
  local linked to_pull=() n pr bd_created comments
  linked=$(linked_beads | jq -r 'keys[]')
  COMMENTED=()
  while read -r n pr bd_created comments; do
    if [ -z "$n" ] || [ "$pr" = true ]; then
      continue
    fi
    if grep -qx "$n" <<<"$linked"; then
      to_pull+=("$n")
    elif [ "$bd_created" = true ] && [ "$ADOPT" != true ]; then
      log "#$n was created by bd but its bead is not published yet; skipping (it syncs once that clone pushes)"
      continue
    else
      to_pull+=("$n")
    fi
    [ "${comments:-0}" -gt 0 ] && COMMENTED+=("$n")
  done < <(describe_issues)

  if [ ${#to_pull[@]} -eq 0 ]; then
    log "no issues to pull"
    return
  fi
  log "pulling ${#to_pull[@]} issue(s): ${to_pull[*]}"
  "$BD" github pull "${to_pull[@]}"
}

# Import the GitHub comments on the given issues that no bead comment
# matches yet (same author and text, counting repeats). Comments that
# bd-gh-watch posted carry a bd-comment marker; they already are beads.
import_comments() {
  [ $# -gt 0 ] || return 0
  local beads have n id missing c tmp
  beads=$(linked_beads)
  have=$("$BD" export 2>/dev/null | jq -cn '
    [inputs | select((._type // "issue") == "issue")
     | {key: .id, value: [(.comments // [])[] | {author, text}]}] | from_entries')
  tmp=$(mktemp)
  for n in "$@"; do
    id=$(jq -r --arg n "$n" '.[$n] // empty' <<<"$beads")
    [ -n "$id" ] || continue
    missing=$("$GH" api --paginate "repos/$REPO/issues/$n/comments?per_page=100" \
      | jq -cs --argjson have "$(jq -c --arg id "$id" '.[$id] // []' <<<"$have")" '
        def norm: gsub("\r\n"; "\n") | sub("^\\s+"; "") | sub("\\s+$"; "");
        reduce (add // [] | .[] | select(.body | test("<!-- bd-comment:") | not)) as $c
          ({have: ($have | map(.text |= norm)), out: []};
           {author: $c.user.login, text: ($c.body | norm)} as $k
           | (.have | index([$k])) as $i
           | if $i == null then .out += [$k] else .have |= del(.[$i]) end)
        | .out[]') || { log "#$n: could not fetch comments, skipping"; continue; }
    while IFS= read -r c; do
      [ -n "$c" ] || continue
      jq -j .text <<<"$c" >"$tmp"
      quiet "$BD" comments add "$id" -a "$(jq -r .author <<<"$c")" -f "$tmp" \
        && log "#$n: imported a comment by $(jq -r .author <<<"$c")"
    done <<<"$missing"
  done
  rm -f "$tmp"
}

# shellcheck disable=SC2016  # GraphQL variables, not shell expansions
RELATIONS_QUERY='query($owner: String!, $repo: String!, $endCursor: String) {
  repository(owner: $owner, name: $repo) {
    issues(first: 100, after: $endCursor) {
      pageInfo { hasNextPage endCursor }
      nodes {
        number
        parent { number repository { nameWithOwner } }
        blockedBy(first: 100) { nodes { number repository { nameWithOwner } } }
      }
    }
  }
}'

# GitHub's relations within this repo as "FROM TYPE TO" (FROM depends on TO,
# in issue numbers), one per line.
gh_relations() {
  local repo_lc
  repo_lc=$(tr '[:upper:]' '[:lower:]' <<<"$REPO")
  # shellcheck disable=SC2016  # jq variables
  "$GH" api graphql --paginate -f owner="${REPO%/*}" -f repo="${REPO#*/}" -f query="$RELATIONS_QUERY" --jq '
    .data.repository.issues.nodes[] as $i
    | (($i.parent // empty)
       | select(.repository.nameWithOwner | ascii_downcase == "'"$repo_lc"'")
       | "\($i.number) parent-child \(.number)"),
      (($i.blockedBy.nodes // [])[]
       | select(.repository.nameWithOwner | ascii_downcase == "'"$repo_lc"'")
       | "\($i.number) blocks \(.number)")'
}

# Three-way merge of relations: GitHub now, GitHub at the last sync (the
# base), and the beads. Links added on GitHub are added to the beads, links
# removed on GitHub are removed from them, and bead-only links (not pushed
# yet by bd-gh-watch) are left alone. Sets RELATIONS_CHANGED.
sync_relations() {
  local beads remote have base plan next_base from type to failed=()
  RELATIONS_CHANGED=false
  beads=$(linked_beads)
  remote=$(gh_relations | jq -Rsc 'split("\n") | map(select(length > 0))') \
    || { log "could not read relations from GitHub; skipping them"; return; }
  have=$("$BD" export 2>/dev/null | jq -cn --argjson beads "$beads" '
    ($beads | with_entries({key: .value, value: .key})) as $num
    | [inputs | select((._type // "issue") == "issue") | (.dependencies // [])[]
       | select(.type == "blocks" or .type == "parent-child")
       | select($num[.issue_id] and $num[.depends_on_id])
       | "\($num[.issue_id]) \(.type) \($num[.depends_on_id])"]')
  base=$(state_get relations)
  plan=$(jq -cn --argjson beads "$beads" --argjson remote "$remote" \
    --argjson have "$have" --argjson base "${base:-[]}" '
    ($remote | map(select(split(" ") | $beads[.[0]] and $beads[.[2]]))) as $remote
    | {add: [($remote - $base - $have)[]],
       remove: [($base - $remote)[] | select(IN($have[]))],
       base: ($remote | unique)}')

  while read -r from type to; do
    [ -n "$from" ] || continue
    if quiet "$BD" dep add "$(jq -r --arg n "$from" '.[$n]' <<<"$beads")" \
        "$(jq -r --arg n "$to" '.[$n]' <<<"$beads")" -t "$type"; then
      log "#$from $type #$to: added"
    else
      log "#$from $type #$to: could not add; will retry next run"
      failed+=("$from $type $to")
    fi
  done < <(jq -r '.add[]' <<<"$plan")
  while read -r from type to; do
    [ -n "$from" ] || continue
    quiet "$BD" dep remove "$(jq -r --arg n "$from" '.[$n]' <<<"$beads")" \
        "$(jq -r --arg n "$to" '.[$n]' <<<"$beads")" \
      && log "#$from $type #$to: removed"
  done < <(jq -r '.remove[]' <<<"$plan")

  next_base=$(jq -c --args '.base - $ARGS.positional' ${failed[@]+"${failed[@]}"} <<<"$plan")
  if [ "$next_base" != "$(jq -c 'sort' <<<"${base:-[]}")" ]; then
    state_set relations "$next_base"
    RELATIONS_CHANGED=true
  fi
}

# One full pass from GitHub into the local database. Sets CHANGED.
sync_github() {
  local before
  before=$(db_fingerprint)
  SINCE=
  if [ "$mode" = since ]; then
    SINCE=$(state_get since)
    if [ -z "$SINCE" ]; then
      log "no previous sync recorded; reconciling every issue"
      mode=all
    else
      log "pulling issues updated since $SINCE"
    fi
  fi
  pull_issues
  import_comments ${COMMENTED[@]+"${COMMENTED[@]}"}
  sync_relations
  CHANGED=$RELATIONS_CHANGED
  [ "$(db_fingerprint)" = "$before" ] || CHANGED=true
  if $CHANGED && [ "$mode" != numbers ]; then
    state_set since "$NEXT_SINCE"
  fi
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
    # bootstrap (and a rejected attempt) leave uncommitted writes, at least
    # in the events table, that would make the merge refuse to run.
    commit_dolt "bd-gh-sync: local state"
    quiet "$BD" dolt pull || log "bd dolt pull failed; syncing on top of the local data"
    sync_github
    if ! $publish; then return; fi
    if ! $CHANGED; then
      log "pull changed nothing; nothing to push"
      return
    fi
    # bd's writes stay in the Dolt working set; commit so the push carries them.
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
    sync_github
    mkdir -p "$(dirname "$JSONL")"
    quiet "$BD" export -o "$JSONL"
    if ! $publish; then return; fi
    git add -- "$JSONL"
    [ -f "$STATE_FILE" ] && git add -- "$STATE_FILE"
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
