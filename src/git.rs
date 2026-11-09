//! Everything refview asks of git. Only commands that read: `for-each-ref`, `merge-base`,
//! `log`, `show` and `diff`. Nothing is checked out, merged or built, and the diff commands
//! refuse external diff drivers and textconv filters, so reviewing a ref never runs a program
//! that the ref (or its `.gitattributes`) names.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The tree every repository has: diffing against it shows a ref with no common history whole.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// The most commits listed under one ref.
const MAX_COMMITS: &str = "500";
const FIELD: char = '\u{1f}';
const RECORD: char = '\u{1e}';

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Commit {
    pub sha: String,
    pub short: String,
    pub author: String,
    pub date: String,
    pub subject: String,
    pub body: String,
}

#[derive(Clone, Debug, Default)]
pub struct RefInfo {
    /// The full name, `refs/jail/fix-login`.
    pub name: String,
    /// Where the ref left HEAD's history; `None` when they share none.
    pub base: Option<String>,
    /// The commits on the ref that HEAD doesn't have, newest first.
    pub commits: Vec<Commit>,
}

impl RefInfo {
    /// The name without `refs/`.
    pub fn short(&self) -> &str {
        self.name.strip_prefix("refs/").unwrap_or(&self.name)
    }
}

#[derive(Clone)]
pub struct Repo {
    pub root: PathBuf,
}

impl Repo {
    /// The repository `dir` is in.
    pub fn open(dir: &Path) -> Result<Repo, String> {
        let out = run(dir, &["rev-parse", "--show-toplevel"])?;
        Ok(Repo { root: PathBuf::from(out.trim_end()) })
    }

    pub fn name(&self) -> String {
        self.root.file_name().map_or_else(|| self.root.display().to_string(), |n| n.to_string_lossy().into_owned())
    }

    fn git(&self, args: &[&str]) -> Result<String, String> {
        run(&self.root, args)
    }

    /// The refs under `patterns` (`refs/jail`, `refs/devvm`), each with the commits HEAD lacks.
    pub fn refs(&self, patterns: &[String]) -> Result<Vec<RefInfo>, String> {
        let mut args = vec!["for-each-ref", "--format=%(refname)"];
        args.extend(patterns.iter().map(String::as_str));
        let names = self.git(&args)?;
        let has_head = self.git(&["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]).is_ok();
        names.lines().filter(|n| !n.is_empty()).map(|name| self.ref_info(name, has_head)).collect()
    }

    fn ref_info(&self, name: &str, has_head: bool) -> Result<RefInfo, String> {
        let base = match has_head {
            true => self.git(&["merge-base", "HEAD", name]).ok().map(|b| b.trim().to_owned()).filter(|b| !b.is_empty()),
            false => None,
        };
        let range = match &base {
            Some(base) => format!("{base}..{name}"),
            None => name.to_owned(),
        };
        let format = format!("--format=%H{FIELD}%h{FIELD}%an{FIELD}%ad{FIELD}%s{FIELD}%b{RECORD}");
        let log = self.git(&["log", "--max-count", MAX_COMMITS, "--date=format:%Y-%m-%d %H:%M", &format, &range, "--"])?;
        Ok(RefInfo { name: name.to_owned(), base, commits: parse_log(&log) })
    }

    /// One commit's patch, against its first parent (a root commit shows as all added).
    pub fn commit_patch(&self, sha: &str) -> Result<String, String> {
        self.git(&["show", "--format=", "--diff-merges=first-parent", "--no-ext-diff", "--no-textconv", "--no-color", "-M", sha, "--"])
    }

    /// Everything the ref changes since it left HEAD's history, as one patch.
    pub fn range_patch(&self, r: &RefInfo) -> Result<String, String> {
        let base = r.base.as_deref().unwrap_or(EMPTY_TREE);
        self.git(&["diff", "--no-ext-diff", "--no-textconv", "--no-color", "-M", base, &r.name, "--"])
    }
}

fn parse_log(log: &str) -> Vec<Commit> {
    log.split(RECORD)
        .filter_map(|rec| {
            let mut f = rec.trim_start_matches('\n').splitn(6, FIELD);
            let sha = f.next().filter(|s| !s.is_empty())?.to_owned();
            Some(Commit {
                sha,
                short: f.next()?.to_owned(),
                author: f.next()?.to_owned(),
                date: f.next()?.to_owned(),
                subject: f.next()?.to_owned(),
                body: f.next().unwrap_or("").trim().to_owned(),
            })
        })
        .collect()
}

fn run(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        // Read-only use: don't take locks to refresh the index, don't page, don't prompt.
        .args(["--no-optional-locks", "--no-pager"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("couldn't run git: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("git {}: {}", args.first().copied().unwrap_or(""), err.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_splits_into_commits_with_their_bodies() {
        let log = format!(
            "aaa{FIELD}a{FIELD}Ann{FIELD}2026-11-09 10:00{FIELD}Fix login{FIELD}Two lines\nof body\n{RECORD}\nbbb{FIELD}b{FIELD}Bo{FIELD}2026-11-08 09:00{FIELD}Add test{FIELD}{RECORD}\n"
        );
        let commits = parse_log(&log);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].subject, "Fix login");
        assert_eq!(commits[0].body, "Two lines\nof body");
        assert_eq!(commits[1].sha, "bbb");
        assert_eq!(commits[1].body, "");
    }

    #[test]
    fn an_empty_log_has_no_commits() {
        assert!(parse_log("").is_empty());
    }
}
