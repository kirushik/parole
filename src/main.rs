//! refview: read what a sandbox wrote before bringing it in.
//!
//! Lists the refs under `refs/jail/` and `refs/devvm/` of a repository, the commits each has
//! that HEAD lacks, and shows the selected commit (or the whole ref) as a diff. It only reads:
//! see `git.rs` for the exact commands.
//!
//! `refview [DIR] [--refs PATTERN]...` opens the window on the repository at DIR (default: the
//! current directory). `--refs` replaces the default patterns and may repeat.
//! Rune's headless flags work too: `refview DIR --snap out.png`, `--tree tree.json`,
//! `--script keys.json`, `--scene split`.

mod git;
mod reviewed;

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use git::{Commit, RefInfo, Repo};
use reviewed::Reviewed;
use rune::{App, AppSpec, Cx, El, Key, KeyEvent, Latest, ScrollSpec, Tx, col, el, row, text};
use rune_kit::diff::{DiffMode, DiffState, DiffView, FileDiff, parse};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, SegmentedControl};

const DEFAULT_PATTERNS: &[&str] = &["refs/jail", "refs/devvm"];
/// How long a selection waits for its patch before the window draws without it. Short enough
/// to go unnoticed, long enough that an ordinary commit never flashes "Reading".
/// `REFVIEW_WAIT=1` waits until git answers, with the patch or an error, so a headless still
/// is never taken mid-read.
const QUICK: Duration = Duration::from_millis(20);
const KEYS: &str = "n / p  commit     j / k  hunk     ] / [  file     s  split     w  wrap     r  reviewed     u  reload";

struct Config {
    dir: PathBuf,
    patterns: Vec<String>,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

fn config() -> &'static Config {
    CONFIG.get_or_init(|| {
        let mut dir = None;
        let mut patterns = Vec::new();
        let mut args = rune::extra_args().iter();
        while let Some(a) = args.next() {
            match a.as_str() {
                "--refs" => match args.next() {
                    // git matches a pattern from `refs/` down; anything else lists nothing.
                    Some(p) if p.starts_with("refs/") => patterns.push(p.clone()),
                    Some(p) => usage(&format!("--refs takes a pattern under refs/, not {p:?}")),
                    None => usage("--refs needs a pattern"),
                },
                a if a.starts_with('-') => usage(&format!("unknown option {a}")),
                a if dir.is_some() => usage(&format!("one directory at a time: {a:?} is a second")),
                a => dir = Some(PathBuf::from(a)),
            }
        }
        if patterns.is_empty() {
            patterns = DEFAULT_PATTERNS.iter().map(|&p| p.to_owned()).collect();
        }
        Config { dir: dir.unwrap_or_else(|| PathBuf::from(".")), patterns }
    })
}

fn usage(problem: &str) -> ! {
    eprintln!("refview: {problem}\nusage: refview [DIR] [--refs PATTERN]...");
    std::process::exit(2);
}

/// What a sidebar row shows: every change on a ref, or one of its commits.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Pick {
    All,
    Commit(usize),
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct Item {
    r: usize,
    pick: Pick,
}

struct State {
    repo: Option<Repo>,
    /// The ref patterns listed, `refs/jail` and `refs/devvm` unless `--refs` said otherwise.
    patterns: Vec<String>,
    /// Wait for a patch however long it takes (`REFVIEW_WAIT`), so a still always has its diff.
    patient: bool,
    refs: Vec<RefInfo>,
    /// The rows that can be selected, in sidebar order.
    items: Vec<Item>,
    sel: usize,
    diff: DiffState,
    /// The selection's patch, read off the UI thread; the newest request wins. It is parsed
    /// on the UI thread when it lands: a parsed `FileDiff` can't cross threads.
    patch: Latest<Result<String, String>>,
    /// The selection's patch hasn't landed yet.
    loading: bool,
    reviewed: Reviewed,
    /// Why there is nothing to show, or what went wrong reading the selection.
    error: Option<String>,
    /// Something that went wrong beside the diff (saving the reviewed list), shown in the footer.
    notice: Option<String>,
}

