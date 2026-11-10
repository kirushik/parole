//! refview: read what a sandbox wrote before bringing it in.
//!
//! Lists the refs under `refs/jail/` and `refs/devvm/` of a repository, the commits each has
//! that HEAD lacks, and shows the selected commit (or the whole ref) as a diff, with the files
//! the host would run marked (`risk.rs`) and a model's notes on what looks dangerous
//! (`agent.rs`). Reviewing only reads: see `git.rs` for the exact commands. A ref
//! whose commits are all marked read can be taken: copied, signed, as a new local branch
//! (`take.rs`).
//!
//! `refview [DIR] [--refs PATTERN]...` opens the window on the repository at DIR (default: the
//! current directory). `--refs` replaces the default patterns and may repeat.
//! `refview --snapshot DIR` shows what `jail diff DIR` prints for a folder that isn't a
//! repository, with the same marks and notes.
//! Rune's headless flags work too: `refview DIR --snap out.png`, `--tree tree.json`,
//! `--script keys.json`, `--scene split`.

mod agent;
mod git;
mod proc;
mod reviewed;
mod risk;
mod take;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use git::{Commit, RefInfo, Repo};
use reviewed::Reviewed;
use rune::{App, AppSpec, Cx, El, Key, KeyEvent, Latest, ScrollSpec, Tx, Waker, col, el, row, text};
use rune_kit::diff::{Annotation, DiffMode, DiffState, DiffView, FileDiff, FileStatus, LineRef, Side, parse};
use take::{Step, Taken};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, SegmentedControl};

const DEFAULT_PATTERNS: &[&str] = &["refs/jail", "refs/devvm"];
/// How long a selection waits for its patch before the window draws without it. Short enough
/// to go unnoticed, long enough that an ordinary commit never flashes "Reading".
/// `REFVIEW_WAIT=1` waits until git answers, with the patch or an error, so a headless still
/// is never taken mid-read.
const QUICK: Duration = Duration::from_millis(20);
const KEYS: &str = "n / p  commit     j / k  hunk     ] / [  file     s  split     w  wrap     r  reviewed     a  ask again     t  take     u  reload";

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
    eprintln!("refview: {problem}\nusage: refview [DIR] [--refs PATTERN]... [--no-agent]\n       refview --snapshot [DIR] [--no-agent]");
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
    /// How many notes the agent left on each commit, by hash, for the sidebar.
    noted: HashMap<String, usize>,
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
    /// jail's program (`jail`, or `REFVIEW_JAIL`).
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

struct Answer {
    patch: Arc<str>,
    /// The commit the diff is of, when it is one commit's.
    sha: Option<String>,
    result: Result<agent::Report, String>,
}

/// At most this many questions are out at once; a diff shown while they are isn't asked
/// about on its own (`a` asks).
const ASKING_AT_ONCE: usize = 3;

