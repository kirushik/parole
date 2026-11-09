# refview

A native window for reading what a sandbox wrote before bringing it in. A trial of
[Rune](https://github.com/anomalyco/rune), pinned to one rev of its `dev` branch.

![A commit from refs/jail in split view](docs/screenshot.png)

It lists the refs under `refs/jail/` and `refs/devvm/`, the commits each has that `HEAD`
lacks (in git's log order, reversed so the oldest is on top), and shows the selected commit,
or the whole ref, as a diff.

## Read it, never run it

refview runs seven git commands, none of which writes to the repository: `rev-parse`,
`hash-object` (without `-w`), `for-each-ref`, `merge-base`, `log`, `show` and `diff`. It
never checks out, merges or builds a ref.

Nothing a reviewed ref supplies can make git run a program or write a file:

- Commits are named only by full hashes that were checked to be hashes, and every revision
  sits behind `--end-of-options`. A commit message can't forge a row whose "hash" is
  `--output=FILE`.
- Diffs are made with `--no-ext-diff --no-textconv --submodule=short`, and signatures are
  never verified (`--no-show-signature`), whatever the repository's config says.
- A missing object in a partial clone is an error, not a fetch (`GIT_NO_LAZY_FETCH`).
- Commit messages are drawn as text.

`cargo test` builds a repository whose config and ref try each of these and checks that
nothing ran and nothing was written. See [src/git.rs](src/git.rs).

What it does not defend against: your own git config pointing at something you don't trust,
and resource exhaustion beyond a 64 MB cap on one command's output (there is no time limit
on a git that hangs).

The list of commits you have marked reviewed lives in `$XDG_STATE_HOME/refview/reviewed`
(`~/.local/state/refview/reviewed`), one full hash per line, so reviewing writes nothing into
the repository. A ref rewritten to different commits shows them unread.

## Use

```sh
nix develop             # or put `use flake` in an .envrc and `direnv allow`
cargo run --release -- ~/path/to/repo
```

Needs git 2.31 or later. `refview [DIR] [--refs PATTERN]...`: DIR defaults to the current
directory; `--refs` replaces the default patterns (`refs/jail`, `refs/devvm`), must start
with `refs/`, and may repeat.

| Key | Does |
|---|---|
| `n` / `p` | next / previous row (a commit, or a ref's "All changes") |
| `j` / `k` | next / previous hunk |
| `]` / `[` | next / previous file |
| `s` | unified or split |
| `w` | wrap long lines |
| `r` | mark reviewed, or unmark (on "All changes": every listed commit of the ref) |
| `u` | read the refs again |

The keys follow the key's place on the keyboard on a Russian layout too.

## Try it without a sandbox

```sh
scripts/fixture.sh /tmp/refview-fixture
cargo run -- /tmp/refview-fixture
```

## Checking it without a window

```sh
cargo test                                  # behaviour, on real scratch repositories
cargo build && scripts/headless/check.sh    # stills and element trees in out/
```

`cargo test` drives the UI in-process (Rune's `Runtime`): keys, presses, reload, the
reviewed list. `check.sh` runs the real binary headless against the fixture, replaying
[keys.json](scripts/headless/keys.json), [click.json](scripts/headless/click.json) and
[slow.json](scripts/headless/slow.json).

## Not done

- A selection's patch is read on a worker, and usually lands within the 20 ms the window
  waits for it; a slower one shows "Reading" first. Listing the refs (on start and on `u`)
  still runs on the UI thread, a few git calls per ref. The patch is parsed on the UI thread
  when it lands, so an enormous "All changes" can hitch once.
- At most 500 commits are listed per ref (the newest; the sidebar says so). "All changes"
  still shows the whole range.
- Dates are in each commit's own timezone.
- Paths and names that aren't UTF-8 are shown with replacement characters.
- Two windows marking in the same instant can lose one mark: there is no lock.
- The reviewed list only grows; delete lines from the file to prune it.
- No file tree beside the diff, no search, no comments.
