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
# Git reads no config but the fixture's own, so a take signs with the fixture's key.
export XDG_STATE_HOME="$tmp/state" RUNE_PERF_WARN=0 REFVIEW_WAIT=1 GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
bin=${REFVIEW_BIN:-target/debug/refview}
# No run here reaches the real model: a stand-in answers, with nothing to say unless a check
# gives it something.
agent="$tmp/agent.sh"
quiet() { printf '#!/bin/sh\ncat > /dev/null\necho END\n' > "$agent"; chmod +x "$agent"; }
quiet
export REFVIEW_AGENT="$agent"
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
has out/last.json 'Speed up CI with a build cache'

# The last ref's commit reaches for the host: its files are listed above the diff with why,
# and come first in it. With --no-agent nothing is sent. Without it the agent is asked as the
# diff is shown: a stand-in here, which gets the numbered patch and names files by number.
printf '#!/bin/sh\ncat > "%s"\nprintf '"'"'FLAG\\t5\\t3\\tSends the cargo credentials file to another host.\\nFLAG\\t3\\t3\\tRuns that script at every build.\\nEND\\n'"'"'\n' "$tmp/agent-saw" > "$agent"
run "$fixture" --no-agent --realtime --script $here/runs.json --at mark:end --snap out/runs.png --theme dark
run "$fixture" --no-agent --realtime --script $here/runs.json --at mark:end --tree out/runs.json
has out/runs.json '4 files here would run on this machine'
has out/runs.json 'cargo runs it at every build'
has out/runs.json 'direnv runs it on entering the folder'
lacks out/runs.json 'The agent left'
[ ! -e "$tmp/agent-saw" ] || fail "--no-agent: the agent was asked all the same"
run "$fixture" --realtime --script $here/runs.json --at mark:end --snap out/agent.png --theme dark
run "$fixture" --realtime --script $here/runs.json --at mark:end --tree out/agent.json
has out/agent.json 'The agent left 2 notes'
has out/agent.json 'scripts/warm-cache.sh:3'
has out/agent.json 'Sends the cargo credentials file to another host.'
grep -q '^     3 +curl' "$tmp/agent-saw" || fail "the agent wasn't given the patch with its line numbers"
grep -q '^=== file 5 ===$' "$tmp/agent-saw" || fail "the agent wasn't given the files' numbers"
quiet

# Taking jail/retry-jitter: refused while unread, then (marked) three signatures and a branch.
# Real time: the signing runs on its own thread, on the wall clock.
for at in blocked confirm; do
    run "$fixture" --realtime --script $here/take.json --at mark:$at --tree out/take-$at.json
done
has out/take-blocked.json "3 of its 3 commits aren't marked reviewed yet"
has out/take-confirm.json 'Take jail/retry-jitter as the branch reviewed/jail/retry-jitter?'
has out/take-confirm.json 'Enter  sign and create the branch'
[ -z "$(git -C "$fixture" for-each-ref refs/heads/reviewed)" ] || fail "a branch exists before any take was confirmed"
run "$fixture" --realtime --script $here/take.json --at mark:confirm --snap out/take-confirm.png --theme dark
run "$fixture" --realtime --script $here/take.json --at mark:end --tree out/take-end.json
has out/take-end.json 'Branch reviewed/jail/retry-jitter created'
taken=refs/heads/reviewed/jail/retry-jitter
[ "$(git -C "$fixture" rev-parse "$taken^{tree}")" = "$(git -C "$fixture" rev-parse 'refs/jail/retry-jitter^{tree}')" ] || fail "the taken branch's files differ from the ref's"
[ "$(git -C "$fixture" rev-list --count "main..$taken")" = 3 ] || fail "the taken branch isn't three commits on main"
for c in $(git -C "$fixture" rev-list "main..$taken"); do
    git -C "$fixture" cat-file commit "$c" | grep -q '^gpgsig ' || fail "$c on the taken branch isn't signed"
    [ "$(git -C "$fixture" log -1 --format='%an/%cn' "$c")" = jail-agent/Reader ] || fail "$c: wrong author or committer"
done
[ "$(git -C "$fixture" symbolic-ref HEAD)" = refs/heads/main ] || fail "the take moved HEAD"
[ -z "$(git -C "$fixture" status --porcelain)" ] || fail "the take touched the working tree"
# Once more with a signer that takes its time: the still catches a signature being waited for.
git -C "$fixture" update-ref -d "$taken"
slowsign="$tmp/slowsign.sh"
printf '#!/bin/sh\nsleep 0.8\nexec ssh-keygen "$@"\n' > "$slowsign"
chmod +x "$slowsign"
rm -rf -- "$tmp/state"
export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=gpg.ssh.program GIT_CONFIG_VALUE_0="$slowsign"
run "$fixture" --realtime --script $here/take.json --at mark:signing --snap out/take-signing.png --theme dark
run "$fixture" --realtime --script $here/take.json --at mark:signing --tree out/take-signing.json
unset GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0
has out/take-signing.json 'Signature 1 of 3: touch your key'
[ -z "$(git -C "$fixture" for-each-ref refs/heads/reviewed)" ] || fail "a take that was cut off left a branch"

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