/// A take in the window: the ref as it was when asked, and how far the signing has got.
struct TakeView {
    r: RefInfo,
    branch: String,
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
    /// Asked, not started. `Some` says why it can't start.
    Confirm(Option<String>),
    Running,
    Done { tip: String, count: usize },
    Failed(String),
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
        let patient = std::env::var_os("REFVIEW_WAIT").is_some_and(|v| !v.is_empty());
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
        State {
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
            snapshot: None,
            patch_text: Arc::from(""),
            order: Vec::new(),
            flags: Vec::new(),
            marks: HashMap::new(),
            marking: Latest::default(),
            reports: HashMap::new(),
            noted: HashMap::new(),
            answers: channel(),
            asking: 0,
            agent: agent::Agent::from_env(),
            auto_agent: false,
            taken: Arc::new(Mutex::new(taken)),
            branches: HashMap::new(),
            take: None,
            waker: None,
            jail_program: std::env::var_os("REFVIEW_JAIL").filter(|p| !p.is_empty()).unwrap_or_else(|| "jail".into()),
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
        // The copies are looked up under the lock; git is asked about them after it is let go.
        let copies: Vec<(usize, String)> = {
            let taken = self.taken.lock().unwrap_or_else(|e| e.into_inner());
            self.refs.iter().enumerate().filter_map(|(i, r)| Some((i, taken.get(&r.tip)?.to_owned()))).collect()
        };
        self.branches = copies.iter().filter_map(|(i, copy)| Some((self.refs[*i].name.clone(), take::taken_as(repo, copy, &self.refs[*i])?))).collect();
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

    /// Puts the agent's notes for this diff under the lines they are about.
    fn annotate(&mut self) {
        let notes = match self.report().and_then(|a| a.report.as_ref()) {
            Some(report) => report
                .findings
                .iter()
                .enumerate()
                .filter(|(_, f)| f.line > 0)
                .filter_map(|(i, f)| Some(Annotation::new(self.file_of(f)?, LineRef::new(Side::New, f.line), i.to_string())))
                .collect(),
            None => Vec::new(),
        };
        self.diff.set_annotations(notes);
    }

    /// Asks the agent about the diff on screen, on a thread of its own: the window goes on
    /// with what it has, and the notes land when they come. `fresh` asks the model even when
    /// an answer to this very diff was kept.
    fn ask_agent(&mut self, fresh: bool) {
        if self.loading || self.diff.files().is_empty() {
            return;
        }
        let asked = self.reports.entry(self.patch_text.clone()).or_default();
        if asked.waiting {
            return;
        }
        asked.waiting = true;
        asked.failed = None;
        self.asking += 1;
        let sha = self.selected().and_then(|(_, c)| Some(c?.sha.clone()));
        let (agent, patch, send, waker) = (self.agent.clone(), self.patch_text.clone(), self.answers.0.clone(), self.waker.clone());
        std::thread::spawn(move || {
            let result = agent.ask(&patch, fresh);
            let _ = send.send(Answer { patch, sha, result });
            if let Some(wake) = &waker {
                wake();
            }
        });
    }

    /// Files an answer under its diff. What an earlier look found stays.
    fn answered(&mut self, answer: Answer) {
        self.asking = self.asking.saturating_sub(1);
        let asked = self.reports.entry(answer.patch.clone()).or_default();
        asked.waiting = false;
        match answer.result {
            Ok(mut new) => {
                if let Some(old) = asked.report.take() {
                    let more: Vec<_> = old.findings.into_iter().filter(|f| !new.findings.contains(f)).collect();
                    new.findings.extend(more);
                    new.dropped = new.dropped.max(old.dropped);
                }
                if let Some(sha) = answer.sha {
                    self.noted.insert(sha, new.findings.len());
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

    /// Opens the take for the selection's ref: a question first, nothing is signed yet.
    fn ask_take(&mut self) {
        // One take at a time, and its panel is left with Esc, not replaced.
        if self.take.is_some() {
            return;
        }
        if self.snapshot.is_some() {
            self.notice = Some("A folder snapshot has no commits to take. Apply it with jail apply.".to_owned());
            return;
        }
        let Some((r, _)) = self.selected() else { return };
        let r = r.clone();
        let branch = take::branch_for(&r);
        let unread = r.commits.iter().filter(|c| !self.reviewed.has(&c.sha)).count();
        let blocked = if let Some(branch) = self.branches.get(&r.name) {
            Some(format!("Already taken: the branch {branch} is its signed copy."))
        } else if r.error.is_some() || r.commits.is_empty() {
            Some("There is nothing on this ref to take.".to_owned())
        } else if r.truncated {
            Some(format!("This ref has more than {} commits; only the listed ones can be read here, so it can't be taken here.", git::MAX_COMMITS))
        } else if unread > 0 {
            Some(format!("{unread} of its {} aren't marked reviewed yet. Read them and press r on each first.", plural(r.commits.len(), "commit")))
        } else {
            None
        };
        self.take = Some(TakeView { r, branch, phase: Phase::Confirm(blocked), signed: HashMap::new(), signing: None, steps: None, cancel: Arc::default() });
    }

    /// Starts the signing, on its own thread: each signature may wait for a touch.
    fn start_take(&mut self) {
        let (Some(repo), Some(t)) = (self.repo.clone(), self.take.as_mut()) else { return };
        if !matches!(t.phase, Phase::Confirm(None) | Phase::Failed(_)) {
            return;
        }
        let (send, steps) = channel();
        t.phase = Phase::Running;
        t.signing = None;
        t.steps = Some(steps);
        t.cancel = Arc::default();
        let (r, branch, cancel) = (t.r.clone(), t.branch.clone(), t.cancel.clone());
        let (reviewed, taken, waker) = (self.reviewed.all(), self.taken.clone(), self.waker.clone());
        std::thread::spawn(move || {
            take::run(&repo, &r, &branch, &reviewed, &taken, &cancel, &mut |step| {
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
        let mut finished = false;
        for step in steps.try_iter() {
            changed = true;
            match step {
                Step::Signing { sha, .. } => t.signing = Some(sha),
                Step::Signed { sha, reused } => {
                    t.signing = None;
                    t.signed.insert(sha, reused);
                }
                Step::Done { tip, count, .. } => {
                    t.phase = Phase::Done { tip, count };
                    finished = true;
                }
                Step::Failed(e) => {
                    t.signing = None;
                    t.phase = Phase::Failed(if e == git::CANCELLED { "Stopped. The signatures made so far are kept.".to_owned() } else { e });
                    finished = true;
                }
            }
        }
        if finished {
            t.steps = None;
            if matches!(t.phase, Phase::Done { .. }) {
                // The sidebar now says the ref is taken.
                self.reload();
            }
        }
        changed
    }

    /// Escape in a take: stop the signing, or close the panel when nothing is running.
    fn leave_take(&mut self) {
        match &self.take {
            Some(t) if t.phase == Phase::Running => t.cancel.store(true, Ordering::Relaxed),
            _ => self.take = None,
        }
    }

    /// Asks a worker for the selection's patch. The last selection's diff goes at once, so the
    /// header never names one commit over another's changes. Most patches are back within
    /// [`QUICK`] and land before the next frame; a slower one shows "Reading" until `poll`
    /// takes it.
    fn load(&mut self) {
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
                self.annotate();
                // The second reader starts on its own; the diff is already up.
                if self.auto_agent && self.report().is_none() && self.asking < ASKING_AT_ONCE {
                    self.ask_agent(false);
                }
            }
            Err(e) => self.error = Some(e),
        }
        true
    }

    fn select(&mut self, i: usize, tx: &mut Tx) {
        // Nothing under a take moves: its commits and their marks are as they were asked about.
        if self.take.is_some() || i >= self.items.len() || i == self.sel {
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

    fn step(&mut self, forward: bool, tx: &mut Tx) {
        let to = if forward { self.sel + 1 } else { self.sel.wrapping_sub(1) };
        self.select(to, tx);
    }

    fn is_reviewed(&self, it: Item) -> bool {
        let r = &self.refs[it.r];
        match it.pick {
            // A folder snapshot has no commits: nothing there is marked.
            Pick::All => !r.commits.is_empty() && r.commits.iter().all(|c| self.reviewed.has(&c.sha)),
            Pick::Commit(c) => self.reviewed.has(&r.commits[c].sha),
        }
    }

    /// Marks the selection read, or unread when it already is. A whole ref marks its commits.
    fn toggle_reviewed(&mut self) {
        if self.take.is_some() {
            return;
        }
        if self.snapshot.is_some() {
            self.notice = Some("A folder snapshot has no commits to mark. Apply it with jail apply once you've read it.".to_owned());
            return;
        }
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
    let count = match s.snapshot {
        Some(_) => "a folder's jail snapshot".to_owned(),
        None => format!("{} to review", plural(s.refs.iter().filter(|r| !r.commits.is_empty()).count(), "ref")),
    };
    let head = col()
        .px(12.0)
        .py(10.0)
        .gap(2.0)
        .child(text(title).font(k.ty.ui_medium).color(k.c.text_base).truncate())
        .child(text(count).font(k.ty.text_small).color(k.c.text_muted));
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
        if let Some(branch) = s.branches.get(&info.name) {
            list = list.child(row().w_full().px(10.0).pb(4.0).child(text(format!("Taken as branch {branch}")).font(k.ty.text_small).color(k.c.success.icon).truncate()));
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
    let (title, detail, runs) = match it.pick {
        Pick::All if s.snapshot.is_some() => ("All changes".to_owned(), "committed or not".to_owned(), 0),
        Pick::All => ("All changes".to_owned(), plural(info.commits.len(), "commit"), 0),
        Pick::Commit(c) => {
            let c = &info.commits[c];
            (c.subject.clone(), format!("{}  {}  {}", c.short, c.author, c.date), s.marks.get(&c.sha).copied().unwrap_or(0))
        }
    };
    let flagged = id.1.as_ref().and_then(|sha| s.noted.get(sha)).copied().unwrap_or(0);
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
        .children(chips(runs, flagged, k))
}

/// What a commit's row says beside its title: how many of its files the host would run, and
/// how many notes the agent left on it.
fn chips(runs: usize, flagged: usize, k: &Kit) -> Vec<El> {
    let chip = |label: String, says: String| {
        row().px(5.0).py(1.0).radius(4.0).fill(k.c.warning.bg).label(says).child(text(label).font(k.ty.text_xsmall_medium).color(k.c.warning.fg))
    };
    let mut out = Vec::new();
    if runs > 0 {
        out.push(chip(format!("runs {runs}"), format!("{} the host would run", plural(runs, "file"))));
    }
    if flagged > 0 {
        out.push(chip(format!("agent {flagged}"), format!("{} from the agent", plural(flagged, "note"))));
    }
    out
}

fn header(s: &State, k: &Kit) -> El {
    let Some((info, commit)) = s.selected() else {
        return row().key("bar").w_full().h(56.0).fill(k.c.layer_01);
    };
    let (title, meta, message) = match commit {
        None if s.snapshot.is_some() => (info.name.clone(), "what jail diff shows  ·  apply it with jail apply".to_owned(), ""),
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
            tx.instant();
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
    let taken = s.branches.contains_key(&info.name);
    let take = Button::new(if taken { "Taken" } else { "Take as branch" })
        .variant(ButtonVariant::Ghost)
        .control_size(ControlSize::Regular)
        .on_press_tx(|s: &mut State, tx: &mut Tx| {
            s.ask_take();
            tx.instant();
            tx.blur();
        })
        .build(k)
        .key("take");
    let mut top = row().w_full().gap(14.0).items_center().child(titles).child(stats).child(modes.build(k));
    if s.snapshot.is_none() {
        top = top.child(mark).child(take);
    }
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
    if let Some(t) = &s.take {
        return take_panel(t, k);
    }
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
    let warn = k.c.warning;
    let small = k.ty.text_small;
    let diff = DiffView::new(|s: &State| &s.diff, |s: &mut State| &mut s.diff)
        // Why a file is marked, at the end of its header.
        .header_actions(move |s: &State, file| match s.flags.get(file).copied().flatten() {
            Some(why) => row().px(6.0).py(1.0).radius(4.0).fill(warn.bg).child(text(why.to_owned()).font(small).color(warn.fg)),
            None => row(),
        })
        // The agent's note, under the line it is about.
        .annotation(move |s: &State, a: &Annotation| {
            let report = s.report().and_then(|a| a.report.as_ref());
            let why = report.and_then(|r| r.findings.get(a.id.parse::<usize>().ok()?)).map_or("", |f| f.why.as_str());
            row()
                .w_full()
                .px(12.0)
                .py(6.0)
                .gap(8.0)
                .fill(warn.bg)
                .child(text("Agent".to_owned()).font(small).color(warn.icon))
                .child(text(why.to_owned()).font(small).color(warn.fg).wrap().grow(1.0).min_w(0.0))
        })
        .build(s, k);
    col().key("review").grow(1.0).min_h(0.0).w_full().child(strip(s, k)).child(diff)
}

/// Above the diff: the files in it the host would run, and what the agent said. All of both,
/// in a box that scrolls once it is taller than a third of a small window.
fn strip(s: &State, k: &Kit) -> El {
    let line = |t: String, ink| text(t).font(k.ty.text_small).color(ink).wrap();
    let entry = |path: String, why: &str, press: El| {
        press
            .w_full()
            .px(6.0)
            .py(2.0)
            .gap(8.0)
            .radius(4.0)
            .hover(k.c.hover)
            .child(text(path).font(k.ty.text_small_medium).color(k.c.text_base).truncate())
            .child(text(why.to_owned()).font(k.ty.text_small).color(k.c.text_muted).truncate().grow(1.0).min_w(0.0))
    };
    let mut out = col().w_full().px(16.0).py(8.0).gap(6.0);
    let runs: Vec<(usize, risk::Why)> = s.flags.iter().enumerate().filter_map(|(i, f)| Some((i, (*f)?))).collect();
    if !runs.is_empty() {
        let mut box_ = col().key("runs").w_full().gap(1.0).child(line(format!("{} here would run on this machine, or change what does:", plural(runs.len(), "file")), k.c.warning.icon));
        for &(i, why) in &runs {
            let path = s.diff.files()[i].path().to_owned();
            box_ = box_.child(entry(path, why, row().key(format!("run-{i}")).on_press_tx(move |s: &mut State, tx: &mut Tx| {
                tx.instant();
                s.diff.scroll_to_file(i, tx);
            })));
        }
        out = out.child(box_);
    }
    let mut agent = col().key("agent").w_full().gap(1.0);
    let asked = s.report();
    match asked.and_then(|a| a.report.as_ref()) {
        None if asked.is_some_and(|a| a.waiting) => agent = agent.child(line("The agent is reading this diff\u{2026}".to_owned(), k.c.text_muted)),
        None if asked.is_some_and(|a| a.failed.is_some()) => {}
        None => agent = agent.child(line("The agent hasn't read this diff. a  asks it; that sends the diff to Claude.".to_owned(), k.c.text_faint)),
        Some(r) => {
            agent = agent.child(match r.findings.len() {
                // Not "safe": it looked and marked nothing, which a patch can also arrange.
                0 => line("The agent flagged nothing. That is not a clearance.".to_owned(), k.c.text_muted),
                n => line(format!("The agent left {}:", plural(n, "note")), k.c.warning.icon),
            });
            for (i, f) in r.findings.iter().enumerate() {
                let (file, at) = (s.file_of(f), f.line);
                let path = file.map_or_else(|| format!("file {}", f.file + 1), |pos| s.diff.files()[pos].path().to_owned());
                let place = if at > 0 { format!("{path}:{at}") } else { path };
                agent = agent.child(entry(place, &f.why, row().key(format!("note-{i}")).on_press_tx(move |s: &mut State, tx: &mut Tx| {
                    tx.instant();
                    let Some(file) = file else { return };
                    match at {
                        0 => s.diff.scroll_to_file(file, tx),
                        at => s.diff.scroll_to_line(file, LineRef::new(Side::New, at), tx),
                    }
                })));
            }
            if r.dropped > 0 {
                agent = agent.child(line(format!("It had {} more, past the {} that are kept.", r.dropped, agent::MAX_FINDINGS), k.c.danger.icon));
            }
            if !r.complete {
                agent = agent.child(line("Its reply was cut short or not in the form asked for: there may be more it didn't get to say.".to_owned(), k.c.danger.icon));
            }
            if asked.is_some_and(|a| a.waiting) {
                agent = agent.child(line("Looking again\u{2026}".to_owned(), k.c.text_muted));
            }
        }
    }
    if let Some(e) = asked.and_then(|a| a.failed.as_ref()) {
        agent = agent.child(line(format!("The agent didn't answer: {e}"), k.c.danger.icon));
    }
    col().key("strip").w_full().max_h(240.0).fill(k.c.layer_01).scroll(ScrollSpec { stick_bottom: false }).child(out.child(agent))
}

/// The one thing in the window that moves on its own: the mark beside the commit whose
/// signature is being waited for. It is there to pull the eye to the key, and every frame of
/// it is the same mark, brighter or dimmer.
const WAITING: rune::Cycle = rune::Cycle { period: 1.2, phase: 0.0, stops: &[(0.0, 1.0), (0.5, 0.3), (1.0, 1.0)], still: 1.0, fps: 30.0 };

/// The take, in place of the diff: the question, then each signature as it is made.
fn take_panel(t: &TakeView, k: &Kit) -> El {
    let n = t.r.commits.len();
    let line = |txt: String, ink| text(txt).font(k.ty.text_small).color(ink).wrap();
    let made = t.signed.len();
    let (title, detail) = match &t.phase {
        Phase::Confirm(_) => (format!("Take {} as the branch {}?", t.r.short(), t.branch), format!(
            "{} copied with you as committer and signed with your key, one signature each. The author, message and files of each stay as they are. Nothing is checked out or merged.",
            if n == 1 { "Its commit is".to_owned() } else { format!("Its {n} commits are") }
        )),
        Phase::Running => match &t.signing {
            Some(_) => (format!("Signature {} of {n}: touch your key", made + 1), "Waiting for the signing program. If your key is blinking, it wants a touch.".to_owned()),
            None => ("Signing\u{2026}".to_owned(), String::new()),
        },
        Phase::Done { tip, count } => (format!("Branch {} created", t.branch), format!(
            "{} at {}. It is yours to switch to, push or merge: git switch {}",
            plural(*count, "signed commit"),
            &tip[..tip.len().min(9)],
            t.branch
        )),
        Phase::Failed(_) => (format!("{} wasn't taken", t.r.short()), "No branch was made.".to_owned()),
    };
    let mut panel = col().key("take-panel").w_full().max_w(640.0).gap(10.0).child(text(title).key("take-title").font(k.ty.ui_medium).color(k.c.text_base).wrap());
    if !detail.is_empty() {
        panel = panel.child(line(detail, k.c.text_muted));
    }
    match &t.phase {
        Phase::Confirm(Some(why)) => panel = panel.child(line(why.clone(), k.c.danger.icon).key("take-blocked")),
        Phase::Failed(e) => panel = panel.child(line(e.clone(), k.c.danger.icon).key("take-error")),
        _ => {}
    }
    let mut list = col().w_full().gap(1.0).pt(4.0);
    for c in &t.r.commits {
        let waiting = t.signing.as_deref() == Some(&c.sha);
        let mark = el().size(8.0, 8.0).radius(4.0);
        let (mark, says) = match t.signed.get(&c.sha) {
            Some(true) => (mark.fill(k.c.success.icon), "signed earlier"),
            Some(false) => (mark.fill(k.c.success.icon), "signed"),
            None if waiting => (mark.fill(k.c.warning.icon).cycle(WAITING), "touch your key"),
            None => (mark.border(1.0, k.c.text_faint), ""),
        };
        let ink = if waiting { k.c.text_base } else { k.c.text_muted };
        list = list.child(
            row()
                .w_full()
                .px(8.0)
                .py(4.0)
                .gap(10.0)
                .items_center()
                .radius(6.0)
                .fill(if waiting { k.c.layer_03 } else { k.c.bg_base })
                .child(mark)
                .child(text(c.subject.clone()).font(k.ty.ui).color(ink).truncate().grow(1.0).min_w(0.0))
                .child(text(says.to_owned()).font(k.ty.text_small).color(if waiting { k.c.warning.icon } else { k.c.text_faint }))
                .child(text(c.short.clone()).font(k.ty.text_xsmall).color(k.c.text_faint)),
        );
    }
    let hint = match &t.phase {
        Phase::Confirm(None) => "Enter  sign and create the branch     Esc  back to the diff",
        Phase::Confirm(Some(_)) => "Esc  back to the diff",
        Phase::Running => "Esc  stop (signatures already made are kept)",
        Phase::Done { .. } => "Esc  back to the diff",
        Phase::Failed(_) => "Enter  try again     Esc  back to the diff",
    };
    let panel = panel.child(list).child(text(hint.to_owned()).key("take-hint").font(k.ty.text_xsmall).color(k.c.text_faint));
    col().key("take").grow(1.0).w_full().min_h(0.0).padding(32.0).items_center().scroll(ScrollSpec { stick_bottom: false }).child(panel)
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
    if s.take.is_some() {
        // The take has the keyboard until it is closed: no key reaches the diff under it.
        tx.instant();
        match ev.key {
            Key::Enter => s.start_take(),
            Key::Escape => s.leave_take(),
            _ => {}
        }
        return true;
    }
    if let Key::Text(t) = &ev.key
        && !(m.cmd || m.ctrl || m.alt)
    {
        tx.instant();
        match shortcut(t) {
            "n" => s.step(true, tx),
            "p" => s.step(false, tx),
            "s" => s.toggle_split(tx),
            "w" => s.diff.set_wrap(!s.diff.wrap()),
            "r" => s.toggle_reviewed(),
            // Again, and from the model itself: a kept answer isn't enough for whoever presses this.
            "a" => s.ask_agent(true),
            "t" => s.ask_take(),
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
        let mut state = State::empty(patterns, reviewed, Taken::at(Some(dir.join("state/refview/taken"))), true);
        // No test asks the real model or keeps answers in the user's own state directory.
        state.agent = agent::Agent { program: "false".into(), cache: Some(dir.join("state/refview/agent")) };
        state.start(&dir.join("repo"));
        let mut rt = Runtime::new(state, view, (1180.0, 760.0), Env { system_dark: true, scale: 1.0 });
        rt.on_key = Some(on_key);
        rt.poll = Some(poll);
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

    /// Whether the window is still moving a frame after `t` was typed.
    fn moves_after(rt: &mut Runtime<State>, t: &str) -> bool {
        type_key(rt, t);
        rt.frame(1.0 / 60.0)
    }

    #[test]
    fn a_key_lands_in_one_frame() {
        let (mut rt, dir) = app("one-frame");
        // Another commit, split, wrap, a mark, back, a hunk, a file: none leaves anything in motion.
        for t in ["n", "n", "s", "w", "r", "p", "j", "]", "s", "t"] {
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
        std::fs::write(&path, format!("#!/bin/sh\n[ -n \"$REFVIEW_PROBE\" ] && exit 0\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Another test's thread may have forked while this file was open for writing; until
        // that child execs, running the script fails with "text file busy". Wait it out.
        for _ in 0..200 {
            match std::process::Command::new(&path).env("REFVIEW_PROBE", "1").status() {
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
        assert!(rt.find("run-1").is_some(), "the strip lists build.rs");
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
    fn the_agents_notes_land_under_their_lines() {
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
        let at: Vec<(usize, LineRef)> = rt.state.diff.annotations().iter().map(|a| (a.file, a.at)).collect();
        assert_eq!(at, [(1, LineRef::new(Side::New, 2)), (3, LineRef::new(Side::New, 1))]);
        assert!(rt.find("note-1").is_some(), "the whole-file note is in the strip");
        // Another commit has no notes; coming back, these are still there, and nothing is sent again.
        type_key(&mut rt, "p");
        assert!(rt.state.diff.annotations().is_empty());
        type_key(&mut rt, "n");
        assert_eq!(rt.state.diff.annotations().len(), 2);
        assert_eq!(calls(&dir), 1);
        // The marks didn't move: nothing the agent says reviews a commit.
        let sha = rt.state.selected().unwrap().1.unwrap().sha.clone();
        assert!(!rt.state.reviewed.has(&sha));
        assert_eq!(rt.state.noted[&sha], 3);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn each_diff_is_asked_about_once_and_a_fresh_look_only_adds() {
        let (mut rt, dir) = app("agent-auto");
        std::fs::write(dir.join("go"), "").unwrap();
        rt.state.agent.program = stand_in(&dir, "agent.sh", "FLAG\\t1\\t1\\tFirst look.\\nEND\\n");
        rt.state.auto_agent = true;
        // Shown, so asked: no key needed.
        type_key(&mut rt, "n");
        until(&mut rt, "the agent's answer", answered);
        assert_eq!((calls(&dir), notes(&rt.state).len()), (1, 1));
        // Left and come back to: answered already.
        type_key(&mut rt, "p");
        until(&mut rt, "the other diff's answer", answered);
        type_key(&mut rt, "n");
        assert!(answered(&rt.state));
        assert_eq!(calls(&dir), 2);
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
        assert_eq!(calls(&dir), 2, "a kept answer was asked for again");
        // `a` asks the model itself. It now says nothing; the first look's note stays.
        rt.state.agent.program = stand_in(&dir, "agent2.sh", "END\\n");
        type_key(&mut rt, "a");
        until(&mut rt, "the fresh look", |s| calls(&dir) == 3 && answered(s));
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
        assert!(!dir.join("state/refview/agent").exists(), "a reply that didn't end properly isn't kept");
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
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_ref_is_taken_only_once_read_and_then_becomes_a_signed_branch() {
        let (mut rt, dir) = app("take-ui");
        let repo = dir.join("repo");
        crate::git::testing::signing(&dir, &repo);
        // On "first" of jail/work, with nothing marked: the take says why not, and Enter does nothing.
        press(&mut rt, "item-2");
        type_key(&mut rt, "t");
        let Some(TakeView { phase: Phase::Confirm(Some(why)), .. }) = &rt.state.take else { panic!("no blocked take") };
        assert!(why.contains("2 of its 2 commits"), "{why}");
        rt.key(KeyEvent { key: Key::Enter, mods: Mods::default() });
        rt.frame(1.0 / 60.0);
        assert!(matches!(rt.state.take.as_ref().unwrap().phase, Phase::Confirm(Some(_))));
        // While it is up, the diff's keys do nothing, and neither do presses on what is
        // still drawn around it: another row, the mark, the take itself.
        type_key(&mut rt, "n");
        assert_eq!(subject(&rt.state), "first");
        press(&mut rt, "item-3");
        assert_eq!(subject(&rt.state), "first");
        press(&mut rt, "mark");
        assert!(!rt.state.reviewed.has(&rt.state.selected().unwrap().1.unwrap().sha));
        press(&mut rt, "take");
        assert!(matches!(rt.state.take.as_ref().unwrap().phase, Phase::Confirm(Some(_))));
        rt.key(KeyEvent { key: Key::Escape, mods: Mods::default() });
        rt.frame(1.0 / 60.0);
        assert!(rt.state.take.is_none());
        // Mark both, ask again, confirm.
        type_key(&mut rt, "r");
        type_key(&mut rt, "n");
        type_key(&mut rt, "r");
        press(&mut rt, "take");
        assert_eq!(rt.state.take.as_ref().unwrap().phase, Phase::Confirm(None));
        assert_eq!(build(&repo, &["for-each-ref", "refs/heads/reviewed"]), "", "asking makes no branch");
        rt.key(KeyEvent { key: Key::Enter, mods: Mods::default() });
        until(&mut rt, "the take", |s| s.take.as_ref().is_some_and(|t| !matches!(t.phase, Phase::Running)));
        let t = rt.state.take.as_ref().unwrap();
        let Phase::Done { tip, count } = &t.phase else { panic!("{:?}", t.phase) };
        assert_eq!(*count, 2);
        assert_eq!(t.signed.len(), 2);
        assert_eq!(build(&repo, &["rev-parse", "refs/heads/reviewed/jail/work"]), *tip);
        assert!(build(&repo, &["cat-file", "commit", "refs/heads/reviewed/jail/work"]).contains("gpgsig "));
        assert_eq!(build(&repo, &["symbolic-ref", "HEAD"]), "refs/heads/main");
        assert_eq!(rt.state.branches.get("refs/jail/work").map(String::as_str), Some("reviewed/jail/work"));
        rt.key(KeyEvent { key: Key::Escape, mods: Mods::default() });
        rt.frame(1.0 / 60.0);
        // Asked again, it says the ref is taken.
        type_key(&mut rt, "t");
        assert!(matches!(&rt.state.take.as_ref().unwrap().phase, Phase::Confirm(Some(why)) if why.contains("Already taken")));
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
        // Nothing to mark or take: both say so and change nothing.
        type_key(&mut rt, "r");
        assert!(rt.state.notice.as_deref().is_some_and(|n| n.contains("jail apply")));
        type_key(&mut rt, "t");
        assert!(rt.state.take.is_none());
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
