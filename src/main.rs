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

use git::{Commit, RefInfo, Repo};
use reviewed::Reviewed;
use rune::{App, AppSpec, Cx, El, Key, KeyEvent, ScrollSpec, Tx, col, el, row, text};
use rune_kit::diff::{DiffMode, DiffState, DiffView, parse};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, SegmentedControl};

const DEFAULT_PATTERNS: &[&str] = &["refs/jail", "refs/devvm"];
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
                "--refs" => patterns.extend(args.next().cloned()),
                a if a.starts_with("--") => eprintln!("refview: ignoring {a}"),
                a => dir = Some(PathBuf::from(a)),
            }
        }
        if patterns.is_empty() {
            patterns = DEFAULT_PATTERNS.iter().map(|&p| p.to_owned()).collect();
        }
        Config { dir: dir.unwrap_or_else(|| PathBuf::from(".")), patterns }
    })
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
    refs: Vec<RefInfo>,
    /// The rows that can be selected, in sidebar order.
    items: Vec<Item>,
    sel: usize,
    diff: DiffState,
    reviewed: Reviewed,
    /// Why there is nothing to show, or what went wrong reading the selection.
    error: Option<String>,
}

impl State {
    fn open() -> State {
        let cfg = config();
        let mut s = State {
            repo: None,
            refs: Vec::new(),
            items: Vec::new(),
            sel: 0,
            diff: DiffState::new(Vec::new()),
            reviewed: Reviewed::load(),
            error: None,
        };
        match Repo::open(&cfg.dir) {
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
        let was = self.selected().map(|(r, c)| (r.name.clone(), c.map(|c| c.sha.clone())));
        self.error = None;
        match repo.refs(&config().patterns) {
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
        self.sel = was
            .and_then(|(name, sha)| {
                self.items.iter().position(|it| {
                    let r = &self.refs[it.r];
                    let c = match it.pick {
                        Pick::All => None,
                        Pick::Commit(c) => Some(&r.commits[c].sha),
                    };
                    r.name == name && c == sha.as_ref()
                })
            })
            .unwrap_or(0);
        self.load();
    }

    fn selected(&self) -> Option<(&RefInfo, Option<&Commit>)> {
        let it = self.items.get(self.sel)?;
        let r = &self.refs[it.r];
        Some((r, match it.pick {
            Pick::All => None,
            Pick::Commit(c) => Some(&r.commits[c]),
        }))
    }

    /// Reads the selection's patch into the diff, keeping the layout the reader chose.
    fn load(&mut self) {
        let (mode, wrap) = (self.diff.mode(), self.diff.wrap());
        let patch = match (&self.repo, self.selected()) {
            (Some(repo), Some((_, Some(c)))) => repo.commit_patch(&c.sha),
            (Some(repo), Some((r, None))) => repo.range_patch(r),
            _ => Ok(String::new()),
        };
        let files = match patch {
            Ok(p) => parse(&p),
            Err(e) => {
                self.error = Some(e);
                Vec::new()
            }
        };
        self.diff = DiffState::new(files);
        self.diff.set_mode(mode);
        self.diff.set_wrap(wrap);
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
        if let Err(e) = self.reviewed.set(&shas, on) {
            self.error = Some(format!("couldn't save the reviewed list: {e}"));
        }
    }

    fn toggle_split(&mut self, tx: &mut Tx) {
        tx.motion(rune::tok::motion::SNAPPY);
        let split = self.diff.mode() == DiffMode::Split;
        self.diff.set_mode(if split { DiffMode::Unified } else { DiffMode::Split });
    }
}

/// A ref with several commits gets an "All changes" row above them; with one, the commit is all.
fn items(refs: &[RefInfo]) -> Vec<Item> {
    let mut out = Vec::new();
    for (r, info) in refs.iter().enumerate() {
        if info.commits.len() > 1 {
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
    let main = col().key("main").grow(1.0).min_w(0.0).h_full().child(header(s, &k)).child(body(s, &k)).child(footer(&k));
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
        if info.commits.is_empty() {
            let note = text("Nothing HEAD doesn't already have").font(k.ty.text_small).color(k.c.text_faint);
            list = list.child(row().px(10.0).py(6.0).child(note));
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
        .on_press_tx(move |s: &mut State, tx: &mut Tx| s.select(i, tx))
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
        },
    );
    let reviewed = s.items.get(s.sel).is_some_and(|&it| s.is_reviewed(it));
    let mark = Button::new(if reviewed { "Reviewed" } else { "Mark reviewed" })
        .variant(if reviewed { ButtonVariant::Ghost } else { ButtonVariant::Neutral })
        .control_size(ControlSize::Regular)
        .on_press(|s: &mut State| s.toggle_reviewed())
        .build(k)
        .key("mark");
    let titles = col()
        .grow(1.0)
        .min_w(0.0)
        .gap(2.0)
        .child(text(title).key("title").font(k.ty.ui_medium).color(k.c.text_base).truncate())
        .child(text(meta).key("meta").font(k.ty.text_small).color(k.c.text_muted).truncate());
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
    if s.items.is_empty() {
        let patterns = config().patterns.join(", ");
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

fn footer(k: &Kit) -> El {
    row()
        .key("keys")
        .w_full()
        .h(28.0)
        .px(16.0)
        .items_center()
        .fill(k.c.layer_01)
        .child(text(KEYS).font(k.ty.text_xsmall).color(k.c.text_faint).truncate())
}

fn on_key(s: &mut State, ev: &KeyEvent, tx: &mut Tx) -> bool {
    let m = ev.mods;
    if let Key::Text(t) = &ev.key
        && tx.focused().is_none()
        && !(m.cmd || m.ctrl || m.alt)
    {
        match t.as_str() {
            "n" => s.step(true, tx),
            "p" => s.step(false, tx),
            "s" => s.toggle_split(tx),
            "w" => s.diff.set_wrap(!s.diff.wrap()),
            "r" => s.toggle_reviewed(),
            "u" => s.reload(),
            _ => return s.diff.key(ev, tx),
        }
        return true;
    }
    s.diff.key(ev, tx)
}

fn main() {
    // Read the arguments before the window: `--help` and a malformed flag exit here.
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

    #[test]
    fn a_ref_with_one_commit_has_no_all_changes_row() {
        let refs = vec![
            RefInfo { name: "refs/jail/a".into(), base: None, commits: vec![commit("1"), commit("2")] },
            RefInfo { name: "refs/jail/b".into(), base: None, commits: vec![commit("3")] },
            RefInfo { name: "refs/jail/c".into(), base: None, commits: vec![] },
        ];
        assert_eq!(
            items(&refs),
            [
                Item { r: 0, pick: Pick::All },
                Item { r: 0, pick: Pick::Commit(0) },
                Item { r: 0, pick: Pick::Commit(1) },
                Item { r: 1, pick: Pick::Commit(0) },
            ]
        );
    }
}
