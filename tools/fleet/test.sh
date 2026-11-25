#!/bin/bash
# Tests for the fleet's parsing, validation and moderation (no network).
# Run: tools/fleet/test.sh
# shellcheck disable=SC2016,SC2001 # literal backticks, jq and GraphQL in single quotes
set -euo pipefail
# shellcheck source=tools/fleet/lib.sh
. "$(dirname "$0")/lib.sh"

pass=0
fail=0
ok() { # NAME COMMAND... : the command must succeed
    local name=$1
    shift
    if "$@" > /dev/null 2>&1; then pass=$((pass + 1)); else
        fail=$((fail + 1))
        echo "FAIL: $name"
    fi
}
no() { # NAME COMMAND... : the command must fail
    local name=$1
    shift
    if "$@" > /dev/null 2>&1; then
        fail=$((fail + 1))
        echo "FAIL: $name"
    else pass=$((pass + 1)); fi
}
eq() { # NAME EXPECTED ACTUAL
    if [ "$2" = "$3" ]; then pass=$((pass + 1)); else
        fail=$((fail + 1))
        printf 'FAIL: %s\n  expected: %s\n  actual:   %s\n' "$1" "$2" "$3"
    fi
}

SHA=0123456789abcdef0123456789abcdef01234567
REPO=example/VIctory-macOS
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# --- tasks
body=$(task_body "$SHA" m3 "$REPO")
eq "task body round-trips" "$SHA m3" "$(parse_task "$body")"
eq "task body passes moderation" "" "$(moderate "$body")"
eq "task body with CRLF" "$SHA m3" "$(parse_task "$(sed 's/$/\r/' <<< "$body")")"
no "task with unknown chip" parse_task "$(task_body "$SHA" m9 "$REPO")"
no "task with short commit" parse_task "$(task_body 0123abc m3 "$REPO")"
no "task without a block" parse_task "commit: $SHA"
no "task with another kind" parse_task "$(sed 's/^kind: test-matrix/kind: shader/' <<< "$body")"

# --- toolkit filenames
eq "toolkit 4 beta 3 filename" "4.0-beta3" "$(gptk_version_of 'Game_Porting_Toolkit_4.0_beta_3.dmg')"
eq "toolkit 2 beta 3 filename" "2.0-beta3" "$(gptk_version_of 'Game_Porting_Toolkit_2.0_beta_3.dmg')"
eq "toolkit stable filename" "3.0" "$(gptk_version_of 'Game_Porting_Toolkit_3.0.dmg')"
eq "toolkit legacy beta filename" "3.0-beta4" "$(gptk_version_of 'Game_Porting_Toolkit_3.0-beta4.dmg')"
eq "toolkit beta without number" "4.0-beta" "$(gptk_version_of 'Game_Porting_Toolkit_4.0_beta.dmg')"
eq "toolkit version prefix" "3.0" "$(gptk_version_of 'Game_Porting_Toolkit_v3.0.dmg')"
eq "toolkit path with spaces" "4.0-beta3" "$(gptk_version_of '/tmp/path with spaces/Game_Porting_Toolkit_4.0 beta 3.dmg')"
eq "missing toolkit filename" "unknown" "$(gptk_version_of '')"
eq "unrecognized toolkit filename" "unknown" "$(gptk_version_of 'Game_Porting_Toolkit_latest.dmg')"
eq "unrelated disk image" "unknown" "$(gptk_version_of '4.0.dmg')"
eq "unfinished toolkit download" "unknown" "$(gptk_version_of 'Game_Porting_Toolkit_4.0_beta_3.dmg.part')"
eq "unsupported toolkit beta number" "unknown" "$(gptk_version_of 'Game_Porting_Toolkit_4.0_beta_10.dmg')"

# --- commands
eq "claim" claim "$(parse_command "/claim")"
eq "claim with spaces" claim "$(parse_command "  /claim  ")"
eq "beat" beat "$(parse_command "/beat")"
eq "release" "release no_time" "$(parse_command "/release no_time")"
eq "release with a bad reason" "" "$(parse_command "/release because")"
eq "claim with extra text" "" "$(parse_command "/claim
please")"
eq "command not on the first line" "" "$(parse_command "hi
/claim")"
eq "plain comment" "" "$(parse_command "Works on my M2 Air")"
eq "result" result "$(parse_command "/result")"

# --- results
good=$(jq -cn --arg c "$SHA" '{schema: 1, kind: "test-matrix", upstream_commit: $c, hw_model: "Mac15,12",
    chip: "Apple M3 Pro", macos: "15.4.1", gptk: "3.0", setup: "ok",
    native: {passed: 300, failed: 0, ignored: 2}, wine: {passed: 340, failed: 3, ignored: 0},
    failed_tests: ["wine/victory_gpu::tests::depth16", "wine/victory_shader::nan_sin"]}')
