//! parole: read what a sandbox wrote before bringing it in.
//!
//! Lists the refs under `refs/jail/` and `refs/devvm/` of a repository, the commits each has
//! that HEAD lacks, and shows the selected commit (or the whole ref) as a diff, with the files
//! the host would run marked (`risk.rs`) and a model's notes on what looks dangerous
//! (`agent.rs`). Reading only reads: see `git.rs` for the exact commands. Nothing marks a
//! commit read but its changed lines having been on screen (`diff/`). A ref whose commits are
//! all read can be taken: copied, signed, as a new local branch (`take.rs`); or dropped. The
//! window itself is drawn in `ui.rs`.
//!
//! `parole [DIR] [--refs PATTERN]...` opens the window on the repository at DIR (default: the
//! current directory). `--refs` replaces the default patterns and may repeat.
//! `parole --snapshot DIR` shows what `jail diff DIR` prints for a folder that isn't a
//! repository, with the same marks and notes.
//! Rune's headless flags work too: `parole DIR --snap out.png`, `--tree tree.json`,
//! `--script keys.json`, `--scene split`.

mod agent;
mod diff;
mod git;
mod proc;
mod reviewed;
mod risk;
mod take;
mod ui;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use git::{Commit, RefInfo, Repo};
use reviewed::Reviewed;
use rune::{App, AppSpec, Cx, El, Key, KeyEvent, Latest, Tx, Waker};
use diff::{Annotation, DiffMode, DiffState, LineRef};
use rune_kit::diff::{FileDiff, FileStatus, Side, parse};
use take::{Step, Taken, Way};

