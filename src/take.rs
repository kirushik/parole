//! Taking a ref: making it a local branch of signed commits, once every commit on it has been
//! read.
//!
//! A sandbox commits unsigned, and as itself. A take copies each commit the ref has and HEAD
//! lacks (same tree, same message, same author) with the reader as committer and the reader's
//! signature, parents before children, and points a new branch at the last copy. The copies
//! are made from the commits' trees with `git commit-tree`: nothing is checked out, so nothing
//! the ref holds runs here.
//!
//! The gate is inside [`run`], on the list read at that moment, not on what the window showed:
//! a commit that isn't marked read stops the take before anything is signed.
//!
//! A ref read as one change (its whole diff, not commit by commit) is taken as one commit
//! instead ([`Way::One`]): the ref's final files on the commit where it left HEAD, signed. The
//! commits in between were not read, so they are not copied: nothing of them reaches the
//! branch, and no later `git bisect` can land on one. You get what you read.
//!
//! Each signature may need a touch of a hardware key. Copies already made are remembered
//! (`$XDG_STATE_HOME/parole/taken`, `OLD NEW` a line), so a take that stops halfway picks up
//! where it stopped instead of asking for every touch again.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use crate::git::{RefInfo, Repo};

/// What a take reports as it goes.
#[derive(Debug, PartialEq)]
pub enum Step {
    /// Copy `i` of `n` (from 0) is waiting for its signature.
    Signing { i: usize, n: usize, sha: String },
    /// The copy of `sha` exists: signed now, or kept from an earlier take (`reused`).
    Signed { sha: String, reused: bool },
    /// The branch exists, at the copy of the ref's tip.
    Done { branch: String, tip: String, count: usize },
    Failed(String),
}

/// How a ref is taken: what was read decides.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Way {
    /// Every commit was read: each is copied, and the history kept.
    Each,
    /// The whole change was read as one diff: one commit holds it.
    One,
}

/// What a ref's whole change is marked read under, and its one-commit copy remembered under:
/// where it left HEAD and the commit it named. The diff between those two is what was read.
pub fn whole_mark(r: &RefInfo) -> String {
    format!("whole:{}:{}", r.base.as_deref().unwrap_or("root"), r.tip)
}

/// What a ref's one-commit copy is remembered under: the change, and the ref it was taken
/// from. Two refs at the same commit are two takes, each with its own branch and message.
pub fn one_key(r: &RefInfo) -> String {
    format!("{}:{}", whole_mark(r), r.name)
}

fn is_one_key(s: &str) -> bool {
    let mut parts = s.splitn(4, ':');
    parts.next() == Some("whole")
        && parts.next().is_some_and(|b| b == "root" || crate::git::is_object_id(b))
        && parts.next().is_some_and(crate::git::is_object_id)
        && parts.next().is_some_and(|name| name.starts_with("refs/") && !name.contains(char::is_whitespace))
}

/// The line a one-commit take's message ends with: which ref it was, and at which commit.
fn one_source(r: &RefInfo) -> String {
    format!("Read as one change with parole, from {} at {}.\n", r.name, r.tip)
}

/// The copies made so far, by the hash of the commit each one copies, or by the
/// [`whole_mark`] of a change taken as one.
#[derive(Default)]
pub struct Taken {
    map: BTreeMap<String, String>,
    path: Option<PathBuf>,
}

impl Taken {
    pub fn load() -> Taken {
        Taken::at(crate::reviewed::state_file("taken"))
    }

    pub fn at(path: Option<PathBuf>) -> Taken {
        let text = path.as_deref().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        let map = text
            .lines()
            .filter_map(|l| l.split_once(' '))
            .filter(|(old, new)| (crate::git::is_object_id(old) || is_one_key(old)) && crate::git::is_object_id(new))
            .map(|(old, new)| (old.to_owned(), new.to_owned()))
            .collect();
        Taken { map, path }
    }

    pub fn get(&self, old: &str) -> Option<&str> {
        self.map.get(old).map(String::as_str)
    }

