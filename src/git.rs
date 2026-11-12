//! Everything refview asks of git. Reviewing only reads: `version`, `rev-parse`, `hash-object`
//! (without `-w`), `for-each-ref`, `merge-base`, `log`, `show`, `diff`, `rev-list`, `cat-file`,
//! `check-ref-format` and `symbolic-ref` (to read). Nothing is checked out, merged or built.
//!
//! Four things write (the last section of [`Repo`]), each only on the reader's own press:
//!
//! - Taking a ref: `commit-tree -S` makes signed copies of the ref's commits from their
//!   existing trees, and `update-ref` creates one new branch at the last of them. No working
//!   tree, index or existing ref is touched, and no hook of the commit kind runs.
//! - Undoing a take: `update-ref -d` deletes that branch, if it is still where the take left
//!   it and no working tree has it checked out.
//! - Dropping a ref and putting it back: `update-ref -d` on the ref if it still points where
//!   it was listed, and `update-ref` to recreate it if nothing is there by then.
//! - Switching to a taken branch: `git switch`, which changes the working tree and runs the
//!   checkout's own `post-checkout` hook, as switching by hand would.
//!
//! What a reviewed ref controls (file contents, paths, commit messages, author names, its own
//! `.gitattributes`) must not be able to make git run a program or write a file. So:
//!
//! - Every commit is named by a full hash that was checked to be one ([`is_object_id`]), and
//!   every revision sits behind `--end-of-options`, so nothing a ref supplies is read as an
//!   option (`--output=FILE` would write one; `--ext-diff` would undo the flags below).
//! - The log is framed with NUL. git formats a message as a C string, so even a commit object
//!   with a NUL in it can't put one inside a field.
//! - The patch commands refuse external diff drivers and textconv filters, never verify
//!   signatures (`log.showSignature` would run the signing program), and keep submodules to
//!   their one-line form (`diff.submodule=diff` would run git inside them without these flags).
//! - A missing object in a partial clone is an error, not a fetch. Only git 2.46 and later
//!   honour that everywhere, so an older git is refused ([`MIN_GIT`]).
//! - Replacement objects (`refs/replace/`) are ignored, so a hash always shows the commit it
//!   names: a reviewed mark can't be carried over to different content.
//!
//! The repository's own config still applies where it can't run anything; prefixes are forced
//! back to `a/` and `b/` so the patch parses whatever `diff.noprefix` says.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The most commits listed under one ref.
pub const MAX_COMMITS: usize = 500;
/// The most of one command's stdout that is read. A patch past this is refused, not truncated:
/// half a diff would look like a whole one. (Of stderr, the first megabyte is kept.)
const MAX_OUTPUT: u64 = 64 << 20;
const FIELDS: usize = 7;
/// The oldest git this runs with: the first where `GIT_NO_LAZY_FETCH` stops every fetch of a
/// missing object, including the ones a diff makes ahead of time.
pub const MIN_GIT: (u32, u32) = (2, 46);

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Commit {
    pub sha: String,
    pub short: String,
    /// The parents' hashes, first parent first.
    pub parents: Vec<String>,
    pub author: String,
    pub date: String,
    pub subject: String,
    pub body: String,
}

#[derive(Clone, Debug, Default)]
pub struct RefInfo {
    /// The full name, `refs/jail/fix-login`.
    pub name: String,
    /// The commit the ref named when it was listed. Everything shown for the ref is read from
    /// this hash, so a ref that moves mid-review can't swap what is under a row.
    pub tip: String,
    /// Where the ref left HEAD's history; `None` when they share none.
    pub base: Option<String>,
    /// The commits on the ref that HEAD doesn't have, newest first, at most [`MAX_COMMITS`].
    pub commits: Vec<Commit>,
    /// There are older commits than the ones listed.
    pub truncated: bool,
    /// Why this ref's commits couldn't be read.
    pub error: Option<String>,
}

impl RefInfo {
    /// The name without `refs/`.
    pub fn short(&self) -> &str {
        self.name.strip_prefix("refs/").unwrap_or(&self.name)
    }
}

#[derive(Clone)]
pub struct Repo {
    /// The directory git is run in: the one given, byte for byte, never one read back from
    /// git's output (which would have to survive a trip through UTF-8).
    dir: PathBuf,
    /// The repository's folder name, for the title.
    name: String,
    /// The empty tree in this repository's hash format: diffing against it shows a ref with no
    /// common history whole.
    empty_tree: String,
}

/// A full object name: 40 (SHA-1) or 64 (SHA-256) lowercase hex digits. Anything else is not
/// passed to git as a revision.
pub fn is_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn object_id(s: &str) -> Result<&str, String> {
    if is_object_id(s) { Ok(s) } else { Err(format!("not a commit hash: {s:?}")) }
}

