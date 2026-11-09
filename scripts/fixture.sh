#!/usr/bin/env bash
# Builds a small repository with sandbox-style refs to try refview on:
#   scripts/fixture.sh /tmp/refview-fixture && cargo run -- /tmp/refview-fixture
# Dates and authors are fixed, so the commit hashes (and headless snapshots) repeat.
# Needs GNU sed (`sed -i`).
set -euo pipefail
dir=${1:?usage: fixture.sh DIR}
# Nothing inherited may point git somewhere other than DIR, or change what it writes there.
unset $(git rev-parse --local-env-vars)
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
unset GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL GIT_COMMITTER_NAME GIT_COMMITTER_EMAIL GIT_TEMPLATE_DIR
rm -rf -- "$dir"
mkdir -p -- "$dir"
cd -- "$dir"

g() { git -c user.name="$who" -c user.email="$who@example.invalid" -c commit.gpgsign=false -c core.hooksPath=/dev/null "$@"; }
n=0
commit() {
    n=$((n + 1))
    local date="2026-11-0$(((n - 1) / 3 + 1))T1$((n % 10)):00:00+00:00"
    g add -A
    GIT_AUTHOR_DATE=$date GIT_COMMITTER_DATE=$date g commit -q "$@"
}

who=kirushik
git init -q -b main --template= .
mkdir -p src docs
cat > src/backoff.rs <<'RS'
use std::time::Duration;

/// How long to wait before trying a request again.
pub struct Backoff {
    base: Duration,
    max: Duration,
    attempt: u32,
}

impl Backoff {
    pub fn new(base: Duration, max: Duration) -> Backoff {
        Backoff { base, max, attempt: 0 }
    }

    /// The wait before the next attempt.
    pub fn next(&mut self) -> Duration {
        let wait = self.base * 2u32.pow(self.attempt);
        self.attempt += 1;
        wait.min(self.max)
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}
RS
cat > src/fetch.ts <<'TS'
export async function fetchWithRetry(url: string, tries = 3): Promise<Response> {
	for (let i = 0; i < tries; i++) {
		const res = await fetch(url)
		if (res.ok) return res
	}
	throw new Error(`gave up on ${url}`)
}
TS
printf '[retry]\nbase_ms = 100\nmax_ms = 1000\n' > retry.toml
printf '# Retry\n\nWaits double from `base_ms` up to `max_ms`.\n' > docs/retry.md
commit -m "Retry with exponential backoff"

# A jail ref with three commits.
who=jail-agent
git checkout -q -b work
sed -i 's|    attempt: u32,|    attempt: u32,\n    jitter: f32,|; s|attempt: 0 }|attempt: 0, jitter: 0.2 }|' src/backoff.rs
sed -i 's|pub fn next(&mut self) -> Duration {|pub fn next(\&mut self, roll: f32) -> Duration {|; s|        wait.min(self.max)|        wait.min(self.max).mul_f32(1.0 + self.jitter * (roll * 2.0 - 1.0))|' src/backoff.rs
commit -m "Add jitter to the backoff" -m "Clients that fail together retried together. Spread each wait by up to 20%.

The roll comes from the caller, so tests stay exact."
sed -i 's|self.base \* 2u32.pow(self.attempt)|self.base.saturating_mul(2u32.saturating_pow(self.attempt))|' src/backoff.rs
printf 'jitter = 0.2\n' >> retry.toml
commit -m "Saturate instead of overflowing on a long outage"
cat > src/fetch.ts <<'TS'
export async function fetchWithRetry(url: string, tries = 3, signal?: AbortSignal): Promise<Response> {
	let last: Response | undefined
	for (let i = 0; i < tries; i++) {
		last = await fetch(url, { signal })
		if (last.ok) return last
		await sleep(backoff(i))
	}
	throw new Error(`gave up on ${url} after ${tries} tries (${last?.status})`)
}
TS
git mv docs/retry.md docs/backoff.md
commit -m "fetchWithRetry waits between tries and can be aborted"
git update-ref refs/jail/retry-jitter HEAD

# A devvm ref with one commit, from main.
git checkout -q main
who=devvm-agent
git checkout -q -b vm
printf '\n## Limits\n\nNo more than five tries.\n' >> docs/retry.md
commit -m "Document the retry limit"
git update-ref refs/devvm/run-0042 HEAD

# A jail ref that main already has: nothing to review.
git checkout -q main
git update-ref refs/jail/merged-already HEAD
git branch -q -D work vm
echo "$dir"
