//! What the window draws round the diff. The diff is the only text here meant to be read;
//! the rest is glanced: a shape, a place, a colour, a count.
//!
//! Colour is rationed by how much a thing matters. Green and red washes are added and removed
//! lines. Amber is a note from the model. A red fill is an alarm, and nothing else is filled
//! with a colour. Everything the window itself says is in ink.

use std::sync::OnceLock;

use rune::{Color, Cursor, Drag, El, Family, Icon, ScrollSpec, TextStyle, Tx, col, el, icon, row, text};
use rune_kit::Kit;
use rune_kit::diff::Side;

use crate::diff::{Annotation, DiffMode, DiffView, LineRef, MapKind};
use crate::{Phase, Pick, State, Way, plural};

pub struct Pal {
    pub bg: Color,
    pub side: Color,
    pub band: Color,
    pub raise: Color,
    pub rule: Color,
    /// A changed line, and what the window most wants read.
    pub ink: Color,
    /// An unchanged line of code.
    pub ctx: Color,
    pub ink2: Color,
    pub ink3: Color,
    pub add: Color,
    pub add_bg: Color,
    pub del: Color,
    pub del_bg: Color,
    pub note: Color,
    pub note_ink: Color,
    pub alarm: Color,
    pub alarm_ink: Color,
}

/// The window's colours. One dark set: the design has no light one yet.
pub fn pal() -> &'static Pal {
    static P: OnceLock<Pal> = OnceLock::new();
    P.get_or_init(|| {
        let c = |s: &str| rune::color::hex(s).unwrap_or(Color { r: 1.0, g: 0.0, b: 1.0, a: 1.0 });
        Pal {
            bg: c("#0e0f11"),
            side: c("#121316"),
            band: c("#1a1c20"),
            raise: c("#23262c"),
            rule: c("#25282e"),
            ink: c("#f1f1f3"),
            ctx: c("#c3c5cc"),
            ink2: c("#a3a6ae"),
            ink3: c("#7e818a"),
            add: c("#3fb26c"),
            add_bg: c("#0f1e17"),
            del: c("#e2625f"),
            del_bg: c("#241518"),
            note: c("#f0a83a"),
            note_ink: c("#ecd2a2"),
            alarm: c("#ff5b4f"),
            alarm_ink: c("#1a0706"),
        }
    })
}