    /// Remembers a copy, in this run and (when there is a state directory) the next. A line
    /// is appended whole; a later line for the same commit wins.
    fn record(&mut self, old: &str, new: &str) {
        self.map.insert(old.to_owned(), new.to_owned());
        let Some(path) = &self.path else { return };
        // Losing this only costs touches on a retry, so a failure isn't the take's.
        let _ = path.parent().map(std::fs::create_dir_all);
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(path) {
            let _ = f.write_all(format!("{old} {new}\n").as_bytes());
        }
    }
}

/// The local branch a ref is taken as: `refs/jail/fix-login` becomes `reviewed/jail/fix-login`.
/// Not `jail/fix-login`: git reads that name as `refs/jail/fix-login` first, so `git merge
/// jail/fix-login` would merge the unsigned original.
pub fn branch_for(r: &RefInfo) -> String {
    format!("reviewed/{}", r.short())
}

/// Whether a ref has been taken, and how: `copy`, a remembered copy, is a signed commit other
/// than the tip with the tip's files, and the branch points at it. For [`Way::Each`] it is the
/// tip's own copy (its message and author); for [`Way::One`] it sits on where the ref left
/// HEAD. The list of copies is only a hint; what is claimed is checked in the repository.
pub fn taken_as(repo: &Repo, copy: &str, way: Way, r: &RefInfo) -> Option<String> {
    let branch = branch_for(r);
    if copy == r.tip || repo.branch_tip(&branch).ok()?.as_deref() != Some(copy) {
        return None;
    }
    let (made, original) = (repo.raw_commit(copy).ok()?, repo.raw_commit(&r.tip).ok()?);
    let same = match way {
        Way::Each => made.message == original.message && made.author == original.author,
        // Its message is this ref's: another commit with the same files isn't this take.
        Way::One => made.parents.as_slice() == r.base.as_slice() && made.message.starts_with(&format!("Take {}: ", r.short())) && made.message.ends_with(&one_source(r)),
    };
    (made.signed && made.tree == original.tree && same).then_some(branch)
}

/// Takes `r` as `branch`, reporting each step. `reviewed` is everything marked read. `taken`
/// is locked only to look a copy up or note one, never across a signature.
#[allow(clippy::too_many_arguments)]
pub fn run(repo: &Repo, r: &RefInfo, branch: &str, way: Way, reviewed: &BTreeSet<String>, taken: &Mutex<Taken>, cancel: &AtomicBool, report: &mut dyn FnMut(Step)) {
    let done = match way {
        Way::Each => copy(repo, r, branch, reviewed, taken, cancel, report),
        Way::One => squash(repo, r, branch, reviewed, taken, cancel, report),
    };
    if let Err(e) = done {
        report(Step::Failed(e));
    }
}

/// What the one commit says: the ref, the subjects of the commits it stands for, and the
/// commit it was made from. The subjects are the sandbox's words, kept as a list.
fn one_message(r: &RefInfo, subjects: &[String]) -> String {
    let mut m = format!("Take {}: {} as one\n\n", r.short(), if subjects.len() == 1 { "1 commit".to_owned() } else { format!("{} commits", subjects.len()) });
    for s in subjects {
        m.push_str(&format!("- {s}\n"));
    }
    m.push('\n');
    m.push_str(&one_source(r));
    m
}

