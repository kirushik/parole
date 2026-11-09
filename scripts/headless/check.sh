#!/usr/bin/env bash
# Drives refview without a window against the fixture repository and writes stills and
# element trees to out/. Run from the repository root inside the dev shell:
#   scripts/headless/check.sh
# The reviewed list goes to a scratch state directory, not the real one.
set -euo pipefail
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
# REFVIEW_WAIT: a still is taken only once its diff has landed.
export XDG_STATE_HOME="$tmp/state" RUNE_PERF_WARN=0 REFVIEW_WAIT=1
bin=${REFVIEW_BIN:-target/debug/refview}
fixture="$tmp/fixture"
scripts/fixture.sh "$fixture" > /dev/null
mkdir -p out
here=scripts/headless

# Each run starts with nothing reviewed, so a script's `r` always marks.
run() {
    rm -rf "$tmp/state"
    "$bin" "$fixture" "$@" 2>&1 | grep -v '^rune:' || true
}

run --snap out/first.png --theme dark
run --tree out/first.json
run --script $here/keys.json --at mark:end --snap out/keys.png --theme dark
run --script $here/keys.json --at mark:end --tree out/keys.json
echo "reviewed after keys.json:"
cat "$XDG_STATE_HOME/refview/reviewed"
run --script $here/click.json --at mark:end --snap out/click.png --theme light
run --script $here/click.json --at mark:end --tree out/click.json
echo "reviewed after click.json:"
cat "$XDG_STATE_HOME/refview/reviewed"
run --scene last --tree out/last.json
"$bin" "$tmp" --snap out/not-a-repo.png --theme dark 2>&1 | grep -v '^rune:' || true

# The slow path: a git that takes 0.3 s outlasts the wait, so the window says "Reading" and
# the diff lands later through `poll`. Real time, since the worker runs on the wall clock.
slow="$tmp/slowbin"
mkdir -p "$slow"
printf '#!/bin/sh\nsleep 0.3\nexec %s "$@"\n' "$(command -v git)" > "$slow/git"
chmod +x "$slow/git"
for at in early end; do
    rm -rf "$tmp/state"
    REFVIEW_WAIT= PATH="$slow:$PATH" "$bin" "$fixture" --realtime --script $here/slow.json --at mark:$at --tree out/slow-$at.json 2>&1 | grep -v '^rune' || true
done
grep -q 'Reading' out/slow-early.json || { echo "slow path: no Reading note while git runs"; exit 1; }
grep -q '4 files' out/slow-end.json || { echo "slow path: the diff never landed"; exit 1; }
grep -q 'Reading' out/slow-end.json && { echo "slow path: still Reading at the end"; exit 1; }
echo "slow path ok"