struct Output {
    /// The exit code; `None` when a signal ended git.
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Output {
    fn ok(self, what: &str) -> Result<String, String> {
        match self.code {
            Some(0) => Ok(self.stdout),
            _ => Err(format!("git {what}: {}", self.stderr.trim())),
        }
    }
}

impl Repo {
    /// The repository `dir` is in.
    pub fn open(dir: &Path) -> Result<Repo, String> {
        let version = run(dir, &["version"])?.ok("version")?;
        match parse_version(&version) {
            Some(v) if v >= MIN_GIT => {}
            _ => return Err(format!("refview needs git {}.{} or later; this is {}", MIN_GIT.0, MIN_GIT.1, version.trim())),
        }
        let top = run(dir, &["rev-parse", "--show-toplevel"])?.ok("rev-parse")?;
        let top = Path::new(top.strip_suffix('\n').unwrap_or(&top));
        let name = top.file_name().map_or_else(|| top.display().to_string(), |n| n.to_string_lossy().into_owned());
        // Hashes the empty tree without writing it (no `-w`).
        let empty = run(dir, &["hash-object", "-t", "tree", "/dev/null"])?.ok("hash-object")?;
        let empty_tree = object_id(empty.trim())?.to_owned();
        Ok(Repo { dir: dir.to_path_buf(), name, empty_tree })
    }

    pub fn name(&self) -> String {
        self.name.clone()
    }

    fn git(&self, args: &[&str]) -> Result<Output, String> {
        run(&self.dir, args)
    }

    /// The refs under `patterns` (`refs/jail`, `refs/devvm`), each with the commits HEAD lacks.
    /// A ref whose commits can't be read is still listed, with its error.
    pub fn refs(&self, patterns: &[String]) -> Result<Vec<RefInfo>, String> {
        let mut args = vec!["for-each-ref", "--format=%(refname)%00%(objectname)", "--"];
        args.extend(patterns.iter().map(String::as_str));
        let listed = self.git(&args)?.ok("for-each-ref")?;
        let head = self.head()?;
        let mut out = Vec::new();
        for line in listed.lines() {
            let Some((name, tip)) = line.split_once('\0') else { continue };
            // A pattern that isn't under refs/ can't have matched; a tip that isn't a hash is
            // not something to hand back to git. (`fill` peels a tag to its commit.)
            if !name.starts_with("refs/") || !is_object_id(tip) {
                continue;
            }
            let mut info = RefInfo { name: name.to_owned(), tip: tip.to_owned(), ..RefInfo::default() };
            if let Err(e) = self.fill(&mut info, head.as_deref()) {
                info.error = Some(e);
            }
            out.push(info);
        }
        Ok(out)
    }

    /// HEAD's commit, or `None` in a repository with no commits yet.
    fn head(&self) -> Result<Option<String>, String> {
        let out = self.git(&["rev-parse", "--verify", "--quiet", "--end-of-options", "HEAD^{commit}"])?;
        match out.code {
            Some(0) => Ok(Some(object_id(out.stdout.trim())?.to_owned())),
            // `--quiet` exits 1 and says nothing when HEAD names no commit.
            Some(1) => Ok(None),
            _ => Err(format!("git rev-parse HEAD: {}", out.stderr.trim())),
        }
    }

    fn fill(&self, info: &mut RefInfo, head: Option<&str>) -> Result<(), String> {
        let tip = info.tip.clone();
        // The tip may be a tag object or a tree; only a commit has anything to list.
        let peeled = self.git(&["rev-parse", "--verify", "--quiet", "--end-of-options", &format!("{tip}^{{commit}}")])?;
        let tip = match peeled.code {
            Some(0) => object_id(peeled.stdout.trim())?.to_owned(),
            _ => return Err("this ref doesn't name a commit".to_owned()),
        };
        info.tip = tip.clone();
        let range = match head {
            Some(head) => {
                let base = self.git(&["merge-base", "--end-of-options", head, &tip])?;
                info.base = match base.code {
                    Some(0) => Some(object_id(base.stdout.trim())?.to_owned()),
                    // Exit 1 is git's "no common ancestor".
                    Some(1) => None,
                    _ => return Err(format!("git merge-base: {}", base.stderr.trim())),
                };
                // Not `base..tip`: with several merge bases that lists commits HEAD has.
                format!("{head}..{tip}")
            }
            None => tip,
        };
        let count = format!("--max-count={}", MAX_COMMITS + 1);
        let log = self
            .git(&[
                "log",
                "-z",
                &count,
                "--no-show-signature",
                // Whatever i18n.logOutputEncoding says: the framing below is read as UTF-8.
                "--encoding=UTF-8",
                "--date=format:%Y-%m-%d %H:%M",
                "--format=%H%x00%h%x00%P%x00%an%x00%ad%x00%s%x00%b",
                "--end-of-options",
                &range,
                "--",
            ])?
            .ok("log")?;
        info.commits = parse_log(&log);
        if info.commits.is_empty() && !log.is_empty() {
            // Output that isn't the framing asked for must not read as "nothing to review".
            return Err("git's log output wasn't in the format asked for".to_owned());
        }
        info.truncated = info.commits.len() > MAX_COMMITS;
        info.commits.truncate(MAX_COMMITS);
        Ok(())
    }

    /// The flags every patch is made with; see the module docs for what each one closes.
    const PATCH: &[&str] = &[
        "--no-ext-diff",
        "--no-textconv",
        "--submodule=short",
        "--ignore-submodules=none",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--no-color",
        "--no-relative",
        "--output-indicator-new=+",
        "--output-indicator-old=-",
        "--output-indicator-context= ",
        "-M",
    ];

    /// One commit's patch, against its first parent (a root commit shows as all added).
    pub fn commit_patch(&self, sha: &str) -> Result<String, String> {
        let sha = object_id(sha)?;
        let mut args = vec!["show", "--format=", "--no-show-signature", "--diff-merges=first-parent", "--root"];
        args.extend(Self::PATCH);
        args.extend(["--end-of-options", sha, "--"]);
        self.git(&args)?.ok("show")
    }

