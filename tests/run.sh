#!/usr/bin/env bash
# End-to-end tests for bd-gh-watch and gh-to-beads.sh.
#
# Uses the real bd binary against a fake GitHub API (tests/fake_github.py)
# and local bare git remotes, so it runs offline and touches no real repo.
#
# Requirements: bd, jq, git, curl, python3. inotifywait is used for the live
# watcher test when present; otherwise that test uses the polling backend.

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(dirname "$HERE")
WATCH=$ROOT/bin/bd-gh-watch
SYNC=$ROOT/scripts/gh-to-beads.sh

WORK=$(mktemp -d)
cleanup() {
  [ -n "${WATCH_PID:-}" ] && kill "$WATCH_PID" 2>/dev/null
  [ -n "${SERVER_PID:-}" ] && kill "$SERVER_PID" 2>/dev/null
  wait 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

python3 "$HERE/fake_github.py" "$WORK/port" &
SERVER_PID=$!
for _ in $(seq 50); do [ -s "$WORK/port" ] && break; sleep 0.1; done
API=http://127.0.0.1:$(cat "$WORK/port")

export GITHUB_API_URL=$API
export GITHUB_TOKEN=test-token GH_TOKEN=test-token
export GITHUB_REPOSITORY=acme/widgets
export GH=$HERE/stubs/gh
export BD_NON_INTERACTIVE=1 NO_COLOR=1
export NO_PROXY=127.0.0.1,localhost no_proxy=127.0.0.1,localhost
export GIT_AUTHOR_NAME=tester GIT_AUTHOR_EMAIL=tester@example.com
export GIT_COMMITTER_NAME=tester GIT_COMMITTER_EMAIL=tester@example.com

PASS=0
step() { printf '\n== %s\n' "$*"; }
ok() { PASS=$((PASS + 1)); printf '   ok: %s\n' "$*"; }
fail() { printf '   FAIL: %s\n' "$*" >&2; exit 1; }
check() { local msg=$1; shift; if "$@"; then ok "$msg"; else fail "$msg"; fi; }

writes() { curl -fsS "$API/_stats" | jq .writes; }
gh_issue() { curl -fsS "$API/repos/acme/widgets/issues/$1"; }
gh_create() { curl -fsS -X POST "$API/repos/acme/widgets/issues" -d "$1" | jq .number; }
gh_edit() { curl -fsS -X PATCH "$API/repos/acme/widgets/issues/$1" -d "$2" >/dev/null; }
bead_field() { bd export | jq -r --arg t "$1" "select(.title | test(\$t)) | $2"; }
count_beads() { bd export | jq -s length; }
quiet() { "$@" >/dev/null 2>&1; }
dolt_ref() { git -C "$1" rev-parse -q --verify refs/dolt/data || echo none; }

# Run the action's script in DIR and keep only its own log lines.
sync_in() {
  local dir=$1; shift
  (cd "$dir" && "$SYNC" --publish "$@") 2>&1 | grep '^gh-to-beads:' || true
}

# Run the action's script in a fresh clone, like a workflow run would.
run_action() {
  local remote=$1; shift
  rm -rf "$WORK/action"
  git clone -q "$remote" "$WORK/action"
  sync_in "$WORK/action" "$@"
}

new_remote() {
  git init -q --bare "$1"
  git -C "$1" symbolic-ref HEAD refs/heads/main
}

# --------------------------------------------------------------------------
step "set up a beads repo with a git-backed Dolt remote"
new_remote "$WORK/origin.git"
git clone -q "$WORK/origin.git" "$WORK/local" 2>/dev/null
cd "$WORK/local"
git checkout -q -b main
quiet bd init --quiet --prefix loc --skip-agents --skip-hooks
git add -A && { git commit -qm "init beads" || true; } && git push -q origin main
quiet bd dolt push
check "origin has Dolt data" test "$(dolt_ref "$WORK/origin.git")" != none

# --------------------------------------------------------------------------
step "watcher: first run records a baseline without pushing"
quiet bd create "pre-existing bead" -t task -p 2
"$WATCH" --once 2>/dev/null
check "no GitHub writes" test "$(writes)" = 0

step "watcher: a new bead becomes a GitHub issue"
quiet bd create "local feature" -t feature -p 1 -l ui -d "made locally"
"$WATCH" --once 2>/dev/null
check "one issue created" test "$(writes)" = 1
check "issue has the bead's title" test "$(gh_issue 1 | jq -r .title)" = "local feature"
check "issue carries bd's labels" test "$(gh_issue 1 | jq -r '[.labels[].name] | sort | join(",")')" = "priority::high,type::feature,ui"
check "bead is linked" test "$(bead_field 'local feature' .external_ref)" = "https://github.com/acme/widgets/issues/1"
check "pre-existing bead was not pushed" test "$(bead_field 'pre-existing' '.external_ref // "none"')" = none

step "watcher: its own link write does not trigger another push"
"$WATCH" --once 2>/dev/null
check "no new GitHub writes" test "$(writes)" = 1

step "watcher: an edit is pushed"
quiet bd update "$(bead_field 'local feature' .id)" --status in_progress
"$WATCH" --once 2>/dev/null
check "issue updated" test "$(writes)" = 2
check "status label on GitHub" test "$(gh_issue 1 | jq -r '[.labels[].name] | index("status::in_progress") != null')" = true

# --------------------------------------------------------------------------
step "action: skips a bd-created issue until its bead is published"
ref_before=$(dolt_ref "$WORK/origin.git")
out=$(run_action "$WORK/origin.git" 1)
check "reported the skip" grep -q "created by bd but its bead is not published" <<<"$out"
check "Dolt remote untouched" test "$(dolt_ref "$WORK/origin.git")" = "$ref_before"

step "action: a linked issue that GitHub has not changed is a no-op"
quiet bd dolt push
ref_before=$(dolt_ref "$WORK/origin.git")
out=$(run_action "$WORK/origin.git" 1)
check "pulled the linked issue" grep -q "pulling 1 issue(s): 1" <<<"$out"
check "nothing to push" grep -q "pull changed nothing" <<<"$out"
check "Dolt remote untouched" test "$(dolt_ref "$WORK/origin.git")" = "$ref_before"
check "action never writes to GitHub" test "$(writes)" = 2

step "action: an issue opened on GitHub becomes a bead"
n=$(gh_create '{"title":"opened on github","body":"from the web"}')
w=$(writes)
out=$(run_action "$WORK/origin.git" "$n")
check "Dolt remote updated" test "$(dolt_ref "$WORK/origin.git")" != "$ref_before"
quiet bd dolt pull
check "bead arrived locally" test "$(bead_field 'opened on github' .external_ref)" = "https://github.com/acme/widgets/issues/$n"

step "loop guard: the imported bead settles after at most one push"
"$WATCH" --once 2>/dev/null   # may add bd's type::/priority:: labels once
w=$(writes)
"$WATCH" --once 2>/dev/null
check "second pass writes nothing" test "$(writes)" = "$w"
out=$(run_action "$WORK/origin.git" "$n")
check "action sees nothing new" grep -q "pull changed nothing" <<<"$out"

step "loop guard: a GitHub edit round-trips without echoing back"
gh_edit "$n" '{"title":"opened on github (edited)"}'
w=$(writes)
run_action "$WORK/origin.git" "$n" >/dev/null
quiet bd dolt pull
check "edit arrived locally" test -n "$(bead_field 'edited' .id)"
"$WATCH" --once 2>/dev/null
check "watcher did not write it back" test "$(writes)" = "$w"

# --------------------------------------------------------------------------
step "watcher: live mode pushes within seconds and then goes quiet"
backend=poll
command -v inotifywait >/dev/null && backend=inotify
"$WATCH" --backend "$backend" --poll 2 --debounce 0.3 2>"$WORK/watch.log" &
WATCH_PID=$!
sleep 2
quiet bd update "$(bead_field 'local feature' .id)" --title "local feature v2"
for _ in $(seq 40); do
  [ "$(gh_issue 1 | jq -r .title)" = "local feature v2" ] && break
  sleep 0.5
done
check "title reached GitHub ($backend)" test "$(gh_issue 1 | jq -r .title)" = "local feature v2"
w=$(writes)
sleep 5
check "no further writes while idle" test "$(writes)" = "$w"
kill "$WATCH_PID"; wait "$WATCH_PID" 2>/dev/null || true; WATCH_PID=

# --------------------------------------------------------------------------
step "action: refuses to fork a Dolt remote nobody has pushed yet"
new_remote "$WORK/fresh.git"
git clone -q "$WORK/fresh.git" "$WORK/fresh" 2>/dev/null
(
  cd "$WORK/fresh"
  git checkout -q -b main
  quiet bd init --quiet --prefix fr --skip-agents --skip-hooks
  git add -A && { git commit -qm "init beads" || true; } && git push -q origin main
)
if out=$(cd "$WORK/fresh" && "$SYNC" --publish 1 2>&1); then
  fail "expected a failure"
fi
check "explains the fix" grep -q "Run 'bd dolt push' once" <<<"$out"
check "pushed nothing" test "$(dolt_ref "$WORK/fresh.git")" = none

# --------------------------------------------------------------------------
step "action (jsonl transport): commits the export"
new_remote "$WORK/jsonl.git"
git clone -q "$WORK/jsonl.git" "$WORK/jl" 2>/dev/null
(
  cd "$WORK/jl"
  git checkout -q -b main
  quiet bd init --quiet --prefix jl --skip-agents --skip-hooks
  quiet bd config unset sync.remote   # JSONL-only repo
  bd export -o .beads/issues.jsonl 2>/dev/null
  git add -A && { git commit -qm "init beads" || true; } && git push -q origin main
)
n=$(gh_create '{"title":"jsonl issue","body":"x"}')
out=$(run_action "$WORK/jsonl.git" "$n")
check "transport picked automatically" grep -q "transport: jsonl" <<<"$out"
check "export committed" test "$(git -C "$WORK/jsonl.git" log -1 --format=%s main)" = "beads: sync from GitHub"
check "export has the issue" grep -q "jsonl issue" <(git -C "$WORK/jsonl.git" show main:.beads/issues.jsonl)
head_before=$(git -C "$WORK/jsonl.git" rev-parse main)
out=$(run_action "$WORK/jsonl.git" "$n")
check "re-run commits nothing" test "$(git -C "$WORK/jsonl.git" rev-parse main)" = "$head_before"

step "action (jsonl transport): survives a push race"
rm -rf "$WORK/stale"
git clone -q "$WORK/jsonl.git" "$WORK/stale"
(cd "$WORK/jl" && git pull -q origin main && echo hi >README && git add README && git commit -qm "unrelated" && git push -q origin main)
gh_edit "$n" '{"title":"jsonl issue (edited)"}'
out=$(sync_in "$WORK/stale" "$n")
check "retried after rejection" grep -q "push rejected" <<<"$out"
check "kept the other commit" grep -q unrelated <(git -C "$WORK/jsonl.git" log --format=%s main)
check "export has the edit" grep -q "jsonl issue (edited)" <(git -C "$WORK/jsonl.git" show main:.beads/issues.jsonl)

printf '\nAll %d checks passed.\n' "$PASS"