/// One signed commit with the ref's files, on the commit where the ref left HEAD's history.
fn squash(repo: &Repo, r: &RefInfo, branch: &str, reviewed: &BTreeSet<String>, taken: &Mutex<Taken>, cancel: &AtomicBool, report: &mut dyn FnMut(Step)) -> Result<(), String> {
    if let Some(e) = &r.error {
        return Err(format!("this ref couldn't be read: {e}"));
    }
    let (mark, key) = (whole_mark(r), one_key(r));
    if !reviewed.contains(&mark) {
        return Err("this ref's whole change isn't marked read".to_owned());
    }
    let list = repo.take_list(r)?;
    if list.is_empty() {
        return Err("HEAD already has everything on this ref".to_owned());
    }
    if repo.branch_tip(branch)?.is_some() {
        return Err(format!("there is already a branch {branch}; rename or delete it first"));
    }
    let originals: Vec<_> = list.iter().map(|sha| repo.raw_commit(sha)).collect::<Result<_, _>>()?;
    let tip = repo.raw_commit(&r.tip)?;
    let subjects: Vec<String> = originals.iter().map(|c| c.message.lines().next().unwrap_or("").to_owned()).collect();
    let message = one_message(r, &subjects);
    // Written by one hand, it keeps that author; by several, or by one git wouldn't write
    // down as given (no name, no email), it is the reader's commit.
    let one_author = originals.iter().all(|c| c.author_name == tip.author_name && c.author_email == tip.author_email) && !tip.author_name.trim().is_empty() && !tip.author_email.trim().is_empty();
    let author = one_author.then_some((tip.author_name.as_str(), tip.author_email.as_str(), tip.author_date.as_str()));
    let by = |c: &crate::git::RawCommit| author.is_none_or(|(name, email, _)| c.author_name == name && c.author_email == email);
    let parents: Vec<String> = r.base.iter().cloned().collect();
    let before = taken.lock().unwrap_or_else(|e| e.into_inner()).get(&key).map(str::to_owned);
    let kept = before.filter(|new| repo.raw_commit(new).is_ok_and(|c| c.signed && c.tree == tip.tree && c.parents == parents && c.message == message && by(&c)));
    let new = match kept {
        Some(new) => {
            report(Step::Signed { sha: r.tip.clone(), reused: true });
            new
        }
        None => {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(crate::git::CANCELLED.to_owned());
            }
            report(Step::Signing { i: 0, n: 1, sha: r.tip.clone() });
            let new = repo.sign_tree(&tip.tree, &parents, &message, author, cancel)?;
            if !repo.raw_commit(&new).is_ok_and(|c| by(&c)) {
                return Err("git wrote the commit's author down changed; it isn't used".to_owned());
            }
            taken.lock().unwrap_or_else(|e| e.into_inner()).record(&key, &new);
            report(Step::Signed { sha: r.tip.clone(), reused: false });
            new
        }
    };
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(crate::git::CANCELLED.to_owned());
    }
    repo.create_branch(branch, &new, &r.name)?;
    report(Step::Done { branch: branch.to_owned(), tip: new, count: 1 });
    Ok(())
}