    /// Everything the ref changes since it left HEAD's history, as one patch.
    pub fn range_patch(&self, r: &RefInfo) -> Result<String, String> {
        let base = object_id(r.base.as_deref().unwrap_or(&self.empty_tree))?;
        let tip = object_id(&r.tip)?;
        let mut args = vec!["diff"];
        args.extend(Self::PATCH);
        args.extend(["--end-of-options", base, tip, "--"]);
        self.git(&args)?.ok("diff")
    }

    /// The paths one commit changes against its first parent, with each one's mode after the
    /// change; deleted files are left out. For marking a commit in the list without reading
    /// its whole patch.
    pub fn touched(&self, sha: &str) -> Result<Vec<(String, String)>, String> {
        let sha = object_id(sha)?;
        let raw = self
            .git(&["show", "--format=", "--no-show-signature", "--diff-merges=first-parent", "--root", "--raw", "-z", "--no-renames", "--no-abbrev", "--ignore-submodules=none", "--end-of-options", sha, "--"])?
            .ok("show")?;
        Ok(parse_raw(&raw))
    }

    /// [`Repo::touched`] for everything a ref changes since it left HEAD's history.
    pub fn touched_range(&self, r: &RefInfo) -> Result<Vec<(String, String)>, String> {
        let base = object_id(r.base.as_deref().unwrap_or(&self.empty_tree))?;
        let tip = object_id(&r.tip)?;
        let raw = self.git(&["diff", "--raw", "-z", "--no-renames", "--no-abbrev", "--ignore-submodules=none", "--end-of-options", base, tip, "--"])?.ok("diff")?;
        Ok(parse_raw(&raw))
    }

    // Taking: the part that writes.

    /// Every commit of `r` that HEAD lacks, parents before children: the ones a take copies.
    /// Read afresh, not from the list on screen, which stops at [`MAX_COMMITS`].
    pub fn take_list(&self, r: &RefInfo) -> Result<Vec<String>, String> {
        let tip = object_id(&r.tip)?;
        let mut args = vec!["rev-list", "--topo-order", "--reverse", "--end-of-options", tip];
        let not_head;
        if let Some(head) = self.head()? {
            not_head = format!("^{head}");
            args.push(&not_head);
        }
        args.push("--");
        let out = self.git(&args)?.ok("rev-list")?;
        out.lines().map(|l| object_id(l).map(str::to_owned)).collect()
    }

    /// The commit object `sha`, as its parts.
    pub fn raw_commit(&self, sha: &str) -> Result<RawCommit, String> {
        let sha = object_id(sha)?;
        let (code, out, err) = run_bytes(&self.dir, &["cat-file", "commit", sha], &Call::default())?;
        if code != Some(0) {
            return Err(format!("git cat-file: {}", err.trim()));
        }
        // Not lossily: a copy must carry the message it was given, or not be made.
        let text = String::from_utf8(out).map_err(|_| format!("commit {} isn't UTF-8; it can't be copied faithfully", &sha[..9]))?;
        let commit = parse_commit(&text).ok_or_else(|| format!("commit {} isn't in a form this can copy", &sha[..9]))?;
        // The copy is made from these parts, and the review was of what git showed. They must
        // be the same thing: ask git for its own reading of the tree and parents.
        let tree = self.git(&["rev-parse", "--verify", "--quiet", "--end-of-options", &format!("{sha}^{{tree}}")])?.ok("rev-parse")?;
        let line = self.git(&["rev-list", "--parents", "--no-walk", "--end-of-options", sha, "--"])?.ok("rev-list")?;
        let mut ids = line.split_whitespace();
        if tree.trim() != commit.tree || ids.next() != Some(sha) || !ids.eq(commit.parents.iter().map(String::as_str)) {
            return Err(format!("git reads commit {} differently from its text; it isn't copied", &sha[..9]));
        }
        Ok(commit)
    }

    /// A signed copy of commit `sha` on `parents`: the same tree, message and author, with the
    /// committer and signature git's config gives (`user.*`, `user.signingkey`). Returns the
    /// copy's hash. Waits for the signing program, a key's touch included; `cancel` stops it.
    pub fn resign(&self, sha: &str, parents: &[String], cancel: &AtomicBool) -> Result<String, String> {
        let c = self.raw_commit(sha)?;
        let mut args = vec!["commit-tree", "-S"];
        for p in parents {
            args.extend(["-p", object_id(p)?]);
        }
        args.push(object_id(&c.tree)?);
        let call = Call {
            stdin: Some(c.message.as_bytes()),
            env: &[("GIT_AUTHOR_NAME", &c.author_name), ("GIT_AUTHOR_EMAIL", &c.author_email), ("GIT_AUTHOR_DATE", &c.author_date)],
            // A signing wrapper that puts up its own dialog for agent sessions takes this as
            // its cue. Here the window is the prompt.
            unset: &["CLAUDECODE"],
            cancel: Some(cancel),
        };
        let (code, out, err) = run_bytes(&self.dir, &args, &call)?;
        if code != Some(0) {
            return Err(format!("not signed: {}", err.trim()));
        }
        let new = object_id(String::from_utf8_lossy(&out).trim())?.to_owned();
        // git has no "sign or fail" for commit-tree beyond -S itself; look at what it made.
        let made = self.raw_commit(&new)?;
        if !made.signed {
            return Err("git made the copy without a signature".to_owned());
        }
        // The author too: git tidies a name on its way in, and a tidied name is another name.
        if made.tree != c.tree || made.parents != parents || made.message != c.message || made.author != c.author {
            return Err(format!("git's signed copy of {} isn't the same commit (its author or message came out changed); it isn't used", &sha[..9]));
        }
        Ok(new)
    }

