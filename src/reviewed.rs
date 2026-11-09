//! The commits already read, by hash, one per line in
//! `$XDG_STATE_HOME/refview/reviewed` (`~/.local/state/refview/reviewed`). Kept outside the
//! repository so reviewing writes nothing into it. A hash names the content, so a ref that is
//! rewritten shows its new commits as unread.

use std::collections::BTreeSet;
use std::path::PathBuf;

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

impl Reviewed {
    pub fn load() -> Reviewed {
        Reviewed::at(default_path())
    }

    fn at(path: Option<PathBuf>) -> Reviewed {
        let text = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        Reviewed { shas: text.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect(), path }
    }

    pub fn has(&self, sha: &str) -> bool {
        self.shas.contains(sha)
    }

    /// Marks `shas` read or unread and saves. The marks stay for this run even when saving fails.
    pub fn set(&mut self, shas: &[&str], on: bool) -> Result<(), String> {
        for &sha in shas {
            match on {
                true => self.shas.insert(sha.to_owned()),
                false => self.shas.remove(sha),
            };
        }
        self.save()
    }

    fn save(&self) -> Result<(), String> {
        let path = self.path.as_ref().ok_or("no state directory (set XDG_STATE_HOME or HOME)")?;
        let dir = path.parent().ok_or("the state path has no directory")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut text = self.shas.iter().map(String::as_str).collect::<Vec<_>>().join("\n");
        text.push('\n');
        // Written beside the file and renamed over it, so a crash leaves the old list whole.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_survive_a_reload_and_unmarking_removes_them() {
        let dir = std::env::temp_dir().join(format!("refview-test-{}", std::process::id()));
        let path = dir.join("refview/reviewed");
        let mut r = Reviewed::at(Some(path.clone()));
        r.set(&["aaa", "bbb"], true).unwrap();
        r.set(&["aaa"], false).unwrap();
        let again = Reviewed::at(Some(path));
        assert!(!again.has("aaa"));
        assert!(again.has("bbb"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