fn copy(repo: &Repo, r: &RefInfo, branch: &str, reviewed: &BTreeSet<String>, taken: &Mutex<Taken>, cancel: &AtomicBool, report: &mut dyn FnMut(Step)) -> Result<(), String> {
    if let Some(e) = &r.error {
        return Err(format!("this ref couldn't be read: {e}"));
    }
    let list = repo.take_list(r)?;
    if list.is_empty() {
        return Err("HEAD already has everything on this ref".to_owned());
    }
    let unread = list.iter().filter(|sha| !reviewed.contains(*sha)).count();
    if unread > 0 {
        return Err(format!("{unread} of this ref's {} commits aren't marked reviewed", list.len()));
    }
    if repo.branch_tip(branch)?.is_some() {
        return Err(format!("there is already a branch {branch}; rename or delete it first"));
    }
    // This take's copies, by original. A parent outside the list is HEAD's: kept as it is.
    let mut copies: HashMap<String, String> = HashMap::new();
    let n = list.len();
    for (i, sha) in list.iter().enumerate() {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(crate::git::CANCELLED.to_owned());
        }
        let original = repo.raw_commit(sha)?;
        let parents: Vec<String> = original.parents.iter().map(|p| copies.get(p).unwrap_or(p).clone()).collect();
        // A copy from an earlier take stands only if it is what this take would make: another
        // commit than the original, signed, with the same tree, parents, message and author.
        // (Whose signature it carries isn't checked: the list is the reader's own file.)
        let before = taken.lock().unwrap_or_else(|e| e.into_inner()).get(sha).map(str::to_owned);
        let kept = before.filter(|new| {
            new != sha
                && repo.raw_commit(new).is_ok_and(|c| {
                    c.signed && c.tree == original.tree && c.parents == parents && c.message == original.message && c.author == original.author
                })
        });
        let new = match kept {
            Some(new) => {
                report(Step::Signed { sha: sha.clone(), reused: true });
                new
            }
            None => {
                report(Step::Signing { i, n, sha: sha.clone() });
                let new = repo.resign(sha, &parents, cancel)?;
                taken.lock().unwrap_or_else(|e| e.into_inner()).record(sha, &new);
                report(Step::Signed { sha: sha.clone(), reused: false });
                new
            }
        };
        copies.insert(sha.clone(), new);
    }
    let tip = copies.get(&r.tip).ok_or("the ref's tip isn't among its own commits")?.clone();
    if repo.raw_commit(&tip)?.tree != repo.raw_commit(&r.tip)?.tree {
        return Err("the copy's files differ from the ref's; no branch was made".to_owned());
    }
    // The last moment to stop: from here the branch exists.
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(crate::git::CANCELLED.to_owned());
    }
    repo.create_branch(branch, &tip, &r.name)?;
    report(Step::Done { branch: branch.to_owned(), tip, count: n });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testing::{build, scratch, signing};
    use std::path::Path;

    /// main with one commit; refs/jail/work two commits ahead, by "agent", unsigned. The
    /// repository signs with a software SSH key made for the test.
    fn repo(dir: &Path) -> (Repo, RefInfo) {
        let work = dir.join("repo");
        std::fs::create_dir_all(&work).unwrap();
        build(&work, &["init", "-q", "-b", "main", "."]);
        let commit = |file: &str, text: &str, subject: &str| {
            std::fs::write(work.join(file), text).unwrap();
            build(&work, &["add", "-A"]);
            build(&work, &["-c", "user.name=agent", "-c", "user.email=agent@jail.invalid", "commit", "-q", "-m", subject]);
        };
        commit("a.txt", "one\n", "base");
        build(&work, &["checkout", "-q", "-b", "work"]);
        commit("a.txt", "one\ntwo\n", "first\n\nWith a body.\n");
        commit("b.txt", "new\n", "second");
        build(&work, &["update-ref", "refs/jail/work", "HEAD"]);
        build(&work, &["checkout", "-q", "main"]);
        signing(dir, &work);
        let repo = Repo::open(&work).unwrap();
        let r = repo.refs(&["refs/jail".to_owned()]).unwrap().remove(0);
        (repo, r)
    }

    fn take(repo: &Repo, r: &RefInfo, reviewed: &[&str], taken: &Mutex<Taken>) -> Vec<Step> {
        let mut steps = Vec::new();
        let reviewed = reviewed.iter().map(|&s| s.to_owned()).collect();
        run(repo, r, &branch_for(r), Way::Each, &reviewed, taken, &AtomicBool::new(false), &mut |s| steps.push(s));
        steps
    }

    #[test]
    fn a_ref_read_as_one_change_becomes_one_signed_commit() {
        let dir = scratch("take-one");
        let (repo, r) = repo(&dir);
        let work = dir.join("repo");
        let taken = Mutex::new(Taken::at(Some(dir.join("taken"))));
        let one = |reviewed: &[String]| {
            let mut steps = Vec::new();
            let reviewed = reviewed.iter().cloned().collect();
            run(&repo, &r, &branch_for(&r), Way::One, &reviewed, &taken, &AtomicBool::new(false), &mut |s| steps.push(s));
            steps
        };
        // Its commits read, but not its whole change: that is the other way to take it.
        let commits: Vec<String> = r.commits.iter().map(|c| c.sha.clone()).collect();
        assert!(matches!(one(&commits).last(), Some(Step::Failed(e)) if e.contains("whole change isn't marked read")));
        assert_eq!(build(&work, &["for-each-ref", "refs/heads/reviewed"]), "");
        let steps = one(&[whole_mark(&r)]);
        let Some(Step::Done { tip, count: 1, .. }) = steps.last() else { panic!("not done: {steps:?}") };
        let made = repo.raw_commit(tip).unwrap();
        // The ref's files, on main, signed, and nothing of the commits in between.
        assert_eq!(made.tree, repo.raw_commit(&r.tip).unwrap().tree);
        assert_eq!(made.parents, [build(&work, &["rev-parse", "main"])]);
        assert!(made.signed);
        assert_eq!(build(&work, &["rev-list", "--count", "main..refs/heads/reviewed/jail/work"]), "1");
        for sha in &commits {
            let within = std::process::Command::new("git").arg("-C").arg(&work).args(["merge-base", "--is-ancestor", sha, "refs/heads/reviewed/jail/work"]).status().unwrap();
            assert!(!within.success(), "{sha} is in the taken branch's history");
        }
        // One hand wrote it all, so it keeps that author; the message lists what it stands for.
        assert_eq!(made.author_name, "agent");
        assert!(made.message.starts_with("Take jail/work: 2 commits as one\n\n- first\n- second\n"), "{}", made.message);
        assert!(made.message.contains(&r.tip));
        assert_eq!(taken_as(&repo, tip, Way::One, &r).as_deref(), Some("reviewed/jail/work"));
        assert_eq!(taken_as(&repo, tip, Way::Each, &r), None);
        // Undone and taken again, the same commit is used: no second signature.
        build(&work, &["update-ref", "-d", "refs/heads/reviewed/jail/work"]);
        let again = one(&[whole_mark(&r)]);
        assert!(again.contains(&Step::Signed { sha: r.tip.clone(), reused: true }), "{again:?}");
        assert!(matches!(again.last(), Some(Step::Done { tip: t, .. }) if t == tip));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_read_ref_becomes_a_branch_of_signed_copies() {
        let dir = scratch("take");
        let (repo, r) = repo(&dir);
        let work = dir.join("repo");
        let shas: Vec<&str> = r.commits.iter().map(|c| c.sha.as_str()).collect();
        let taken = Mutex::new(Taken::at(Some(dir.join("state/taken"))));
        let steps = take(&repo, &r, &shas, &taken);
        let Some(Step::Done { branch, tip, count }) = steps.last() else { panic!("not done: {steps:?}") };
        assert_eq!((branch.as_str(), *count), ("reviewed/jail/work", 2));
        assert_eq!(build(&work, &["rev-parse", "refs/heads/reviewed/jail/work"]), *tip);
        // The same files, on the same base, with the agent as author and the reader as committer.
        assert_eq!(build(&work, &["rev-parse", "refs/heads/reviewed/jail/work^{tree}"]), build(&work, &["rev-parse", "refs/jail/work^{tree}"]));
        assert_eq!(build(&work, &["rev-parse", "refs/heads/reviewed/jail/work~2"]), build(&work, &["rev-parse", "main"]));
        for (copy, original) in [("refs/heads/reviewed/jail/work", "refs/jail/work"), ("refs/heads/reviewed/jail/work~1", "refs/jail/work~1")] {
            let show = |rev: &str, format: &str| build(&work, &["log", "-1", &format!("--format={format}"), rev]);
            assert_ne!(show(copy, "%H"), show(original, "%H"));
            // The raw objects, so not a byte of the message or the author line can differ unseen.
            let (made, was) = (repo.raw_commit(&show(copy, "%H")).unwrap(), repo.raw_commit(&show(original, "%H")).unwrap());
            assert_eq!((&made.tree, &made.message, &made.author), (&was.tree, &was.message, &was.author), "{copy}");
            assert!(made.signed && !was.signed);
            assert_eq!(show(copy, "%cn <%ce>"), "Reader <reader@example.invalid>");
            // And the signature is one the test's key made, not just a header that says so.
            let signers = format!("gpg.ssh.allowedSignersFile={}", dir.join("signers").display());
            build(&work, &["-c", &signers, "verify-commit", copy]);
        }
        // Nothing else moved: HEAD, the index and the working tree are as they were.
        assert_eq!(build(&work, &["status", "--porcelain"]), "");
        assert_eq!(build(&work, &["symbolic-ref", "HEAD"]), "refs/heads/main");
        let copy = taken.lock().unwrap().get(&r.tip).unwrap().to_owned();
        assert_eq!(taken_as(&repo, &copy, Way::Each, &r).as_deref(), Some("reviewed/jail/work"));
        // The claim rests on the repository, not on the list: the tip itself, or some other
        // commit, is not a copy.
        assert_eq!(taken_as(&repo, &r.tip, Way::Each, &r), None);
        assert_eq!(taken_as(&repo, &build(&work, &["rev-parse", "main"]), Way::Each, &r), None);
        // A second take doesn't move the branch.
        let again = take(&repo, &r, &shas, &taken);
        assert!(matches!(again.last(), Some(Step::Failed(e)) if e.contains("already a branch")), "{again:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_unread_commit_stops_the_take_before_any_signature() {
        let dir = scratch("take-gate");
        let (repo, r) = repo(&dir);
        let steps = take(&repo, &r, &[&r.commits[0].sha], &Mutex::new(Taken::at(None)));
        assert!(matches!(steps.as_slice(), [Step::Failed(e)] if e.contains("1 of this ref's 2 commits")), "{steps:?}");
        assert_eq!(repo.branch_tip("reviewed/jail/work").unwrap(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_take_that_stopped_keeps_the_copies_it_made() {
        let dir = scratch("take-resume");
        let (repo, r) = repo(&dir);
        let work = dir.join("repo");
        let shas: Vec<&str> = r.commits.iter().map(|c| c.sha.as_str()).collect();
        let state = dir.join("state/taken");
        // With no key to sign with, the take fails and leaves no branch.
        let key = build(&work, &["config", "user.signingkey"]);
        build(&work, &["config", "user.signingkey", "/nonexistent/key"]);
        let taken = Mutex::new(Taken::at(Some(state.clone())));
        let steps = take(&repo, &r, &shas, &taken);
        assert!(matches!(steps.last(), Some(Step::Failed(e)) if e.contains("not signed")), "{steps:?}");
        assert_eq!(repo.branch_tip("reviewed/jail/work").unwrap(), None, "no branch from a take that failed");
        // With the key back, the take is cancelled once the first copy is signed.
        build(&work, &["config", "user.signingkey", &key]);
        let cancel = AtomicBool::new(false);
        let reviewed: BTreeSet<String> = shas.iter().map(|&s| s.to_owned()).collect();
        let mut signed = 0;
        run(&repo, &r, "reviewed/jail/work", Way::Each, &reviewed, &taken, &cancel, &mut |s| {
            if matches!(s, Step::Signed { .. }) {
                signed += 1;
                // Stop after the first copy.
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        });
        assert_eq!(signed, 1);
        assert_eq!(repo.branch_tip("reviewed/jail/work").unwrap(), None);
        // A new run, reading the file: the first copy is kept, only the second is signed.
        let steps = take(&repo, &r, &shas, &Mutex::new(Taken::at(Some(state))));
        // `shas` is newest first, as the refs are listed; the take goes oldest first.
        assert_eq!(steps[0], Step::Signed { sha: shas[1].to_owned(), reused: true });
        assert!(matches!(steps[1], Step::Signing { i: 1, n: 2, .. }));
        assert!(matches!(steps.last(), Some(Step::Done { count: 2, .. })), "{steps:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Writes a commit object by hand, as a sandbox can, and points refs/jail/odd at it.
    fn handmade(work: &Path, text: &str) -> String {
        use std::io::Write;
        let mut cmd = std::process::Command::new("git");
        cmd.arg("-C").arg(work).args(["hash-object", "-t", "commit", "-w", "--stdin", "--literally"]);
        crate::git::testing::isolate(&mut cmd);
        let mut child = cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        let sha = String::from_utf8(out.stdout).unwrap().trim().to_owned();
        build(work, &["update-ref", "refs/jail/odd", &sha]);
        sha
    }

    /// A commit object can say more than git shows: a `parent` or a second `tree` below the
    /// place git reads them from. What was reviewed is what git showed, so such a commit is
    /// never copied from its text.
    #[test]
    fn a_commit_that_reads_two_ways_is_not_taken() {
        let dir = scratch("take-odd");
        let (repo, r) = repo(&dir);
        let work = dir.join("repo");
        let main = build(&work, &["rev-parse", "main"]);
        let tree = build(&work, &["rev-parse", "refs/jail/work^{tree}"]);
        let other = build(&work, &["rev-parse", "refs/jail/work~1^{tree}"]);
        let who = "agent <agent@jail.invalid> 1700000000 +0000";
        for (what, text) in [
            ("a parent below the committer", format!("tree {tree}\nparent {main}\nauthor {who}\ncommitter {who}\nparent {}\n\nodd\n", r.tip)),
            ("a second tree", format!("tree {tree}\nparent {main}\nauthor {who}\ncommitter {who}\ntree {other}\n\nodd\n")),
            ("a second author", format!("tree {tree}\nparent {main}\nauthor {who}\ncommitter {who}\nauthor {who}\n\nodd\n")),
        ] {
            let sha = handmade(&work, &text);
            let odd = repo.refs(&["refs/jail/odd".to_owned()]).unwrap().remove(0);
            assert_eq!(odd.commits.len(), 1, "{what}: git lists one commit");
            let steps = take(&repo, &odd, &[&sha], &Mutex::new(Taken::at(None)));
            assert!(matches!(steps.as_slice(), [Step::Failed(e)] if e.contains("form this can copy")), "{what}: {steps:?}");
            assert_eq!(repo.branch_tip("reviewed/jail/odd").unwrap(), None, "{what}");
        }
        // The control: the same commit in git's own shape is taken.
        let sha = handmade(&work, &format!("tree {tree}\nparent {main}\nauthor {who}\ncommitter {who}\n\nplain\n"));
        let odd = repo.refs(&["refs/jail/odd".to_owned()]).unwrap().remove(0);
        let steps = take(&repo, &odd, &[&sha], &Mutex::new(Taken::at(None)));
        assert!(matches!(steps.last(), Some(Step::Done { .. })), "{steps:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// git tidies an author's name when it writes one (`:Alice:` becomes `Alice`). A copy
    /// whose author came out different isn't the commit that was read.
    #[test]
    fn a_copy_whose_author_git_changed_is_refused() {
        let dir = scratch("take-author");
        let (repo, _) = repo(&dir);
        let work = dir.join("repo");
        let main = build(&work, &["rev-parse", "main"]);
        let tree = build(&work, &["rev-parse", "refs/jail/work^{tree}"]);
        let sha = handmade(&work, &format!("tree {tree}\nparent {main}\nauthor :Alice: <a@example.invalid> 1700000000 +0000\ncommitter c <c@example.invalid> 1700000000 +0000\n\nodd\n"));
        let odd = repo.refs(&["refs/jail/odd".to_owned()]).unwrap().remove(0);
        let steps = take(&repo, &odd, &[&sha], &Mutex::new(Taken::at(None)));
        assert!(matches!(steps.last(), Some(Step::Failed(e)) if e.contains("author")), "{steps:?}");
        assert_eq!(repo.branch_tip("reviewed/jail/odd").unwrap(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The list of copies is a file: what it says is checked before it is believed.
    #[test]
    fn a_remembered_copy_that_isnt_one_is_signed_again() {
        let dir = scratch("take-forged");
        let (repo, r) = repo(&dir);
        let shas: Vec<&str> = r.commits.iter().map(|c| c.sha.as_str()).collect();
        let state = dir.join("state/taken");
        std::fs::create_dir_all(state.parent().unwrap()).unwrap();
        // Each commit "copied" by itself.
        std::fs::write(&state, format!("{0} {0}\n{1} {1}\n", shas[0], shas[1])).unwrap();
        let steps = take(&repo, &r, &shas, &Mutex::new(Taken::at(Some(state))));
        assert_eq!(steps.iter().filter(|s| matches!(s, Step::Signing { .. })).count(), 2, "{steps:?}");
        assert!(!steps.iter().any(|s| matches!(s, Step::Signed { reused: true, .. })));
        assert!(matches!(steps.last(), Some(Step::Done { .. })));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_stop_after_the_last_signature_still_makes_no_branch() {
        let dir = scratch("take-late-stop");
        let (repo, r) = repo(&dir);
        let reviewed: BTreeSet<String> = r.commits.iter().map(|c| c.sha.clone()).collect();
        let cancel = AtomicBool::new(false);
        let (mut signed, mut last) = (0, None);
        run(&repo, &r, "reviewed/jail/work", Way::Each, &reviewed, &Mutex::new(Taken::at(None)), &cancel, &mut |s| {
            if matches!(s, Step::Signed { .. }) {
                signed += 1;
                cancel.store(signed == 2, std::sync::atomic::Ordering::Relaxed);
            }
            last = Some(s);
        });
        assert_eq!(last, Some(Step::Failed(crate::git::CANCELLED.to_owned())));
        assert_eq!(repo.branch_tip("reviewed/jail/work").unwrap(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_symbolic_ref_where_the_branch_would_go_is_left_alone() {
        let dir = scratch("take-symref");
        let (repo, r) = repo(&dir);
        let work = dir.join("repo");
        build(&work, &["symbolic-ref", "refs/heads/reviewed/jail/work", "refs/heads/nowhere"]);
        let shas: Vec<&str> = r.commits.iter().map(|c| c.sha.as_str()).collect();
        let steps = take(&repo, &r, &shas, &Mutex::new(Taken::at(None)));
        assert!(matches!(steps.last(), Some(Step::Failed(e)) if e.contains("symbolic ref")), "{steps:?}");
        assert_eq!(build(&work, &["symbolic-ref", "refs/heads/reviewed/jail/work"]), "refs/heads/nowhere");
        assert_eq!(build(&work, &["for-each-ref", "refs/heads/nowhere"]), "");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_merge_on_the_ref_keeps_both_parents() {
        let dir = scratch("take-merge");
        let (repo, _) = repo(&dir);
        let work = dir.join("repo");
        build(&work, &["checkout", "-q", "work"]);
        build(&work, &["checkout", "-q", "-b", "side", "HEAD~1"]);
        std::fs::write(work.join("c.txt"), "side\n").unwrap();
        build(&work, &["add", "-A"]);
        build(&work, &["commit", "-q", "-m", "side"]);
        build(&work, &["checkout", "-q", "work"]);
        build(&work, &["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
        build(&work, &["update-ref", "refs/jail/work", "HEAD"]);
        build(&work, &["checkout", "-q", "main"]);
        let r = repo.refs(&["refs/jail".to_owned()]).unwrap().remove(0);
        let shas: Vec<&str> = r.commits.iter().map(|c| c.sha.as_str()).collect();
        assert_eq!(shas.len(), 4);
        let steps = take(&repo, &r, &shas, &Mutex::new(Taken::at(None)));
        assert!(matches!(steps.last(), Some(Step::Done { count: 4, .. })), "{steps:?}");
        // The same shape: commit for commit, the same files, subject and number of parents.
        let shape = |tip: &str| build(&work, &["log", "--topo-order", "--format=%T %s %p", &format!("main..{tip}")]).lines().map(|l| (l.split(' ').take(2).collect::<Vec<_>>().join(" "), l.split(' ').count())).collect::<Vec<_>>();
        assert_eq!(shape("refs/heads/reviewed/jail/work"), shape("refs/jail/work"));
        // And no copy has an original for a parent.
        let originals = build(&work, &["rev-list", "main..refs/jail/work"]);
        let parents = build(&work, &["log", "--format=%P", "main..refs/heads/reviewed/jail/work"]);
        assert!(parents.split_whitespace().all(|p| !originals.contains(p)), "a copy still has an original for a parent");
        // Every commit between main and the branch is a signed copy.
        let copies = build(&work, &["rev-list", "main..refs/heads/reviewed/jail/work"]);
        assert_eq!(copies.lines().count(), 4);
        for sha in copies.lines() {
            assert!(build(&work, &["cat-file", "commit", sha]).contains("gpgsig "));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