    /// A new signed commit of `tree` on `parents` saying `message`, by `author` (name, email,
    /// date) or, without one, by whoever git's config says the reader is. Returns its hash.
    pub fn sign_tree(&self, tree: &str, parents: &[String], message: &str, author: Option<(&str, &str, &str)>, cancel: &AtomicBool) -> Result<String, String> {
        let mut args = vec!["commit-tree", "-S"];
        for p in parents {
            args.extend(["-p", object_id(p)?]);
        }
        args.push(object_id(tree)?);
        let env: Vec<(&str, &str)> = author.map_or(Vec::new(), |(name, email, date)| vec![("GIT_AUTHOR_NAME", name), ("GIT_AUTHOR_EMAIL", email), ("GIT_AUTHOR_DATE", date)]);
        let call = Call { stdin: Some(message.as_bytes()), env: &env, unset: &["CLAUDECODE"], cancel: Some(cancel) };
        let (code, out, err) = run_bytes(&self.dir, &args, &call)?;
        if code != Some(0) {
            return Err(format!("not signed: {}", err.trim()));
        }
        let new = object_id(String::from_utf8_lossy(&out).trim())?.to_owned();
        let made = self.raw_commit(&new)?;
        if !made.signed {
            return Err("git made the commit without a signature".to_owned());
        }
        if made.tree != tree || made.parents != parents || made.message != message {
            return Err("git's signed commit isn't the one asked for; it isn't used".to_owned());
        }
        Ok(new)
    }

    /// The commit a local branch points at, or `None` when there is no such branch.
    pub fn branch_tip(&self, branch: &str) -> Result<Option<String>, String> {
        let name = self.branch_ref(branch)?;
        let out = self.git(&["rev-parse", "--verify", "--quiet", "--end-of-options", &format!("{name}^{{commit}}")])?;
        match out.code {
            Some(0) => Ok(Some(object_id(out.stdout.trim())?.to_owned())),
            Some(1) => Ok(None),
            _ => Err(format!("git rev-parse {name}: {}", out.stderr.trim())),
        }
    }

    /// `refs/heads/BRANCH`, once git agrees it is a branch name.
    fn branch_ref(&self, branch: &str) -> Result<String, String> {
        let name = format!("refs/heads/{branch}");
        // Also refuses a name starting with `-`.
        match self.git(&["check-ref-format", "--branch", branch])?.code {
            Some(0) if !branch.starts_with('-') => Ok(name),
            _ => Err(format!("{branch:?} isn't a branch name git accepts")),
        }
    }

    /// Creates the local branch `branch` at `tip`. Never moves one that exists: git is told
    /// the branch must not be there, and fails if another process made it meanwhile.
    pub fn create_branch(&self, branch: &str, tip: &str, from: &str) -> Result<(), String> {
        let name = self.branch_ref(branch)?;
        let tip = object_id(tip)?;
        // A symbolic ref that points nowhere counts as absent below, and would be written
        // through. Exit 1 is "not a symbolic ref".
        if self.git(&["symbolic-ref", "--quiet", "--end-of-options", &name])?.code != Some(1) {
            return Err(format!("{name} is a symbolic ref; it is left alone"));
        }
        let absent = "0".repeat(tip.len());
        let reason = format!("refview: take {from}");
        self.git(&["update-ref", "--no-deref", "-m", &reason, "--end-of-options", &name, tip, &absent])?.ok("update-ref").map(drop)
    }

    /// Deletes the ref `name` (a full name, `refs/…`) if it still points at `tip`. Its commits
    /// stay in the repository until git prunes them, so [`Repo::restore_ref`] can undo it.
    pub fn delete_ref(&self, name: &str, tip: &str) -> Result<(), String> {
        let tip = object_id(tip)?;
        self.plain_ref(name)?;
        self.git(&["update-ref", "--no-deref", "-d", "-m", "refview: drop", "--end-of-options", name, tip])?.ok("update-ref").map(drop)
    }

    /// Puts the ref `name` back at `tip`, unless something is there by now.
    pub fn restore_ref(&self, name: &str, tip: &str) -> Result<(), String> {
        let tip = object_id(tip)?;
        self.plain_ref(name)?;
        let absent = "0".repeat(tip.len());
        self.git(&["update-ref", "--no-deref", "-m", "refview: undo drop", "--end-of-options", name, tip, &absent])?.ok("update-ref").map(drop)
    }

    /// A full ref name this may delete or put back: one git accepts, that reached us whole,
    /// and that is a ref of its own. A name that wasn't UTF-8 was read with stand-in
    /// characters, and would name another ref; a symbolic ref would come back as a plain one.
    fn plain_ref(&self, name: &str) -> Result<(), String> {
        if !name.starts_with("refs/") || name.contains('\u{fffd}') || self.git(&["check-ref-format", name])?.code != Some(0) {
            return Err(format!("{name:?} isn't a ref name this can act on"));
        }
        // Exit 1 is "not a symbolic ref".
        if self.git(&["symbolic-ref", "--quiet", "--end-of-options", name])?.code != Some(1) {
            return Err(format!("{name} is a symbolic ref; it is left alone"));
        }
        Ok(())
    }