/// A row by what it shows, not where it is: the ref's name and the commit's hash (`None` for
/// "All changes"). A press carries this, so a row that moved under a reload isn't mistaken
/// for the one that was drawn.
type RowId = (String, Option<String>);

/// What the worker reads.
enum Target {
    Commit(String),
    Range(RefInfo),
}

impl State {
    fn open() -> State {
        let cfg = config();
        let patient = std::env::var_os("REFVIEW_WAIT").is_some_and(|v| !v.is_empty());
        State::at(&cfg.dir, cfg.patterns.clone(), Reviewed::load(), patient)
    }

    fn at(dir: &std::path::Path, patterns: Vec<String>, reviewed: Reviewed, patient: bool) -> State {
        let mut s = State {
            repo: None,
            patterns,
            patient,
            refs: Vec::new(),
            items: Vec::new(),
            sel: 0,
            diff: DiffState::new(Vec::new()),
            patch: Latest::default(),
            loading: false,
            reviewed,
            error: None,
            notice: None,
        };
        match Repo::open(dir) {
            Ok(repo) => {
                s.repo = Some(repo);
                s.reload();
            }
            Err(e) => s.error = Some(e),
        }
        s
    }

    /// Reads the refs again and keeps the selection where it was, when it's still there.
    fn reload(&mut self) {
        let Some(repo) = &self.repo else { return };
        let was = self.items.get(self.sel).map(|&it| self.row_id(it));
        self.error = None;
        match repo.refs(&self.patterns) {
            Ok(mut refs) => {
                // Oldest first: the order the work was written in, and the order to read it.
                refs.iter_mut().for_each(|r| r.commits.reverse());
                self.refs = refs;
            }
            Err(e) => {
                self.refs.clear();
                self.error = Some(e);
            }
        }
        self.items = items(&self.refs);
        self.sel = was.and_then(|id| self.position(&id)).unwrap_or(0);
        self.load();
    }

    fn row_id(&self, it: Item) -> RowId {
        let r = &self.refs[it.r];
        (r.name.clone(), match it.pick {
            Pick::All => None,
            Pick::Commit(c) => Some(r.commits[c].sha.clone()),
        })
    }

    fn position(&self, id: &RowId) -> Option<usize> {
        self.items.iter().position(|&it| self.row_id(it) == *id)
    }

    /// Selects the row a press was drawn for, if it is still there.
    fn select_row(&mut self, id: &RowId, tx: &mut Tx) {
        if let Some(i) = self.position(id) {
            self.select(i, tx);
        }
    }

    fn selected(&self) -> Option<(&RefInfo, Option<&Commit>)> {
        let it = self.items.get(self.sel)?;
        let r = &self.refs[it.r];
        Some((r, match it.pick {
            Pick::All => None,
            Pick::Commit(c) => Some(&r.commits[c]),
        }))
    }

    /// Swaps in the diff's files, keeping the layout the reader chose.
    fn show(&mut self, files: Vec<FileDiff>) {
        let (mode, wrap) = (self.diff.mode(), self.diff.wrap());
        self.diff = DiffState::new(files);
        self.diff.set_mode(mode);
        self.diff.set_wrap(wrap);
    }

