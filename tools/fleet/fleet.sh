#!/bin/bash
# Mac test fleet worker (see docs/fleet.md; agents read docs/fleet-join.md).
# Takes a test-matrix task from this repository's issues, runs the native
# and Wine test suites at the task's commit, and posts the counts. Only the
# checked result JSON is posted. Logs stay in ~/Library/Caches/victory-fleet.
# shellcheck disable=SC2016 # literal backticks, jq and GraphQL in single quotes
set -euo pipefail
# shellcheck source=tools/fleet/lib.sh
. "$(dirname "$0")/lib.sh"

ROOT="$(cd "$FLEET_DIR/../.." && pwd)"
REPO="${FLEET_REPO:-$(conf .repo)}"
CACHE="$HOME/Library/Caches/victory-fleet"
WORK="$CACHE/work"
LEGAL="$ROOT/docs/fleet-legal.md"
WINE_BIN="$HOME/Library/Caches/victory-macos/target/wine-cx/bin/wine"
MIN_FREE_GB=25

say() { printf 'fleet: %s\n' "$*"; }
die() {
    printf 'fleet: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat << 'EOF'
Usage: tools/fleet/fleet.sh <command>

  doctor            check this Mac is ready (run first)
  accept            read and accept docs/fleet-legal.md (a person runs this in Terminal)
  next              list open tasks for this Mac's chip
  claim N           take task N (posts /claim)
  run N             run both test suites for task N (long; posts /beat while running)
  submit N          post the result of task N (posts /result)
  release N REASON  hand task N back; REASON is setup_failed, wrong_hardware, no_time or other
  status            list the tasks you hold
EOF
}

me() { gh api user --jq .login; }
brand() { sysctl -n machdep.cpu.brand_string 2> /dev/null || true; }
my_chip() { chip_family_of "$(brand)"; }
legal_hash() { shasum -a 256 "$LEGAL" | cut -d ' ' -f 1; }
accepted() { [ -f "$CACHE/accepted-legal" ] && [ "$(cat "$CACHE/accepted-legal")" = "$(legal_hash)" ]; }
gptk_dmg() { find "$HOME/Downloads" -maxdepth 1 -name 'Game_Porting_Toolkit*.dmg' 2> /dev/null | sort | tail -n 1; }

gptk_version() {
    local v
    v=$(basename "$(gptk_dmg)" .dmg | sed -nE 's/^Game_Porting_Toolkit[_ -]?v?([0-9]{1,2}(\.[0-9]{1,2}){0,2}(-beta[0-9]?)?)$/\1/p')
    echo "${v:-unknown}"
}

need_number() { [[ ${1:-} =~ ^[0-9]{1,7}$ ]] || die "give a task number"; }