    /// Deletes the local branch `branch` if it still points at `tip` and no working tree has
    /// it checked out.
    pub fn delete_branch(&self, branch: &str, tip: &str) -> Result<(), String> {
        let name = self.branch_ref(branch)?;
        // git fills `worktreepath` for a branch that is some working tree's HEAD.
        let held = self.git(&["for-each-ref", "--format=%(worktreepath)", "--", &name])?.ok("for-each-ref")?;
        if let Some(path) = held.lines().find(|l| !l.is_empty()) {
            return Err(format!("{branch} is checked out in {path}; switch away from it first"));
        }
        self.delete_ref(&name, tip)
    }

    /// The local branch HEAD is on, or `None` when it is detached.
    pub fn head_branch(&self) -> Option<String> {
        let out = self.git(&["symbolic-ref", "--quiet", "--short", "HEAD"]).ok()?;
        (out.code == Some(0)).then(|| out.stdout.trim().to_owned()).filter(|b| !b.is_empty())
    }

    /// `git switch BRANCH`: changes the working tree, which runs the checkout's own hooks.
    /// Only ever asked for by the reader, on a branch they have taken.
    pub fn switch(&self, branch: &str) -> Result<(), String> {
        self.branch_ref(branch)?;
        let out = self.git(&["switch", "--no-guess", "--end-of-options", branch])?;
        match out.code {
            Some(0) => Ok(()),
            _ => Err(out.stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("git switch failed").trim().to_owned()),
        }
    }
}

/// A commit object's parts: what a copy of it is made from.
#[derive(Debug, PartialEq)]
pub struct RawCommit {
    pub tree: String,
    pub parents: Vec<String>,
    /// The author header as it stands: `Name <email> seconds zone`.
    pub author: String,
    pub author_name: String,
    pub author_email: String,
    /// `@SECONDS ZONE`, the form git reads back exactly.
    pub author_date: String,
    /// Everything after the headers, byte for byte.
    pub message: String,
    pub signed: bool,
}

/// Reads `cat-file commit`: headers, a blank line, the message. Only the shape git itself
/// writes is accepted: `tree`, then the parents, then `author`, then `committer`, each once
/// and in that order, then other headers. A commit object can be hand-made with a second
/// `tree` or a `parent` further down; git ignores those and a naive reader wouldn't, so such
/// a commit is `None` here, as is one in another text encoding.
fn parse_commit(text: &str) -> Option<RawCommit> {
    let (head, message) = text.split_once("\n\n").unwrap_or((text.strip_suffix('\n').unwrap_or(text), ""));
    let mut lines = head.split('\n').peekable();
    let tree = lines.next()?.strip_prefix("tree ")?;
    let mut parents = Vec::new();
    while let Some(p) = lines.peek().and_then(|l| l.strip_prefix("parent ")) {
        parents.push(p.to_owned());
        lines.next();
    }
    let author = lines.next()?.strip_prefix("author ")?;
    lines.next()?.strip_prefix("committer ")?;
    let mut signed = false;
    for line in lines {
        // A header's continuation lines (a signature's body) start with a space.
        if line.starts_with(' ') {
            continue;
        }
        match line.split_once(' ')? {
            ("tree" | "parent" | "author" | "committer", _) => return None,
            ("gpgsig" | "gpgsig-sha256", _) => signed = true,
            // The message's bytes are in some other encoding: not copied as if UTF-8.
            ("encoding", value) if !value.eq_ignore_ascii_case("utf-8") => return None,
            _ => {}
        }
    }
    // `Name <email> seconds zone`; the name may be empty or hold spaces.
    let (who, when) = author.rsplit_once("> ")?;
    let (name, email) = who.rsplit_once('<')?;
    let (secs, zone) = when.split_once(' ')?;
    let zone_ok = zone.len() == 5 && zone.starts_with(['+', '-']) && zone[1..].bytes().all(|b| b.is_ascii_digit());
    if secs.is_empty() || !secs.bytes().all(|b| b.is_ascii_digit()) || !zone_ok || !is_object_id(tree) || !parents.iter().all(|p| is_object_id(p)) {
        return None;
    }
    Some(RawCommit {
        tree: tree.to_owned(),
        parents,
        author: author.to_owned(),
        author_name: name.strip_suffix(' ').unwrap_or(name).to_owned(),
        author_email: email.to_owned(),
        author_date: format!("@{secs} {zone}"),
        message: message.to_owned(),
        signed,
    })
}

/// Reads `--raw -z --no-renames`: `:old new oldsha newsha S`, NUL, path, NUL, repeated.
fn parse_raw(raw: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut fields = raw.split('\0');
    while let (Some(meta), Some(path)) = (fields.next(), fields.next()) {
        let mut parts = meta.trim_start_matches(':').split(' ');
        let (Some(_old), Some(mode)) = (parts.next(), parts.next()) else { continue };
        // Mode 000000 after the change: the file is gone.
        if mode.bytes().all(|b| b == b'0') {
            continue;
        }
        out.push((path.to_owned(), mode.to_owned()));
    }
    out
}

/// Reads `log -z --format=%H%x00…%b`: seven NUL-ended fields a commit. No field can hold a
/// NUL, so no commit can forge one; a record whose hash isn't one is dropped all the same.
fn parse_log(log: &str) -> Vec<Commit> {
    let fields: Vec<&str> = log.split('\0').collect();
    fields
        .as_chunks::<FIELDS>()
        .0
        .iter()
        .filter(|f| is_object_id(f[0]))
        .map(|f| Commit {
            sha: f[0].to_owned(),
            short: f[1].to_owned(),
            parents: f[2].split(' ').filter(|p| is_object_id(p)).map(str::to_owned).collect(),
            author: f[3].to_owned(),
            date: f[4].to_owned(),
            subject: f[5].to_owned(),
            body: f[6].trim_end().to_owned(),
        })
        .collect()
}

/// `git version 2.46.1` (or `2.46.1.windows.1`, `2.47.0-rc1`) as `(2, 46)`.
fn parse_version(out: &str) -> Option<(u32, u32)> {
    let mut parts = out.trim().strip_prefix("git version ")?.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor: String = parts.next()?.chars().take_while(char::is_ascii_digit).collect();
    Some((major, minor.parse().ok()?))
}

fn run(dir: &Path, args: &[&str]) -> Result<Output, String> {
    let (code, out, stderr) = run_bytes(dir, args, &Call::default())?;
    Ok(Output { code, stdout: String::from_utf8_lossy(&out).into_owned(), stderr })
}

/// What a git call needs beyond its arguments. Reading needs none of it.
#[derive(Default)]
struct Call<'a> {
    /// Written to git's stdin, which is otherwise empty.
    stdin: Option<&'a [u8]>,
    env: &'a [(&'a str, &'a str)],
    /// Variables git must not inherit.
    unset: &'a [&'a str],
    /// Set from another thread to stop waiting: git is killed and the call fails.
    cancel: Option<&'a AtomicBool>,
}

