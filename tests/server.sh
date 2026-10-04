#!/usr/bin/env bash
# End-to-end tests for `bd-gh-sync server`: two projects, signed webhooks,
# batching, and webhook registration, against the fake GitHub API and local
# bare git remotes.
#
# Requirements: cargo, bd, jq, git, curl, openssl, python3.

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(dirname "$HERE")

cargo build -q --manifest-path "$ROOT/Cargo.toml"
BIN=$ROOT/target/debug/bd-gh-sync

WORK=$(mktemp -d)
cleanup() {
  [ -n "${SERVE_PID:-}" ] && kill "$SERVE_PID" 2>/dev/null
  [ -n "${FAKE_PID:-}" ] && kill "$FAKE_PID" 2>/dev/null
  wait 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

python3 "$HERE/fake_github.py" "$WORK/port" &
FAKE_PID=$!
for _ in $(seq 50); do [ -s "$WORK/port" ] && break; sleep 0.1; done
API=http://127.0.0.1:$(cat "$WORK/port")
PORT=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
URL=http://127.0.0.1:$PORT

export GITHUB_API_URL=$API
export GITHUB_TOKEN=test-token
export WEBHOOK_SECRET=test-secret-0123456789
export BD_NON_INTERACTIVE=1 NO_COLOR=1
export NO_PROXY=127.0.0.1,localhost no_proxy=127.0.0.1,localhost
export GIT_AUTHOR_NAME=tester GIT_AUTHOR_EMAIL=tester@example.com
export GIT_COMMITTER_NAME=tester GIT_COMMITTER_EMAIL=tester@example.com

PASS=0
step() { printf '\n== %s\n' "$*"; }
ok() { PASS=$((PASS + 1)); printf '   ok: %s\n' "$*"; }
fail() {
  printf '   FAIL: %s\n' "$*" >&2
  [ -s "$WORK/server.log" ] && tail -40 "$WORK/server.log" >&2
  exit 1
}
check() { local msg=$1; shift; if "$@"; then ok "$msg"; else fail "$msg"; fi; }
quiet() { "$@" >/dev/null 2>&1; }

gh_create() { curl -fsS -X POST "$API/repos/$1/issues" -d "$2" | jq .number; }
gh_post() { curl -fsS -X POST "$API/repos/$1/issues/$2" -d "$3" >/dev/null; }
gh_id() { curl -fsS "$API/repos/$1/issues/$2" | jq .id; }
dolt_ref() { git -C "$WORK/git/$1.git" rev-parse -q --verify refs/dolt/data || echo none; }
server() { "$BIN" server --config "$WORK/config.toml" "$@"; }
syncs() { grep -c " syncing " "$WORK/server.log" || true; }
syncs_at_least() { [ "$(syncs)" -ge "$1" ]; }
ref_moved() { [ "$(dolt_ref "$1")" != "$2" ]; }

# send EVENT PAYLOAD [SECRET]: deliver a webhook, print the HTTP status.
send() {
  local sig
  sig=$(printf '%s' "$2" | openssl dgst -sha256 -hmac "${3:-$WEBHOOK_SECRET}" | awk '{print $NF}')
  curl -sS -o /dev/null -w '%{http_code}' -X POST "$URL/webhook" \
    -H "X-GitHub-Event: $1" -H "X-Hub-Signature-256: sha256=$sig" \
    -H 'Content-Type: application/json' --data-binary "$2"
}
issue_json() { jq -cn --arg r "$1" --argjson n "$2" '{number: $n, repository_url: "https://api.github.com/repos/\($r)"}'; }
issue_event() { jq -cn --arg r "$1" --argjson i "$(issue_json "$1" "$2")" '{action: "opened", repository: {full_name: $r}, issue: $i}'; }

# wait_for SECONDS CMD...: poll until CMD succeeds.
wait_for() {
  local deadline=$((SECONDS + $1)); shift
  until "$@"; do
    [ "$SECONDS" -lt "$deadline" ] || return 1
    sleep 0.2
  done
}

# Pull a project's Dolt data into a scratch clone and print its beads.
beads_of() {
  rm -rf "$WORK/check"
  git clone -q "$WORK/git/$1.git" "$WORK/check" 2>/dev/null
  (cd "$WORK/check" && quiet bd bootstrap --yes && bd export 2>/dev/null)
}
has_bead() { beads_of "$1" | jq -e --arg t "$2" 'select(.title == $t)' >/dev/null; }

new_project() {
  local repo=$1 prefix=$2
  git init -q --bare "$WORK/git/$repo.git"
  git -C "$WORK/git/$repo.git" symbolic-ref HEAD refs/heads/main
  git clone -q "$WORK/git/$repo.git" "$WORK/src/$repo" 2>/dev/null
  (
    cd "$WORK/src/$repo"
    git checkout -q -b main
    quiet bd init --quiet --prefix "$prefix" --skip-agents --skip-hooks
    git add -A && { git commit -qm "init beads" || true; } && git push -q origin main
    quiet bd dolt push
  )
}

cat >"$WORK/config.toml" <<EOF
listen = "127.0.0.1:$PORT"
data_dir = "$WORK/data"
public_url = "$URL/"
debounce_ms = 1000
max_wait_ms = 5000
git_base = "file://$WORK/git"
api_url = "$API"
EOF

# --------------------------------------------------------------------------
step "add: clones, syncs and registers the webhook"
new_project acme/widgets wid
new_project acme/gadgets gad
n=$(gh_create acme/widgets '{"title":"existing widget issue","body":"x"}')
server add acme/widgets 2>"$WORK/add.log"
check "cloned" test -d "$WORK/data/acme/widgets/.git"
check "initial sync pulled the existing issue" has_bead acme/widgets "existing widget issue"
hooks=$(curl -fsS "$API/repos/acme/widgets/hooks")
check "webhook created" test "$(jq length <<<"$hooks")" = 1
check "webhook points at the server" test "$(jq -r '.[0].config.url' <<<"$hooks")" = "$URL/webhook"
check "webhook subscribes to relation events" test "$(jq -r '.[0].events | index("sub_issues") != null' <<<"$hooks")" = true
check "webhook has the secret" test "$(jq -r '.[0].config.secret' <<<"$hooks")" = "$WEBHOOK_SECRET"
server add acme/widgets 2>"$WORK/add.log"
check "re-adding updates instead of duplicating" test "$(curl -fsS "$API/repos/acme/widgets/hooks" | jq length)" = 1
server add acme/gadgets --no-webhook 2>/dev/null
check "--no-webhook registers nothing" test "$(curl -fsS "$API/repos/acme/gadgets/hooks" | jq length)" = 0
check "list shows both projects" test "$(server list | cut -f1 | paste -sd,)" = "acme/gadgets,acme/widgets"

# --------------------------------------------------------------------------
step "serve: answers pings and rejects bad signatures"
server serve >"$WORK/server.log" 2>&1 &
SERVE_PID=$!
wait_for 20 curl -fsS -o /dev/null "$URL/healthz" || fail "server did not start"
wait_for 30 syncs_at_least 2 || fail "startup reconcile did not run"
sleep 2
check "ping" test "$(send ping '{"zen":"hi"}')" = 200
ref=$(dolt_ref acme/widgets)
check "bad signature is 401" test "$(send issues "$(issue_event acme/widgets "$n")" wrong-secret-000000)" = 401
check "unsigned is 401" test "$(curl -sS -o /dev/null -w '%{http_code}' -X POST "$URL/webhook" -H 'X-GitHub-Event: issues' -d '{}')" = 401
check "untracked repo is ignored" test "$(send issues "$(issue_event acme/other 1)")" = 202

# --------------------------------------------------------------------------
step "serve: a GitHub issue reaches the project's beads within seconds"
n=$(gh_create acme/widgets '{"title":"webhook widget issue","body":"x"}')
before=$(syncs)
start=$SECONDS
check "accepted" test "$(send issues "$(issue_event acme/widgets "$n")")" = 202
wait_for 30 ref_moved acme/widgets "$ref" || fail "Dolt remote not updated"
ok "Dolt remote updated after $((SECONDS - start))s"
check "bead arrived" has_bead acme/widgets "webhook widget issue"

step "serve: a burst of events is one sync"
before=$(syncs)
a=$(gh_create acme/widgets '{"title":"burst one","body":"x"}')
b=$(gh_create acme/widgets '{"title":"burst two","body":"x"}')
for i in "$a" "$b" "$a"; do send issues "$(issue_event acme/widgets "$i")" >/dev/null; sleep 0.2; done
wait_for 30 has_bead acme/widgets "burst two" || fail "burst not synced"
check "one sync for three events" test "$(syncs)" = $((before + 1))
check "both issues in that sync" grep -q "syncing issues $a $b" "$WORK/server.log"

step "serve: sub-issue events sync relations"
gh_post acme/widgets "$a/sub_issues" "{\"sub_issue_id\": $(gh_id acme/widgets "$b")}"
payload=$(jq -cn --argjson p "$(issue_json acme/widgets "$a")" --argjson s "$(issue_json acme/widgets "$b")" \
  '{action: "sub_issue_added", repository: {full_name: "acme/widgets"}, parent_issue: $p, sub_issue: $s}')
check "accepted" test "$(send sub_issues "$payload")" = 202
child_has_parent() {
  beads_of acme/widgets | jq -se '
    (map({key: .id, value: .title}) | from_entries) as $t
    | any(.[]; .title == "burst two" and any(.dependencies[]?; .type == "parent-child" and $t[.depends_on_id] == "burst one"))' >/dev/null
}
wait_for 30 child_has_parent || fail "sub-issue not synced"
ok "sub-issue became a parent-child dependency"

step "serve: projects are kept apart"
gref=$(dolt_ref acme/gadgets)
wref=$(dolt_ref acme/widgets)
g=$(gh_create acme/gadgets '{"title":"gadget issue","body":"x"}')
check "accepted" test "$(send issues "$(issue_event acme/gadgets "$g")")" = 202
wait_for 30 has_bead acme/gadgets "gadget issue" || fail "gadget issue not synced"
ok "gadget issue reached acme/gadgets"
check "acme/widgets untouched" test "$(dolt_ref acme/widgets)" = "$wref"
check "acme/gadgets has no widget issues" test "$(beads_of acme/gadgets | jq -s 'map(select(.title | test("widget|burst"))) | length')" = 0

step "serve: shuts down on SIGTERM"
kill -TERM "$SERVE_PID"
wait_for 10 sh -c "! kill -0 $SERVE_PID 2>/dev/null" || fail "still running"
SERVE_PID=
ok "stopped"

printf '\nAll %d checks passed.\n' "$PASS"