macro_rules! glyph {
    ($name:ident, $id:literal, $body:literal) => {
        static $name: Icon = Icon { name: $id, view: 16.0, body: $body };
    };
}
glyph!(BOT, "parole-bot", r#"<rect x="2.5" y="5" width="11" height="8.5" rx="2" stroke="currentColor" stroke-width="1.5" fill="none"/><path d="M8 5V2.5M6 8.5v1.5M10 8.5v1.5" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" fill="none"/>"#);
glyph!(TERM, "parole-term", r#"<rect x="1.5" y="2.5" width="13" height="11" rx="2" stroke="currentColor" stroke-width="1.5" fill="none"/><path d="M4.5 6.5l2 1.5-2 1.5M8.5 10h3" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
glyph!(KEY, "parole-key", r#"<rect x="4.5" y="1.5" width="7" height="13" rx="2" stroke="currentColor" stroke-width="1.5" fill="none"/><circle cx="8" cy="6" r="1.8" stroke="currentColor" stroke-width="1.5" fill="none"/>"#);
glyph!(ARROW, "parole-arrow", r#"<path d="M2.5 8h11M9.5 4l4 4-4 4" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
glyph!(UNIFIED, "parole-unified", r#"<rect x="2.5" y="2.5" width="11" height="11" rx="1.5" stroke="currentColor" stroke-width="1.5" fill="none"/><path d="M5 6h6M5 8.5h6M5 11h4" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" fill="none"/>"#);
glyph!(SPLIT, "parole-split", r#"<rect x="2.5" y="2.5" width="11" height="11" rx="1.5" stroke="currentColor" stroke-width="1.5" fill="none"/><path d="M8 2.5v11" stroke="currentColor" stroke-width="1.5" fill="none"/>"#);
glyph!(WARN, "parole-warn", r#"<path d="M8 2l6.5 11.5h-13zM8 6.5v3.5M8 12v.2" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);

/// The faces the window uses, sized once.
struct Ty {
    ui: TextStyle,
    strong: TextStyle,
    small: TextStyle,
    title: TextStyle,
    mono: TextStyle,
    /// The model's hand: a serif italic, so its words are never taken for the window's or the code's.
    hand: TextStyle,
    alarm: TextStyle,
}

const SERIF: Family = Family::Named("Source Serif 4, Source Serif Pro, Noto Serif, DejaVu Serif, Liberation Serif");

impl Ty {
    fn new(k: &Kit) -> Ty {
        Ty {
            ui: TextStyle { size: 12.5, line_height: 18.0, ..k.ty.ui },
            strong: TextStyle { size: 12.5, line_height: 18.0, ..k.ty.ui_medium },
            small: TextStyle { size: 11.5, line_height: 16.0, ..k.ty.ui },
            title: TextStyle { size: 17.0, line_height: 22.0, ..k.ty.ui_medium },
            mono: TextStyle { size: 11.5, line_height: 16.0, ..k.ty.mono },
            hand: TextStyle { size: 14.0, line_height: 20.0, family: SERIF, italic: true, ..k.ty.ui },
            alarm: TextStyle { size: 14.5, line_height: 20.0, family: SERIF, weight: 600.0, ..k.ty.ui },
        }
    }
}

pub fn view(s: &State) -> El {
    let k = Kit::neutral(true);
    let (p, t) = (pal(), Ty::new(&k));
    let main = col().key("main").grow(1.0).min_w(0.0).h_full().child(header(s, &t)).child(body(s, &k, &t)).child(decide(s, &t));
    let rule = col().w(1.0).h_full().fill(p.rule);
    let mut root = row().w_full().h_full().fill(p.bg).child(sidebar(s, &t)).child(rule).child(main);
    if s.keys_open {
        root = root.child(keys_card(&t));
    }
    root
}

/// A letter on a key: only ever inside the button it presses, or on the keys card.
fn keycap(letter: &str, ink: Color, t: &Ty) -> El {
    row().px(5.0).h(18.0).items_center().radius(4.0).border(1.0, ink.alpha(0.45)).child(text(letter.to_owned()).font(t.mono).color(ink.alpha(0.8)))
}

#[derive(Clone, Copy, PartialEq)]
enum Style {
    Quiet,
    Plain,
    /// The one filled button: the thing to do next.
    Go,
    /// Filled red: only Drop, only when the model raised an alarm.
    Fire,
    /// Take, not yet earned: filled as far as the change has been read.
    Part(f32),
}

fn button(key: &str, label: &str, cap: &str, style: Style, t: &Ty) -> El {
    let p = pal();
    let (fill, line, ink) = match style {
        Style::Quiet => (None, p.rule, p.ink2),
        Style::Plain => (None, p.ink3, p.ink),
        Style::Go => (Some(p.ink), p.ink, p.bg),
        Style::Fire => (Some(p.alarm), p.alarm, p.alarm_ink),
        Style::Part(_) => (None, p.rule, p.ink3),
    };
    let mut b = row().key(key.to_owned()).h(34.0).radius(7.0).border(1.0, line).role("button").label(label.to_owned()).cursor(Cursor::Pointer);
    if let Some(fill) = fill {
        b = b.fill(fill);
    }
    if let Style::Part(done) = style {
        b = b.clip_rounded(7.0).child(el().key("fill").pin(Some(0.0), Some(0.0), None, Some(0.0)).w_frac(done.clamp(0.0, 1.0)).fill(p.raise));
    }
    let face = if style == Style::Quiet { t.ui } else { t.strong };
    b.child(row().h_full().px(12.0).gap(10.0).items_center().child(text(label.to_owned()).font(face).color(ink)).child(keycap(cap, ink, t)))
}

/// A branch as git draws one, a pill: dashed while it doesn't exist, solid once it does,
/// filled when the checkout is on it.
#[derive(Clone, Copy, PartialEq)]
enum Pill {
    Ghost,
    Made,
    On,
}

fn pill(name: &str, kind: Pill, t: &Ty) -> El {
    let p = pal();
    let b = row().h(20.0).px(8.0).items_center().radius(10.0).min_w(0.0).shrink();
    let (b, ink) = match kind {
        Pill::Ghost => (b.border(1.0, p.ink3).border_dash(3.0, 3.0), p.ink3),
        Pill::Made => (b.border(1.0, p.ink2), p.ink),
        Pill::On => (b.fill(p.ink), p.bg),
    };
    b.child(text(name.to_owned()).font(t.mono).color(ink).truncate_head())
}

fn pill_of(s: &State, r: &crate::git::RefInfo) -> (String, Pill) {
    match s.branches.get(&r.name) {
        Some(branch) if s.head.as_deref() == Some(branch) => (branch.clone(), Pill::On),
        Some(branch) => (branch.clone(), Pill::Made),
        None => (crate::take::branch_for(r), Pill::Ghost),
    }
}

/// A commit's stop on its ref's track.
#[derive(Clone, Copy, PartialEq)]
enum Dot {
    Unseen,
    /// Opened, not read to the end.
    Half,
    Seen,
    /// Seen, and its signed copy exists: the same dot with a ring, a seal.
    Signed,
}

fn dot(kind: Dot, on: Color) -> El {
    let p = pal();
    let d = el().size(10.0, 10.0).radius(5.0);
    let d = match kind {
        Dot::Unseen => d.border(1.5, p.ink3).fill(on),
        Dot::Half => d.border(1.5, p.ink2).fill(on).clip_rounded(5.0).child(el().pin(Some(0.0), Some(0.0), None, Some(0.0)).w(5.0).fill(p.ink2)),
        Dot::Seen | Dot::Signed => d.fill(p.ink2),
    };
    let ring = row().size(16.0, 16.0).center().radius(8.0);
    if kind == Dot::Signed { ring.border(1.5, p.ink2).child(d) } else { ring.child(d) }
}

/// The exception marks: a dot for notes, a square for an alarm.
fn level_mark(notes: usize, alarms: usize) -> Option<El> {
    let p = pal();
    match (alarms, notes) {
        (0, 0) => None,
        (0, _) => Some(el().size(6.0, 6.0).radius(3.0).fill(p.note).label("has notes")),
        _ => Some(el().size(9.0, 9.0).radius(2.0).fill(p.alarm).label("has an alarm")),
    }
}

fn sidebar(s: &State, t: &Ty) -> El {
    let p = pal();
    let title = match (&s.repo, &s.snapshot) {
        (_, Some(_)) => "a folder's jail snapshot".to_owned(),
        (Some(repo), _) => repo.name(),
        _ => "parole".to_owned(),
    };
    let mut list = col().w_full().px(8.0).pb(8.0);
    let mut i = 0;
    for (r, info) in s.refs.iter().enumerate() {
        let whole = s.items.get(i).filter(|it| it.r == r && it.pick == Pick::All).map(|_| i);
        let selected = whole == Some(s.sel);
        let mut head = row()
            .w_full()
            .h(28.0)
            .px(8.0)
            .items_center()
            .radius(6.0)
            .fill(if selected { p.raise } else { p.side })
            .role("option")
            .label(format!("All changes on {}", info.short()))
            .child(text(info.short().to_owned()).font(t.strong).color(if selected { p.ink } else { p.ink2 }).truncate_head().grow(1.0).min_w(0.0));
        let (notes, alarms) = s.ref_level(info);
        if let Some(mark) = level_mark(notes, alarms) {
            head = head.child(mark);
        }
        // The whole change is a thing to read like a commit, so it has a commit's dot.
        if whole.is_some() {
            let kind = if s.took_one.contains(&info.name) {
                Dot::Signed
            } else if s.reviewed.has(&crate::take::whole_mark(info)) {
                Dot::Seen
            } else {
                Dot::Unseen
            };
            head = head.child(dot(kind, if selected { p.raise } else { p.side }));
        }
        // The ref's own row shows the whole change; with one commit that is the commit.
        let to = whole.or_else(|| s.items.iter().position(|it| it.r == r));
        if let Some(to) = to {
            let id = s.row_id(s.items[to]);
            head = head.key(if whole.is_some() { crate::item_key(to) } else { format!("ref-{r}") }).cursor(Cursor::Pointer).on_press_tx(move |s: &mut State, tx: &mut Tx| s.select_row(&id, tx));
        }
        let mut track = col().w_full().pb(14.0).child(head);
        let said = |words: String| row().w_full().px(8.0).py(4.0).child(text(words).font(t.small).color(p.ink3).wrap());
        if let Some(e) = &info.error {
            track = track.child(said(format!("Couldn't read this ref: {e}")));
        } else if info.commits.is_empty() {
            track = track.child(said("Nothing HEAD doesn't already have".to_owned()));
        } else if info.truncated {
            track = track.child(said(format!("Older commits not listed: showing the newest {}", crate::git::MAX_COMMITS)));
        }
        if whole.is_some() {
            i += 1;
            if selected {
                track = track.child(files(s, t));
            }
        }
        let mut first = true;
        while let Some(&it) = s.items.get(i).filter(|it| it.r == r) {
            track = track.child(stop(s, i, it, first, t));
            if i == s.sel {
                track = track.child(files(s, t));
            }
            first = false;
            i += 1;
        }
        if !info.commits.is_empty() && s.snapshot.is_none() {
            let (name, kind) = pill_of(s, info);
            let line = col().pin(Some(15.5), Some(0.0), None, None).w(1.0).h(8.0).fill(p.rule);
            track = track.child(row().w_full().pt(6.0).px(8.0).child(line).child(pill(&name, kind, t)));
        }
        list = list.child(track);
    }
    // What was dropped this run: struck out, and one press from being back.
    for d in &s.dropped {
        let name = d.short().to_owned();
        let (full, tip) = (d.name.clone(), d.tip.clone());
        let label = text(name.clone()).font(t.strong).color(p.ink3).strike(0, name.len(), Some(p.ink3)).truncate_head().grow(1.0).min_w(0.0);
        let back = row()
            .key(format!("undrop-{name}"))
            .h(22.0)
            .px(8.0)
            .items_center()
            .radius(5.0)
            .border(1.0, p.rule)
            .role("button")
            .label(format!("Put {name} back"))
            .cursor(Cursor::Pointer)
            .child(text("Undo".to_owned()).font(t.small).color(p.ink2))
            .on_press_tx(move |s: &mut State, tx: &mut Tx| {
                tx.instant();
                s.restore(&full, &tip);
            });
        list = list.child(row().w_full().h(28.0).px(8.0).mb(6.0).gap(8.0).items_center().child(label).child(back));
    }
    let scroll = col().key("refs").w_full().grow(1.0).min_h(0.0).scroll(ScrollSpec { stick_bottom: false }).child(list);
    let help = row()
        .key("keys")
        .size(24.0, 24.0)
        .center()
        .radius(12.0)
        .border(1.0, if s.keys_open { p.ink2 } else { p.rule })
        .role("button")
        .label("Keys")
        .cursor(Cursor::Pointer)
        .child(text("?".to_owned()).font(t.strong).color(p.ink2))
        .on_press_tx(|s: &mut State, tx: &mut Tx| {
            tx.instant();
            s.keys_open = !s.keys_open;
        });
    let foot = row().w_full().h(56.0).px(16.0).items_center().child(help);
    col()
        .key("sidebar")
        .w(248.0)
        .h_full()
        .fill(p.side)
        .child(row().w_full().px(16.0).pt(12.0).pb(8.0).child(text(title).font(t.strong).color(p.ink3).truncate()))
        .child(scroll)
        .child(col().w_full().h(1.0).fill(p.rule))
        .child(foot)
}

fn stop(s: &State, i: usize, it: crate::Item, first: bool, t: &Ty) -> El {
    let p = pal();
    let info = &s.refs[it.r];
    let Pick::Commit(c) = it.pick else { return row() };
    let c = &info.commits[c];
    let selected = i == s.sel;
    let on = if selected { p.raise } else { p.side };
    // Taken as one commit, the ref's own commits have no copies: the seal is on the ref's row.
    let each = !s.took_one.contains(&info.name);
    let signed = each && (s.branches.contains_key(&info.name) || s.take.as_ref().is_some_and(|t| t.way == Way::Each && t.r.name == info.name && t.signed.contains_key(&c.sha)));
    let kind = if signed {
        Dot::Signed
    } else if s.reviewed.has(&c.sha) {
        Dot::Seen
    } else if s.partial.contains(&c.sha) {
        Dot::Half
    } else {
        Dot::Unseen
    };
    let id = s.row_id(it);
    let line = col().pin(Some(15.5), Some(if first { 12.0 } else { 0.0 }), None, Some(0.0)).w(1.0).fill(p.rule);
    let mut marks = row().gap(5.0).h(18.0).items_center();
    if s.marks.get(&c.sha).copied().unwrap_or(0) > 0 {
        marks = marks.child(icon(&TERM, 13.0).color(p.ink2).label("changes what your machine runs"));
    }
    let (notes, alarms) = s.levels.get(&c.sha).copied().unwrap_or((0, 0));
    if let Some(mark) = level_mark(notes, alarms) {
        marks = marks.child(mark);
    }
    row()
        .key(crate::item_key(i))
        .role("option")
        .label(c.subject.clone())
        .checked(s.reviewed.has(&c.sha))
        .w_full()
        .px(8.0)
        .py(4.0)
        .gap(6.0)
        .items_start()
        .radius(6.0)
        .fill(on)
        .cursor(Cursor::Pointer)
        .on_press_tx(move |s: &mut State, tx: &mut Tx| s.select_row(&id, tx))
        .child(line)
        .child(row().h(18.0).items_center().child(dot(kind, on)))
        .child(text(c.subject.clone()).font(t.ui).color(if selected { p.ink } else { p.ink2 }).wrap().max_lines(3).grow(1.0).min_w(0.0))
        .child(marks)
}

/// The files of the diff on screen, under the row that shows it: the outline of what is open.
fn files(s: &State, t: &Ty) -> El {
    let p = pal();
    let levels = s.file_levels();
    let mut out = col().key("files").w_full().pl(28.0).pt(1.0).pb(3.0).child(col().pin(Some(15.5), Some(0.0), None, Some(0.0)).w(1.0).fill(p.rule));
    if s.loading {
        return out;
    }
    for (f, file) in s.diff.files().iter().enumerate() {
        let runs = s.flags.get(f).copied().flatten().is_some();
        let here = f == s.visible_file;
        let mut r = row()
            .key(format!("file-{f}"))
            .w_full()
            .h(22.0)
            .px(6.0)
            .gap(6.0)
            .items_center()
            .radius(5.0)
            .fill(if here { p.raise } else { p.side })
            .cursor(Cursor::Pointer)
            .on_press_tx(move |s: &mut State, tx: &mut Tx| {
                tx.instant();
                s.diff.scroll_to_file(f, tx);
            });
        if runs {
            r = r.child(icon(&TERM, 12.0).color(p.ink));
        }
        r = r.child(text(file.path().to_owned()).font(t.mono).color(if here || runs { p.ink } else { p.ink3 }).truncate_head().grow(1.0).min_w(0.0));
        let (notes, alarms) = levels.get(f).copied().unwrap_or((0, 0));
        if let Some(mark) = level_mark(notes, alarms) {
            r = r.child(mark);
        }
        out = out.child(r);
    }
    out
}

/// The header holds the commit's own words and nothing else.
fn header(s: &State, t: &Ty) -> El {
    let p = pal();
    let Some((info, commit)) = s.selected() else {
        return row().key("bar").w_full().h(56.0);
    };
    let said = |words: String| text(words).font(t.small).color(p.ink3);
    let mono = |words: String| text(words).font(t.mono).color(p.ink3);
    let (title, message, mut meta) = match commit {
        None if s.snapshot.is_some() => (info.name.clone(), "", vec![said("what jail diff shows".to_owned())]),
        Some(c) => (c.subject.clone(), c.body.as_str(), vec![mono(c.short.clone()), said(c.author.clone()), said(c.date.clone())]),
        None => {
            let base = match &info.base {
                Some(b) => mono(b[..b.len().min(9)].to_owned()),
                None => said("no history shared with HEAD".to_owned()),
            };
            (info.short().to_owned(), "", vec![said(plural(info.commits.len(), "commit")), said("on".to_owned()), base])
        }
    };
    // No counts while the patch is still on its way: "+0" would be a claim.
    if !s.loading {
        let (adds, dels) = s.diff.files().iter().fold((0, 0), |(a, d), f| (a + f.additions(), d + f.deletions()));
        if adds > 0 {
            meta.push(text(format!("+{adds}")).key("adds").font(t.mono).color(p.add));
        }
        if dels > 0 {
            meta.push(text(format!("\u{2212}{dels}")).key("dels").font(t.mono).color(p.del));
        }
    }
    let mut titles = col().grow(1.0).min_w(0.0).gap(4.0).child(text(title).key("title").font(t.title).color(p.ink).truncate());
    if !message.is_empty() {
        // The message was written in the sandbox too: shown as text to read, nothing more.
        titles = titles.child(text(message.to_owned()).key("message").font(t.ui).color(p.ink2).wrap().max_lines(6).max_w(680.0).selectable(true));
    }
    titles = titles.child(row().key("meta").gap(12.0).items_center().children(meta));
    let split = s.diff.mode() == DiffMode::Split;
    let side = |key: &'static str, glyph: &'static Icon, label: &'static str, on: bool, to_split: bool| {
        row()
            .key(key)
            .size(30.0, 26.0)
            .center()
            .fill(if on { p.raise } else { p.bg })
            .role("button")
            .label(label)
            .cursor(Cursor::Pointer)
            .child(icon(glyph, 14.0).color(if on { p.ink } else { p.ink3 }))
            .on_press_tx(move |s: &mut State, tx: &mut Tx| {
                if to_split != (s.diff.mode() == DiffMode::Split) {
                    s.toggle_split(tx);
                }
                tx.blur();
            })
    };
    let modes = row().radius(6.0).border(1.0, p.rule).clip_rounded(6.0).child(side("unified", &UNIFIED, "One column", !split, false)).child(side("split", &SPLIT, "Two columns", split, true));
    col()
        .key("bar")
        .w_full()
        .child(row().w_full().pl(20.0).pr(16.0).pt(14.0).pb(12.0).gap(24.0).items_start().child(titles).child(modes))
        .child(col().w_full().h(1.0).fill(p.rule))
}

fn body(s: &State, k: &Kit, t: &Ty) -> El {
    let p = pal();
    let note = |title: &str, detail: String| {
        col()
            .key("empty")
            .grow(1.0)
            .w_full()
            .center()
            .gap(6.0)
            .padding(32.0)
            .child(text(title.to_owned()).font(t.strong).color(p.ink))
            .child(text(detail).font(t.ui).color(p.ink2).wrap())
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
        // Built, so about to be drawn: an empty diff is read by this page.
        s.empty_drawn.set(!s.loading && s.error.is_none());
        return note("No changes", "Nothing here changes a file.".to_owned());
    }
    let (strong, hand, alarm) = (t.strong, t.hand, t.alarm);
    let diff = DiffView::new(|s: &State| &s.diff, |s: &mut State| &mut s.diff)
        // Why the host runs a file, after its path: a fact, so in ink and in words, once.
        .header_actions(move |s: &State, file| match s.flags.get(file).copied().flatten() {
            Some(why) => row().gap(7.0).items_center().pl(6.0).child(icon(&TERM, 14.0).color(p.ink)).child(text(why.to_owned()).font(strong).color(p.ink).truncate()),
            None => row(),
        })
        .heavy(|s: &State, file| s.flags.get(file).copied().flatten().is_some())
        // A note sits on its own line, in the space the code leaves, so its arrival moves nothing.
        .inline(move |s: &State, a: &Annotation| {
            let f = s.finding(&a.id)?;
            if f.alarm {
                return None;
            }
            let open = s.open_note.as_deref() == Some(a.id.as_str());
            let words = text(f.why.clone()).font(hand).color(p.note_ink).min_w(0.0).shrink();
            let id = a.id.clone();
            Some(
                row()
                    .grow(1.0)
                    .min_w(170.0)
                    .gap(7.0)
                    .pr(12.0)
                    .items_start()
                    .role("note")
                    .label(format!("Note: {}", f.why))
                    .cursor(Cursor::Pointer)
                    .on_press_tx(move |s: &mut State, tx: &mut Tx| {
                        tx.instant();
                        s.open_note = if s.open_note.as_deref() == Some(id.as_str()) { None } else { Some(id.clone()) };
                    })
                    // A leader from the code to its note: one line, one thought.
                    .child(row().grow(1.0).min_w(14.0).h(20.0).ml(12.0).items_center().child(el().w_full().h(1.0).fill(p.note.alpha(0.4))))
                    .child(row().h(20.0).items_center().child(icon(&BOT, 14.0).color(p.note)))
                    .child(if open { words.wrap() } else { words.truncate() }),
            )
        })
        // An alarm goes under its line, filled: the one thing allowed to push the page down.
        // In two columns a note has no room on its line, so it goes under too, quietly.
        .annotation(move |s: &State, a: &Annotation| {
            let Some(f) = s.finding(&a.id) else { return row() };
            if f.alarm {
                row()
                    .w_full()
                    .px(16.0)
                    .py(8.0)
                    .gap(12.0)
                    .items_start()
                    .fill(p.alarm)
                    .role("alert")
                    .label(format!("Alarm: {}", f.why))
                    .child(row().h(20.0).items_center().child(icon(&WARN, 16.0).color(p.alarm_ink)))
                    .child(text(f.why.clone()).font(alarm).color(p.alarm_ink).wrap().grow(1.0).min_w(0.0))
            } else {
                row()
                    .w_full()
                    .px(16.0)
                    .py(4.0)
                    .gap(8.0)
                    .items_start()
                    .child(row().h(20.0).items_center().child(icon(&BOT, 14.0).color(p.note)))
                    .child(text(f.why.clone()).font(hand).color(p.note_ink).wrap().grow(1.0).min_w(0.0))
            }
        })
        .on_seen(|s: &mut State| s.seen_more())
        .on_visible_file(|s: &mut State, f| s.visible_file = f)
        .build(s, k);
    // The diff takes what the map leaves, however wide its lines are.
    let diff = col().grow(1.0).min_w(0.0).h_full().child(diff);
    row().key("review").grow(1.0).min_h(0.0).w_full().child(diff).child(col().w(1.0).h_full().fill(p.rule)).child(map(s))
}

/// How tall one line is on the map, and the gap under it.
const MAP_LINE: f32 = 2.0;
const MAP_GAP: f32 = 1.0;

/// The diff from top to bottom, one stroke a line and as long as the line: dim until that
/// line has been on screen, with the notes and alarms beside theirs. It is the scrollbar.
fn map(s: &State) -> El {
    let p = pal();
    let (rows, visible) = s.diff.map();
    let tall = rune::memo::layout("map").map_or(560.0, |r| r[3]) - 16.0;
    // More lines than fit: each stroke stands for `per` of them.
    let step = MAP_LINE + MAP_GAP;
    let per = ((rows.len() as f32 * step) / tall.max(step)).ceil().max(1.0) as usize;
    let marks = s.line_levels();
    let mut strokes = col().w_full().gap(MAP_GAP);
    for chunk in rows.chunks(per) {
        let changed = |k: MapKind| chunk.iter().any(|r| r.kind == k);
        let (adds, dels) = (changed(MapKind::Added) || changed(MapKind::Both), changed(MapKind::Removed) || changed(MapKind::Both));
        let seen = chunk.iter().all(|r| r.seen);
        let cols = chunk.iter().map(|r| r.cols).max().unwrap_or(0);
        let ink = match (adds, dels, seen) {
            (true, _, true) => p.add,
            (false, true, true) => p.del,
            (true, _, false) => p.add.mix(p.bg, 0.6),
            (false, true, false) => p.del.mix(p.bg, 0.6),
            (false, false, _) if chunk.iter().all(|r| matches!(r.kind, MapKind::File | MapKind::Fold)) => p.bg,
            (false, false, _) => p.ink3.mix(p.bg, 0.45),
        };
        let level = chunk.iter().filter_map(|r| marks.get(&(r.file, r.line?.line)).copied()).max();
        let mut line = row().w_full().h(MAP_LINE).child(el().w((cols.clamp(4, 84) as f32 / 84.0 * 30.0).ceil()).h(MAP_LINE).radius(1.0).fill(ink));
        if let Some(alarm) = level {
            let (w, h, c) = if alarm { (9.0, 8.0, p.alarm) } else { (5.0, 6.0, p.note) };
            line = line.child(el().pin(Some(40.0 - w), Some(-(h - MAP_LINE) / 2.0), None, None).size(w, h).radius(1.5).fill(c));
        }
        strokes = strokes.child(line);
    }
    let at = |row: usize| 8.0 + (row / per) as f32 * step;
    let window = el().key("window").pin(Some(3.0), Some(at(visible.start) - 3.0), Some(3.0), None).h((at(visible.end) - at(visible.start) + 4.0).max(8.0)).radius(4.0).border(1.0, p.ink3);
    col()
        .key("map")
        .w(52.0)
        .h_full()
        .pt(8.0)
        .pl(8.0)
        .a11y_hidden()
        .cursor(Cursor::Pointer)
        .on_drag(move |s: &mut State, d: &mut Drag, tx: &mut Tx| {
            tx.instant();
            let (rows, _) = s.diff.map();
            let i = (((d.y - 8.0) / step).max(0.0) as usize * per).min(rows.len().saturating_sub(1));
            // The nearest row that is a line: a header or a fold has no line to land on.
            let Some(r) = rows.get(i) else { return };
            match rows[i..].iter().find(|r| r.line.is_some()).filter(|l| l.file == r.file) {
                Some(l) => s.diff.scroll_to_line(l.file, l.line.unwrap_or(LineRef::new(Side::New, 1)), tx),
                None => s.diff.scroll_to_file(r.file, tx),
            }
        })
        .child(strokes)
        .child(window)
}

/// The one thing in the window that moves on its own: the bead of the signature being waited
/// for. Every frame of it is the same bead, brighter or dimmer.
const WAITING: rune::Cycle = rune::Cycle { period: 1.2, phase: 0.0, stops: &[(0.0, 1.0), (0.5, 0.25), (1.0, 1.0)], still: 1.0, fps: 30.0 };

/// What the model has said about the diff on screen, as a mark and a count. Pressing it goes
/// to its next note.
fn reader(s: &State, t: &Ty) -> El {
    let p = pal();
    let asked = s.report();
    let count = |n: String, ink: Color| text(n).font(TextStyle { weight: 600.0, ..t.mono }).color(ink);
    let mut b = row().key("reader").h(34.0).px(8.0).gap(7.0).items_center().radius(7.0).role("button").cursor(Cursor::Pointer).on_press_tx(|s: &mut State, tx: &mut Tx| {
        tx.instant();
        s.next_note(tx);
    });
    let waiting = asked.is_some_and(|a| a.waiting);
    let failed = asked.is_some_and(|a| a.failed.is_some());
    let (label, ink) = match asked.and_then(|a| a.report.as_ref()) {
        _ if failed => ("The model didn't answer".to_owned(), p.ink),
        None if waiting => ("The model is still reading".to_owned(), p.ink3),
        None => ("The model hasn't read this".to_owned(), p.ink3),
        Some(r) => (format!("{} from the model", plural(r.findings.len(), "note")), p.ink2),
    };
    b = b.label(label).child(icon(&BOT, 15.0).color(ink));
    match asked.and_then(|a| a.report.as_ref()) {
        _ if failed => b = b.child(count("!".to_owned(), p.ink)),
        None if waiting => b = b.child(count("\u{00b7}\u{00b7}\u{00b7}".to_owned(), p.ink3)),
        None => b = b.child(count("\u{2013}".to_owned(), p.ink3)),
        Some(r) => {
            let alarms = r.findings.iter().filter(|f| f.alarm).count();
            let notes = r.findings.len() - alarms;
            if alarms > 0 {
                b = b.child(row().px(6.0).h(18.0).items_center().radius(4.0).fill(p.alarm).child(count(alarms.to_string(), p.alarm_ink)));
            }
            if notes > 0 || alarms == 0 {
                b = b.child(count(notes.to_string(), if notes > 0 { p.note } else { p.ink2 }));
            }
            // It had more to say than was kept, or its reply was cut short.
            if r.dropped > 0 || !r.complete {
                b = b.child(count("+".to_owned(), p.ink));
            }
            if waiting {
                b = b.child(count("\u{00b7}\u{00b7}\u{00b7}".to_owned(), p.ink3));
            }
        }
    }
    b
}

/// The decision, about the ref: one strip, always there. Drop sits left of Take and they never
/// swap; only which of them is filled changes.
fn decide(s: &State, t: &Ty) -> El {
    let p = pal();
    let strip = |left: El, right: El| {
        col()
            .key("decide")
            .w_full()
            .child(col().w_full().h(1.0).fill(p.rule))
            .child(row().w_full().h(56.0).pl(20.0).pr(12.0).gap(12.0).items_center().fill(p.side).child(left.grow(1.0).min_w(0.0)).child(right))
    };
    let said = |words: &str, ink: Color| row().items_center().min_w(0.0).child(text(words.to_owned()).key("notice").font(t.ui).color(ink).truncate());
    let Some((info, _)) = s.selected() else {
        return strip(s.notice.as_deref().map_or(row(), |n| said(n, p.ink)), row());
    };
    if s.snapshot.is_some() {
        return strip(said(s.notice.as_deref().unwrap_or("A folder snapshot: nothing to take. Apply it with jail apply."), p.ink2), row().child(reader(s, t)));
    }
    let on = |f: fn(&mut State, &mut Tx)| move |s: &mut State, tx: &mut Tx| {
        tx.instant();
        f(s, tx);
        tx.blur();
    };
    let (name, kind) = pill_of(s, info);
    let what = || row().gap(10.0).items_center().min_w(0.0).child(text(info.short().to_owned()).font(t.mono).color(p.ink2).truncate_head().shrink().min_w(0.0)).child(icon(&ARROW, 14.0).color(p.ink3)).child(pill(&name, kind, t));
    // Something went wrong, or was refused: said where the eye already is for this ref.
    let left = match (&s.notice, s.report().and_then(|a| a.failed.as_ref())) {
        // In ink: red is an alarm's alone, and a refusal isn't one.
        (Some(n), _) => said(n, p.ink),
        (None, Some(e)) => said(&format!("The model didn't answer: {e}"), p.ink),
        _ => what(),
    };
    if let Some(take) = s.take.as_ref().filter(|take| take.r.name == info.name) {
        // One bead a signature: one for each commit, or one for the lot.
        let each: Vec<&str> = match take.way {
            Way::Each => take.r.commits.iter().map(|c| c.sha.as_str()).collect(),
            Way::One => vec![take.r.tip.as_str()],
        };
        let mut beads = row().key("beads").gap(12.0).items_center().role("img").label(format!("{} of {} signed", take.signed.len(), each.len()));
        for sha in each {
            let b = el().size(12.0, 12.0).radius(6.0);
            beads = beads.child(if take.signed.contains_key(sha) {
                row().size(18.0, 18.0).center().radius(9.0).border(1.5, p.ink).child(b.size(9.0, 9.0).fill(p.ink))
            } else if take.signing.as_deref() == Some(sha) {
                row().size(18.0, 18.0).center().child(b.fill(p.ink).cycle(WAITING))
            } else {
                row().size(18.0, 18.0).center().child(b.border(1.5, p.ink3))
            });
        }
        return match &take.phase {
            Phase::Running => {
                let ask = row().gap(12.0).items_center().child(icon(&KEY, 18.0).color(p.ink)).child(text("Touch your key".to_owned()).key("touch").font(TextStyle { size: 14.0, ..t.strong }).color(p.ink)).child(beads);
                strip(ask, button("pause", "Pause", "esc", Style::Quiet, t).on_press_tx(on(|s, _| s.pause_take())))
            }
            Phase::Paused(why) => {
                let mut l = row().gap(14.0).items_center().child(beads);
                if let Some(why) = why {
                    l = l.child(said(why, p.ink));
                }
                let acts = row().gap(10.0).child(button("drop", "Drop", "x", Style::Quiet, t).on_press_tx(on(|s, _| s.drop_ref()))).child(button("take", "Continue", "t", Style::Go, t).on_press_tx(on(|s, _| s.take_now())));
                strip(l, acts)
            }
        };
    }
    if kind != Pill::Ghost {
        let mut acts = row().gap(10.0);
        if kind == Pill::Made {
            acts = acts.child(button("undo", "Undo", "z", Style::Quiet, t).on_press_tx(on(|s, _| s.undo_take()))).child(button("switch", "Switch to it", "\u{23ce}", Style::Go, t).on_press_tx(on(|s, _| s.switch_to())));
        }
        return strip(left, acts);
    }
    let (_, alarms) = s.ref_level(info);
    // The button says what it would make: you get what you read.
    let words = match (s.way_shown(info), info.commits.len()) {
        (Way::One, _) => "Take as one commit".to_owned(),
        (Way::Each, 1) => "Take".to_owned(),
        (Way::Each, n) => format!("Take {n} commits"),
    };
    let take = match s.ready(info) {
        true => button("take", &words, "t", if alarms > 0 { Style::Plain } else { Style::Go }, t),
        false => button("take", &words, "t", Style::Part(s.progress(info)), t),
    };
    let acts = row()
        .gap(10.0)
        .items_center()
        .child(reader(s, t))
        .child(button("drop", "Drop", "x", if alarms > 0 { Style::Fire } else { Style::Quiet }, t).on_press_tx(on(|s, _| s.drop_ref())))
        .child(take.on_press_tx(on(|s, _| s.take_now())));
    strip(left, acts)
}

/// Every key, on a card the `?` button opens. Keys that step two ways are drawn as the pair
/// they are, back on the left and on on the right.
fn keys_card(t: &Ty) -> El {
    let p = pal();
    let pair = |back: &str, what: &str, on: &str| {
        row().w_full().h(26.0).items_center().child(row().w(34.0).justify_end().child(keycap(back, p.ink2, t))).child(text(what.to_owned()).font(t.ui).color(p.ink).w(132.0).px(12.0)).child(keycap(on, p.ink2, t))
    };
    let one = |key: &str, what: &str| row().w_full().h(26.0).items_center().child(row().w(34.0).justify_end().child(keycap(key, p.ink2, t))).child(text(what.to_owned()).font(t.ui).color(p.ink).px(12.0));
    let label = |words: &str| row().w_full().pt(8.0).pb(2.0).child(text(words.to_owned()).font(t.small).color(p.ink3));
    col()
        .key("keys-card")
        .pin(Some(12.0), None, None, Some(64.0))
        .w(248.0)
        .padding(14.0)
        .radius(10.0)
        .fill(p.raise)
        .border(1.0, p.ink3)
        .shadow(24.0, 8.0, Color { r: 0.0, g: 0.0, b: 0.0, a: 0.5 })
        .role("dialog")
        .label("Keys")
        .child(label("earlier \u{2190}                          \u{2192} later"))
        .child(pair("P", "ref", "N"))
        .child(pair("p", "commit", "n"))
        .child(pair("k", "change", "j"))
        .child(pair("[", "file", "]"))
        .child(label("on this change"))
        .child(one("m", "next note from the model"))
        .child(one("a", "ask the model again"))
        .child(one("s", "one column or two"))
        .child(label("on this ref"))
        .child(one("t", "take as a signed branch"))
        .child(one("x", "drop"))
        .child(one("z", "undo the last take or drop"))
        .child(one("u", "read the refs again"))
}