    /// Asks a worker for the selection's patch. The last selection's diff goes at once, so the
    /// header never names one commit over another's changes. Most patches are back within
    /// [`QUICK`] and land before the next frame; a slower one shows "Reading" until `poll`
    /// takes it.
    fn load(&mut self) {
        self.show(Vec::new());
        let job = match (&self.repo, self.selected()) {
            (Some(repo), Some((_, Some(c)))) => Some((repo.clone(), Target::Commit(c.sha.clone()))),
            (Some(repo), Some((r, None))) => Some((repo.clone(), Target::Range(r.clone()))),
            _ => None,
        };
        let Some(job) = job else {
            // Nothing to read: an error from listing the refs stays up.
            self.patch.cancel();
            self.loading = false;
            return;
        };
        // A new read starts clean; the last selection's failure isn't this one's.
        self.error = None;
        self.loading = true;
        self.patch.request(job, |(repo, target)| {
            match target {
                Target::Commit(sha) => repo.commit_patch(&sha),
                Target::Range(r) => repo.range_patch(&r),
            }
        });
        let until = Instant::now() + QUICK;
        while !self.land() && (self.patient || Instant::now() < until) {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Takes the worker's answer when there is one; whether it did.
    fn land(&mut self) -> bool {
        let Some(answer) = self.patch.take() else { return false };
        self.loading = false;
        match answer {
            Ok(patch) => self.show(parse(&patch)),
            Err(e) => self.error = Some(e),
        }
        true
    }

    fn select(&mut self, i: usize, tx: &mut Tx) {
        if i >= self.items.len() || i == self.sel {
            return;
        }
        self.sel = i;
        self.error = None;
        self.load();
        tx.scroll_into_view(item_key(i));
    }

    fn step(&mut self, forward: bool, tx: &mut Tx) {
        let to = if forward { self.sel + 1 } else { self.sel.wrapping_sub(1) };
        self.select(to, tx);
    }

    fn is_reviewed(&self, it: Item) -> bool {
        let r = &self.refs[it.r];
        match it.pick {
            Pick::All => r.commits.iter().all(|c| self.reviewed.has(&c.sha)),
            Pick::Commit(c) => self.reviewed.has(&r.commits[c].sha),
        }
    }

    /// Marks the selection read, or unread when it already is. A whole ref marks its commits.
    fn toggle_reviewed(&mut self) {
        let Some(&it) = self.items.get(self.sel) else { return };
        let on = !self.is_reviewed(it);
        let r = &self.refs[it.r];
        let shas: Vec<&str> = match it.pick {
            Pick::All => r.commits.iter().map(|c| c.sha.as_str()).collect(),
            Pick::Commit(c) => vec![r.commits[c].sha.as_str()],
        };
        // The marks hold for this run either way; the diff stays up.
        self.notice = self.reviewed.set(&shas, on).err().map(|e| format!("Couldn't save the reviewed list: {e}"));
    }

    fn toggle_split(&mut self, tx: &mut Tx) {
        tx.motion(rune::tok::motion::SNAPPY);
        let split = self.diff.mode() == DiffMode::Split;
        self.diff.set_mode(if split { DiffMode::Unified } else { DiffMode::Split });
    }
}

/// A ref gets an "All changes" row above its commits, unless that row would show exactly what
/// its one commit shows: a single commit whose first parent is where the ref left HEAD. (A
/// lone merge commit is not that: its own diff is against its first parent, the ref's against
/// HEAD's side.)
fn items(refs: &[RefInfo]) -> Vec<Item> {
    let mut out = Vec::new();
    for (r, info) in refs.iter().enumerate() {
        let same_as_its_commit = match info.commits.as_slice() {
            [only] => only.parents.first() == info.base.as_ref(),
            _ => false,
        };
        if !info.commits.is_empty() && !same_as_its_commit {
            out.push(Item { r, pick: Pick::All });
        }
        out.extend((0..info.commits.len()).map(|c| Item { r, pick: Pick::Commit(c) }));
    }
    out
}

fn item_key(i: usize) -> String {
    format!("item-{i}")
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {one}s") }
}

fn view(s: &State, cx: &mut Cx<State>) -> El {
    let k = Kit::neutral(cx.system_dark());
    let main = col().key("main").grow(1.0).min_w(0.0).h_full().child(header(s, &k)).child(body(s, &k)).child(footer(s, &k));
    row().w_full().h_full().fill(k.c.bg_base).child(sidebar(s, &k)).child(main)
}

fn sidebar(s: &State, k: &Kit) -> El {
    let title = match &s.repo {
        Some(repo) => repo.name(),
        None => "refview".to_owned(),
    };
    let head = col()
        .px(12.0)
        .py(10.0)
        .gap(2.0)
        .child(text(title).font(k.ty.ui_medium).color(k.c.text_base).truncate())
        .child(text(format!("{} to review", plural(s.refs.iter().filter(|r| !r.commits.is_empty()).count(), "ref"))).font(k.ty.text_small).color(k.c.text_muted));
    let mut list = col().w_full().px(6.0).pb(8.0).gap(1.0);
    let mut i = 0;
    for (r, info) in s.refs.iter().enumerate() {
        list = list.child(ref_header(info, k));
        let note = |t: String| row().w_full().px(10.0).py(6.0).child(text(t).font(k.ty.text_small).color(k.c.text_faint).wrap());
        if let Some(e) = &info.error {
            list = list.child(note(format!("Couldn't read this ref: {e}")));
        } else if info.commits.is_empty() {
            list = list.child(note("Nothing HEAD doesn't already have".to_owned()));
        } else if info.truncated {
            // Oldest first, so the ones left out would have come before these.
            list = list.child(note(format!("Older commits not listed: showing the newest {}", git::MAX_COMMITS)));
        }
        while let Some(&it) = s.items.get(i).filter(|it| it.r == r) {
            list = list.child(item_row(s, i, it, k));
            i += 1;
        }
    }
    let scroll = col().key("refs").w_full().grow(1.0).min_h(0.0).scroll(ScrollSpec { stick_bottom: false }).child(list);
    col().key("sidebar").w(300.0).h_full().fill(k.c.layer_01).child(head).child(scroll)
}

fn ref_header(info: &RefInfo, k: &Kit) -> El {
    let count = text(info.commits.len().to_string()).font(k.ty.text_xsmall_medium).color(k.c.text_faint);
    row()
        .w_full()
        .px(10.0)
        .pt(12.0)
        .pb(4.0)
        .gap(8.0)
        .items_center()
        .child(text(info.short().to_owned()).font(k.ty.text_small_medium).color(k.c.text_muted).truncate().grow(1.0).min_w(0.0))
        .child(count)
}

fn item_row(s: &State, i: usize, it: Item, k: &Kit) -> El {
    let info = &s.refs[it.r];
    let id = s.row_id(it);
    let selected = i == s.sel;
    let reviewed = s.is_reviewed(it);
    let (title, detail) = match it.pick {
        Pick::All => ("All changes".to_owned(), plural(info.commits.len(), "commit")),
        Pick::Commit(c) => {
            let c = &info.commits[c];
            (c.subject.clone(), format!("{}  {}  {}", c.short, c.author, c.date))
        }
    };
    // A filled dot once read, a ring until then.
    let dot = match reviewed {
        true => el().size(8.0, 8.0).radius(4.0).fill(k.c.success.icon),
        false => el().size(8.0, 8.0).radius(4.0).border(1.0, k.c.text_faint),
    };
    let ink = if reviewed && !selected { k.c.text_muted } else { k.c.text_base };
    let lines = col()
        .grow(1.0)
        .min_w(0.0)
        .gap(1.0)
        .child(text(title.clone()).font(k.ty.ui).color(ink).truncate())
        .child(text(detail).font(k.ty.text_xsmall).color(k.c.text_faint).truncate());
    row()
        .key(item_key(i))
        .role("option")
        .label(title)
        .checked(reviewed)
        .w_full()
        .px(10.0)
        .py(6.0)
        .gap(10.0)
        .items_center()
        .radius(6.0)
        .fill(if selected { k.c.layer_03 } else { k.c.layer_01 })
        .hover(k.c.hover)
        .on_press_tx(move |s: &mut State, tx: &mut Tx| s.select_row(&id, tx))
        .child(dot)
        .child(lines)
}

fn header(s: &State, k: &Kit) -> El {
    let Some((info, commit)) = s.selected() else {
        return row().key("bar").w_full().h(56.0).fill(k.c.layer_01);
    };
    let (title, meta, message) = match commit {
        Some(c) => (c.subject.clone(), format!("{}  ·  {}  ·  {}  ·  {}", c.short, c.author, c.date, info.short()), c.body.as_str()),
        None => {
            let base = match &info.base {
                Some(b) => format!("since {}", &b[..b.len().min(9)]),
                None => "no history shared with HEAD".to_owned(),
            };
            (format!("All changes on {}", info.short()), format!("{}  ·  {base}", plural(info.commits.len(), "commit")), "")
        }
    };
    let (adds, dels) = s.diff.files().iter().fold((0, 0), |(a, d), f| (a + f.additions(), d + f.deletions()));
    let stats = row()
        .gap(8.0)
        .items_center()
        .child(text(plural(s.diff.files().len(), "file")).font(k.ty.text_small).color(k.c.text_muted))
        .child(text(format!("+{adds}")).key("adds").font(k.ty.text_small_medium).color(k.c.success.icon))
        .child(text(format!("\u{2212}{dels}")).key("dels").font(k.ty.text_small_medium).color(k.c.danger.icon));
    let split = s.diff.mode() == DiffMode::Split;
    let modes = SegmentedControl::new(&["Unified", "Split"], Some(split as usize)).width(Some(150.0)).on_pick(
        |s: &mut State, i, tx: &mut Tx| {
            if (i == 1) != (s.diff.mode() == DiffMode::Split) {
                s.toggle_split(tx);
            }
            // A click leaves focus on the segment; hand it back so the window reads as it did.
            tx.blur();
        },
    );
    let reviewed = s.items.get(s.sel).is_some_and(|&it| s.is_reviewed(it));
    let mark = Button::new(if reviewed { "Reviewed" } else { "Mark reviewed" })
        .variant(if reviewed { ButtonVariant::Ghost } else { ButtonVariant::Neutral })
        .control_size(ControlSize::Regular)
        .on_press_tx(|s: &mut State, tx: &mut Tx| {
            s.toggle_reviewed();
            tx.blur();
        })
        .build(k)
        .key("mark");
    let titles = col()
        .grow(1.0)
        .min_w(0.0)
        .gap(2.0)
        .child(text(title).key("title").font(k.ty.ui_medium).color(k.c.text_base).truncate())
        .child(text(meta).key("meta").font(k.ty.text_small).color(k.c.text_muted).truncate());
    // No counts while the patch is still on its way: "0 files" would be a claim.
    let stats = if s.loading { row() } else { stats };
    let top = row().w_full().gap(14.0).items_center().child(titles).child(stats).child(modes.build(k)).child(mark);
    let mut bar = col().key("bar").w_full().px(16.0).py(10.0).gap(8.0).fill(k.c.layer_01).child(top);
    if !message.is_empty() {
        // The message was written in the sandbox too: shown as text to read, nothing more.
        let msg = text(message.to_owned()).key("message").font(k.ty.text_small).color(k.c.text_muted).wrap().max_lines(6).selectable(true);
        bar = bar.child(msg);
    }
    bar
}

fn body(s: &State, k: &Kit) -> El {
    let note = |title: &str, detail: String| {
        col()
            .key("empty")
            .grow(1.0)
            .w_full()
            .center()
            .gap(6.0)
            .padding(32.0)
            .child(text(title.to_owned()).font(k.ty.ui_medium).color(k.c.text_base))
            .child(text(detail).font(k.ty.text_small).color(k.c.text_muted).wrap())
    };
    if let Some(e) = &s.error {
        return note("Couldn't read that", e.clone());
    }
    if s.loading {
        return note("Reading\u{2026}", "git is still producing this diff.".to_owned());
    }
    if s.items.is_empty() {
        let patterns = s.patterns.join(", ");
        let failed = s.refs.iter().filter(|r| r.error.is_some()).count();
        if failed > 0 {
            // Unread is not the same as nothing to read.
            return note("Couldn't read the refs", format!("{} under {patterns} couldn't be listed; the sidebar says why.", plural(failed, "ref")));
        }
        let detail = match s.refs.is_empty() {
            true => format!("No refs under {patterns}. Fetch some (jail fetch), then press u."),
            false => format!("Every ref under {patterns} is already in HEAD."),
        };
        return note("Nothing to review", detail);
    }
    if s.diff.files().is_empty() {
        return note("No changes", "This commit changes no files.".to_owned());
    }
    DiffView::new(|s: &State| &s.diff, |s: &mut State| &mut s.diff).build(s, k)
}

fn footer(s: &State, k: &Kit) -> El {
    let line = match &s.notice {
        Some(n) => text(n.clone()).key("notice").font(k.ty.text_xsmall).color(k.c.danger.icon).truncate(),
        None => text(KEYS).font(k.ty.text_xsmall).color(k.c.text_faint).truncate(),
    };
    row().key("keys").w_full().h(28.0).px(16.0).items_center().fill(k.c.layer_01).child(line)
}

/// The Latin letter on the key that typed `t`, for the layouts this is used with: a shortcut
/// is a place on the keyboard, and `n` on a Russian layout types "т".
fn shortcut(t: &str) -> &str {
    match t {
        "т" | "Т" => "n",
        "з" | "З" => "p",
        "ы" | "Ы" => "s",
        "ц" | "Ц" => "w",
        "к" | "К" => "r",
        "г" | "Г" => "u",
        "о" | "О" => "j",
        "л" | "Л" => "k",
        "ъ" | "Ъ" => "]",
        "х" | "Х" => "[",
        t => t,
    }
}

fn on_key(s: &mut State, ev: &KeyEvent, tx: &mut Tx) -> bool {
    // Not gated on focus: the window has no text field, and a button that took focus (from a
    // click or Tab) has already had its own keys (Space, Return) by the time this runs.
    // `DiffState::key` does stand down while anything is focused, so its four keys are here.
    let m = ev.mods;
    if let Key::Text(t) = &ev.key
        && !(m.cmd || m.ctrl || m.alt)
    {
        match shortcut(t) {
            "n" => s.step(true, tx),
            "p" => s.step(false, tx),
            "s" => s.toggle_split(tx),
            "w" => s.diff.set_wrap(!s.diff.wrap()),
            "r" => s.toggle_reviewed(),
            "u" => s.reload(),
            "j" => drop(s.diff.next_hunk(true, tx)),
            "k" => drop(s.diff.next_hunk(false, tx)),
            "]" => drop(s.diff.next_file(true, tx)),
            "[" => drop(s.diff.next_file(false, tx)),
            _ => return s.diff.key(ev, tx),
        }
        return true;
    }
    s.diff.key(ev, tx)
}

fn main() {
    // Read the arguments before the window: `--help` and a bad flag, Rune's or ours, exit here.
    config();
    let spec = AppSpec {
        scenes: &[
            ("split", |s: &mut State| s.diff.set_mode(DiffMode::Split)),
            ("last", |s: &mut State| {
                s.sel = s.items.len().saturating_sub(1);
                s.load();
            }),
        ],
        on_key: Some(on_key),
        // The waker runs `poll` when a patch that outlasted `QUICK` lands.
        setup: Some(|s: &mut State, waker| s.patch.set_waker(waker)),
        poll: Some(|s: &mut State, _, _| s.land()),
        min_size: Some((760.0, 420.0)),
        families: ("Inter", "ui-monospace, Inter"),
        ..AppSpec::new("refview", (1180.0, 760.0), State::open, view)
    };
    App::new(spec).modifier_keys().run();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(sha: &str) -> Commit {
        Commit { sha: sha.to_owned(), ..Commit::default() }
    }

    fn child(sha: &str, parents: &[&str]) -> Commit {
        Commit { parents: parents.iter().map(|&p| p.to_owned()).collect(), ..commit(sha) }
    }

    use crate::git::testing::{build, scratch};
    use rune::{Env, Mods, Runtime};

    /// main, plus refs/jail/work two commits ahead of it and refs/devvm/one a single commit ahead.
    fn repo(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).unwrap();
        build(dir, &["init", "-q", "-b", "main", "."]);
        let commit = |file: &str, text: &str, subject: &str| {
            std::fs::write(dir.join(file), text).unwrap();
            build(dir, &["add", "-A"]);
            build(dir, &["commit", "-q", "-m", subject]);
        };
        commit("a.txt", "one\n", "base");
        build(dir, &["checkout", "-q", "-b", "work"]);
        commit("a.txt", "one\ntwo\n", "first");
        commit("b.txt", "new\n", "second");
        build(dir, &["update-ref", "refs/jail/work", "HEAD"]);
        build(dir, &["checkout", "-q", "main"]);
        build(dir, &["checkout", "-q", "-b", "vm"]);
        commit("c.txt", "vm\n", "from the vm");
        build(dir, &["update-ref", "refs/devvm/one", "HEAD"]);
        build(dir, &["checkout", "-q", "main"]);
    }

    /// The app on a fresh copy of that repository, drawn once. Rows, in order:
    /// 0 "from the vm", 1 "All changes" on jail/work, 2 "first", 3 "second".
    fn app(name: &str) -> (Runtime<State>, std::path::PathBuf) {
        let dir = scratch(name);
        repo(&dir.join("repo"));
        let reviewed = Reviewed::at(Some(dir.join("state/refview/reviewed")));
        let patterns = DEFAULT_PATTERNS.iter().map(|&p| p.to_owned()).collect();
        let state = State::at(&dir.join("repo"), patterns, reviewed, true);
        let mut rt = Runtime::new(state, view, (1180.0, 760.0), Env { system_dark: true, scale: 1.0 });
        rt.on_key = Some(on_key);
        rt.frame(0.0);
        (rt, dir)
    }

    fn press(rt: &mut Runtime<State>, key: &str) {
        let (x, y) = rt.find(key).unwrap_or_else(|| panic!("no element keyed {key}"));
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(1.0 / 60.0);
    }

    fn type_key(rt: &mut Runtime<State>, t: &str) {
        rt.key(KeyEvent { key: Key::Text(t.to_owned()), mods: Mods::default() });
        rt.frame(1.0 / 60.0);
    }

    fn subject(s: &State) -> String {
        match s.selected() {
            Some((_, Some(c))) => c.subject.clone(),
            Some((r, None)) => format!("all of {}", r.short()),
            None => "nothing".to_owned(),
        }
    }

    fn paths(s: &State) -> Vec<String> {
        s.diff.files().iter().map(|f| f.path().to_owned()).collect()
    }

    #[test]
    fn it_opens_on_the_first_commit_with_its_diff() {
        let (rt, dir) = app("opens");
        let s = &rt.state;
        assert_eq!(s.error, None);
        assert_eq!(s.items.len(), 4);
        assert_eq!(subject(s), "from the vm");
        assert_eq!(paths(s), ["c.txt"]);
        assert!(!s.loading);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn n_and_p_walk_the_rows_and_each_shows_its_own_diff() {
        let (mut rt, dir) = app("walk");
        type_key(&mut rt, "n");
        assert_eq!(subject(&rt.state), "all of jail/work");
        assert_eq!(paths(&rt.state), ["a.txt", "b.txt"]);
        type_key(&mut rt, "n");
        assert_eq!(subject(&rt.state), "first");
        assert_eq!(paths(&rt.state), ["a.txt"]);
        type_key(&mut rt, "n");
        assert_eq!(subject(&rt.state), "second");
        assert_eq!(paths(&rt.state), ["b.txt"]);
        type_key(&mut rt, "n");
        assert_eq!(subject(&rt.state), "second", "n stops at the last row");
        type_key(&mut rt, "p");
        assert_eq!(subject(&rt.state), "first");
        type_key(&mut rt, "з");
        assert_eq!(subject(&rt.state), "all of jail/work", "p on a Russian layout");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_press_on_a_row_selects_it() {
        let (mut rt, dir) = app("press");
        press(&mut rt, "item-3");
        assert_eq!(subject(&rt.state), "second");
        assert_eq!(paths(&rt.state), ["b.txt"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_keys_still_work_after_a_click_on_the_button() {
        let (mut rt, dir) = app("focus");
        press(&mut rt, "item-2");
        let first = rt.state.selected().unwrap().1.unwrap().sha.clone();
        press(&mut rt, "mark");
        assert!(rt.state.reviewed.has(&first), "the button marks the commit");
        assert_eq!(rt.state.notice, None);
        type_key(&mut rt, "n");
        assert_eq!(subject(&rt.state), "second", "n after the click");
        type_key(&mut rt, "s");
        assert_eq!(rt.state.diff.mode(), DiffMode::Split, "s after the click");
        type_key(&mut rt, "r");
        let second = rt.state.selected().unwrap().1.unwrap().sha.clone();
        assert!(rt.state.reviewed.has(&second), "r after the click");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn r_on_all_changes_marks_and_unmarks_every_commit_of_the_ref() {
        let (mut rt, dir) = app("all");
        type_key(&mut rt, "n");
        type_key(&mut rt, "r");
        let saved = std::fs::read_to_string(dir.join("state/refview/reviewed")).unwrap();
        let work = &rt.state.refs[1];
        assert_eq!(work.short(), "jail/work");
        let mut want: Vec<&str> = work.commits.iter().map(|c| c.sha.as_str()).collect();
        want.sort();
        assert_eq!(saved.lines().collect::<Vec<_>>(), want);
        type_key(&mut rt, "r");
        assert_eq!(std::fs::read_to_string(dir.join("state/refview/reviewed")).unwrap().trim(), "");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_reload_keeps_the_selection_and_a_stale_press_finds_its_row_by_identity() {
        let (mut rt, dir) = app("reload");
        press(&mut rt, "item-3");
        let second = rt.state.row_id(rt.state.items[3]);
        // The vm ref goes away: every row below it moves up one.
        build(&dir.join("repo"), &["update-ref", "-d", "refs/devvm/one"]);
        type_key(&mut rt, "u");
        assert_eq!(rt.state.items.len(), 3);
        assert_eq!(subject(&rt.state), "second", "the selection follows the commit, not the index");
        assert_eq!(rt.state.sel, 2);
        // A press drawn before the reload names its row by what it showed.
        type_key(&mut rt, "p");
        rt.update(|s, tx| s.select_row(&second, tx));
        rt.frame(1.0 / 60.0);
        assert_eq!(subject(&rt.state), "second");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_save_leaves_the_diff_up_and_says_so_in_the_footer() {
        let (mut rt, dir) = app("save");
        // A directory where the list should be: saving fails.
        std::fs::create_dir_all(dir.join("state/refview/reviewed")).unwrap();
        type_key(&mut rt, "r");
        assert!(rt.state.notice.is_some());
        assert_eq!(rt.state.error, None);
        assert_eq!(paths(&rt.state), ["c.txt"], "the diff is still shown");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn shortcuts_follow_the_key_not_the_layout() {
        assert_eq!(shortcut("т"), "n");
        assert_eq!(shortcut("Ъ"), "]");
        assert_eq!(shortcut("n"), "n");
        assert_eq!(shortcut("q"), "q");
    }

    #[test]
    fn a_ref_with_one_commit_has_no_all_changes_row() {
        let refs = vec![
            RefInfo { name: "refs/jail/a".into(), commits: vec![commit("1"), commit("2")], ..RefInfo::default() },
            RefInfo { name: "refs/jail/b".into(), base: Some("base".into()), commits: vec![child("3", &["base"])], ..RefInfo::default() },
            RefInfo { name: "refs/jail/c".into(), ..RefInfo::default() },
            // A lone merge whose first parent isn't where the ref left HEAD: its own diff and
            // the ref's differ, so both rows are offered.
            RefInfo { name: "refs/jail/d".into(), base: Some("base".into()), commits: vec![child("4", &["side", "base"])], ..RefInfo::default() },
        ];
        assert_eq!(
            items(&refs),
            [
                Item { r: 0, pick: Pick::All },
                Item { r: 0, pick: Pick::Commit(0) },
                Item { r: 0, pick: Pick::Commit(1) },
                Item { r: 1, pick: Pick::Commit(0) },
                Item { r: 3, pick: Pick::All },
                Item { r: 3, pick: Pick::Commit(0) },
            ]
        );
    }
}