const DEFAULT_PATTERNS: &[&str] = &["refs/jail", "refs/devvm"];
/// How long a selection waits for its patch before the window draws without it. Short enough
/// to go unnoticed, long enough that an ordinary commit never flashes "Reading".
/// `PAROLE_WAIT=1` waits until git answers, with the patch or an error, so a headless still
/// is never taken mid-read.
const QUICK: Duration = Duration::from_millis(20);
struct Config {
    dir: PathBuf,
    patterns: Vec<String>,
    /// Show `jail diff DIR` for this folder instead of a repository's refs.
    snapshot: bool,
    /// Don't send each diff to the agent as it is shown; only on `a`.
    no_agent: bool,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

fn config() -> &'static Config {
    CONFIG.get_or_init(|| {
        let mut dir = None;
        let mut patterns = Vec::new();
        let mut snapshot = false;
        let mut no_agent = false;
        let mut args = rune::extra_args().iter();
        while let Some(a) = args.next() {
            match a.as_str() {
                "--snapshot" => snapshot = true,
                "--no-agent" => no_agent = true,
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
        if snapshot && !patterns.is_empty() {
            usage("--snapshot shows a folder, which has no refs: leave --refs out");
        }
        if patterns.is_empty() {
            patterns = DEFAULT_PATTERNS.iter().map(|&p| p.to_owned()).collect();
        }
        Config { dir: dir.unwrap_or_else(|| PathBuf::from(".")), patterns, snapshot, no_agent }
    })
}

fn usage(problem: &str) -> ! {
    eprintln!("parole: {problem}\nusage: parole [DIR] [--refs PATTERN]... [--no-agent]\n       parole --snapshot [DIR] [--no-agent]");
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
    /// Wait for a patch however long it takes (`PAROLE_WAIT`), so a still always has its diff.
    patient: bool,
    refs: Vec<RefInfo>,
    /// The rows that can be selected, in sidebar order.
    items: Vec<Item>,
    sel: usize,
    diff: DiffState,
    /// The selection's patch, read off the UI thread; the newest request wins. It is parsed
    /// on the UI thread when it lands: a parsed `FileDiff` can't cross threads.
    patch: Latest<Result<Read, String>>,
    /// The selection's patch hasn't landed yet.
    loading: bool,
    reviewed: Reviewed,
    /// Why there is nothing to show, or what went wrong reading the selection.
    error: Option<String>,
    /// Something that went wrong beside the diff (saving the reviewed list), shown in the footer.
    notice: Option<String>,
    /// The folder whose `jail diff` is shown, when this isn't a repository's refs.
    snapshot: Option<PathBuf>,
    /// The patch on screen as it was read: what the agent is sent, and what its answer is
    /// filed under.
    patch_text: Arc<str>,
    /// Where each file of that patch is in the diff on screen, by its place in the patch.
    /// Empty when the two don't line up file for file; then no note is tied to a line.
    order: Vec<usize>,
    /// Why each file on screen is one the host would run, by its place in the diff.
    flags: Vec<Option<risk::Why>>,
    /// How many files each commit changes that the host would run, by hash; read off the UI
    /// thread after the refs are listed.
    marks: HashMap<String, usize>,
    marking: Latest<HashMap<String, usize>>,
    /// What the agent said about each diff shown this run, by the diff's own text: a diff
    /// that changes is another diff.
    reports: HashMap<Arc<str>, Asked>,
    /// How many notes and how many alarms the agent left on each diff it has read: by hash
    /// for a commit, by [`whole_key`] for a ref's whole change.
    levels: HashMap<String, (usize, usize)>,
    /// The changed lines that have been on screen, kept while another diff is up: by what
    /// the diff was of ([`State::key`]), its own text, and the order its files were shown in.
    /// Lines are named by their file's place in the diff, so only the same order will do.
    seen_by: HashMap<Shown, Seen>,
    /// What the diff on screen is filed under, from when it landed.
    shown_key: Option<String>,
    /// Where the take left each taken ref's branch, by the ref's name: the signed copy of
    /// its tip. Undo and Switch act on the branch only while it is still there.
    took: HashMap<String, String>,
    /// The taken refs that were taken as one commit, by name: their commits have no copies.
    took_one: HashSet<String>,
    /// Commits the model has been asked about without their diff being opened.
    preread: HashSet<String>,
    /// Those still to ask about, and whether the thread that asks is running.
    ahead: Arc<Mutex<(std::collections::VecDeque<String>, bool)>>,
    /// The "no changes" page of an empty diff has been built for a frame.
    pub(crate) empty_drawn: std::cell::Cell<bool>,
    /// Commits opened this run and not yet read to the end.
    partial: HashSet<String>,
    /// The branch the checkout is on.
    head: Option<String>,
    /// Refs dropped this run, as they were: struck out in the sidebar until put back.
    dropped: Vec<RefInfo>,
    /// What `z` would undo, newest last.
    undo: Vec<Did>,
    /// The keys card is up.
    keys_open: bool,
    /// The note opened to its full length, by its annotation's id.
    open_note: Option<String>,
    /// Which note `m` goes to next.
    note_at: usize,
    /// The file at the top of the diff, for the outline.
    visible_file: usize,
    /// Answers come back here from the threads that asked.
    answers: (Sender<Answer>, Receiver<Answer>),
    /// How many questions are out.
    asking: usize,
    agent: agent::Agent,
    /// Ask about each diff as it is shown (`--no-agent` turns that off; `a` still asks).
    auto_agent: bool,
    /// The signed copies made so far; the take's worker adds to it.
    taken: Arc<Mutex<Taken>>,
    /// The branch each ref was taken as, by ref name.
    branches: HashMap<String, String>,
    /// The take on screen, from the question to its end.
    take: Option<TakeView>,
    /// Wakes the window from another thread: `poll` then picks up what landed.
    waker: Option<Waker>,
    /// jail's program (`jail`, or `PAROLE_JAIL`).
    jail_program: std::ffi::OsString,
}

/// What a worker read for a selection: its patch, and the mode of each path it changes (a
/// patch leaves the mode out when a file only moves).
struct Read {
    patch: String,
    modes: Vec<(String, String)>,
}

/// Where the agent is with one diff. A fresh look keeps what an earlier one found: notes are
/// only ever added.
#[derive(Default)]
struct Asked {
    waiting: bool,
    report: Option<agent::Report>,
    failed: Option<String>,
}

/// A diff as it was shown: what it was of, its text, and the order of its files.
type Shown = (Option<String>, Arc<str>, Vec<usize>);

/// Changed lines, each by its file's place in the diff, its side and its number.
type Seen = HashSet<(u32, Side, u32)>;

/// Something done to a ref that `z` takes back.
enum Did {
    /// The ref, by its full name and the commit it named: it is in `dropped`.
    Dropped(String, String),
    /// The ref, by its full name: its branch is in `branches`.
    Took(String),
}

struct Answer {
    patch: Arc<str>,
    /// What the diff is of: a commit's hash, or a ref's [`whole_key`].
    key: Option<String>,
    result: Result<agent::Report, String>,
    /// Asked about the diff on screen, not ahead of the reader.
    shown: bool,
}

/// At most this many questions are out at once; a diff shown while they are isn't asked
/// about on its own (`a` asks).
const ASKING_AT_ONCE: usize = 3;
/// The most commits of one ref the model is asked about without their being opened.
const LOOK_AHEAD: usize = 40;

/// A take in the window: the ref as it was when taken, and how far the signing has got.
struct TakeView {
    r: RefInfo,
    /// Commit by commit, or as one: fixed when the take starts.
    way: Way,
    phase: Phase,
    /// The commits whose copies exist, and whether each was kept from an earlier take.
    signed: HashMap<String, bool>,
    /// The commit whose signature is being waited for.
    signing: Option<String>,
    steps: Option<Receiver<Step>>,
    cancel: Arc<AtomicBool>,
}

#[derive(PartialEq, Debug)]
enum Phase {
    Running,
    /// Stopped with signatures still to make: by the reader (`None`), or by a failure.
    Paused(Option<String>),
}

/// What a ref's whole change is filed under: its name and the commit it named.
fn whole_key(r: &RefInfo) -> String {
    format!("{}@{}", r.name, r.tip)
}

/// A row by what it shows, not where it is: the ref's name and the commit's hash (`None` for
/// "All changes"). A press carries this, so a row that moved under a reload isn't mistaken
/// for the one that was drawn.
type RowId = (String, Option<String>);

/// What the worker reads.
enum Target {
    Commit(String),
    Range(RefInfo),
    Snapshot(PathBuf),
}

/// What `jail diff DIR` prints, from its first file on (a summary comes before). Bounded like
/// a git read: a minute, and no more output than one git command may give.
fn snapshot_patch(program: &std::ffi::OsStr, dir: &std::path::Path) -> Result<String, String> {
    let mut cmd = std::process::Command::new(program);
    cmd.arg("diff").arg(dir);
    let out = proc::run(cmd, None, 64 << 20, Duration::from_secs(60))?;
    if !out.status.success() {
        return Err(format!("jail diff: {}", out.stderr.trim()));
    }
    Ok(out.stdout.find("diff --git ").map_or(String::new(), |at| out.stdout[at..].to_owned()))
}

impl State {
    fn open() -> State {
        let cfg = config();
        let patient = std::env::var_os("PAROLE_WAIT").is_some_and(|v| !v.is_empty());
        if cfg.snapshot {
            return State::snapshot_with(&cfg.dir, patient, None, !cfg.no_agent);
        }
        let mut s = State::empty(cfg.patterns.clone(), Reviewed::load(), Taken::load(), patient);
        s.auto_agent = !cfg.no_agent;
        s.start(&cfg.dir);
        s
    }

    /// The window on a folder's `jail diff`: one row, nothing to mark or take.
    fn snapshot_with(dir: &std::path::Path, patient: bool, jail: Option<std::ffi::OsString>, auto_agent: bool) -> State {
        let mut s = State::empty(Vec::new(), Reviewed::at(None), Taken::at(None), patient);
        s.auto_agent = auto_agent;
        s.jail_program = jail.unwrap_or(s.jail_program);
        s.snapshot = Some(dir.to_path_buf());
        s.refs = vec![RefInfo { name: format!("jail snapshot of {}", dir.display()), ..RefInfo::default() }];
        s.items = vec![Item { r: 0, pick: Pick::All }];
        s.load();
        s
    }

    fn start(&mut self, dir: &std::path::Path) {
        match Repo::open(dir) {
            Ok(repo) => {
                self.repo = Some(repo);
                self.reload();
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn empty(patterns: Vec<String>, reviewed: Reviewed, taken: Taken, patient: bool) -> State {
        // Long lines wrap: no line of a change is ever cut off, and a note has its line's end.
        let mut diff = DiffState::new(Vec::new());
        diff.set_wrap(true);
        State {
            repo: None,
            patterns,
            patient,
            refs: Vec::new(),
            items: Vec::new(),
            sel: 0,
            diff,
            patch: Latest::default(),
            loading: false,
            reviewed,
            error: None,
            notice: None,
            snapshot: None,
            patch_text: Arc::from(""),
            order: Vec::new(),
            flags: Vec::new(),
            marks: HashMap::new(),
            marking: Latest::default(),
            reports: HashMap::new(),
            levels: HashMap::new(),
            seen_by: HashMap::new(),
            shown_key: None,
            took: HashMap::new(),
            took_one: HashSet::new(),
            preread: HashSet::new(),
            ahead: Arc::default(),
            empty_drawn: std::cell::Cell::new(false),
            partial: HashSet::new(),
            head: None,
            dropped: Vec::new(),
            undo: Vec::new(),
            keys_open: false,
            open_note: None,
            note_at: 0,
            visible_file: 0,
            answers: channel(),
            asking: 0,
            agent: agent::Agent::from_env(),
            auto_agent: false,
            taken: Arc::new(Mutex::new(taken)),
            branches: HashMap::new(),
            take: None,
            waker: None,
            jail_program: std::env::var_os("PAROLE_JAIL").filter(|p| !p.is_empty()).unwrap_or_else(|| "jail".into()),
        }
    }

    fn set_waker(&mut self, waker: Waker) {
        self.patch.set_waker(waker.clone());
        self.marking.set_waker(waker.clone());
        self.waker = Some(waker);
    }

    /// Reads the refs again and keeps the selection where it was, when it's still there.
    fn reload(&mut self) {
        if self.snapshot.is_some() {
            self.load();
            return;
        }
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
        self.head = repo.head_branch();
        // The copies are looked up under the lock; git is asked about them after it is let go.
        let copies: Vec<(usize, Way, String)> = {
            let taken = self.taken.lock().unwrap_or_else(|e| e.into_inner());
            let of = |i: usize, way: Way, key: &str| Some((i, way, taken.get(key)?.to_owned()));
            self.refs.iter().enumerate().flat_map(|(i, r)| [of(i, Way::Each, &r.tip), of(i, Way::One, &take::one_key(r))]).flatten().collect()
        };
        (self.branches, self.took, self.took_one) = Default::default();
        for (i, way, copy) in copies {
            let r = &self.refs[i];
            if let Some(branch) = take::taken_as(repo, &copy, way, r) {
                self.branches.insert(r.name.clone(), branch);
                self.took.insert(r.name.clone(), copy);
                if way == Way::One {
                    self.took_one.insert(r.name.clone());
                }
            }
        }
        // Which commits change something the host runs: one git call a commit, so not here.
        let shas: Vec<String> = self.refs.iter().flat_map(|r| &r.commits).map(|c| c.sha.clone()).filter(|sha| !self.marks.contains_key(sha)).collect();
        if !shas.is_empty() {
            self.marking.request((repo.clone(), shas), |(repo, shas)| {
                shas.into_iter()
                    .filter_map(|sha| {
                        let touched = repo.touched(&sha).ok()?;
                        let n = touched.iter().filter(|(path, mode)| risk::classify(path, Some(mode)).is_some()).count();
                        Some((sha, n))
                    })
                    .collect()
            });
        }
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

    /// Swaps in the diff's files, keeping the layout the reader chose. The files the host
    /// would run come first, in the order they had. `modes` fills in a mode the patch didn't
    /// state; `sections` is how many files the patch's text has.
    fn show(&mut self, files: Vec<FileDiff>, modes: &[(String, String)], sections: usize) {
        let (mode, wrap) = (self.diff.mode(), self.diff.wrap());
        let lined_up = files.len() == sections;
        let mut files: Vec<(usize, Option<risk::Why>, FileDiff)> = files
            .into_iter()
            .enumerate()
            .map(|(i, f)| {
                let mode = modes.iter().find(|(path, _)| path == f.path()).map(|(_, mode)| mode.as_str()).or(f.new_mode.as_deref());
                // A file that is gone runs nothing.
                let why = if f.status == FileStatus::Deleted { None } else { risk::classify(f.path(), mode) };
                (i, why, f)
            })
            .collect();
        files.sort_by_key(|(_, why, _)| why.is_none());
        self.order = vec![0; if lined_up { files.len() } else { 0 }];
        for (now, (was, _, _)) in files.iter().enumerate() {
            if let Some(slot) = self.order.get_mut(*was) {
                *slot = now;
            }
        }
        self.flags = files.iter().map(|(_, why, _)| *why).collect();
        self.diff = DiffState::new(files.into_iter().map(|(_, _, f)| f).collect());
        self.diff.set_mode(mode);
        self.diff.set_wrap(wrap);
    }

    /// What the agent has said about the diff on screen.
    fn report(&self) -> Option<&Asked> {
        self.reports.get(&self.patch_text)
    }

    /// The diff file a finding is about, when the patch and the diff line up.
    fn file_of(&self, f: &agent::Finding) -> Option<usize> {
        self.order.get(f.file).copied()
    }

    /// Finding `id` (an annotation's id) of the diff on screen.
    fn finding(&self, id: &str) -> Option<&agent::Finding> {
        self.report()?.report.as_ref()?.findings.get(id.parse::<usize>().ok()?)
    }

    /// Where each of the agent's notes goes in the diff on screen: its file and line. A note
    /// on a whole file goes on the file's first line, and one whose file can't be told (the
    /// patch and the diff don't line up) on the first line of all, so none is ever dropped.
    fn placed(&self) -> Vec<(usize, usize, LineRef)> {
        let Some(report) = self.report().and_then(|a| a.report.as_ref()) else { return Vec::new() };
        let first = |file: usize| {
            let f = self.diff.files().get(file)?;
            // A file with no lines has one row, and its notes go under that.
            if f.hunks.is_empty() {
                return Some(LineRef::new(Side::New, 0));
            }
            let l = f.hunks.first()?.lines.first()?;
            Some(match (l.new, l.old) {
                (Some(n), _) => LineRef::new(Side::New, n),
                (None, Some(n)) => LineRef::new(Side::Old, n),
                (None, None) => return None,
            })
        };
        let somewhere = (0..self.diff.files().len()).find_map(|f| Some((f, first(f)?)));
        report
            .findings
            .iter()
            .enumerate()
            .filter_map(|(i, f)| {
                let lined = |file: usize| self.diff.files().get(file).is_some_and(|f| !f.hunks.is_empty());
                let at = self.file_of(f).and_then(|file| Some((file, if f.line > 0 && lined(file) { LineRef::new(Side::New, f.line) } else { first(file)? })));
                let (file, at) = at.or(somewhere)?;
                Some((i, file, at))
            })
            .collect()
    }

    /// Puts the agent's notes for this diff on the lines they are about.
    fn annotate(&mut self) {
        let notes = self.placed().into_iter().map(|(i, file, at)| Annotation::new(file, at, i.to_string())).collect();
        self.diff.set_annotations(notes);
    }

    /// Notes and alarms on each file of the diff on screen.
    fn file_levels(&self) -> Vec<(usize, usize)> {
        let mut out = vec![(0, 0); self.diff.files().len()];
        for (i, file, _) in self.placed() {
            match self.finding(&i.to_string()).is_some_and(|f| f.alarm) {
                true => out[file].1 += 1,
                false => out[file].0 += 1,
            }
        }
        out
    }

    /// The lines of the diff on screen that carry a note, and whether it is an alarm.
    fn line_levels(&self) -> HashMap<(usize, u32), bool> {
        let mut out = HashMap::new();
        for (i, file, at) in self.placed() {
            let alarm = self.finding(&i.to_string()).is_some_and(|f| f.alarm);
            *out.entry((file, at.line)).or_insert(false) |= alarm;
        }
        out
    }

    /// Notes and alarms on everything of a ref the agent has read.
    fn ref_level(&self, r: &RefInfo) -> (usize, usize) {
        let keys = r.commits.iter().map(|c| c.sha.clone()).chain([whole_key(r)]);
        keys.filter_map(|k| self.levels.get(&k)).fold((0, 0), |(n, a), &(notes, alarms)| (n + notes, a + alarms))
    }

    /// Goes to the agent's next note on this diff, alarms first, and opens it.
    fn next_note(&mut self, tx: &mut Tx) {
        let mut placed = self.placed();
        placed.sort_by_key(|(i, _, _)| !self.finding(&i.to_string()).is_some_and(|f| f.alarm));
        if placed.is_empty() {
            return;
        }
        let (i, file, at) = placed[self.note_at % placed.len()];
        self.note_at = (self.note_at + 1) % placed.len();
        self.open_note = Some(i.to_string());
        self.diff.scroll_to_line(file, at, tx);
    }

    /// What the selection's diff is filed under.
    fn key(&self) -> Option<String> {
        let (r, c) = self.selected()?;
        self.snapshot.is_none().then(|| c.map_or_else(|| whole_key(r), |c| c.sha.clone()))
    }

    /// Asks the agent about the diff on screen, on a thread of its own: the window goes on
    /// with what it has, and the notes land when they come. `fresh` asks the model even when
    /// an answer to this very diff was kept.
    fn ask_agent(&mut self, fresh: bool) {
        if self.loading || self.diff.files().is_empty() {
            return;
        }
        let key = self.key();
        let asked = self.reports.entry(self.patch_text.clone()).or_default();
        if asked.waiting {
            return;
        }
        asked.waiting = true;
        asked.failed = None;
        self.asking += 1;
        let (agent, patch, send, waker) = (self.agent.clone(), self.patch_text.clone(), self.answers.0.clone(), self.waker.clone());
        std::thread::spawn(move || {
            let result = agent.ask(&patch, fresh);
            let _ = send.send(Answer { patch, key, result, shown: true });
            if let Some(wake) = &waker {
                wake();
            }
        });
    }

    /// With a ref's whole change on screen, asks the model about each of its commits too:
    /// at most [`LOOK_AHEAD`] of a ref, one at a time for the whole window, on one thread that
    /// goes through a queue. Their diffs are not being read, so this is where something that
    /// came and went between commits can still show: as a mark on that commit's stop, and on
    /// the ref.
    fn look_ahead(&mut self) {
        let (Some(repo), Some((r, None))) = (self.repo.clone(), self.selected()) else { return };
        if !self.auto_agent || self.snapshot.is_some() {
            return;
        }
        let had = r.commits.iter().filter(|c| self.preread.contains(&c.sha)).count();
        let shas: Vec<String> = r.commits.iter().map(|c| c.sha.clone()).filter(|sha| !self.levels.contains_key(sha) && !self.preread.contains(sha)).take(LOOK_AHEAD.saturating_sub(had)).collect();
        if shas.is_empty() {
            return;
        }
        self.preread.extend(shas.iter().cloned());
        let start = {
            let mut q = self.ahead.lock().unwrap_or_else(|e| e.into_inner());
            q.0.extend(shas);
            !std::mem::replace(&mut q.1, true)
        };
        if !start {
            return;
        }
        let (agent, send, waker, queue) = (self.agent.clone(), self.answers.0.clone(), self.waker.clone(), self.ahead.clone());
        std::thread::spawn(move || {
            loop {
                let sha = {
                    let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                    match q.0.pop_front() {
                        Some(sha) => sha,
                        None => {
                            q.1 = false;
                            return;
                        }
                    }
                };
                let Ok(patch) = repo.commit_patch(&sha) else { continue };
                let patch: Arc<str> = Arc::from(patch);
                let result = agent.ask(&patch, false);
                // The window is gone: nothing more is asked.
                if send.send(Answer { patch, key: Some(sha), result, shown: false }).is_err() {
                    return;
                }
                if let Some(wake) = &waker {
                    wake();
                }
            }
        });
    }

    /// Files an answer under its diff. What an earlier look found stays, whichever look it
    /// was: a look ahead adds its findings and leaves a look the reader is waiting for to land
    /// on its own; one that failed says nothing, and the commit is asked about when opened.
    fn answered(&mut self, answer: Answer) {
        if answer.shown {
            self.asking = self.asking.saturating_sub(1);
        } else if answer.result.is_err() {
            return;
        }
        let asked = self.reports.entry(answer.patch.clone()).or_default();
        if answer.shown {
            asked.waiting = false;
        }
        match answer.result {
            Ok(mut new) => {
                if let Some(old) = asked.report.take() {
                    let more: Vec<_> = old.findings.into_iter().filter(|f| !new.findings.contains(f)).collect();
                    new.findings.extend(more);
                    new.dropped = new.dropped.max(old.dropped);
                }
                if let Some(key) = answer.key {
                    self.levels.insert(key, level(&new));
                }
                asked.report = Some(new);
            }
            Err(e) => asked.failed = Some(e),
        }
        if answer.patch == self.patch_text {
            self.annotate();
        }
    }

    /// Everything other threads have ready; whether anything changed.
    fn poll(&mut self) -> bool {
        let mut changed = self.land();
        self.seen_more();
        if let Some(marks) = self.marking.take() {
            self.marks.extend(marks);
            changed = true;
        }
        while let Ok(answer) = self.answers.1.try_recv() {
            self.answered(answer);
            changed = true;
        }
        changed | self.take_steps()
    }

    /// Signatures are being made: nothing else writes until that stops.
    fn signing(&self) -> bool {
        self.take.as_ref().is_some_and(|t| t.phase == Phase::Running)
    }

    /// Every commit of `r` has been read, and it is a ref that can be taken at all. What the
    /// model says is not asked; only that a first look at the diff on screen isn't still out,
    /// so an alarm on its way is not missed by a quick press.
    fn ready(&self, r: &RefInfo) -> bool {
        self.way(r).is_some() && !self.report().is_some_and(|a| a.waiting && a.report.is_none())
    }

    /// How `r` would be taken now, if it can be: commit by commit when every commit has been
    /// read, as one commit when only its whole change has.
    fn way(&self, r: &RefInfo) -> Option<Way> {
        if r.error.is_some() || r.truncated || r.commits.is_empty() {
            None
        } else if r.commits.iter().all(|c| self.reviewed.has(&c.sha)) {
            Some(Way::Each)
        } else {
            self.reviewed.has(&take::whole_mark(r)).then_some(Way::One)
        }
    }

    /// How the Take button would take `r`: as it can be taken now, or else as what is on
    /// screen would let it be once read.
    fn way_shown(&self, r: &RefInfo) -> Way {
        self.way(r).unwrap_or(match self.selected() {
            Some((on, None)) if on.name == r.name => Way::One,
            _ => Way::Each,
        })
    }

    /// How much of `r` has been read, from 0 to 1: its read commits, and as much of the diff
    /// on screen as has been seen when that is more.
    fn progress(&self, r: &RefInfo) -> f32 {
        let n = r.commits.len().max(1) as f32;
        if self.reviewed.has(&take::whole_mark(r)) {
            return 1.0;
        }
        let read = r.commits.iter().filter(|c| self.reviewed.has(&c.sha)).count() as f32;
        let (seen, all) = self.diff.read();
        let here = if self.loading || all == 0 { 0.0 } else { seen as f32 / all as f32 };
        match self.selected() {
            Some((on, None)) if on.name == r.name => (read / n).max(here),
            Some((on, Some(c))) if on.name == r.name && !self.reviewed.has(&c.sha) => (read + here) / n,
            _ => read / n,
        }
    }

    /// A changed line has been on screen for the first time, or a diff has just landed: once
    /// every one of its changed lines has been, what it shows is read. A ref's whole change
    /// is read as itself ([`take::whole_mark`]), not as its commits: what went on in between
    /// was not on screen. Nothing asks the reader to say so.
    fn seen_more(&mut self) {
        if self.loading || self.error.is_some() || self.snapshot.is_some() {
            return;
        }
        let (seen, all) = self.diff.read();
        let Some((r, c)) = self.selected() else { return };
        // Never before a frame of it was drawn: a diff with files counts once its rows have
        // been laid out in the window, and one with none (an empty commit, a ref whose
        // commits cancel out) once the page saying so has been built.
        let drawn = if self.diff.files().is_empty() { self.empty_drawn.get() } else { self.diff.laid() };
        if !drawn {
            return;
        }
        let shas: Vec<String> = match c {
            Some(c) => vec![c.sha.clone()],
            None => vec![take::whole_mark(r)],
        };
        if seen < all {
            if c.is_some() {
                self.partial.extend(shas);
            }
            return;
        }
        let new: Vec<&str> = shas.iter().map(String::as_str).filter(|sha| !self.reviewed.has(sha)).collect();
        if !new.is_empty() {
            // The marks hold for this run either way.
            self.notice = self.reviewed.set(&new, true).err().map(|e| format!("Couldn't save what has been read: {e}"));
        }
        for sha in &shas {
            self.partial.remove(sha);
        }
    }

    /// Takes the selection's ref: signs a copy of each commit, then makes the branch. On its
    /// own thread: each signature may wait for a touch. A ref that isn't ready says why.
    fn take_now(&mut self) {
        if self.signing() {
            return;
        }
        // An empty diff has no rows to report being seen: its page, drawn, is asked after here.
        self.seen_more();
        let (Some(repo), Some((r, _))) = (self.repo.clone(), self.selected()) else { return };
        if self.snapshot.is_some() || self.branches.contains_key(&r.name) {
            return;
        }
        let r = r.clone();
        // A paused take goes on the way it began: its beads are of that take.
        let paused = self.take.as_ref().filter(|t| t.r.name == r.name && t.r.tip == r.tip).map(|t| t.way);
        let way = paused.or_else(|| self.way(&r));
        let unread = if way.is_some() { 0 } else { r.commits.iter().filter(|c| !self.reviewed.has(&c.sha)).count() };
        let (seen, all) = self.diff.read();
        let blocked = if r.error.is_some() || r.commits.is_empty() {
            Some("There is nothing on this ref to take.".to_owned())
        } else if r.truncated {
            Some(format!("This ref has more than {} commits; only the listed ones can be read here, so it can't be taken here.", git::MAX_COMMITS))
        } else if unread > 0 && seen < all {
            Some(format!("{} of this change {} not been on screen yet. The dim part of the map is what is left.", plural(all - seen, "line"), if all - seen == 1 { "has" } else { "have" }))
        } else if unread > 0 {
            Some(format!("{} of its {} not read yet, nor its whole change.", unread, plural(r.commits.len(), "commit")))
        } else if !self.ready(&r) {
            Some("The model is still reading this.".to_owned())
        } else {
            None
        };
        if let Some(why) = blocked {
            self.notice = Some(why);
            return;
        }
        self.notice = None;
        let (send, steps) = channel();
        let branch = take::branch_for(&r);
        let cancel: Arc<AtomicBool> = Arc::default();
        let Some(way) = way else { return };
        self.take = Some(TakeView { r: r.clone(), way, phase: Phase::Running, signed: HashMap::new(), signing: None, steps: Some(steps), cancel: cancel.clone() });
        let (reviewed, taken, waker) = (self.reviewed.all(), self.taken.clone(), self.waker.clone());
        std::thread::spawn(move || {
            take::run(&repo, &r, &branch, way, &reviewed, &taken, &cancel, &mut |step| {
                let _ = send.send(step);
                if let Some(wake) = &waker {
                    wake();
                }
            });
        });
    }

    /// Takes in what the signing thread has reported; whether there was anything.
    fn take_steps(&mut self) -> bool {
        let Some(t) = &mut self.take else { return false };
        let Some(steps) = &t.steps else { return false };
        let mut changed = false;
        let mut done = None;
        for step in steps.try_iter() {
            changed = true;
            match step {
                Step::Signing { sha, .. } => t.signing = Some(sha),
                Step::Signed { sha, reused } => {
                    t.signing = None;
                    t.signed.insert(sha, reused);
                }
                Step::Done { .. } => done = Some(true),
                Step::Failed(e) => {
                    t.signing = None;
                    // Stopped by the reader says nothing: the beads show where it got to.
                    t.phase = Phase::Paused((e != git::CANCELLED).then_some(e));
                    done = Some(false);
                }
            }
        }
        match done {
            Some(true) => {
                let name = t.r.name.clone();
                self.take = None;
                self.undo.push(Did::Took(name));
                // The sidebar now draws the branch.
                self.reload();
            }
            Some(false) => t.steps = None,
            None => {}
        }
        changed
    }

    /// Stops the signing after the signature being made. Those made are kept, so taking
    /// again goes on from there.
    fn pause_take(&mut self) {
        if let Some(t) = self.take.as_ref().filter(|t| t.phase == Phase::Running) {
            t.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Drops the selection's ref: deletes it, and keeps it struck out in the sidebar. Its
    /// commits stay in the repository until git prunes them, so `z` puts it back.
    fn drop_ref(&mut self) {
        if self.take.as_ref().is_some_and(|t| t.phase == Phase::Running) || self.snapshot.is_some() {
            return;
        }
        let (Some(repo), Some((r, _))) = (self.repo.clone(), self.selected()) else { return };
        let r = r.clone();
        match repo.delete_ref(&r.name, &r.tip) {
            Ok(()) => {
                self.notice = None;
                if self.take.as_ref().is_some_and(|t| t.r.name == r.name) {
                    self.take = None;
                }
                self.undo.push(Did::Dropped(r.name.clone(), r.tip.clone()));
                self.dropped.push(r);
                self.reload();
            }
            Err(e) => self.notice = Some(format!("Couldn't drop it: {e}")),
        }
    }

    /// Puts a dropped ref back where it was.
    fn restore(&mut self, name: &str, tip: &str) {
        if self.signing() {
            return;
        }
        let (Some(repo), Some(at)) = (self.repo.clone(), self.dropped.iter().position(|d| d.name == name && d.tip == tip)) else { return };
        match repo.restore_ref(name, tip) {
            Ok(()) => {
                self.dropped.remove(at);
                self.undo.retain(|d| !matches!(d, Did::Dropped(n, t) if n == name && t == tip));
                self.notice = None;
                self.reload();
            }
            Err(e) => self.notice = Some(format!("Couldn't put it back: {e}")),
        }
    }

    /// Takes back the take of `name`: deletes the branch it made. The signed copies stay, so
    /// taking again asks for no touch.
    fn untake(&mut self, name: &str) {
        if self.signing() {
            return;
        }
        let (Some(repo), Some(branch), Some(tip)) = (self.repo.clone(), self.branches.get(name).cloned(), self.took.get(name).cloned()) else { return };
        // Asked now, not remembered: the checkout may have moved since the refs were listed.
        self.head = repo.head_branch();
        if self.head.as_deref() == Some(&branch) {
            self.notice = Some(format!("You are on {branch}. Switch away from it first."));
            return;
        }
        // Only at the commit the take left it on: a branch that has moved since has work of
        // its own, and git refuses.
        match repo.delete_branch(&branch, &tip) {
            Ok(()) => {
                self.undo.retain(|d| !matches!(d, Did::Took(n) if n == name));
                self.notice = None;
                self.reload();
            }
            Err(e) => {
                self.notice = Some(format!("Couldn't undo the take: {e}"));
                // What is drawn may be out of date: that is one way to get here.
                self.reload();
            }
        }
    }

    /// Undo on the strip: the selection's take.
    fn undo_take(&mut self) {
        if let Some(name) = self.selected().map(|(r, _)| r.name.clone()) {
            self.untake(&name);
        }
    }

    /// `z`: takes back the last take or drop.
    fn undo_last(&mut self) {
        match self.undo.last() {
            Some(Did::Dropped(name, tip)) => self.restore(&name.clone(), &tip.clone()),
            Some(Did::Took(name)) => self.untake(&name.clone()),
            None => {}
        }
    }

    /// Switches the checkout to the branch the selection's ref was taken as. The one thing
    /// here that changes the working tree, and only on this press.
    fn switch_to(&mut self) {
        let (Some(repo), Some((r, _))) = (self.repo.clone(), self.selected()) else { return };
        let (Some(branch), Some(tip)) = (self.branches.get(&r.name).cloned(), self.took.get(&r.name).cloned()) else { return };
        if self.signing() {
            return;
        }
        self.head = repo.head_branch();
        if self.head.as_deref() == Some(&branch) {
            return;
        }
        // Only to what was taken: a branch that moved since the refs were listed is not it.
        if repo.branch_tip(&branch).ok().flatten().as_deref() != Some(&tip) {
            self.notice = Some(format!("{branch} is no longer where the take left it."));
            self.reload();
            return;
        }
        self.notice = repo.switch(&branch).err().map(|e| format!("git switch: {e}"));
        self.head = repo.head_branch();
    }

    /// Asks a worker for the selection's patch. The last selection's diff goes at once, so the
    /// header never names one commit over another's changes. Most patches are back within
    /// [`QUICK`] and land before the next frame; a slower one shows "Reading" until `poll`
    /// takes it.
    fn load(&mut self) {
        // What was seen of the diff going away is kept for when it is back.
        if !self.patch_text.is_empty() {
            self.seen_by.insert((self.shown_key.take(), self.patch_text.clone(), self.order.clone()), self.diff.seen().clone());
        }
        self.open_note = None;
        self.note_at = 0;
        self.visible_file = 0;
        self.empty_drawn.set(false);
        self.patch_text = Arc::from("");
        self.show(Vec::new(), &[], 0);
        self.annotate();
        let jail = self.jail_program.clone();
        let job = match (&self.repo, &self.snapshot, self.selected()) {
            (_, Some(dir), _) => Some((None, Target::Snapshot(dir.clone()))),
            (Some(repo), _, Some((_, Some(c)))) => Some((Some(repo.clone()), Target::Commit(c.sha.clone()))),
            (Some(repo), _, Some((r, None))) => Some((Some(repo.clone()), Target::Range(r.clone()))),
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
        self.patch.request(job, move |(repo, target)| {
            // The modes are a help, not the diff: without them a moved file keeps its name's verdict.
            Ok(match (target, repo) {
                (Target::Snapshot(dir), _) => Read { patch: snapshot_patch(&jail, &dir)?, modes: Vec::new() },
                (Target::Commit(sha), Some(repo)) => Read { patch: repo.commit_patch(&sha)?, modes: repo.touched(&sha).unwrap_or_default() },
                (Target::Range(r), Some(repo)) => Read { patch: repo.range_patch(&r)?, modes: repo.touched_range(&r).unwrap_or_default() },
                _ => return Err("no repository".to_owned()),
            })
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
            Ok(read) => {
                let sections = read.patch.lines().filter(|l| l.starts_with("diff --git ")).count();
                self.show(parse(&read.patch), &read.modes, sections);
                self.patch_text = Arc::from(read.patch);
                self.shown_key = self.key();
                if let Some(seen) = self.seen_by.get(&(self.shown_key.clone(), self.patch_text.clone(), self.order.clone())) {
                    self.diff.set_seen(seen.clone());
                }
                // Notes kept from the same patch under another commit count for this one too.
                if let (Some(key), Some(report)) = (self.shown_key.clone(), self.report().and_then(|a| a.report.as_ref())) {
                    self.levels.insert(key, level(report));
                }
                self.annotate();
                // A diff with no lines to read (a mode change, a move) is read once shown.
                self.seen_more();
                // The second reader starts on its own; the diff is already up.
                if self.auto_agent && self.report().is_none() && self.asking < ASKING_AT_ONCE {
                    self.ask_agent(false);
                }
                self.look_ahead();
            }
            Err(e) => self.error = Some(e),
        }
        true
    }

    fn select(&mut self, i: usize, tx: &mut Tx) {
        // Nothing moves while signatures are being made.
        if self.take.as_ref().is_some_and(|t| t.phase == Phase::Running) || i >= self.items.len() || i == self.sel {
            return;
        }
        self.sel = i;
        self.error = None;
        self.notice = None;
        self.load();
        // One commit's diff replaces another's in one frame: nothing of the old one fades or
        // slides under the new one's header.
        tx.instant();
        tx.scroll_into_view(item_key(i));
    }

    /// `n` and `p`: the next or previous commit. A ref's whole change is not on this path:
    /// reading it is a choice (it decides how the ref is taken), not a stop on the way.
    fn step(&mut self, forward: bool, tx: &mut Tx) {
        let mut to = self.sel;
        loop {
            to = if forward { to + 1 } else { to.wrapping_sub(1) };
            match self.items.get(to) {
                Some(it) if it.pick == Pick::All => continue,
                Some(_) => return self.select(to, tx),
                None => return,
            }
        }
    }

    /// `N` and `P`: the next or previous ref, at its first row: its whole change, or its one
    /// commit when that is the same thing.
    fn step_ref(&mut self, forward: bool, tx: &mut Tx) {
        let Some(at) = self.items.get(self.sel).map(|it| it.r) else { return };
        let to = match forward {
            true => self.items.iter().position(|it| it.r > at),
            false => self.items.iter().rev().find(|it| it.r < at).map(|it| it.r).and_then(|r| self.items.iter().position(|it| it.r == r)),
        };
        if let Some(to) = to {
            self.select(to, tx);
        }
    }

    /// Lands at once. A spring here moved every line sideways through the other pane's text,
    /// and no frame on the way was readable.
    fn toggle_split(&mut self, tx: &mut Tx) {
        tx.instant();
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

/// How many notes and how many alarms a report holds.
fn level(report: &agent::Report) -> (usize, usize) {
    let alarms = report.findings.iter().filter(|f| f.alarm).count();
    (report.findings.len() - alarms, alarms)
}

fn item_key(i: usize) -> String {
    format!("item-{i}")
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {one}s") }
}

fn view(s: &State, _: &mut Cx<State>) -> El {
    ui::view(s)
}

/// The Latin letter on the key that typed `t`, for the layouts this is used with: a shortcut
/// is a place on the keyboard, and `n` on a Russian layout types "т".
fn shortcut(t: &str) -> &str {
    match t {
        "т" => "n",
        "з" => "p",
        "Т" => "N",
        "З" => "P",
        "ы" | "Ы" => "s",
        "ч" | "Ч" => "x",
        "я" | "Я" => "z",
        "ь" | "Ь" => "m",
        "г" | "Г" => "u",
        "ф" | "Ф" => "a",
        "е" | "Е" => "t",
        "о" | "О" => "j",
        "л" | "Л" => "k",
        "ъ" | "Ъ" => "]",
        "х" | "Х" => "[",
        t => t,
    }
}

/// What changes what is shown (another commit, the layout, a mark, the refs) lands in one
/// frame. Rune animates a key's commit unless told otherwise: old rows fade out under new
/// ones and kept rows slide, so for a fifth of a second the window showed a diff that never
/// existed. Hunk and file jumps land at once too, as the kit's reveal already did.
fn on_key(s: &mut State, ev: &KeyEvent, tx: &mut Tx) -> bool {
    // Not gated on focus: the window has no text field, and a button that took focus (from a
    // click or Tab) has already had its own keys (Space, Return) by the time this runs.
    // `DiffState::key` does stand down while anything is focused, so its four keys are here.
    let m = ev.mods;
    // A held key acts once where it writes: `x` held would drop ref after ref.
    let writes = ev.key == Key::Enter || matches!(&ev.key, Key::Text(t) if matches!(shortcut(t), "t" | "x" | "z"));
    if tx.key_repeat() && (writes || s.keys_open) {
        return true;
    }
    if s.keys_open {
        // Any key puts the card away; it is only a card.
        tx.instant();
        s.keys_open = false;
        return true;
    }
    if s.take.as_ref().is_some_and(|t| t.phase == Phase::Running) {
        // The signing has the keyboard until it stops: no key reaches the diff under it.
        tx.instant();
        if ev.key == Key::Escape {
            s.pause_take();
        }
        return true;
    }
    if ev.key == Key::Enter && !(m.cmd || m.ctrl || m.alt) && s.selected().is_some_and(|(r, _)| s.branches.contains_key(&r.name)) {
        tx.instant();
        s.switch_to();
        return true;
    }
    if let Key::Text(t) = &ev.key
        && !(m.cmd || m.ctrl || m.alt)
    {
        tx.instant();
        match shortcut(t) {
            "n" => s.step(true, tx),
            "p" => s.step(false, tx),
            "N" => s.step_ref(true, tx),
            "P" => s.step_ref(false, tx),
            "s" => s.toggle_split(tx),
            "m" => s.next_note(tx),
            // Again, and from the model itself: a kept answer isn't enough for whoever presses this.
            "a" => s.ask_agent(true),
            "t" => s.take_now(),
            "x" => s.drop_ref(),
            "z" => s.undo_last(),
            "u" => s.reload(),
            "?" => s.keys_open = true,
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

/// What another thread finished (a late diff, the agent, a signature) lands in one frame, like
/// everything else.
fn poll(s: &mut State, _: f64, tx: &mut Tx) -> bool {
    tx.instant();
    s.poll()
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
        setup: Some(State::set_waker),
        poll: Some(poll),
        min_size: Some((760.0, 420.0)),
        families: ("Inter", "ui-monospace, Inter"),
        ..AppSpec::new("parole", (1180.0, 760.0), State::open, view)
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
        let reviewed = Reviewed::at(Some(dir.join("state/parole/reviewed")));
        let patterns = DEFAULT_PATTERNS.iter().map(|&p| p.to_owned()).collect();
        let mut state = State::empty(patterns, reviewed, Taken::at(Some(dir.join("state/parole/taken"))), true);
        // No test asks the real model or keeps answers in the user's own state directory.
        state.agent = agent::Agent { program: "false".into(), cache: Some(dir.join("state/parole/agent")) };
        state.start(&dir.join("repo"));
        let mut rt = Runtime::new(state, view, (1180.0, 760.0), Env { system_dark: true, scale: 1.0 });
        rt.on_key = Some(on_key);
        rt.poll = Some(poll);
        rt.frame(0.0);
        // The first frame lays the diff out; the next knows which of its lines are on screen.
        rt.frame(1.0 / 60.0);
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

    /// Whether the window is still moving a frame after `t` was typed.
    fn moves_after(rt: &mut Runtime<State>, t: &str) -> bool {
        type_key(rt, t);
        rt.frame(1.0 / 60.0)
    }

    #[test]
    fn a_key_lands_in_one_frame() {
        let (mut rt, dir) = app("one-frame");
        // Another commit, split, wrap, a note, back, a hunk, a file, the keys card: none leaves
        // anything in motion.
        for t in ["n", "n", "s", "m", "p", "j", "]", "s", "?", "?"] {
            assert!(!moves_after(&mut rt, t), "still animating a frame after {t:?}");
        }
        std::fs::remove_dir_all(dir).unwrap();
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
        // n and p walk commits, and pass over a ref's whole change.
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
        assert_eq!(subject(&rt.state), "from the vm", "p on a Russian layout, past the whole change");
        assert!(rt.state.reviewed.all().iter().all(|m| !m.starts_with("whole:")), "a whole change passed over isn't read");
        // N and P walk refs: each one's whole change, or its one commit.
        type_key(&mut rt, "N");
        assert_eq!(subject(&rt.state), "all of jail/work");
        assert_eq!(paths(&rt.state), ["a.txt", "b.txt"]);
        type_key(&mut rt, "N");
        assert_eq!(subject(&rt.state), "all of jail/work", "N stops at the last ref");
        type_key(&mut rt, "P");
        assert_eq!(subject(&rt.state), "from the vm");
        type_key(&mut rt, "Т");
        assert_eq!(subject(&rt.state), "all of jail/work", "N on a Russian layout");
        // From a whole change, n goes into its commits.
        type_key(&mut rt, "n");
        assert_eq!(subject(&rt.state), "first");
        type_key(&mut rt, "P");
        assert_eq!(subject(&rt.state), "from the vm", "P from a commit is the ref before");
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
    fn the_keys_still_work_after_a_click_on_a_button() {
        let (mut rt, dir) = app("focus");
        press(&mut rt, "item-2");
        press(&mut rt, "split");
        assert_eq!(rt.state.diff.mode(), DiffMode::Split, "the button splits");
        type_key(&mut rt, "n");
        assert_eq!(subject(&rt.state), "second", "n after the click");
        type_key(&mut rt, "s");
        assert_eq!(rt.state.diff.mode(), DiffMode::Unified, "s after the click");
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn saved(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("state/parole/reviewed")).unwrap_or_default().lines().map(str::to_owned).collect()
    }

    #[test]
    fn a_change_is_read_once_every_changed_line_has_been_on_screen() {
        let (mut rt, dir) = app("seen");
        // The first commit fits the window: shown is read, with no key pressed.
        let vm = rt.state.refs[0].commits[0].sha.clone();
        assert!(rt.state.reviewed.has(&vm));
        assert_eq!(saved(&dir), std::slice::from_ref(&vm));
        // A ref's whole change, read, is read as that: its commits were never on screen, and
        // stay unread. It can be taken, as the one commit that was read.
        type_key(&mut rt, "N");
        rt.frame(1.0 / 60.0);
        let work = rt.state.refs[1].clone();
        assert_eq!(work.short(), "jail/work");
        let mut want = vec![vm, take::whole_mark(&work)];
        want.sort();
        assert_eq!(saved(&dir), want);
        assert!(work.commits.iter().all(|c| !rt.state.reviewed.has(&c.sha)));
        assert_eq!(rt.state.way(&work), Some(Way::One));
        assert!(rt.state.ready(&work));
        // Each commit read as well, it is taken commit by commit.
        type_key(&mut rt, "n");
        type_key(&mut rt, "n");
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.way(&work), Some(Way::Each));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_change_longer_than_the_window_is_read_only_by_going_through_it() {
        let (mut rt, dir) = app("long");
        let repo = dir.join("repo");
        build(&repo, &["checkout", "-q", "-b", "long", "main"]);
        let text: String = (1..=400).map(|i| format!("line {i}\n")).collect();
        std::fs::write(repo.join("long.txt"), text).unwrap();
        build(&repo, &["add", "-A"]);
        build(&repo, &["commit", "-q", "-m", "a long one"]);
        build(&repo, &["update-ref", "refs/jail/zz-long", "HEAD"]);
        build(&repo, &["checkout", "-q", "main"]);
        type_key(&mut rt, "u");
        let last = rt.state.items.len() - 1;
        press(&mut rt, &item_key(last));
        rt.frame(1.0 / 60.0);
        let sha = rt.state.selected().unwrap().1.unwrap().sha.clone();
        let (seen, all) = rt.state.diff.read();
        assert_eq!(all, 400);
        assert!(seen > 0 && seen < 100, "{seen} of {all} seen with the top on screen");
        assert!(!rt.state.reviewed.has(&sha) && rt.state.partial.contains(&sha));
        // Jumping to the end shows the end, not the middle: still not read, and not takeable.
        let jump = |rt: &mut Runtime<State>, line: u32| {
            rt.update(|s, tx| s.diff.scroll_to_line(0, LineRef::new(Side::New, line), tx));
            rt.frame(1.0 / 60.0);
            rt.frame(1.0 / 60.0);
        };
        jump(&mut rt, 400);
        assert!(!rt.state.reviewed.has(&sha));
        type_key(&mut rt, "t");
        assert!(rt.state.take.is_none());
        assert!(rt.state.notice.as_deref().is_some_and(|n| n.contains("not been on screen")), "{:?}", rt.state.notice);
        let progress = rt.state.progress(&rt.state.refs.last().unwrap().clone());
        assert!(progress > 0.0 && progress < 1.0, "{progress}");
        // Leaving and coming back keeps what was seen.
        let before = rt.state.diff.read().0;
        type_key(&mut rt, "p");
        type_key(&mut rt, "n");
        assert!(rt.state.diff.read().0 >= before);
        // Gone through from top to bottom, it is read.
        for line in (1..=400).step_by(15) {
            jump(&mut rt, line);
        }
        jump(&mut rt, 400);
        assert_eq!(rt.state.diff.read(), (400, 400));
        assert!(rt.state.reviewed.has(&sha) && !rt.state.partial.contains(&sha));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_line_that_wraps_past_the_window_is_read_only_at_its_end() {
        let (mut rt, dir) = app("tall");
        let repo = dir.join("repo");
        build(&repo, &["checkout", "-q", "-b", "tall", "main"]);
        // No space in it: it must still break at the window's edge, not run on past it.
        std::fs::write(repo.join("min.js"), format!("first\n{}\nlast\n", "x".repeat(60000))).unwrap();
        build(&repo, &["add", "-A"]);
        build(&repo, &["commit", "-q", "-m", "a tall one"]);
        build(&repo, &["update-ref", "refs/jail/zz-tall", "HEAD"]);
        build(&repo, &["checkout", "-q", "main"]);
        type_key(&mut rt, "u");
        let last = rt.state.items.len() - 1;
        press(&mut rt, &item_key(last));
        rt.frame(1.0 / 60.0);
        let sha = rt.state.selected().unwrap().1.unwrap().sha.clone();
        // The long line's start is on screen, and its end far below.
        assert_eq!(rt.state.diff.read(), (1, 3));
        assert!(!rt.state.reviewed.has(&sha));
        // A screen further down is still the middle of it.
        for _ in 0..2 {
            rt.wheel(700.0, 400.0, -300.0);
            rt.frame(1.0 / 60.0);
        }
        assert_eq!(rt.state.diff.read(), (1, 3));
        assert!(!rt.state.reviewed.has(&sha));
        // Scrolled to where the next line shows under it, it has been gone through.
        for _ in 0..400 {
            rt.wheel(700.0, 400.0, -300.0);
            rt.frame(1.0 / 60.0);
        }
        assert_eq!(rt.state.diff.read(), (3, 3));
        assert!(rt.state.reviewed.has(&sha));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_hunk_and_file_keys_move_the_mark_in_both_layouts() {
        let (mut rt, dir) = app("nav");
        press(&mut rt, "item-1");
        assert!(paths(&rt.state).len() > 1, "{:?}", paths(&rt.state));
        let marked = |s: &State| -> Vec<usize> {
            let rows = s.diff.map().0;
            (0..rows.len()).filter(|&i| s.diff.in_nav(i)).map(|i| rows[i].file).collect()
        };
        assert!(marked(&rt.state).is_empty());
        // One column first: on, and back.
        type_key(&mut rt, "j");
        assert!(marked(&rt.state).first() == Some(&0), "j marks the first file's hunk");
        type_key(&mut rt, "j");
        assert!(marked(&rt.state).first() == Some(&1), "j again goes on to the next hunk, though nothing scrolled");
        type_key(&mut rt, "k");
        assert!(marked(&rt.state).first() == Some(&0), "k goes back");
        type_key(&mut rt, "s");
        assert!(marked(&rt.state).is_empty(), "a new layout starts over");
        type_key(&mut rt, "j");
        assert!(marked(&rt.state).first() == Some(&0), "j marks the first file's hunk in two columns");
        type_key(&mut rt, "]");
        assert!(marked(&rt.state).first() == Some(&1), "] marks the next file's first hunk");
        type_key(&mut rt, "[");
        assert!(marked(&rt.state).first() == Some(&0));
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
    fn a_failed_save_leaves_the_diff_up_and_says_so() {
        let (mut rt, dir) = app("save");
        // A directory where the list should be: saving what the next diff's reading marks fails.
        std::fs::remove_file(dir.join("state/parole/reviewed")).unwrap();
        std::fs::create_dir_all(dir.join("state/parole/reviewed")).unwrap();
        type_key(&mut rt, "N");
        rt.frame(1.0 / 60.0);
        rt.frame(1.0 / 60.0);
        assert!(rt.state.notice.is_some());
        assert_eq!(rt.state.error, None);
        assert_eq!(paths(&rt.state), ["a.txt", "b.txt"], "the diff is still shown");
        assert!(rt.find("notice").is_some(), "and the window says so");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Draws frames until `done`, for what another thread brings (a signature, the agent).
    fn until(rt: &mut Runtime<State>, what: &str, done: impl Fn(&State) -> bool) {
        for _ in 0..2000 {
            rt.frame(1.0 / 60.0);
            if done(&rt.state) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("never happened: {what}");
    }

    fn script(dir: &std::path::Path, name: &str, body: &str) -> std::ffi::OsString {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n[ -n \"$PAROLE_PROBE\" ] && exit 0\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Another test's thread may have forked while this file was open for writing; until
        // that child execs, running the script fails with "text file busy". Wait it out.
        for _ in 0..200 {
            match std::process::Command::new(&path).env("PAROLE_PROBE", "1").status() {
                Ok(_) => return path.into_os_string(),
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        panic!("{} never became runnable", path.display());
    }

    /// Adds to the test repository a ref whose one commit brings a build script, a direnv
    /// file and an executable beside ordinary source.
    fn risky(repo: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        build(repo, &["checkout", "-q", "-b", "tooling", "main"]);
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::create_dir_all(repo.join("tools")).unwrap();
        std::fs::write(repo.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        std::fs::write(repo.join("build.rs"), "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
        std::fs::write(repo.join(".envrc"), "use flake\n").unwrap();
        std::fs::write(repo.join("tools/gen"), "echo\n").unwrap();
        std::fs::set_permissions(repo.join("tools/gen"), std::fs::Permissions::from_mode(0o755)).unwrap();
        // -f: the user's own ignore file (which a null global config doesn't switch off) may list .envrc.
        build(repo, &["add", "-A", "-f"]);
        build(repo, &["commit", "-q", "-m", "tooling"]);
        build(repo, &["update-ref", "refs/jail/zz-tooling", "HEAD"]);
        build(repo, &["checkout", "-q", "main"]);
    }

    #[test]
    fn files_the_host_would_run_come_first_and_say_why() {
        let (mut rt, dir) = app("runs");
        risky(&dir.join("repo"));
        type_key(&mut rt, "u");
        press(&mut rt, "item-4");
        assert_eq!(subject(&rt.state), "tooling");
        // As git lists them: .envrc, build.rs, src/lib.rs, tools/gen. The plain one goes last.
        assert_eq!(paths(&rt.state), [".envrc", "build.rs", "tools/gen", "src/lib.rs"]);
        let why: Vec<&str> = rt.state.flags.iter().map(|f| f.unwrap_or("")).collect();
        assert!(why[0].contains("direnv") && why[1].contains("cargo") && why[2].contains("executable") && why[3].is_empty(), "{why:?}");
        assert!(rt.find("file-1").is_some(), "the outline under the commit lists build.rs");
        // The sidebar's count arrives from the worker.
        let sha = rt.state.selected().unwrap().1.unwrap().sha.clone();
        until(&mut rt, "the commit's mark", |s| s.marks.contains_key(&sha));
        assert_eq!(rt.state.marks[&sha], 3);
        let first = rt.state.refs[1].commits[0].sha.clone();
        until(&mut rt, "an ordinary commit's mark", |s| s.marks.contains_key(&first));
        assert_eq!(rt.state.marks[&first], 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn answered(s: &State) -> bool {
        s.report().is_some_and(|a| !a.waiting && (a.report.is_some() || a.failed.is_some()))
    }

    fn notes(s: &State) -> Vec<(usize, u32, String)> {
        let report = s.report().and_then(|a| a.report.as_ref());
        report.map_or(Vec::new(), |r| r.findings.iter().map(|f| (f.file, f.line, f.why.clone())).collect())
    }

    /// A stand-in for the model. It notes each call in `calls`, keeps what it was given in
    /// `seen`, waits for the file `go` to exist, then prints `reply`.
    fn stand_in(dir: &std::path::Path, name: &str, reply: &str) -> std::ffi::OsString {
        let (calls, seen, go) = (dir.join("calls"), dir.join("seen"), dir.join("go"));
        script(dir, name, &format!("echo x >> '{}'\ncat > '{}'\nwhile [ ! -e '{}' ]; do sleep 0.01; done\nprintf '{reply}'\n", calls.display(), seen.display(), go.display()))
    }

    fn calls(dir: &std::path::Path) -> usize {
        std::fs::read_to_string(dir.join("calls")).map_or(0, |c| c.lines().count())
    }

    #[test]
    fn the_agents_notes_land_on_their_lines() {
        let (mut rt, dir) = app("agent");
        risky(&dir.join("repo"));
        // In the patch: 1 .envrc, 2 build.rs, 3 src/lib.rs, 4 tools/gen.
        rt.state.agent.program = stand_in(&dir, "agent.sh", "FLAG\\t2\\t2\\tPrints at build.\\nFLAG\\t1\\t0\\tRuns on cd.\\nFLAG\\t9\\t1\\tNo such file.\\nFLAG\\t3\\t1\\tPlain source.\\nEND\\n");
        type_key(&mut rt, "u");
        press(&mut rt, "item-4");
        // Not asked on its own here (the tests' window has that off), and nothing was sent.
        assert!(rt.state.report().is_none());
        assert_eq!(calls(&dir), 0);
        type_key(&mut rt, "ф");
        // While the model thinks, the diff is up and its keys work.
        until(&mut rt, "the question to go out", |_| calls(&dir) == 1);
        assert!(rt.state.report().is_some_and(|a| a.waiting));
        assert_eq!(paths(&rt.state), [".envrc", "build.rs", "tools/gen", "src/lib.rs"]);
        type_key(&mut rt, "s");
        assert_eq!(rt.state.diff.mode(), DiffMode::Split);
        std::fs::write(dir.join("go"), "").unwrap();
        until(&mut rt, "the agent's answer", answered);
        assert!(rt.state.report().unwrap().report.as_ref().unwrap().complete);
        assert_eq!(notes(&rt.state).len(), 3, "a finding about a file the patch doesn't have is dropped");
        let sent = std::fs::read_to_string(dir.join("seen")).unwrap();
        assert!(sent.contains("=== file 2 ===\ndiff --git a/build.rs b/build.rs"), "{sent}");
        assert!(sent.contains("     2 +    println!(\"hi\");"), "the patch goes with its line numbers:\n{sent}");
        // The notes follow their files through the reorder: build.rs is second in the patch
        // and second on screen, src/lib.rs third in the patch and last on screen.
        // A note on a whole file goes on the file's first line: none is left without a place.
        let at: Vec<(usize, LineRef)> = rt.state.diff.annotations().iter().map(|a| (a.file, a.at)).collect();
        assert_eq!(at, [(1, LineRef::new(Side::New, 2)), (0, LineRef::new(Side::New, 1)), (3, LineRef::new(Side::New, 1))]);
        assert_eq!(rt.state.file_levels(), [(1, 0), (1, 0), (0, 0), (1, 0)]);
        // Another commit has no notes; coming back, these are still there, and nothing is sent again.
        type_key(&mut rt, "p");
        assert!(rt.state.diff.annotations().is_empty());
        type_key(&mut rt, "n");
        assert_eq!(rt.state.diff.annotations().len(), 3);
        assert_eq!(calls(&dir), 1);
        let sha = rt.state.selected().unwrap().1.unwrap().sha.clone();
        assert_eq!(rt.state.levels[&sha], (3, 0));
        // m walks the notes and opens the one it lands on.
        type_key(&mut rt, "m");
        assert_eq!(rt.state.open_note.as_deref(), Some("0"));
        type_key(&mut rt, "ь");
        assert_eq!(rt.state.open_note.as_deref(), Some("1"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn each_diff_is_asked_about_once_and_a_fresh_look_only_adds() {
        let (mut rt, dir) = app("agent-auto");
        std::fs::write(dir.join("go"), "").unwrap();
        rt.state.agent.program = stand_in(&dir, "agent.sh", "FLAG\\t1\\t1\\tFirst look.\\nEND\\n");
        rt.state.auto_agent = true;
        // Shown, so asked: no key needed.
        type_key(&mut rt, "N");
        until(&mut rt, "the agent's answer", answered);
        assert_eq!(notes(&rt.state).len(), 1);
        // With a whole change on screen, each of its commits is asked about too, unopened:
        // what came and went between them shows on their stops.
        let work = rt.state.refs[1].clone();
        until(&mut rt, "the look ahead", |s| work.commits.iter().all(|c| s.levels.contains_key(&c.sha)));
        assert_eq!(calls(&dir), 3);
        assert_eq!(rt.state.ref_level(&work), (3, 0));
        // Left and come back to: answered already.
        type_key(&mut rt, "P");
        until(&mut rt, "the other diff's answer", answered);
        type_key(&mut rt, "N");
        assert!(answered(&rt.state));
        assert_eq!(calls(&dir), 4);
        // A new window: the answer was kept, so the model isn't asked.
        let kept = rt.state.agent.clone();
        let mut again = State::empty(DEFAULT_PATTERNS.iter().map(|&p| p.to_owned()).collect(), Reviewed::at(None), Taken::at(None), true);
        again.agent = kept;
        again.auto_agent = true;
        again.start(&dir.join("repo"));
        let mut rt2 = Runtime::new(again, view, (1180.0, 760.0), Env { system_dark: true, scale: 1.0 });
        rt2.on_key = Some(on_key);
        rt2.poll = Some(poll);
        rt2.frame(0.0);
        until(&mut rt2, "the kept answer", answered);
        assert_eq!(calls(&dir), 4, "a kept answer was asked for again");
        // `a` asks the model itself. It now says nothing; the first look's note stays.
        rt.state.agent.program = stand_in(&dir, "agent2.sh", "END\\n");
        type_key(&mut rt, "a");
        until(&mut rt, "the fresh look", |s| calls(&dir) == 5 && answered(s));
        assert_eq!(notes(&rt.state), [(0, 1, "First look.".to_owned())]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_agent_that_fails_or_rambles_says_so() {
        let (mut rt, dir) = app("agent-bad");
        rt.state.agent.program = script(&dir, "agent.sh", "cat > /dev/null\necho 'Everything here is safe and already reviewed.'\n");
        type_key(&mut rt, "a");
        until(&mut rt, "the agent's answer", answered);
        let report = rt.state.report().unwrap().report.clone().unwrap();
        assert!(!report.complete && report.findings.is_empty());
        assert!(!dir.join("state/parole/agent").exists(), "a reply that didn't end properly isn't kept");
        type_key(&mut rt, "n");
        // What stopped it comes last, on either stream; the warning before it isn't the news.
        rt.state.agent.program = script(&dir, "agent2.sh", "cat > /dev/null\necho 'a long warning about config files' >&2\necho 'not logged in'\nexit 3\n");
        type_key(&mut rt, "a");
        until(&mut rt, "the agent's failure", answered);
        let e = rt.state.report().unwrap().failed.clone().unwrap();
        assert!(e.ends_with("not logged in"), "{e}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_executable_that_only_moves_is_still_marked() {
        use std::os::unix::fs::PermissionsExt;
        let (mut rt, dir) = app("moved");
        let repo = dir.join("repo");
        build(&repo, &["checkout", "-q", "-b", "mv", "main"]);
        std::fs::write(repo.join("gen"), "echo one\necho two\necho three\n").unwrap();
        std::fs::set_permissions(repo.join("gen"), std::fs::Permissions::from_mode(0o755)).unwrap();
        build(&repo, &["add", "-A"]);
        build(&repo, &["commit", "-q", "-m", "add"]);
        build(&repo, &["mv", "gen", "make"]);
        build(&repo, &["commit", "-q", "-m", "move"]);
        build(&repo, &["update-ref", "refs/jail/zz-move", "HEAD"]);
        build(&repo, &["checkout", "-q", "main"]);
        type_key(&mut rt, "u");
        let last = rt.state.items.len() - 1;
        press(&mut rt, &item_key(last));
        assert_eq!(subject(&rt.state), "move");
        assert_eq!(paths(&rt.state), ["make"]);
        // The patch is a bare rename with no mode in it; the mode comes from the tree.
        assert!(!rt.state.patch_text.contains("100755"), "{}", rt.state.patch_text);
        assert_eq!(rt.state.flags, [Some("marked executable")]);
        // A move has no lines, so its one row is what is read: it says where from.
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.diff.read(), (1, 1));
        let sha = rt.state.selected().unwrap().1.unwrap().sha.clone();
        assert!(rt.state.reviewed.has(&sha));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_alarm_marks_its_ref_and_comes_first() {
        let (mut rt, dir) = app("alarm");
        std::fs::write(dir.join("go"), "").unwrap();
        rt.state.agent.program = stand_in(&dir, "agent.sh", "FLAG\\t1\\t1\\tA note.\\nALARM\\t2\\t1\\tSends a key away.\\nEND\\n");
        type_key(&mut rt, "N");
        type_key(&mut rt, "a");
        until(&mut rt, "the agent's answer", answered);
        let work = rt.state.refs[1].clone();
        assert_eq!(rt.state.ref_level(&work), (1, 1));
        assert_eq!(rt.state.file_levels(), [(1, 0), (0, 1)]);
        assert_eq!(rt.state.line_levels(), HashMap::from([((0, 1), false), ((1, 1), true)]));
        // The alarm is the first note m goes to, though the model listed it second.
        type_key(&mut rt, "m");
        assert_eq!(rt.state.open_note.as_deref(), Some("1"));
        assert!(rt.state.finding("1").unwrap().alarm);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn enter(rt: &mut Runtime<State>) {
        rt.key(KeyEvent { key: Key::Enter, mods: Mods::default() });
        rt.frame(1.0 / 60.0);
    }

    #[test]
    fn a_ref_is_taken_only_once_read_and_then_becomes_a_signed_branch() {
        let (mut rt, dir) = app("take-ui");
        let repo = dir.join("repo");
        crate::git::testing::signing(&dir, &repo);
        // On "first" of jail/work, with "second" never opened: the take says why not.
        press(&mut rt, "item-2");
        type_key(&mut rt, "t");
        assert!(rt.state.take.is_none());
        assert!(rt.state.notice.as_deref().is_some_and(|n| n.contains("1 of its 2 commits not read")), "{:?}", rt.state.notice);
        press(&mut rt, "take");
        assert!(rt.state.take.is_none());
        assert_eq!(build(&repo, &["for-each-ref", "refs/heads/reviewed"]), "", "a refused take makes no branch");
        // "second" opened and so read: the take goes ahead, with no question first.
        type_key(&mut rt, "n");
        press(&mut rt, "take");
        assert!(rt.state.notice.is_none(), "{:?}", rt.state.notice);
        assert!(rt.state.take.as_ref().is_some_and(|t| t.phase == Phase::Running));
        // While signatures are made nothing else moves.
        type_key(&mut rt, "p");
        assert_eq!(subject(&rt.state), "second");
        let taken = |s: &State| s.take.is_none() && s.branches.contains_key("refs/jail/work");
        until(&mut rt, "the take", taken);
        assert_eq!(rt.state.branches.get("refs/jail/work").map(String::as_str), Some("reviewed/jail/work"));
        let tip = build(&repo, &["rev-parse", "refs/heads/reviewed/jail/work"]);
        assert!(build(&repo, &["cat-file", "commit", "refs/heads/reviewed/jail/work"]).contains("gpgsig "));
        assert_eq!(build(&repo, &["rev-list", "--count", "main..refs/heads/reviewed/jail/work"]), "2");
        // Taking makes the branch and stops: the checkout is where it was.
        assert_eq!(build(&repo, &["symbolic-ref", "HEAD"]), "refs/heads/main");
        assert!(rt.find("switch").is_some() && rt.find("take").is_none());
        // A branch someone has moved since is not the take's: neither Enter nor z touches it.
        build(&repo, &["update-ref", "refs/heads/reviewed/jail/work", "main"]);
        enter(&mut rt);
        assert!(rt.state.notice.as_deref().is_some_and(|n| n.contains("no longer where the take left it")), "{:?}", rt.state.notice);
        assert_eq!(build(&repo, &["symbolic-ref", "HEAD"]), "refs/heads/main");
        build(&repo, &["update-ref", "refs/heads/reviewed/jail/work", &tip]);
        type_key(&mut rt, "u");
        build(&repo, &["update-ref", "refs/heads/reviewed/jail/work", "main"]);
        type_key(&mut rt, "z");
        assert!(rt.state.notice.as_deref().is_some_and(|n| n.contains("Couldn't undo the take")), "{:?}", rt.state.notice);
        assert_eq!(build(&repo, &["rev-parse", "refs/heads/reviewed/jail/work"]), build(&repo, &["rev-parse", "main"]));
        build(&repo, &["update-ref", "refs/heads/reviewed/jail/work", &tip]);
        type_key(&mut rt, "u");
        // Nor one another working tree has checked out.
        let other = dir.join("other");
        build(&repo, &["worktree", "add", "-q", other.to_str().unwrap(), "reviewed/jail/work"]);
        type_key(&mut rt, "z");
        assert!(rt.state.notice.as_deref().is_some_and(|n| n.contains("is checked out in")), "{:?}", rt.state.notice);
        build(&repo, &["worktree", "remove", "--force", other.to_str().unwrap()]);
        // z takes the take back: the branch goes, the signed copies stay.
        type_key(&mut rt, "я");
        assert!(rt.state.branches.is_empty());
        assert_eq!(build(&repo, &["for-each-ref", "refs/heads/reviewed"]), "");
        // Taken again, the same copies are used: the branch is where it was.
        type_key(&mut rt, "t");
        until(&mut rt, "the second take", taken);
        assert_eq!(build(&repo, &["rev-parse", "refs/heads/reviewed/jail/work"]), tip);
        // Enter switches to it, and only then.
        enter(&mut rt);
        assert_eq!(rt.state.notice, None);
        assert_eq!(build(&repo, &["symbolic-ref", "HEAD"]), "refs/heads/reviewed/jail/work");
        assert_eq!(rt.state.head.as_deref(), Some("reviewed/jail/work"));
        // The branch the checkout is on isn't deleted from under it.
        press(&mut rt, "item-3");
        type_key(&mut rt, "z");
        assert!(rt.state.notice.as_deref().is_some_and(|n| n.contains("You are on")));
        assert_eq!(build(&repo, &["rev-parse", "refs/heads/reviewed/jail/work"]), tip);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_whole_change_read_is_taken_as_one_commit() {
        let (mut rt, dir) = app("take-one-ui");
        let repo = dir.join("repo");
        crate::git::testing::signing(&dir, &repo);
        // The ref's own row: its whole change, which fits the window and so is read.
        press(&mut rt, "item-1");
        rt.frame(1.0 / 60.0);
        let work = rt.state.refs[1].clone();
        assert_eq!(rt.state.way(&work), Some(Way::One));
        press(&mut rt, "take");
        let taken = |s: &State| s.take.is_none() && s.branches.contains_key("refs/jail/work");
        until(&mut rt, "the take", taken);
        // One signed commit on main with the ref's files; its own commits are not in it.
        assert_eq!(build(&repo, &["rev-list", "--count", "main..refs/heads/reviewed/jail/work"]), "1");
        assert_eq!(build(&repo, &["rev-parse", "refs/heads/reviewed/jail/work^{tree}"]), build(&repo, &["rev-parse", "refs/jail/work^{tree}"]));
        assert!(build(&repo, &["cat-file", "commit", "refs/heads/reviewed/jail/work"]).contains("gpgsig "));
        assert!(rt.state.took_one.contains("refs/jail/work"));
        assert!(work.commits.iter().all(|c| !rt.state.reviewed.has(&c.sha)), "the commits were not read, and are not marked so");
        // z takes it back like any take.
        type_key(&mut rt, "z");
        assert!(rt.state.branches.is_empty(), "{:?}", rt.state.notice);
        assert_eq!(build(&repo, &["for-each-ref", "refs/heads/reviewed"]), "");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_empty_commit_is_read_once_its_page_is_drawn() {
        let (mut rt, dir) = app("empty");
        let repo = dir.join("repo");
        build(&repo, &["checkout", "-q", "-b", "e", "main"]);
        build(&repo, &["commit", "-q", "--allow-empty", "-m", "nothing"]);
        build(&repo, &["update-ref", "refs/jail/zz-empty", "HEAD"]);
        build(&repo, &["checkout", "-q", "main"]);
        type_key(&mut rt, "u");
        let last = rt.state.items.len() - 1;
        let sha = rt.state.refs.last().unwrap().commits[0].sha.clone();
        // Selected and landed, but not yet drawn: not read.
        rt.update(|s, tx| s.select(last, tx));
        assert!(!rt.state.loading && rt.state.diff.files().is_empty());
        assert!(!rt.state.reviewed.has(&sha));
        // Drawn, and asked after (by the next poll, or by Take itself): read.
        rt.frame(1.0 / 60.0);
        rt.update(|s, _| s.take_now());
        assert!(rt.state.reviewed.has(&sha));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_dropped_ref_is_struck_out_until_it_is_put_back() {
        let (mut rt, dir) = app("drop");
        let repo = dir.join("repo");
        let was = build(&repo, &["rev-parse", "refs/devvm/one"]);
        // On "from the vm": x drops its ref.
        type_key(&mut rt, "ч");
        assert_eq!(build(&repo, &["for-each-ref", "refs/devvm"]), "");
        assert_eq!(rt.state.items.len(), 3);
        assert_eq!(rt.state.dropped.len(), 1);
        assert!(rt.find("undrop-devvm/one").is_some(), "it is still drawn, struck out");
        assert_eq!(subject(&rt.state), "all of jail/work");
        // z puts it back where it was.
        type_key(&mut rt, "z");
        assert_eq!(build(&repo, &["rev-parse", "refs/devvm/one"]), was);
        assert!(rt.state.dropped.is_empty());
        assert_eq!(rt.state.items.len(), 4);
        // Dropped again, the button on its struck row does the same.
        press(&mut rt, "item-0");
        press(&mut rt, "drop");
        assert_eq!(rt.state.dropped.len(), 1);
        press(&mut rt, "undrop-devvm/one");
        assert_eq!(build(&repo, &["rev-parse", "refs/devvm/one"]), was);
        // Dropped, made again elsewhere, dropped again: z puts back the last one dropped.
        press(&mut rt, "item-0");
        type_key(&mut rt, "x");
        let work = build(&repo, &["rev-parse", "refs/jail/work"]);
        build(&repo, &["update-ref", "refs/devvm/one", &work]);
        type_key(&mut rt, "u");
        press(&mut rt, "item-0");
        assert!(rt.state.selected().is_some_and(|(r, _)| r.name == "refs/devvm/one" && r.tip == work));
        type_key(&mut rt, "x");
        assert_eq!(rt.state.dropped.len(), 2);
        type_key(&mut rt, "z");
        assert_eq!(build(&repo, &["rev-parse", "refs/devvm/one"]), work);
        assert_eq!(rt.state.dropped.len(), 1);
        // A ref that names another ref is left alone: it could only come back as a plain one.
        build(&repo, &["symbolic-ref", "refs/jail/alias", "refs/jail/work"]);
        let git = Repo::open(&repo).unwrap();
        assert!(git.delete_ref("refs/jail/alias", &work).is_err());
        assert_eq!(build(&repo, &["symbolic-ref", "refs/jail/alias"]), "refs/jail/work");
        assert!(git.delete_ref("refs/jail/w\u{fffd}rk", &work).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_folder_snapshot_shows_what_jail_diff_prints() {
        let dir = scratch("snapshot");
        let jail = script(
            &dir,
            "jail.sh",
            "[ \"$1\" = diff ] || exit 9\ncat <<'EOF'\n .envrc | 1 +\n 1 file changed\ndiff --git a/.envrc b/.envrc\nnew file mode 100644\nindex 0000000..1111111\n--- /dev/null\n+++ b/.envrc\n@@ -0,0 +1 @@\n+use flake\nEOF\n",
        );
        let state = State::snapshot_with(&dir, true, Some(jail), false);
        let mut rt = Runtime::new(state, view, (1180.0, 760.0), Env { system_dark: true, scale: 1.0 });
        rt.on_key = Some(on_key);
        rt.poll = Some(poll);
        rt.frame(0.0);
        assert_eq!(rt.state.error, None);
        assert_eq!(paths(&rt.state), [".envrc"]);
        assert!(rt.state.flags[0].is_some());
        // Nothing to take or drop: the strip says what to do instead, and the keys change nothing.
        type_key(&mut rt, "t");
        type_key(&mut rt, "x");
        assert!(rt.state.take.is_none() && rt.state.dropped.is_empty());
        assert!(rt.find("take").is_none() && rt.find("drop").is_none());
        // A jail that fails is an error on screen, not an empty diff.
        let state = State::snapshot_with(&dir, true, Some(script(&dir, "nojail.sh", "echo 'jail: no snapshot of this folder' >&2\nexit 1\n")), false);
        assert!(state.error.as_deref().is_some_and(|e| e.contains("no snapshot")), "{:?}", state.error);
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
