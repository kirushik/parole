# refview

Decides what a sandbox wrote may become a local branch of yours. A native window, and a
trial of [Rune](https://github.com/anomalyco/rune), pinned to one rev of its `dev` branch.

![A commit from refs/jail that reaches for the host, with the agent's notes](docs/screenshot.png)

A jail or a dev VM leaves its work as refs (`refs/jail/*`, `refs/devvm/*`): unsigned commits
by an agent, which nobody has read. refview takes a ref through three steps:

1. **Read it.** The commits HEAD lacks, oldest first, each as a diff. The files your machine
   would run without being asked are marked and come first. A small model reads each diff
   too, and leaves notes on what looks dangerous.
2. **Mark it.** `r` on each commit you have read.
3. **Take it.** Once every commit is marked, `t` copies them, signed by you, onto a new local
   branch `reviewed/…`. That branch is yours to switch to, push or merge.

Nothing the ref holds is ever checked out, built or run on the way.

## Read it, never run it

Reviewing runs these git commands, none of which writes to the repository: `version`,
`rev-parse`, `hash-object` (without `-w`), `for-each-ref`, `merge-base`, `log`, `show`,
`diff`, `rev-list`, `cat-file`, `check-ref-format` and `symbolic-ref` (to read).

Nothing a reviewed ref supplies can make git run a program or write a file:

- Commits are named only by full hashes that were checked to be hashes, and every revision
  sits behind `--end-of-options`. A commit message can't forge a row whose "hash" is
  `--output=FILE`.
- Diffs are made with `--no-ext-diff --no-textconv --submodule=short`, and signatures are
  never verified (`--no-show-signature`), whatever the repository's config says.
- A missing object in a partial clone is an error, not a fetch (`GIT_NO_LAZY_FETCH`). Only
  git 2.46 and later honour that everywhere, so an older git is refused.
- Replacement objects are ignored (`--no-replace-objects`): a hash shows the commit it names.
- Commit messages are drawn as text.

`cargo test` builds a repository that tries the forged row, an external diff program, a
textconv filter and a signature verifier, with controls showing the config is live, and
checks that no program ran and the forged file wasn't written. The submodule, lazy-fetch and
replacement flags are passed on every command but have no test of their own. See
[src/git.rs](src/git.rs).

What it does not defend against: your own git config pointing at something you don't trust,
and resource exhaustion beyond a 64 MB cap on one command's stdout (there is no time limit
on a git that hangs).

## What would run on your machine

Some files run, or decide what runs, without you choosing to run them: `build.rs` at the next
build, `.envrc` when you enter the folder, a workflow on the forge, `CLAUDE.md` when an agent
opens the project. refview marks them by path and file mode ([src/risk.rs](src/risk.rs) is
the whole list): they are listed above the diff with the reason, sorted first in it, and
counted on the commit's row (`runs 4`).

The list is fixed and short of complete. It can't tell that a crate is a proc macro, or that
an ordinary source file is imported by a build script. A file it doesn't mark is not thereby
safe; ordinary source runs when you run it.

## A second reader

Each diff you open is also sent to a small model (the `claude` CLI, model `sonnet`, no
tools), asked only for what would execute on your machine and what looks dangerous, deceptive
or reckless. It runs on its own thread: the diff is up and usable at once, and the notes
appear above it and under their lines when they come.

A reply is kept (`$XDG_STATE_HOME/refview/agent/`), named by a hash of the model, the
instructions and the diff, so the same diff is sent once. `a` asks the model again; what an
earlier look found stays, and a new look only adds to it. `--no-agent` sends nothing until you
press `a`. A diff over 300 KB isn't sent.

The diff was written by the thing being judged, and may try to talk the model round. So:

- Its notes only add marks. "The agent flagged nothing" is shown as that, with "that is not
  a clearance". Nothing it says marks a commit read or lets a take through.
- Its reply is read as lines of a fixed shape. Each names a file by its number in the patch
  and a line the patch shows; the rest is drawn as text. A reply that doesn't end the way it
  was asked to, or says more than is kept, is shown as such.
- It runs in an empty private folder, with your own Claude settings and none a folder could
  supply.

`REFVIEW_AGENT` names another program to run in its place.

## Taking a ref

`t`, or **Take as branch**, opens the take in place of the diff. It refuses, and says why,
unless every commit on the ref is marked reviewed. Enter starts it:

- Each commit HEAD lacks is copied with `git commit-tree`: the same tree, message, author and
  author date, with you as committer and your signature (`user.signingkey`, whatever
  `gpg.format` you use). Parents are rewritten to the copies, so merges keep their shape.
- A copy is checked against its original before it is used. A commit git would not reproduce
  exactly is refused, not approximated: one whose author git would tidy, one whose text says
  more than git shows (a second `tree`, a `parent` out of place), one whose message isn't
  UTF-8.
- Each signature is asked for in the window, one commit at a time, with the one being waited
  for marked: with a hardware key, that is one touch a commit. A signing program that puts up
  its own dialog for agent sessions (`CLAUDECODE`) doesn't see that variable here.
- When all are signed, the branch `reviewed/jail/NAME` is created at the last copy. An
  existing branch, or a symbolic ref at that name, is never moved. Not `jail/NAME`: git reads that as `refs/jail/NAME` first,
  and `git merge jail/NAME` would merge the unsigned original.

Nothing is checked out or merged, HEAD and the working tree stay as they are, and no commit
hook runs. While a take is up, nothing else in the window changes selection or marks. Esc
stops a take; a take that stops or fails makes no branch. The copies already signed are
remembered (`$XDG_STATE_HOME/refview/taken`), so the next try asks only for the signatures
still missing. A remembered copy is used only if it is a signed commit with the original's
tree, parents, message and author; whose signature it carries isn't checked, so that file is
trusted as far as that.

The copies are new commits with new hashes: the ref stays listed, with "Taken as branch …"
under it, until you delete it.

## State

The commits you have marked reviewed live in `$XDG_STATE_HOME/refview/reviewed`
(`~/.local/state/refview/reviewed`), one full hash per line, so reviewing writes nothing into
the repository. A ref rewritten to different commits shows them unread. If neither
`XDG_STATE_HOME` nor `HOME` is an absolute path, marks last only for the run.

## Use

```sh
nix develop             # or put `use flake` in an .envrc and `direnv allow`
cargo run --release -- ~/path/to/repo
```

Needs git 2.46 or later. `refview [DIR] [--refs PATTERN]...`: DIR defaults to the current
directory; `--refs` replaces the default patterns (`refs/jail`, `refs/devvm`), must start
with `refs/`, and may repeat.

`--no-agent` sends no diff to the model unless you press `a`.

`refview --snapshot [DIR]` is for a folder that isn't a repository (`jail --dir`): it shows
what `jail diff DIR` prints, committed or not, with the same marks and the same second
reader. There are no commits there to mark or take; `jail apply` is how that work comes in.

| Key | Does |
|---|---|
| `n` / `p` | next / previous row (a commit, or a ref's "All changes") |
| `j` / `k` | next / previous hunk |
| `]` / `[` | next / previous file |
| `s` | unified or split |
| `w` | wrap long lines |
| `r` | mark reviewed, or unmark (on "All changes": every listed commit of the ref) |
| `a` | ask the agent again about the diff on screen |
| `t` | take the ref as a signed branch (Enter confirms, Esc leaves) |
| `u` | read the refs again |

The keys follow the key's place on the keyboard on a Russian layout too.

Every key and click lands in one frame. One thing moves on its own: the mark beside the
commit whose signature is being waited for.

## Try it without a sandbox

```sh
scripts/fixture.sh /tmp/refview-fixture
cargo run -- /tmp/refview-fixture
```

The fixture has a ref that reaches for the host (`jail/speed-up-ci`) and its own throwaway
signing key, so a take there never asks for yours.

## Checking it without a window

```sh
cargo test                                  # behaviour, on real scratch repositories
cargo build && scripts/headless/check.sh    # stills and element trees in out/
```

`cargo test` drives the UI in-process (Rune's `Runtime`): keys, presses, reload, the
reviewed list, a take with a software key, a stand-in agent, a stand-in `jail`. `check.sh`
runs the real binary headless against the fixture, replaying the scripts in
[scripts/headless](scripts/headless), and checks the branch a take leaves.

## Not done

- Nothing here has signed with a hardware key or asked the real model: the tests sign with a
  software key and a stand-in answers for the agent.
- A take copies each commit's tree, parents, message, author and author date. Other headers
  (an old signature, a `mergetag`) are dropped.
- Stopping a take kills git, not the signing program it started, which gives up when its own
  wait for a touch ends. Closing the window mid-take stops nothing already started: the
  signature being waited for may still be made, and no branch is.
- In `--snapshot` mode a file that only moved isn't marked by its mode: the patch doesn't
  state it, and there is no tree here to ask.
- The agent is asked about at most three diffs at once; one opened while three are out waits
  for `a`.
- A ref with more than 500 commits can't be taken: only the newest 500 are listed to read.
- A selection's patch is read on a worker, and usually lands within the 20 ms the window
  waits for it; a slower one shows "Reading" first. Listing the refs (on start and on `u`)
  still runs on the UI thread, a few git calls per ref. The patch is parsed on the UI thread
  when it lands, so an enormous "All changes" can hitch once.
- Dates are in each commit's own timezone.
- Paths and names that aren't UTF-8 are shown with replacement characters.
- Two windows marking in the same instant can lose one mark: there is no lock.
- The reviewed and taken lists are never pruned on their own.
- No file tree beside the diff, no search, no comments.
