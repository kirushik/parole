//! The commits already read, by full hash, one per line in
//! `$XDG_STATE_HOME/refview/reviewed` (default `~/.local/state/refview/reviewed`), so reviewing
//! writes nothing into the repository being reviewed. A hash names a commit's content and
//! history: a ref rewritten to different commits shows them unread, and a commit that comes
//! back with the same hash is the same commit and stays read.
//!
//! Two windows may be open at once. Each change is applied to the file as it is on disk at
//! that moment, not to this window's copy of it, so one window's marks don't undo another's.
//! There is no lock: two changes in the same instant can still lose one.

use std::collections::BTreeSet;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Reviewed {
    shas: BTreeSet<String>,
    path: Option<PathBuf>,
}

fn default_path() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
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
        Reviewed { shas, path }
    }

    pub fn has(&self, sha: &str) -> bool {
        self.shas.contains(sha)
    }

    /// Marks `shas` read or unread and saves. The marks hold for this run even when saving fails.
    pub fn set(&mut self, shas: &[&str], on: bool) -> Result<(), String> {
        let apply = |set: &mut BTreeSet<String>| {
            for &sha in shas {
                match on {
                    true => set.insert(sha.to_owned()),
                    false => set.remove(sha),
                };
            }
        };
        apply(&mut self.shas);
        let path = self.path.as_ref().ok_or("no state directory (set XDG_STATE_HOME or HOME)")?;
        // From the file as it is now: another window may have written since this one read it.
        let mut on_disk = read(path)?;
        apply(&mut on_disk);
        write(path, &on_disk)?;
        self.shas = on_disk;
        Ok(())
    }
}

/// Writes the list beside the file and renames it over, so a reader sees the old list or the
/// new one, never half of either. The temporary name is this process's own.
fn write(path: &Path, shas: &BTreeSet<String>) -> Result<(), String> {
    let dir = path.parent().ok_or("the state path has no directory")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut text = shas.iter().map(String::as_str).collect::<Vec<_>>().join("\n");
    text.push('\n');
    let put = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    put().map_err(|e| {
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