/// What a cancelled call fails with.
pub const CANCELLED: &str = "cancelled";

fn run_bytes(dir: &Path, args: &[&str], call: &Call) -> Result<(Option<i32>, Vec<u8>, String), String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        // Don't take locks to refresh the index, don't page, don't prompt, don't fetch an
        // object a partial clone lacks (that runs transports and helpers), and show each
        // object as itself, not as what refs/replace/ maps it to.
        .args(["--no-optional-locks", "--no-pager", "--no-replace-objects"])
        // A blank context line keeps its leading space, so every hunk line parses as one.
        .args(["-c", "diff.suppressBlankEmpty=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .envs(call.env.iter().copied())
        .stdin(if call.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for var in call.unset {
        cmd.env_remove(var);
    }
    // A test's git answers to the test's repository alone: never the user's config, and so
    // never the user's signing key.
    #[cfg(test)]
    testing::isolate(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("couldn't run git: {e}"))?;
    let what = args.first().copied().unwrap_or("");
    // stderr drains on its own thread, so a chatty git can't block on a full pipe.
    let mut err_pipe = child.stderr.take().expect("stderr is piped");
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut err_pipe).take(1 << 20).read_to_end(&mut buf);
        // Past the cap, keep draining so git can finish.
        let _ = std::io::copy(&mut err_pipe, &mut std::io::sink());
        buf
    });
    let mut out = Vec::new();
    let mut out_pipe = child.stdout.take().expect("stdout is piped");
    let in_pipe = child.stdin.take();
    let overflow = AtomicBool::new(false);
    // A plain read needs no more than this thread. A call that feeds git or can be cancelled
    // gets a watcher, which stays until git is gone: it is what kills git on a cancel or on
    // too much output, and git going is what lets a blocked writer finish.
    let watched = in_pipe.is_some() || call.cancel.is_some();
    let (read, cancelled) = std::thread::scope(|scope| {
        if let (Some(mut pipe), Some(input)) = (in_pipe, call.stdin) {
            scope.spawn(move || {
                let _ = pipe.write_all(input);
            });
        }
        let watcher = watched.then(|| {
            scope.spawn(|| loop {
                if let Ok(Some(_)) | Err(_) = child.try_wait() {
                    return false;
                }
                let cancel = call.cancel.is_some_and(|c| c.load(Ordering::Relaxed));
                if cancel || overflow.load(Ordering::Relaxed) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return cancel;
                }
                std::thread::sleep(Duration::from_millis(15));
            })
        });
        let read = (&mut out_pipe).take(MAX_OUTPUT + 1).read_to_end(&mut out);
        if read.is_err() || out.len() as u64 > MAX_OUTPUT {
            overflow.store(true, Ordering::Relaxed);
        }
        // Closed here, so a git still writing fails instead of filling the pipe.
        drop(out_pipe);
        (read, watcher.is_some_and(|w| w.join().unwrap_or(false)))
    });
    if cancelled {
        // stderr isn't waited for: something git started (a signing program waiting for a
        // touch) may hold it open until it gives up on its own.
        return Err(CANCELLED.to_owned());
    }
    if read.is_err() || out.len() as u64 > MAX_OUTPUT {
        let _ = child.kill();
        let _ = child.wait();
        let _ = err_reader.join();
        return Err(match read {
            Err(e) => format!("git {what}: {e}"),
            Ok(_) => format!("git {what}: more than {} MB of output; too large to show", MAX_OUTPUT >> 20),
        });
    }
    let status = child.wait().map_err(|e| format!("git {what}: {e}"))?;
    let stderr = err_reader.join().unwrap_or_default();
    Ok((status.code(), out, String::from_utf8_lossy(&stderr).into_owned()))
}