comment=$(printf '/result\n```json\n%s\n```\n' "$good")
eq "result block extracts" "$good" "$(result_json "$comment")"
no "result with text before the block" result_json "$(printf '/result\nhello\n```json\n{}\n```')"
no "result with text after the block" result_json "$(printf '/result\n```json\n{}\n```\nmore')"
no "result without a block" result_json "/result"
ok "valid result" validate_result "$good" "$SHA" m3
no "wrong commit" validate_result "$good" "${SHA%7}8" m3
no "wrong chip family" validate_result "$good" "$SHA" m2
bad() { validate_result "$(jq -c "$1" <<< "$good")" "$SHA" m3; }
no "extra field" bad '.notes = "see https://example.com"'
no "missing field" bad 'del(.gptk)'
no "free text in hw_model" bad '.hw_model = "Mac15,12 see my link"'
no "free text in chip" bad '.chip = "Apple M3 Pro https://x"'
no "free text in macos" bad '.macos = "15.4 beta (lol)"'
no "free text in a test name" bad '.failed_tests = ["wine/a b"]'
no "test name without a suite" bad '.failed_tests = ["victory_gpu::x"]'
no "too many test names" bad '.failed_tests = [range(101) | "wine/t\(.)"]'
no "negative count" bad '.native.failed = -1'
no "fractional count" bad '.native.failed = 1.5'
no "extra count field" bad '.native.note = 1'
no "bad setup value" bad '.setup = "maybe"'
no "array instead of object" validate_result '[1]' "$SHA" m3
no "not JSON" validate_result 'nope' "$SHA" m3
no "too large" validate_result "$(printf '%9000s' x)" "$SHA" m3
ok "null suites when setup failed" bad '.setup = "failed" | .native = null | .wine = null | .failed_tests = []'
ok "unknown toolkit version" bad '.gptk = "unknown"'
ok "normalized toolkit beta version passes schema" bad '.gptk = "4.0-beta3"'
ok "MacBook model id" bad '.hw_model = "MacBookAir10,1"'
ok "failing result" result_failing "$good"
no "passing result" result_failing "$(jq -c '.wine.failed = 0' <<< "$good")"
ok "setup failure is failing" result_failing "$(jq -c '.setup = "failed" | .native = null | .wine = null' <<< "$good")"
eq "validation error text" "upstream_commit is not the task commit" "$(validate_result "$good" "${SHA%7}8" m3 || true)"

# --- moderation
verdict() { moderate "$1" | cut -d ' ' -f 1; }
eq "clean comment" "" "$(verdict "Setup failed at the Wine build on my M1, log attached locally")"
eq "torrent link" delete "$(verdict "grab it here magnet:?xt=urn:btih:abc")"
eq "file host link" delete "$(verdict "https://MEGA.nz/file/abc")"
eq "game dump talk" hide "$(verdict "where do I get a game dump")"
eq "dumped my game" hide "$(verdict "I dumped my game yesterday")"
eq "decryption" hide "$(verdict "how do I decrypt the package")"
eq "container names" hide "$(verdict "it is an XVD file")"
eq "image attachment" hide "$(verdict "![x](https://github.com/user-attachments/assets/1)")"
eq "long encoded run" hide "$(verdict "$(printf 'A%.0s' $(seq 1 220))")"
eq "words containing short tokens" "" "$(verdict "the dumpster fire of the rpfoo tickets")"

# --- cargo output
cat > "$tmp/native.log" << 'EOF'
running 3 tests
test a::one ... ok
test a::two ... FAILED
test a::three ... ignored
test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.01s

running 2 tests
test b::four ... ok
test b::five - should panic ... FAILED
test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
EOF
eq "counts are summed" "2 2 1" "$(parse_counts "$tmp/native.log")"
eq "counts json" '{"passed":2,"failed":2,"ignored":1}' "$(counts_json "$tmp/native.log")"
eq "failed names keep only plain names" "native/a::two" "$(failed_names "$tmp/native.log" native)"
printf 'error: could not compile\n' > "$tmp/broken.log"
eq "no test run gives null" null "$(counts_json "$tmp/broken.log")"
eq "chip family" m3 "$(chip_family_of "Apple M3 Pro")"
eq "chip family of an Intel Mac" "" "$(chip_family_of "Intel(R) Core(TM) i7")"

# --- report
r1=$(jq -c '. + {n: 1, closed: "2026-11-20T10:00:00Z"}' <<< "$good")
r2=$(jq -c '. + {n: 2, closed: "2026-11-21T10:00:00Z", chip: "Apple M1", hw_model: "MacBookAir10,1", failed_tests: [], wine: {passed: 343, failed: 0, ignored: 0}}' <<< "$good")
r3=$(jq -c '. + {n: 3, closed: "2026-11-22T10:00:00Z", chip: "Apple M2", setup: "failed", native: null, wine: null, failed_tests: []}' <<< "$good")
report=$(printf '%s\n%s\n%s\n' "$r1" "$r2" "$r3" | report_markdown)
ok "report counts results" grep -q '^3 accepted results across 1 commits' <<< "$report"
ok "report coverage" grep -q '3 of 5: m1, m2, m3 | 2 of 3' <<< "$report"
ok "report chip-specific failure" grep -q 'victory_gpu::tests::depth16`: fails on m3; passes on m1' <<< "$report"
ok "report lists every result" grep -q '^| #3 |' <<< "$report"
eq "empty report" "None found." "$(: | report_markdown | sed -n '/^## Tests/{n;n;p;}')"

echo "fleet tests: $pass passed, $fail failed"
[ "$fail" = 0 ]
