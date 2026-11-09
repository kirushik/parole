#!/usr/bin/env bash
# Drives refview without a window against the fixture repository and writes stills and
# element trees to out/. Run from the repository root inside the dev shell:
#   scripts/headless/check.sh
# The reviewed list goes to a scratch state directory, not the real one.
set -euo pipefail
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
export XDG_STATE_HOME="$tmp/state" RUNE_PERF_WARN=0
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