/// Helpers for tests that need a real repository.
#[cfg(test)]
pub mod testing {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// An empty scratch directory for one test.
    pub fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("refview-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Runs git in `dir` to build a test repository, apart from the user's own config.
    pub fn build(dir: &Path, args: &[&str]) -> String {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false"])
            .args(args)
            ;
        isolate(&mut cmd);
        let out = cmd.output().expect("git runs");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Makes a git answer to the test's repository alone: none of the user's config (and so
    /// never the user's signing key), and nothing inherited that points it at another
    /// repository or names who commits. The list is `git rev-parse --local-env-vars`.
    pub fn isolate(cmd: &mut Command) {
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1");
        for var in [
            "GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_COMMON_DIR", "GIT_NAMESPACE", "GIT_PREFIX", "GIT_CONFIG", "GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT",
            "GIT_GRAFT_FILE", "GIT_IMPLICIT_WORK_TREE", "GIT_SHALLOW_FILE", "GIT_REPLACE_REF_BASE", "GIT_NO_REPLACE_OBJECTS",
            "GIT_INTERNAL_SUPER_PREFIX", "GIT_COMMITTER_NAME", "GIT_COMMITTER_EMAIL", "GIT_COMMITTER_DATE",
        ] {
            cmd.env_remove(var);
        }
    }

    /// Gives the repository at `work` an identity and a key to sign with.
    pub fn signing(dir: &Path, work: &Path) {
        let key = dir.join("key");
        let made = std::process::Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-C", "test", "-f"]).arg(&key).status().expect("ssh-keygen runs");
        assert!(made.success());
        for (k, v) in [("user.name", "Reader"), ("user.email", "reader@example.invalid"), ("gpg.format", "ssh"), ("user.signingkey", key.to_str().unwrap())] {
            build(work, &["config", k, v]);
        }
        // For `git verify-commit`: the one key this repository's signatures may come from.
        let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
        std::fs::write(dir.join("signers"), format!("reader@example.invalid {public}")).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::testing::build;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn a_log_splits_into_commits_with_their_bodies() {
        let log = format!("{A}\0a\0{B} {A}\0Ann\x002026-11-09 10:00\0Fix login\0Two lines\nof body\n\0{B}\0b\0\0Bo\x002026-11-08 09:00\0Add test\0\0");
        assert_eq!(
            parse_log(&log),
            [
                Commit {
                    sha: A.into(),
                    short: "a".into(),
                    parents: vec![B.into(), A.into()],
                    author: "Ann".into(),
                    date: "2026-11-09 10:00".into(),
                    subject: "Fix login".into(),
                    body: "Two lines\nof body".into(),
                },
                Commit {
                    sha: B.into(),
                    short: "b".into(),
                    parents: vec![],
                    author: "Bo".into(),
                    date: "2026-11-08 09:00".into(),
                    subject: "Add test".into(),
                    body: String::new(),
                },
            ]
        );
    }

    #[test]
    fn an_empty_or_cut_short_log_has_no_commits() {
        assert!(parse_log("").is_empty());
        assert!(parse_log(&format!("{A}\0a\0Ann")).is_empty());
    }

    #[test]
    fn a_record_whose_hash_is_an_option_is_dropped() {
        let log = format!("--output=/tmp/x\0s\0\0a\0d\0Looks fine\0\0{B}\0b\0\0Bo\0d\0Real\0\0");
        let commits = parse_log(&log);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].sha, B);
    }

    /// A repository whose config and reviewed ref both try to make a review run a program or
    /// write a file. Reading it must do neither, and must still show the real changes.
    #[test]
    fn a_hostile_ref_and_config_run_nothing_and_write_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("refview-hostile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo_dir = dir.join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        let ran = dir.join("ran");
        let written = dir.join("written");
        let tool = dir.join("tool.sh");
        std::fs::write(&tool, format!("#!/bin/sh\necho ran >> '{}'\n", ran.display())).unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tool = tool.to_str().unwrap();

        build(&repo_dir, &["init", "-q", "-b", "main", "."]);
        std::fs::write(repo_dir.join("a.txt"), "one\n").unwrap();
        // Attributes in the working tree are the ones git reads for a diff.
        std::fs::write(repo_dir.join(".gitattributes"), "* diff=evil\n").unwrap();
        build(&repo_dir, &["add", "-A"]);
        build(&repo_dir, &["commit", "-q", "-m", "base"]);
        std::fs::write(repo_dir.join("a.txt"), "one\ntwo\n").unwrap();
        build(&repo_dir, &["add", "-A"]);
        // The separators an earlier log format used, forging a record whose hash is an option.
        let message = format!("Looks fine\n\nbody\u{1e}--output={}\u{1f}s\u{1f}a\u{1f}d\u{1f}Forged\u{1f}", written.display());
        // Signed, by a stand-in signer, so that verifying it would run `gpg.program`.
        let signer = dir.join("sign.sh");
        std::fs::write(&signer, "#!/bin/sh\necho '[GNUPG:] SIG_CREATED ' >&2\necho '-----BEGIN PGP SIGNATURE-----'\necho 'bm90IGEgc2lnbmF0dXJl'\necho '-----END PGP SIGNATURE-----'\n").unwrap();
        std::fs::set_permissions(&signer, std::fs::Permissions::from_mode(0o755)).unwrap();
        let signer = format!("gpg.program={}", signer.display());
        build(&repo_dir, &["-c", &signer, "commit", "-q", "-S", "-m", &message]);
        assert!(build(&repo_dir, &["cat-file", "commit", "HEAD"]).contains("gpgsig"), "the commit carries a signature");
        build(&repo_dir, &["update-ref", "refs/jail/evil", "HEAD"]);
        build(&repo_dir, &["reset", "-q", "--hard", "HEAD~1"]);
        for (key, value) in [
            ("diff.external", tool),
            ("diff.evil.textconv", tool),
            ("diff.evil.command", tool),
            ("diff.noprefix", "true"),
            ("diff.relative", "true"),
            ("diff.outputIndicatorNew", "X"),
            ("diff.outputIndicatorOld", "Y"),
            ("diff.suppressBlankEmpty", "true"),
            ("diff.submodule", "diff"),
            ("log.showRoot", "false"),
            ("log.showSignature", "true"),
            ("gpg.program", tool),
            ("i18n.logOutputEncoding", "UTF-16LE"),
        ] {
            build(&repo_dir, &["config", key, value]);
        }

        let repo = Repo::open(&repo_dir).unwrap();
        let refs = repo.refs(&["refs/jail".to_owned()]).unwrap();
        assert_eq!(refs.len(), 1);
        let r = &refs[0];
        assert_eq!(r.error, None);
        assert_eq!(r.commits.len(), 1, "the forged record is no commit: {:?}", r.commits);
        assert!(is_object_id(&r.commits[0].sha));
        assert_eq!(r.commits[0].subject, "Looks fine");
        assert!(r.commits[0].body.contains("--output="), "the message is kept as text");

        for patch in [repo.commit_patch(&r.commits[0].sha).unwrap(), repo.range_patch(r).unwrap()] {
            assert!(patch.contains("diff --git a/a.txt b/a.txt"), "standard prefixes despite diff.noprefix:\n{patch}");
            assert!(patch.contains("+two"), "the real change is shown:\n{patch}");
        }
        // The root commit, which `log.showRoot=false` would show empty.
        let root = build(&repo_dir, &["rev-parse", "HEAD"]);
        assert!(repo.commit_patch(&root).unwrap().contains("+one"));

        // An option where a hash belongs is refused before git sees it.
        let option = format!("--output={}", written.display());
        assert!(repo.commit_patch(&option).is_err());
        let forged = RefInfo { tip: option.clone(), ..r.clone() };
        assert!(repo.range_patch(&forged).is_err());
        let forged = RefInfo { base: Some(option), ..r.clone() };
        assert!(repo.range_patch(&forged).is_err());

        assert!(!ran.exists(), "a configured program ran: {:?}", std::fs::read_to_string(&ran));
        assert!(!written.exists(), "a review wrote a file");
        // The control: the same repository does run its verifier when asked the plain way.
        build(&repo_dir, &["log", "-1", "refs/jail/evil"]);
        assert!(ran.exists(), "log.showSignature should have run gpg.program without --no-show-signature");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The control for the test above: the same config does run its program when the flags
    /// are left off, so that test can fail.
    #[test]
    fn the_hostile_config_is_live_without_the_flags() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("refview-control-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo_dir = dir.join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        let ran = dir.join("ran");
        let tool = dir.join("tool.sh");
        std::fs::write(&tool, format!("#!/bin/sh\necho ran >> '{}'\n", ran.display())).unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        build(&repo_dir, &["init", "-q", "-b", "main", "."]);
        std::fs::write(repo_dir.join("a.txt"), "one\n").unwrap();
        build(&repo_dir, &["add", "-A"]);
        build(&repo_dir, &["commit", "-q", "-m", "base"]);
        std::fs::write(repo_dir.join("a.txt"), "one\ntwo\n").unwrap();
        build(&repo_dir, &["add", "-A"]);
        build(&repo_dir, &["commit", "-q", "-m", "next"]);
        build(&repo_dir, &["config", "diff.external", tool.to_str().unwrap()]);
        build(&repo_dir, &["diff", "HEAD~1", "HEAD"]);
        assert!(ran.exists(), "git diff without --no-ext-diff should have run the configured program");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn git_versions_parse_to_major_and_minor() {
        assert_eq!(parse_version("git version 2.54.0\n"), Some((2, 54)));
        assert_eq!(parse_version("git version 2.46.1.windows.1"), Some((2, 46)));
        assert_eq!(parse_version("git version 2.47-rc1"), Some((2, 47)));
        assert_eq!(parse_version("git version 3.0.0"), Some((3, 0)));
        assert_eq!(parse_version("hello"), None);
        assert!(parse_version("git version 2.45.2") < Some(MIN_GIT));
    }

    #[test]
    fn only_full_lowercase_hex_is_an_object_id() {
        assert!(is_object_id(A));
        assert!(is_object_id(&A.repeat(2)[..64]));
        assert!(!is_object_id("--output=/tmp/x"));
        assert!(!is_object_id(&A[..39]));
        assert!(!is_object_id(&A.to_uppercase()));
        assert!(!is_object_id("HEAD"));
    }
}
