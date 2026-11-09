#!/usr/bin/env bash
# Draws refview without a window against the fixture repository: stills and element trees
# in out/, for looking at. Behaviour is asserted by `cargo test`; this checks what only a real
# run shows: that the binary starts, the scripts replay, the reviewed list is written where it
# should be, and a slow git goes through "Reading" to its diff.
# Run from the repository root inside the dev shell:
#   cargo build && scripts/headless/check.sh
set -euo pipefail
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT
# The reviewed list goes to a scratch state directory, not the real one.
# REFVIEW_WAIT: a still is taken only once git has answered.
export XDG_STATE_HOME="$tmp/state" RUNE_PERF_WARN=0 REFVIEW_WAIT=1
bin=${REFVIEW_BIN:-target/debug/refview}
fixture="$tmp/fixture"
scripts/fixture.sh "$fixture" > /dev/null
rm -rf out
mkdir -p out
here=scripts/headless
reviewed="$tmp/state/refview/reviewed"

fail() { echo "check.sh: $*" >&2; exit 1; }

# Each run starts with nothing reviewed, so a script's `r` always marks. Rune's own progress
# lines are dropped; the app's exit status is kept.
run() {
    rm -rf -- "$tmp/state"
    local log="$tmp/log"
    "$bin" "$@" > "$log" 2>&1 || { cat "$log" >&2; fail "refview $* failed"; }
    grep -v '^rune' "$log" || true
}

sha() { git -C "$fixture" rev-parse "$1"; }
# The tree prints a header field as `"key": "title"` ... `"text": "..."`; one grep per claim.
has() { grep -qF -- "$2" "$1" || fail "$1: expected $2"; }
lacks() { ! grep -qF -- "$2" "$1" || fail "$1: did not expect $2"; }

run "$fixture" --snap out/first.png --theme dark
run "$fixture" --tree out/first.json
has out/first.json 'Document the retry limit'
has out/first.json '"1 file"'

# n, n, r, s: onto the first commit of jail/retry-jitter, marked, split.
run "$fixture" --script $here/keys.json --at mark:end --snap out/keys.png --theme dark
run "$fixture" --script $here/keys.json --at mark:end --tree out/keys.json
[ "$(cat "$reviewed")" = "$(sha 'refs/jail/retry-jitter~2')" ] || fail "keys.json: reviewed list is $(cat "$reviewed")"
has out/keys.json 'Clients that fail together retried together'
has out/keys.json '"Reviewed"'

# Click the last row, click "Mark reviewed", then p: the key must still work after the click.
run "$fixture" --script $here/click.json --at mark:end --snap out/click.png --theme light
run "$fixture" --script $here/click.json --at mark:end --tree out/click.json
[ "$(cat "$reviewed")" = "$(sha refs/jail/retry-jitter)" ] || fail "click.json: reviewed list is $(cat "$reviewed")"
has out/click.json "$(git -C "$fixture" rev-parse --short 'refs/jail/retry-jitter~1')  ·  jail-agent"
lacks out/click.json '"focused": true'

run "$fixture" --scene last --tree out/last.json
has out/last.json 'docs/backoff.md'

run "$tmp" --snap out/not-a-repo.png --theme dark
run "$tmp" --tree out/not-a-repo.json
has out/not-a-repo.json 'not a git repository'

# A bad argument is refused before any window.
if "$bin" "$fixture" --refs 2> "$tmp/err"; then fail "--refs with no pattern was accepted"; fi
grep -q 'usage: refview' "$tmp/err" || fail "no usage line for a bad argument"

# The slow path: a git that takes 0.3 s outlasts the wait, so the window says "Reading" and
# the diff lands later through `poll`. Real time, since the worker runs on the wall clock.
slow="$tmp/slowbin"
mkdir -p "$slow"
REAL_GIT=$(command -v git)
export REAL_GIT
printf '#!/bin/sh\nsleep 0.3\nexec "$REAL_GIT" "$@"\n' > "$slow/git"
chmod +x "$slow/git"
for at in early end; do
    REFVIEW_WAIT= PATH="$slow:$PATH" run "$fixture" --realtime --script $here/slow.json --at mark:$at --tree out/slow-$at.json
done
has out/slow-early.json 'Reading'
lacks out/slow-early.json '"4 files"'
has out/slow-end.json '"4 files"'
lacks out/slow-end.json 'Reading'
echo "headless checks ok"