# Loads task N into T_COMMIT, T_CHIP, T_STATE, T_ASSIGNEE and T_LABELS.
load_task() {
    local j author task
    j=$(gh issue view "$1" -R "$REPO" --json body,author,state,assignees,labels) || die "cannot read issue #$1"
    author=$(jq -r .author.login <<< "$j")
    is_maintainer "$author" || die "issue #$1 was not opened by a maintainer, so it is not a fleet task"
    task=$(parse_task "$(jq -r .body <<< "$j")") || die "issue #$1 is not a fleet task"
    T_COMMIT=${task% *}
    T_CHIP=${task#* }
    T_STATE=$(jq -r .state <<< "$j")
    T_ASSIGNEE=$(jq -r '.assignees[0].login // ""' <<< "$j")
    T_LABELS=$(jq -r '[.labels[].name] | join(" ")' <<< "$j")
    [ "$T_CHIP" = "$(my_chip)" ] || die "task #$1 is for $T_CHIP Macs and this is $(brand)"
}

require_lease() {
    [ "$T_ASSIGNEE" = "$(me)" ] && [[ " $T_LABELS " == *" $(label leased) "* ]] ||
        die "you do not hold the lease on task #$1 (it may have expired); run: tools/fleet/fleet.sh claim $1"
}

cmd_doctor() {
    local ok=1 free
    check() {
        if [ "$2" = 1 ]; then say "ok    $1"; else
            say "FAIL  $1: $3"
            ok=0
        fi
    }
    t() { if "$@" > /dev/null 2>&1; then echo 1; else echo 0; fi; }
    check "Apple Silicon Mac" "$(t test "$(sysctl -n hw.optional.arm64 2> /dev/null)" = 1)" "the fleet needs an M-series Mac"
    check "macOS 15 or newer" "$(t test "$(sw_vers -productVersion 2> /dev/null | cut -d . -f 1)" -ge 15)" "update macOS"
    check "known chip ($(brand))" "$(t is_chip "$(my_chip)")" "this chip is not in tools/fleet/fleet.json yet"
    check "Homebrew" "$(t command -v brew)" "the person installs it from https://brew.sh"
    check "git and jq" "$(t sh -c 'command -v git && command -v jq')" "install with: brew install git jq"
    check "GitHub CLI signed in" "$(t gh auth status)" "the person runs: gh auth login"
    if [ -x "$WINE_BIN" ]; then
        check "setup already done once" 1 ""
    else
        free=$(df -g "$HOME" | awk 'NR == 2 { print $4 }')
        check "$MIN_FREE_GB GB free for the first setup" "$(t test "${free:-0}" -ge $MIN_FREE_GB)" "only ${free:-?} GB free"
        check "Game Porting Toolkit in Downloads" "$(t test -n "$(gptk_dmg)")" \
            "the person downloads it from Apple (README step 2) and accepts Apple's terms themselves"
    fi
    check "fleet terms accepted" "$(t accepted)" "the person reads docs/fleet-legal.md and runs, in Terminal: tools/fleet/fleet.sh accept"
    if [ "$ok" = 1 ]; then say "this Mac is ready"; else die "fix the FAIL lines above first"; fi
}

cmd_accept() {
    [ -t 0 ] && [ -t 1 ] || die "a person runs this in Terminal, not an agent: tools/fleet/fleet.sh accept"
    sed -n '/^## The rules/,/^## /p' "$LEGAL" | sed '$d'
    printf '\nFull text: %s\nType "I agree" to accept the fleet rules: ' "$LEGAL"
    local answer
    read -r answer
    [ "$answer" = "I agree" ] || die "not accepted"
    mkdir -p "$CACHE"
    legal_hash > "$CACHE/accepted-legal"
    say "accepted"
}

cmd_next() {
    local chip list
    chip=$(my_chip)
    is_chip "$chip" || die "unknown chip: $(brand)"
    list=$(gh issue list -R "$REPO" --state open --label "$(label task)" --label "$(label open)" \
        --label "$(chip_label "$chip")" --limit 10 --json number,title --jq '.[] | "#\(.number)\t\(.title)"')
    if [ -z "$list" ]; then say "no open tasks for $chip Macs right now"; else printf '%s\n' "$list"; fi
}

cmd_claim() {
    need_number "${1:-}"
    accepted || die "the fleet terms are not accepted yet; run tools/fleet/fleet.sh doctor"
    load_task "$1"
    [ "$T_STATE" = OPEN ] && [[ " $T_LABELS " == *" $(label open) "* ]] || die "task #$1 is not open"
    local who
    who=$(me)
    gh issue comment "$1" -R "$REPO" --body "/claim" > /dev/null
    say "claim posted; waiting for the queue to answer"
    for _ in $(seq 1 36); do
        sleep 5
        load_task "$1"
        if [ "$T_ASSIGNEE" = "$who" ] && [[ " $T_LABELS " == *" $(label leased) "* ]]; then
            say "task #$1 is yours. Next: tools/fleet/fleet.sh run $1"
            return
        fi
        [ -n "$T_ASSIGNEE" ] && die "task #$1 went to someone else; run tools/fleet/fleet.sh next"
    done
    die "no lease after 3 minutes; read the replies on https://github.com/$REPO/issues/$1"
}

cmd_run() {
    need_number "${1:-}"
    local n=$1 logs setup native wine failed result
    accepted || die "the fleet terms are not accepted yet; run tools/fleet/fleet.sh doctor"
    load_task "$n"
    require_lease "$n"
    logs="$CACHE/logs/$n"
    mkdir -p "$logs"

    say "checking out ${T_COMMIT:0:12} in $WORK"
    git -C "$ROOT" fetch --quiet "https://github.com/$REPO.git" "$T_COMMIT"
    git -C "$ROOT" worktree remove --force "$WORK" > /dev/null 2>&1 || true
    git -C "$ROOT" worktree prune
    git -C "$ROOT" worktree add --quiet --detach "$WORK" "$T_COMMIT"
    [ "$(git -C "$WORK" rev-parse HEAD)" = "$T_COMMIT" ] || die "checkout did not land on $T_COMMIT"

    # Keep the lease alive while the suites run.
    (while sleep 3000; do gh issue comment "$n" -R "$REPO" --body "/beat" > /dev/null 2>&1 || true; done) &
    local beat_pid=$!
    trap 'kill "$beat_pid" 2> /dev/null || true' EXIT

    say "running setup.sh (the first run builds Wine and takes about an hour); log: $logs/setup.log"
    (cd "$WORK" && ./setup.sh) > "$logs/setup.log" 2>&1 && setup=ok || setup=failed
    : > "$logs/native.log"
    : > "$logs/wine.log"
    if [ "$setup" = ok ]; then
        say "running the native tests; log: $logs/native.log"
        (cd "$WORK" && PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --workspace --no-fail-fast) \
            > "$logs/native.log" 2>&1 || true
        say "running the Wine + D3DMetal tests; log: $logs/wine.log"
        (cd "$WORK" && WINE="$WINE_BIN" WINEDLLOVERRIDES="d3d12,d3d12core,dxgi=b" \
            tools/macos/test-wine.sh --workspace --no-fail-fast) > "$logs/wine.log" 2>&1 || true
    else
        say "setup failed; the result will say so. The log stays on this Mac: $logs/setup.log"
    fi
    native=$(counts_json "$logs/native.log")
    wine=$(counts_json "$logs/wine.log")
    failed=$({
        failed_names "$logs/native.log" native
        failed_names "$logs/wine.log" wine
    } | head -n 100 | jq -Rsc 'split("\n") | map(select(length > 0))')

    result=$(jq -n --arg commit "$T_COMMIT" --arg model "$(sysctl -n hw.model)" --arg chip "$(brand)" \
        --arg macos "$(sw_vers -productVersion)" --arg gptk "$(gptk_version)" --arg setup "$setup" \
        --argjson native "$native" --argjson wine "$wine" --argjson failed "$failed" \
        '{schema: 1, kind: "test-matrix", upstream_commit: $commit, hw_model: $model, chip: $chip,
          macos: $macos, gptk: $gptk, setup: $setup, native: $native, wine: $wine, failed_tests: $failed}')
    validate_result "$result" "$T_COMMIT" "$T_CHIP" > "$logs/result-errors.txt" ||
        die "the result does not pass the schema (see $logs/result-errors.txt); please report this"
    printf '%s\n' "$result" > "$logs/result.json"
    jq . <<< "$result"
    say "done. Show this to the person, then: tools/fleet/fleet.sh submit $n"
}

cmd_submit() {
    need_number "${1:-}"
    local n=$1 file="$CACHE/logs/$1/result.json" result
    load_task "$n"
    require_lease "$n"
    [ -f "$file" ] || die "no result for task #$n yet; run tools/fleet/fleet.sh run $n"
    result=$(jq -c . "$file")
    validate_result "$result" "$T_COMMIT" "$T_CHIP" > /dev/null || die "$file does not pass the schema"
    gh issue comment "$n" -R "$REPO" --body "$(printf '/result\n```json\n%s\n```\n' "$result")" > /dev/null
    say "result posted; waiting for the queue to check it"
    for _ in $(seq 1 24); do
        sleep 5
        if [ "$(gh issue view "$n" -R "$REPO" --json state --jq .state)" = CLOSED ]; then
            say "accepted: https://github.com/$REPO/issues/$n"
            return
        fi
    done
    die "not accepted yet; read the replies on https://github.com/$REPO/issues/$n"
}

cmd_release() {
    need_number "${1:-}"
    is_release_reason "${2:-}" || die "give a reason: $(conf '.release_reasons | join(", ")')"
    load_task "$1"
    require_lease "$1"
    gh issue comment "$1" -R "$REPO" --body "/release $2" > /dev/null
    say "release posted for task #$1"
}

cmd_status() {
    gh issue list -R "$REPO" --state open --assignee @me --label "$(label leased)" \
        --json number,title --jq '.[] | "#\(.number)\t\(.title)"'
}

case "${1:-help}" in
    doctor) cmd_doctor ;;
    accept) cmd_accept ;;
    next) cmd_next ;;
    claim) cmd_claim "${2:-}" ;;
    run) cmd_run "${2:-}" ;;
    submit) cmd_submit "${2:-}" ;;
    release) cmd_release "${2:-}" "${3:-}" ;;
    status) cmd_status ;;
    help | -h | --help) usage ;;
    *)
        usage >&2
        exit 2
        ;;
esac
