# parole

A parole board for the code your agents write in a sandbox: you read what it did, and it
gets out as a branch under your signature, or it doesn't.

If you let a coding agent work in a container or a VM, its commits come back unsigned, by
an author that isn't you, and unread. Checking the branch out to look at it is already
running it: your editor's language server builds it, `direnv` loads its `.envrc`, a hook
fires. parole is a native window that shows such work without checking anything out, keeps
track of what you have actually had in front of you, and only then makes a signed branch.

It is also a trial of [Rune](https://github.com/anomalyco/rune), a GPU UI toolkit for Rust
that is not released yet: parole builds against one pinned revision of it.

![A commit from refs/jail that reaches for the host: the model's notes on their lines, and one alarm](docs/screenshot.png)

It expects the sandbox's work as git refs outside `refs/heads` (by default `refs/jail/*` and
`refs/devvm/*`; `--refs` names others), fetched into your repository but never checked
out. It takes each ref through three steps:

1. **Read it.** The commits HEAD lacks, oldest first, each as a diff, or the ref's whole
   change as one. The files your machine would run without being asked come first and say
   why. A small model reads each diff too: its notes sit on the lines they are about, and
   what it takes for deliberate harm is an alarm, in red.
2. **Decide.** No key marks anything read. The window knows which changed lines have been on
   screen; a commit is read once all of its have. A ref can be read two ways: commit by
   commit, or as one change (press the ref's name, or `N`). **Take** fills as you go. **Drop**
   deletes the ref instead.
3. **Take it.** `t` makes a new local branch `reviewed/…`, signed by you, of what you read.
   Read commit by commit, it is a copy of each commit: one touch of your key a commit. Read
   as one change, it is one commit: one touch. Switching to the branch is a separate press.

Nothing the ref holds is ever checked out, built or run on the way.

## Read it, never run it

Reading runs these git commands, none of which writes to the repository: `version`,
`rev-parse`, `hash-object` (without `-w`), `for-each-ref`, `merge-base`, `log`, `show`,
`diff`, `rev-list`, `cat-file`, `check-ref-format` and `symbolic-ref` (to read).

Nothing a reviewed ref supplies can make git run a program or write a file:

- Commits are named only by full hashes that were checked to be hashes, and where a command
  takes options after its revisions they sit behind `--end-of-options`. A commit message can't forge a row whose "hash" is
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
opens the project. parole marks them by path and file mode ([src/risk.rs](src/risk.rs) is
the whole list): they come first in the diff, each with the reason after its path, and the
commit's row in the sidebar carries a terminal mark.

The list is fixed and short of complete. It can't tell that a crate is a proc macro, or that
an ordinary source file is imported by a build script. A file it doesn't mark is not thereby
safe; ordinary source runs when you run it.

## A second reader

Each diff you open is also sent to a small model (the `claude` CLI, model `sonnet`, no
tools), asked only for what would execute on your machine and what looks dangerous, deceptive
or reckless. It runs on its own thread: the diff is up and usable at once, and the notes
appear on their lines when they come, in the space the code leaves. A line takes one note;
any more for the same line go under it, as every note does in the two-column view.
A note too long for its line is cut; press it, or go to it with `m`, to open it. An alarm
goes under its line, filled red, and pushes the page down; so does a second note on one
line, and any note in two columns. Its ref and
file are marked, and Drop becomes the filled button. The model is fallible either way: an
alarm is a reason to look, and no alarm is not a clearance.

A reply is kept (`$XDG_STATE_HOME/parole/agent/`), named by a hash of the model, the
instructions and the diff, so the same diff is sent once. `a` asks the model again; what an
earlier look found stays, and a new look only adds to it. With a ref's whole change on
screen, the model is also asked about each of its commits (up to 40), one after another:
what came and went between them shows as a mark on that commit's stop and on the ref, though
it does not stop a take of the whole change, which leaves those commits out. `--no-agent` sends nothing until you
press `a`. A diff over 300 KB isn't sent.

The diff was written by the thing being judged, and may try to talk the model round. So:

- Its notes only add marks. Nothing it says marks a commit read or lets a take through, and
  an empty reply is drawn as no notes, not as a pass. Take waits only for a first reply to
  the diff on screen, so an alarm on its way isn't missed by a quick press.
- Its reply is read as lines of a fixed shape. Each names a file by its number in the patch
  and a line the patch shows; the rest is drawn as text. A reply that doesn't end the way it
  was asked to, or says more than is kept, is shown as such.
- It runs in an empty private folder, with your own Claude settings and none a folder could
  supply.

`PAROLE_AGENT` names another program to run in its place.

## Taking a ref

`t`, or **Take**, signs and makes the branch, with no question first: the question was the
reading. The button says which branch it will make: **Take 3 commits** when every commit has
been read, **Take as one commit** when only the whole change has.

You get what you read. A whole change read as one diff says nothing about the commits it was
made of: something written in one and removed in the next was never on screen. So those
commits are not copied. The one commit has the ref's final files, sits on the commit where
the ref left HEAD, and lists the subjects it stands for; nothing of the in-between commits
is in the branch's history, so no later `git bisect` can check one out and build it. It
keeps the commits' author when they all share one, and is yours otherwise. To keep the
history, read each commit: once all are read, Take copies them instead. It refuses, and says why, while a changed line has not been on screen or the model
is still on its first look at the diff on screen.

What "on screen" means: the top and the bottom of the line's row have each been inside the
window, clear of the file header that sticks to its top. Long lines wrap, always, so no part
of one is off to the side, and a line that wraps past the window's height counts only once
you have scrolled to its end. A file with no lines to show (a binary, a move, a mode change)
has one row saying all of that, and that row is read like a line: such files are not skipped.
An empty commit counts once drawn. A diff that fits in the window is therefore read as soon
as it is shown: the window can tell what was in front of you, not what you took in.

The marks are kept between runs (see State), by commit hash.

- Each commit HEAD lacks is copied with `git commit-tree`: the same tree, message, author and
  author date, with you as committer and your signature (`user.signingkey`, whatever
  `gpg.format` you use). Parents are rewritten to the copies, so merges keep their shape.
- A copy is checked against its original before it is used. A commit git would not reproduce
  exactly is refused, not approximated: one whose author git would tidy, one whose text says
  more than git shows (a second `tree`, a `parent` out of place), one whose message isn't
  UTF-8.
- Each signature is asked for in the window, one commit at a time, as a row of beads with the
  one being waited for pulsing: with a hardware key, that is one touch a commit. A signing program that puts up
  its own dialog for agent sessions (`CLAUDECODE`) doesn't see that variable here.
- When all are signed, the branch `reviewed/jail/NAME` is created at the last copy. An
  existing branch, or a symbolic ref at that name, is never moved. Not `jail/NAME`: git reads that as `refs/jail/NAME` first,
  and `git merge jail/NAME` would merge the unsigned original.

Nothing is checked out or merged, HEAD and the working tree stay as they are, and no commit
hook runs. While signatures are being made no key but Esc does anything, no other row can
be selected, and nothing else can be taken, dropped, undone or switched to. Esc pauses; a
take that is paused or fails makes no branch. The copies already signed are
remembered (`$XDG_STATE_HOME/parole/taken`), so going on asks only for the signatures
still missing. A remembered copy is used only if it is a signed commit with the original's
tree, parents, message and author; whose signature it carries isn't checked, so that file is
trusted as far as that.

The copies are new commits with new hashes: the ref stays listed, and the dashed pill at the
end of its track turns solid.

Once taken, **Switch to it** (Enter) runs `git switch` on the new branch. That is the one
thing here that changes your working tree, so it is never part of the take. **Undo** (`z`)
deletes the branch again; the signed copies stay. Both look again first. Undo refuses a
branch that has moved since the take (it has work of its own by then) or that any working
tree has checked out, and Switch refuses one that is no longer where the take left it.
`git switch` runs your checkout's own `post-checkout` hook, as switching by hand would.

## Dropping a ref

`x`, or **Drop**, deletes the ref (`git update-ref -d`, only if it still points where it
did; a symbolic ref, or one whose name isn't UTF-8, is refused). It stays in the sidebar, struck out, for the rest of the run, and `z` or its Undo
button puts it back. Its commits are still in the repository until git prunes them. The jail
clone or VM run the ref came from is not touched.

## State

The commits you have read live in `$XDG_STATE_HOME/parole/reviewed`
(`~/.local/state/parole/reviewed`), one a line: a commit's full hash, or for a ref's whole
change `whole:` with the two hashes it lies between. So reading writes nothing into the
repository. A ref rewritten to different commits shows them unread. If neither
`XDG_STATE_HOME` nor `HOME` is an absolute path, marks last only for the run.

## Use

```sh
nix develop             # or put `use flake` in an .envrc and `direnv allow`
cargo run --release -- ~/path/to/repo
```

Linux, on Wayland or X11, with Vulkan. Without Nix you need the Rust toolchain Rune pins
(1.99.1), git and ssh-keygen, and the libraries [flake.nix](flake.nix) lists (libxkbcommon,
wayland, vulkan-loader, fontconfig and the X11 ones). The first build fetches and compiles
Rune from GitHub. Other platforms are untried.

Needs git 2.46 or later. `parole [DIR] [--refs PATTERN]...`: DIR defaults to the current
directory; `--refs` replaces the default patterns (`refs/jail`, `refs/devvm`), must start
with `refs/`, and may repeat.

`--no-agent` sends no diff to the model unless you press `a`.

`parole --snapshot [DIR]` is for a folder that isn't a repository (`jail --dir`): it shows
what `jail diff DIR` prints, committed or not, with the same marks and the same second
reader. There are no commits there to take or drop; `jail apply` is how that work comes in.

| Key | Does |
|---|---|
| `p` / `n` | previous / next commit (a ref's whole change is passed over) |
| `P` / `N` | previous / next ref: its whole change, or its one commit |
| `k` / `j` | previous / next change |
| `[` / `]` | previous / next file |
| `m` | the model's next note, alarms first |
| `a` | ask the model again about the diff on screen |
| `s` | one column or two |
| `t` | take the ref as a signed branch (Esc pauses) |
| `x` | drop the ref |
| `z` | undo the last take or drop |
| Enter | on a taken ref: switch to its branch |
| `u` | read the refs again |
| `?` | this list, in the window |

The window shows no row of key hints: a key is drawn only on the button it presses, and the
rest are on the card `?` opens.

The keys follow the key's place on the keyboard on a Russian layout too.

Every key and click lands in one frame. One thing moves on its own: the bead of the
signature being waited for.

The diff view is rune-kit's, copied into [src/diff/](src/diff/) and changed: it knows which
lines have been on screen, puts a note on its own line, and has no syntax colours. The map
on the right is the scrollbar: one stroke a row (in a long diff, several rows), dim until
that row has been on screen.

A held key repeats for moving about, never for `t`, `x`, `z` or Enter.

## Try it without a sandbox

```sh
scripts/fixture.sh /tmp/parole-fixture
cargo run -- /tmp/parole-fixture
```

The fixture has a ref that reaches for the host (`jail/speed-up-ci`) and its own throwaway
signing key, so a take there never asks for yours. To try a take with your own key (and its
touch), build it with `scripts/fixture.sh /tmp/parole-fixture --own-key`.

Its commit hashes are the same every time it is built, so what you read and took there last
time is still read and taken. For a clean start, keep this run's state apart:

```sh
XDG_STATE_HOME=$(mktemp -d) cargo run -- /tmp/parole-fixture
```

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

- A take with a hardware key and the real model have each been tried by hand, on the fixture
  below. The tests sign with a software key, and a stand-in answers for the agent.
- One dark theme, and no syntax colours.
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
