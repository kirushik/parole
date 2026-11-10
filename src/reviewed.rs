//! The commits already read, by full hash, one per line in
//! `$XDG_STATE_HOME/refview/reviewed` (default `~/.local/state/refview/reviewed`), so reviewing
//! writes nothing into the repository being reviewed. A hash names a commit's content and
//! history: a ref rewritten to different commits shows them unread, and a commit that comes
//! back with the same hash is the same commit and stays read.
//!
//! Two windows may be open at once. Each change is applied to the file as it is on disk at
//! that moment, not to this window's copy of it, so one window's marks don't undo another's.
//! There is no lock: two changes in the same instant can still lose one.
//!
//! The file is only ever looked for under an absolute state directory. With neither
//! `XDG_STATE_HOME` nor `HOME` absolute there is no file, and marks last for the run.

use std::collections::BTreeSet;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Reviewed {
    shas: BTreeSet<String>,
    path: Option<PathBuf>,
    /// Changes made since the last save that worked, oldest first: hash, and marked or not.
    /// The next save applies them again, so one that failed isn't forgotten by one that works.
    unsaved: Vec<(String, bool)>,
}

/// Where refview keeps `name` between runs, when there is an absolute state directory.
pub fn state_file(name: &str) -> Option<PathBuf> {
    Some(default_path()?.with_file_name(name))
}

fn default_path() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        // A relative HOME would put the list under wherever this was started: a repository, say.
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_absolute()).map(|h| h.join(".local/state")))?;
    Some(state.join("refview/reviewed"))
}

/// The list on disk. A file that isn't there is an empty list; one that can't be read is an
/// error, so it is never mistaken for empty and written over.
fn read(path: &Path) -> Result<BTreeSet<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

impl Reviewed {
    pub fn load() -> Reviewed {
        Reviewed::at(default_path())
    }

    /// The list kept at `path`; `None` keeps marks for this run only.
    pub fn at(path: Option<PathBuf>) -> Reviewed {
        // An unreadable list starts this run with nothing marked; `set` will refuse to replace it.
        let shas = path.as_deref().map(read).and_then(Result::ok).unwrap_or_default();
        Reviewed { shas, path, unsaved: Vec::new() }
    }

    pub fn has(&self, sha: &str) -> bool {
        self.shas.contains(sha)
    }

    /// Every hash marked read, as of now.
    pub fn all(&self) -> BTreeSet<String> {
        self.shas.clone()
    }

    /// Marks `shas` read or unread and saves. The marks hold for this run even when saving fails.
    pub fn set(&mut self, shas: &[&str], on: bool) -> Result<(), String> {
        let apply = |set: &mut BTreeSet<String>, changes: &[(String, bool)]| {
            for (sha, on) in changes {
                match on {
                    true => set.insert(sha.clone()),
                    false => set.remove(sha),
                };
            }
        };
        let now: Vec<(String, bool)> = shas.iter().map(|&sha| (sha.to_owned(), on)).collect();
        apply(&mut self.shas, &now);
        self.unsaved.extend(now);
        let path = self.path.as_ref().ok_or("no state directory (XDG_STATE_HOME or HOME must be an absolute path)")?;
        // From the file as it is now: another window may have written since this one read it.
        let mut on_disk = read(path)?;
        apply(&mut on_disk, &self.unsaved);
        write(path, &on_disk)?;
        self.shas = on_disk;
        self.unsaved.clear();
        Ok(())
    }
}

/// Writes the list beside the file and renames it over, so a reader sees the old list or the
/// new one, never half of either. The temporary file is created new (`O_EXCL`): a file or a
/// symlink already at that name is an error, never something to write through, and only a
/// file this call created is removed.
fn write(path: &Path, shas: &BTreeSet<String>) -> Result<(), String> {
    let dir = path.parent().ok_or("the state path has no directory")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut text = shas.iter().map(String::as_str).collect::<Vec<_>>().join("\n");
    text.push('\n');
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let put = f.write_all(text.as_bytes()).and_then(|()| f.sync_all()).and_then(|()| std::fs::rename(&tmp, path));
    put.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("refview-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn each_change_is_on_disk_before_the_next() {
        let dir = scratch("each");
        let path = dir.join("refview/reviewed");
        let mut r = Reviewed::at(Some(path.clone()));
        r.set(&["aaa", "bbb"], true).unwrap();
        let after_mark = Reviewed::at(Some(path.clone()));
        assert!(after_mark.has("aaa") && after_mark.has("bbb"));
        r.set(&["aaa"], false).unwrap();
        let after_unmark = Reviewed::at(Some(path));
        assert!(!after_unmark.has("aaa") && after_unmark.has("bbb"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_second_window_keeps_the_firsts_marks() {
        let dir = scratch("two");
        let path = dir.join("refview/reviewed");
        let mut one = Reviewed::at(Some(path.clone()));
        let mut two = Reviewed::at(Some(path.clone()));
        one.set(&["aaa"], true).unwrap();
        two.set(&["bbb"], true).unwrap();
        let both = Reviewed::at(Some(path));
        assert!(both.has("aaa") && both.has("bbb"));
        assert!(two.has("aaa"), "the second window learns the first's mark when it saves");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_save_that_works_carries_the_ones_that_failed() {
        let dir = scratch("retry");
        let path = dir.join("refview/reviewed");
        std::fs::create_dir_all(&path).unwrap();
        let mut r = Reviewed::at(Some(path.clone()));
        assert!(r.set(&["aaa"], true).is_err());
        assert!(r.set(&["bbb"], true).is_err());
        assert!(r.set(&["bbb"], false).is_err());
        // The obstacle goes; the next save writes everything this run decided.
        std::fs::remove_dir(&path).unwrap();
        r.set(&["ccc"], true).unwrap();
        let saved = Reviewed::at(Some(path));
        assert!(saved.has("aaa") && !saved.has("bbb") && saved.has("ccc"));
        assert!(r.has("aaa") && !r.has("bbb") && r.has("ccc"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_symlink_at_the_temporary_name_is_not_written_through() {
        let dir = scratch("symlink");
        let path = dir.join("refview/reviewed");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let victim = dir.join("victim");
        std::fs::write(&victim, "keep me\n").unwrap();
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        std::os::unix::fs::symlink(&victim, &tmp).unwrap();
        let mut r = Reviewed::at(Some(path));
        assert!(r.set(&["aaa"], true).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me\n");
        assert!(tmp.is_symlink(), "a file this run didn't create is left alone");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_unreadable_list_is_not_replaced() {
        let dir = scratch("unreadable");
        // A directory where the file should be: reading it fails, and not with "not found".
        let path = dir.join("refview/reviewed");
        std::fs::create_dir_all(&path).unwrap();
        let mut r = Reviewed::at(Some(path.clone()));
        assert!(r.set(&["aaa"], true).is_err());
        assert!(r.has("aaa"), "the mark still holds for this run");
        assert!(path.is_dir());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
