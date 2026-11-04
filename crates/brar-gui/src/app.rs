//! The B-Rar window: progress, "file already exists", password and result pages, talking to
//! the extraction (or the archive being created) running on a worker thread; and the "Add to
//! archive" page that comes before creating one. Standard Windows controls; a timer polls the
//! worker ten times a second.

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, Mutex, mpsc},
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime},
};

use brar_core::{Conflict, ConflictPolicy, CreateOptions, Decision, Error, Options, PartFinder, Progress, Resolution, Summary};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::*,
    },
    core::w,
};

use crate::{
    Mode, shell,
    theme::{self, Tone},
    win::{self, Builder, Fonts, IDCANCEL, IDOK, Role},
};

const ID_PROGRESS: i32 = 100;
const ID_AMOUNT: i32 = 101;
const ID_FILES: i32 = 104;
const ID_CURRENT: i32 = 105;
const ID_PAUSE: i32 = 106;
const ID_SKIP: i32 = 107;
const ID_RENAME: i32 = 108;
const ID_ALL: i32 = 109;
const ID_PASSWORD: i32 = 110;
const ID_SHOW: i32 = 111;
const ID_PATH: i32 = 120;
const ID_LEVEL: i32 = 121; // ..= 124, one per LEVELS entry
const ID_ENCRYPT: i32 = 125;
const ID_PW1: i32 = 126;
const ID_PW2: i32 = 127;
const ID_PROBLEM: i32 = 128;
const ID_ELAPSED: i32 = 112;
const ID_LEFT: i32 = 113;
const ID_SPEED: i32 = 114;

// Status glyphs (Segoe Fluent Icons / MDL2 Assets).
const GLYPH_OK: u16 = 0xEC61;
const GLYPH_ERROR: u16 = 0xEB90;
const GLYPH_WARNING: u16 = 0xE7BA;
const GLYPH_LOCK: u16 = 0xE72E;
const TIMER: usize = 1;

/// Page margins, at 100% scaling.
const MARGIN_X: i32 = 24;
const MARGIN_Y: i32 = 20;

pub fn run(archive: PathBuf, mode: Mode, password: Option<String>) -> ExitCode {
    win::init_controls();
    win::register(w!("BRarProgress"), Some(wndproc));
    // SAFETY: a registered class; the window's state lives in APP until WM_DESTROY.
    let hwnd = unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        CreateWindowExW(
            WS_EX_CONTROLPARENT,
            w!("BRarProgress"),
            w!("B-Rar"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
    };
    let Ok(hwnd) = hwnd else {
        shell::error_box("B-Rar", "Could not open the B-Rar window.");
        return ExitCode::FAILURE;
    };
    theme::dress_window(hwnd);
    let app = App::new(hwnd, archive, mode);
    theme::set_fonts(app.fonts.paint());
    APP.with(|a| *a.borrow_mut() = Some(app));
    with(|a| a.begin(password));
    // SAFETY: the window just created.
    unsafe {
        SetTimer(Some(hwnd), TIMER, 100, None);
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    win::run_loop(hwnd, |_| false);
    if SUCCEEDED.get() { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static SUCCEEDED: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f` on the window's state, unless it's already in use further up the stack (a dialog
/// opened from a button pumps messages while the click is still being handled).
fn with<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|app| app.try_borrow_mut().ok()?.as_mut().map(f))
}

/// State shared with the worker thread.
#[derive(Default)]
struct Shared {
    progress: Mutex<Arc<Progress>>,
    outcome: Mutex<Option<brar_core::Result<Summary>>>,
    conflict: Mutex<Option<Ask>>,
}

/// A pending "file already exists" question; the worker blocks until `reply` answers.
struct Ask {
    path: PathBuf,
    new_size: Option<u64>,
    new_modified: Option<SystemTime>,
    old_size: Option<u64>,
    old_modified: Option<SystemTime>,
    reply: mpsc::Sender<Decision>,
}

enum Page {
    /// Creating an archive: its name, level and password first.
    Add,
    Running,
    Conflict(Ask),
    Password {
        wrong: bool,
    },
    Done {
        summary: Summary,
        secs: f64,
    },
    Failed(String),
}

impl Page {
    /// The page's width (without margins), at 100% scaling; the height follows the content.
    fn width(&self) -> i32 {
        match self {
            Page::Conflict(_) | Page::Add => 492,
            Page::Password { .. } => 432,
            Page::Running | Page::Done { .. } | Page::Failed(_) => 472,
        }
    }
}

/// Elapsed time minus pauses, the speed, and a steady "time left". Decoding comes in bursts
/// (big and small files, disk writes), so the speed over the last moment swings a lot: it's
/// averaged over about 5 s, and the time left moves at most once a second, easing toward the
/// new estimate and counting down in between.
struct Clock {
    started: Instant,
    paused_since: Option<Instant>,
    paused_total: Duration,
    samples: VecDeque<(Instant, u64)>,
    /// Bytes per second, smoothed.
    speed: f64,
    /// The time left last shown, and when.
    left: Option<(Instant, Duration)>,
}

impl Clock {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            paused_since: None,
            paused_total: Duration::ZERO,
            samples: VecDeque::new(),
            speed: 0.0,
            left: None,
        }
    }

    fn active(&self) -> Duration {
        let paused = self.paused_total + self.paused_since.map_or(Duration::ZERO, |p| p.elapsed());
        self.started.elapsed().saturating_sub(paused)
    }

    fn sample(&mut self, done: u64) {
        let now = Instant::now();
        let since = self.samples.back().map(|&(t, _)| now.duration_since(t));
        self.samples.push_back((now, done));
        while self.samples.len() > 2 && now.duration_since(self.samples[0].0) > Duration::from_millis(1500) {
            self.samples.pop_front();
        }
        let window = match (self.samples.front(), self.samples.back()) {
            (Some(&(t0, b0)), Some(&(t1, b1))) if t1 > t0 => (b1 - b0) as f64 / (t1 - t0).as_secs_f64(),
            _ => return,
        };
        // An average with a ~5 s time constant; the first measurement starts it.
        self.speed = match since {
            Some(dt) if self.speed > 0.0 => {
                let weight = 1.0 - (-dt.as_secs_f64() / 5.0).exp();
                self.speed + (window - self.speed) * weight
            }
            _ => window,
        };
    }

    fn rate(&self) -> f64 {
        self.speed
    }

    /// The time left for `remaining` bytes, as shown: refreshed once a second, each refresh
    /// moving a third of the way to the new estimate, counting down in between.
    fn left(&mut self, remaining: u64) -> Option<Duration> {
        if self.speed <= 0.0 {
            return None;
        }
        let now = Instant::now();
        let estimate = Duration::from_secs_f64(remaining as f64 / self.speed);
        let shown = match self.left {
            Some((at, shown)) if now.duration_since(at) < Duration::from_secs(1) => return Some(shown.saturating_sub(now.duration_since(at))),
            Some((at, shown)) => {
                let counted = shown.saturating_sub(now.duration_since(at));
                counted.mul_f64(2.0 / 3.0) + estimate.mul_f64(1.0 / 3.0)
            }
            None => estimate,
        };
        self.left = Some((now, shown));
        Some(shown)
    }
}

struct App {
    hwnd: HWND,
    archive: PathBuf,
    name: String,
    mode: Mode,
    fonts: Fonts,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    page: Page,
    /// The current page's controls.
    built: win::Page,
    clock: Clock,
    /// The password the job last ran with, to run it again into another folder.
    last_password: Option<String>,
    /// Failed on a folder only administrators can write to (the Store version can't ask):
    /// the failure page offers another folder.
    protected: bool,
    /// Placed on the screen yet (the first page centres the window; later ones keep its centre).
    placed: bool,
}

/// Compression levels the "Add to archive" page offers: name, Deflate level.
const LEVELS: [(&str, u32); 4] = [("Store", 0), ("Fastest", 1), ("Normal", 6), ("Best", 9)];

impl App {
    fn new(hwnd: HWND, archive: PathBuf, mode: Mode) -> Self {
        Self {
            hwnd,
            name: archive.file_name().unwrap_or_default().to_string_lossy().into_owned(),
            archive,
            mode,
            fonts: Fonts::new(),
            shared: Arc::default(),
            worker: None,
            page: Page::Running,
            built: win::Page::default(),
            clock: Clock::new(),
            last_password: None,
            protected: false,
            placed: false,
        }
    }

    /// The first page: "Add to archive", or straight to work.
    fn begin(&mut self, password: Option<String>) {
        if matches!(self.mode, Mode::Create { ask: true, .. }) {
            self.page = Page::Add;
            self.show();
        } else {
            self.start(password);
        }
    }

    fn dest(&self) -> Option<&Path> {
        match &self.mode {
            Mode::Extract { dest, .. } => Some(dest),
            Mode::Test | Mode::Create { .. } => None,
        }
    }

    fn creating(&self) -> bool {
        matches!(self.mode, Mode::Create { .. })
    }

    fn progress(&self) -> Arc<Progress> {
        Arc::clone(&self.shared.progress.lock().unwrap())
    }

    fn close(&self) {
        // SAFETY: posting to this window.
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }

    /// (Re)starts the job, e.g. after the user typed a password.
    fn start(&mut self, password: Option<String>) {
        self.last_password = password.clone();
        self.protected = false;
        let progress = Arc::new(Progress::default());
        *self.shared.progress.lock().unwrap() = Arc::clone(&progress);
        self.clock = Clock::new();
        self.page = Page::Running;
        self.show();

        let ask = {
            let shared = Arc::clone(&self.shared);
            move |c: &Conflict<'_>| {
                let (reply, answer) = mpsc::channel();
                let old = std::fs::metadata(c.path).ok();
                *shared.conflict.lock().unwrap() = Some(Ask {
                    path: c.path.to_path_buf(),
                    new_size: c.new_size,
                    new_modified: c.new_modified,
                    old_size: old.as_ref().map(|m| m.len()),
                    old_modified: old.and_then(|m| m.modified().ok()),
                    reply,
                });
                answer.recv().unwrap_or(Decision {
                    resolution: Resolution::Cancel,
                    apply_to_all: false,
                })
            }
        };
        let opts = Options {
            password,
            progress: Some(progress),
            on_conflict: ConflictPolicy::Ask(Arc::new(ask)),
            find_part: PartFinder::new(shell::pick_part),
            selection: match &self.mode {
                Mode::Extract { selection, .. } => selection.clone(),
                Mode::Test | Mode::Create { .. } => None,
            },
            ..Options::default()
        };
        let (shared, archive, mode) = (Arc::clone(&self.shared), self.archive.clone(), self.mode.clone());
        self.worker = Some(std::thread::spawn(move || {
            let result = match &mode {
                Mode::Extract { dest, .. } => brar_core::extract(&archive, dest, &opts),
                Mode::Test => brar_core::test(&archive, &opts),
                Mode::Create {
                    inputs,
                    level,
                    password,
                    overwrite,
                    ..
                } => {
                    let create = CreateOptions {
                        level: *level,
                        password: password.clone(),
                        progress: opts.progress.clone(),
                        overwrite: *overwrite,
                        ..Default::default()
                    };
                    brar_core::create(&archive, inputs, &create)
                }
            };
            *shared.outcome.lock().unwrap() = Some(result);
        }));
    }

    fn answer(&mut self, resolution: Resolution) {
        let all = win::checked(win::item(self.hwnd, ID_ALL));
        if let Page::Conflict(ask) = std::mem::replace(&mut self.page, Page::Running) {
            let _ = ask.reply.send(Decision {
                resolution,
                apply_to_all: all,
            });
        }
        self.show();
    }

    /// Stops the worker (answering any open question with Cancel) and waits for it.
    fn shutdown(&mut self) {
        self.progress().cancel();
        if let Some(ask) = self.shared.conflict.lock().unwrap().take() {
            let _ = ask.reply.send(Decision {
                resolution: Resolution::Cancel,
                apply_to_all: true,
            });
        }
        if let Page::Conflict(ask) = std::mem::replace(&mut self.page, Page::Running) {
            let _ = ask.reply.send(Decision {
                resolution: Resolution::Cancel,
                apply_to_all: true,
            });
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }

    /// Ten times a second: the worker's outcome or question, and the progress page's numbers.
    fn tick(&mut self) {
        let outcome = self.shared.outcome.lock().unwrap().take();
        if let Some(outcome) = outcome {
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
            self.finish(outcome);
            return;
        }
        let ask = if matches!(self.page, Page::Running) {
            self.shared.conflict.lock().unwrap().take()
        } else {
            None
        };
        if let Some(ask) = ask {
            self.page = Page::Conflict(ask);
            self.show();
            return;
        }
        if matches!(self.page, Page::Running) {
            self.update_running();
        }
    }

    fn finish(&mut self, outcome: brar_core::Result<Summary>) {
        self.page = match outcome {
            Ok(summary) => {
                SUCCEEDED.set(true);
                Page::Done {
                    summary,
                    secs: self.clock.active().as_secs_f64(),
                }
            }
            Err(Error::Cancelled) => return self.close(),
            Err(e @ (Error::PasswordRequired | Error::WrongPassword)) => Page::Password {
                wrong: matches!(e, Error::WrongPassword),
            },
            // Denied partway (one protected subfolder, say): the Store version can't ask for
            // administrator rights, so it says how to get them.
            Err(e)
                if e.is_access_denied()
                    && !shell::is_elevated()
                    && !crate::can_elevate()
                    && let Some(dest) = self.dest() =>
            {
                let message = crate::protected_folder(dest);
                self.protected = true;
                Page::Failed(message)
            }
            // Otherwise the same hand-over to an elevated copy as main.rs does up front.
            Err(e) if e.is_access_denied() && !shell::is_elevated() && self.dest().is_some() => match crate::elevate(&self.archive, &self.mode) {
                Ok(true) => {
                    SUCCEEDED.set(true);
                    return self.close();
                }
                Ok(false) => Page::Failed(crate::declined(self.dest().unwrap_or(Path::new("")))),
                Err(message) => Page::Failed(message),
            },
            Err(e) => Page::Failed(e.to_string()),
        };
        self.show();
    }

    // ---- pages -------------------------------------------------------------------------------

    /// Replaces the window's controls with the current page's, and fits the window to it.
    fn show(&mut self) {
        self.built.clear();
        let mut b = Builder::new(self.hwnd, &self.fonts, self.page.width());
        let title = match &self.page {
            Page::Add => self.add_page(&mut b),
            Page::Running => self.running_page(&mut b),
            Page::Conflict(ask) => conflict_page(&mut b, ask),
            Page::Password { wrong } => password_page(&mut b, &self.name, *wrong),
            Page::Done { summary, secs } => self.done_page(&mut b, *summary, *secs),
            Page::Failed(message) => self.failed_page(&mut b, message),
        };
        let (mx, my) = (b.px(MARGIN_X), b.px(MARGIN_Y));
        self.built = b.finish();
        self.built.place(mx, my);
        win::fit_client(self.hwnd, self.built.width + 2 * mx, self.built.height + 2 * my, !self.placed);
        self.placed = true;
        win::redraw(self.hwnd);
        win::set_title(self.hwnd, &title);
        // The field to type in, or the main button.
        let focus = [ID_PASSWORD, ID_PATH, IDOK.0, ID_PAUSE]
            .into_iter()
            .map(|id| win::item(self.hwnd, id))
            .find(|h| !h.is_invalid());
        if let Some(h) = focus {
            win::focus(h);
        }
        if matches!(self.page, Page::Add) {
            let path = win::item(self.hwnd, ID_PATH);
            let len = win::text(path).encode_utf16().count();
            win::send(path, win::EM_SETSEL, len, len as isize);
            self.validate();
        }
        if matches!(self.page, Page::Running) {
            self.update_running();
        }
    }

    fn running_page(&self, b: &mut Builder) -> String {
        let title = match (self.dest(), self.creating()) {
            (Some(dest), _) => format!("Extracting to {}", dest.display()),
            (None, true) => format!("Compressing to {}", self.name),
            (None, false) => format!("Testing {}", self.name),
        };
        let t = b.label(0, &title, Role::Normal, false);
        win::send(t, WM_SETFONT, self.fonts.heading.0 as usize, 1);
        b.space(10);
        b.label(ID_CURRENT, "", Role::Weak, false);
        b.space(4);
        b.progress(ID_PROGRESS, false);
        b.space(12);
        b.grid(&[
            [("Elapsed", ID_ELAPSED, ""), ("Processed", ID_AMOUNT, "")],
            [("Left", ID_LEFT, ""), ("Speed", ID_SPEED, "")],
            [("Files", ID_FILES, ""), ("", 0, "")],
        ]);
        b.space(10);
        b.buttons(&[(ID_PAUSE, "Pause"), (IDCANCEL.0, "Cancel")]);
        self.running_title(0, 0)
    }

    /// "27% Extracting name.zip", the window's title while working.
    fn running_title(&self, done: u64, total: u64) -> String {
        let verb = match (self.dest(), self.creating()) {
            (Some(_), _) => "Extracting",
            (None, true) => "Compressing",
            (None, false) => "Testing",
        };
        match total {
            0 => format!("{verb} {} - B-Rar", self.name),
            total => format!("{:.0}% {verb} {} - B-Rar", percent(done, total), self.name),
        }
    }

    /// The progress page's numbers, from the worker's counters.
    fn update_running(&mut self) {
        let p = self.progress();
        let (total, done) = (p.total_bytes(), p.done_bytes());
        let paused = p.is_paused();
        if !paused {
            self.clock.sample(done);
        }
        let item = |id| win::item(self.hwnd, id);
        let fraction = if total > 0 { done.min(total) as f64 / total as f64 } else { 0.0 };
        theme::set_bar(item(ID_PROGRESS), (fraction * 10_000.0) as u32, paused, false);

        let amount = if total > 0 {
            format!("{} of {}", human(done), human(total))
        } else {
            human(done)
        };
        win::set_text(item(ID_AMOUNT), &amount);
        let elapsed = self.clock.active();
        win::set_text(item(ID_ELAPSED), &clock(elapsed));
        let rate = if paused { 0.0 } else { self.clock.rate() };
        let speed = if paused {
            "Paused".to_owned()
        } else if rate > 0.0 {
            format!("{}/s", human(rate as u64))
        } else {
            String::new()
        };
        win::set_text(item(ID_SPEED), &speed);
        let left = (!paused && total > done && elapsed > Duration::from_secs(2))
            .then(|| self.clock.left(total - done))
            .flatten()
            .map(clock)
            .unwrap_or_default();
        win::set_text(item(ID_LEFT), &left);
        win::set_text(item(ID_FILES), &p.files().to_string());
        let current = p.current_file();
        win::set_text(item(ID_CURRENT), current.rsplit(['/', '\\']).next().unwrap_or_default());
        win::set_text(item(ID_PAUSE), if paused { "Resume" } else { "Pause" });
        let title = self.running_title(done, total);
        win::set_title(self.hwnd, &title);
    }

    fn toggle_pause(&mut self) {
        let p = self.progress();
        let paused = p.is_paused();
        p.set_paused(!paused);
        if paused {
            if let Some(since) = self.clock.paused_since.take() {
                self.clock.paused_total += since.elapsed();
            }
            self.clock.samples.clear();
            self.clock.left = None;
        } else {
            self.clock.paused_since = Some(Instant::now());
        }
        self.update_running();
    }

    fn done_page(&self, b: &mut Builder, s: Summary, secs: f64) -> String {
        if self.creating() {
            return self.created_page(b, s, secs);
        }
        let files = plural(s.files, "file");
        let title = if self.dest().is_some() {
            format!("Done: {files} extracted, no errors")
        } else {
            format!("No errors found in {files}")
        };
        b.header_glyph(GLYPH_OK, Tone::Ok, &title, &self.name);
        b.space(12);
        let speed = format!("{}/s", human((s.bytes as f64 / secs.max(1e-3)) as u64));
        b.grid(&[
            [("Size", 0, &human(s.bytes)), ("Time", 0, &seconds(secs))],
            [("Folders", 0, &s.dirs.to_string()), ("Speed", 0, &speed)],
        ]);
        let mut notes = Vec::new();
        if let Some(dest) = self.dest() {
            notes.push(format!("Saved to {}", dest.display()));
        }
        if s.kept_existing > 0 {
            notes.push(format!("kept {} existing files", s.kept_existing));
        }
        if s.skipped > 0 {
            notes.push(format!("skipped {} links", s.skipped));
        }
        if !notes.is_empty() {
            b.space(4);
            b.label(0, &notes.join(" · "), Role::Weak, false);
        }
        b.space(14);
        if self.dest().is_some() {
            b.buttons(&[(IDOK.0, "Open folder"), (IDCANCEL.0, "Close")]);
        } else {
            b.buttons(&[(IDOK.0, "Close")]);
        }
        "Done - B-Rar".into()
    }

    fn created_page(&self, b: &mut Builder, s: Summary, secs: f64) -> String {
        b.header_glyph(
            GLYPH_OK,
            Tone::Ok,
            &format!("Archive created: {}", self.name),
            &format!("in {}", folder(&self.archive)),
        );
        b.space(12);
        let packed = std::fs::metadata(&self.archive).map(|m| m.len()).unwrap_or(0);
        let ratio = if s.bytes > 0 { packed as f64 * 100.0 / s.bytes as f64 } else { 100.0 };
        b.grid(&[
            [
                ("Size", 0, &format!("{} → {}", human(s.bytes), human(packed))),
                ("Time", 0, &seconds(secs)),
            ],
            [("Files", 0, &s.files.to_string()), ("Ratio", 0, &format!("{ratio:.0}%"))],
        ]);
        if s.skipped > 0 {
            b.space(4);
            b.label(0, &format!("skipped {} links", s.skipped), Role::Weak, false);
        }
        b.space(14);
        b.buttons(&[(IDOK.0, "Show in folder"), (IDCANCEL.0, "Close")]);
        "Done - B-Rar".into()
    }

    fn failed_page(&self, b: &mut Builder, message: &str) -> String {
        let verb = match (self.dest(), self.creating()) {
            (Some(_), _) => "extract",
            (None, true) => "create",
            (None, false) => "test",
        };
        b.header_glyph(GLYPH_ERROR, Tone::Error, &format!("Couldn't {verb} the archive"), &self.name);
        b.space(12);
        b.label(0, message, Role::Normal, true);
        b.space(16);
        if self.protected {
            b.buttons(&[(IDOK.0, "Choose another folder"), (IDCANCEL.0, "Close")]);
        } else {
            b.buttons(&[(IDOK.0, "Close")]);
        }
        "Error - B-Rar".into()
    }

    fn add_page(&self, b: &mut Builder) -> String {
        let what = match &self.mode {
            Mode::Create { inputs, .. } => match inputs.as_slice() {
                [one] => one.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                many => format!("{} items", many.len()),
            },
            _ => String::new(),
        };
        let level = match &self.mode {
            Mode::Create { level, .. } => *level,
            _ => 6,
        };
        b.header(win::app_icon(b.px(40)), "Add to archive", &what);
        b.space(14);
        b.label(0, "Archive", Role::Weak, false);
        b.space(2);
        b.edit(ID_PATH, &self.archive.display().to_string(), 0, "", 0);
        b.space(12);
        b.label(0, "Compression", Role::Weak, false);
        b.space(2);
        let names: Vec<&str> = LEVELS.iter().map(|(n, _)| *n).collect();
        let on = LEVELS.iter().position(|(_, l)| *l == level).unwrap_or(2);
        b.radios(ID_LEVEL, &names, on);
        b.space(10);
        b.check(ID_ENCRYPT, "Encrypt with a password (AES-256)", false);
        b.space(4);
        b.edit(ID_PW1, "", win::ES_PASSWORD, "Password", 0);
        b.space(6);
        b.edit(ID_PW2, "", win::ES_PASSWORD, "Type it again", 0);
        b.space(8);
        b.label(ID_PROBLEM, "", Role::Weak, false);
        b.space(12);
        b.buttons(&[(IDOK.0, "Create"), (IDCANCEL.0, "Cancel")]);
        "Add to archive - B-Rar".into()
    }

    /// The "Add to archive" form's state: what's wrong with it, if anything (Create stays off),
    /// and whether the archive exists (Create becomes Replace).
    fn validate(&self) -> Option<(PathBuf, bool)> {
        let item = |id| win::item(self.hwnd, id);
        let path = PathBuf::from(win::text(item(ID_PATH)).trim());
        let encrypt = win::checked(item(ID_ENCRYPT));
        let (pw1, pw2) = (win::text(item(ID_PW1)), win::text(item(ID_PW2)));
        win::enable(item(ID_PW1), encrypt);
        win::enable(item(ID_PW2), encrypt);
        let problem = if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")) {
            Some("The archive's name must end in .zip.")
        } else if encrypt && pw1.is_empty() {
            Some("Enter a password.")
        } else if encrypt && pw1 != pw2 {
            Some("The two passwords don't match.")
        } else {
            None
        };
        let exists = path.exists();
        let label = item(ID_PROBLEM);
        // Don't scold before there's anything to scold: no "enter a password" while the
        // fields are still empty, no "don't match" before the second one is typed.
        match problem.filter(|p| !p.starts_with("Enter") && !(p.starts_with("The two") && pw2.is_empty())) {
            Some(p) => {
                win::set_text(label, p);
                win::set_role(label, Role::Error);
            }
            None => {
                win::set_text(
                    label,
                    if exists {
                        "An archive with this name exists; it will be replaced."
                    } else {
                        ""
                    },
                );
                win::set_role(label, Role::Weak);
            }
        }
        let ok = item(IDOK.0);
        win::enable(ok, problem.is_none());
        win::set_text(ok, if exists { "Replace" } else { "Create" });
        problem.is_none().then_some((path, exists))
    }

    fn create(&mut self) {
        let Some((path, exists)) = self.validate() else { return };
        let item = |id| win::item(self.hwnd, id);
        let level = (0..LEVELS.len())
            .find(|&i| win::checked(item(ID_LEVEL + i as i32)))
            .map_or(6, |i| LEVELS[i].1);
        let password = win::checked(item(ID_ENCRYPT)).then(|| win::text(item(ID_PW1)));
        let path = std::path::absolute(&path).unwrap_or(path);
        if let Mode::Create {
            level: l,
            password: pw,
            overwrite,
            ..
        } = &mut self.mode
        {
            *l = level;
            *pw = password;
            *overwrite = exists;
        }
        self.name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        self.archive = path;
        self.start(None);
    }

    // ---- buttons and keys --------------------------------------------------------------------

    fn command(&mut self, id: i32, code: u32) {
        let ok = id == IDOK.0;
        let cancel = id == IDCANCEL.0;
        match &self.page {
            Page::Running if cancel => self.progress().cancel(),
            Page::Running if id == ID_PAUSE => self.toggle_pause(),
            Page::Conflict(_) if ok => self.answer(Resolution::Overwrite),
            Page::Conflict(_) if id == ID_SKIP => self.answer(Resolution::Skip),
            Page::Conflict(_) if id == ID_RENAME => self.answer(Resolution::Rename),
            Page::Conflict(_) if cancel => self.answer(Resolution::Cancel),
            Page::Password { .. } if id == ID_SHOW => win::reveal(win::item(self.hwnd, ID_PASSWORD), win::checked(win::item(self.hwnd, ID_SHOW))),
            Page::Password { .. } if id == ID_PASSWORD && code == win::EN_CHANGE => {
                let typed = !win::text(win::item(self.hwnd, ID_PASSWORD)).is_empty();
                win::enable(win::item(self.hwnd, IDOK.0), typed);
            }
            Page::Password { .. } if ok => {
                let password = win::text(win::item(self.hwnd, ID_PASSWORD));
                if !password.is_empty() {
                    self.start(Some(password));
                }
            }
            Page::Add if ok => self.create(),
            Page::Add if (ID_PATH..=ID_PROBLEM).contains(&id) && (code == win::EN_CHANGE || code == win::BN_CLICKED) => {
                self.validate();
            }
            Page::Done { .. } if ok => {
                if self.creating() {
                    shell::show_in_folder(&self.archive);
                } else if let Some(dest) = self.dest() {
                    shell::open_path(dest);
                }
                self.close();
            }
            Page::Failed(_) if ok && self.protected => match shell::pick_folder(&self.archive, self.hwnd.0 as isize) {
                Ok(Some(folder)) => {
                    if let Mode::Extract { dest, .. } = &mut self.mode {
                        *dest = folder;
                    }
                    let password = self.last_password.clone();
                    self.start(password);
                }
                Ok(None) => {}
                Err(e) => {
                    self.page = Page::Failed(e);
                    self.show();
                }
            },
            Page::Password { .. } | Page::Add | Page::Done { .. } | Page::Failed(_) if ok || cancel => self.close(),
            _ => {}
        }
    }
}

fn conflict_page(b: &mut Builder, ask: &Ask) -> String {
    let file = ask.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let folder = ask.path.parent().map(|p| p.display().to_string()).unwrap_or_default();
    b.header_glyph(GLYPH_WARNING, Tone::Warn, &format!("The folder already has {file}"), &folder);
    b.space(12);
    b.rows(&[
        ("Replace", &describe(ask.old_size, ask.old_modified)),
        ("with", &describe(ask.new_size, ask.new_modified)),
    ]);
    b.space(6);
    b.check(ID_ALL, "Apply to all", false);
    b.space(12);
    b.buttons(&[(IDOK.0, "Yes"), (ID_SKIP, "No"), (ID_RENAME, "Rename"), (IDCANCEL.0, "Cancel")]);
    "Confirm file replace - B-Rar".into()
}

fn password_page(b: &mut Builder, name: &str, wrong: bool) -> String {
    b.header_glyph(GLYPH_LOCK, Tone::Accent, "Password required", &format!("{name} is encrypted"));
    b.space(14);
    b.edit(ID_PASSWORD, "", win::ES_PASSWORD, "Enter password", 0);
    b.space(6);
    b.check(ID_SHOW, "Show password", false);
    if wrong {
        b.space(4);
        b.label(0, "That password is incorrect. Try again.", Role::Error, false);
    }
    b.space(14);
    b.buttons(&[(IDOK.0, "OK"), (IDCANCEL.0, "Cancel")]);
    win::enable(win::item(b.parent, IDOK.0), false);
    "Password - B-Rar".into()
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CTLCOLORSTATIC => win::ctl_color(wparam, lparam),
        WM_CTLCOLOREDIT => win::edit_color(wparam),
        WM_DRAWITEM if theme::draw_item(lparam) => LRESULT(1),
        WM_COMMAND => {
            let (id, code) = ((wparam.0 & 0xFFFF) as i32, ((wparam.0 >> 16) & 0xFFFF) as u32);
            if code == win::BN_CLICKED && lparam.0 != 0 {
                theme::clicked(hwnd, HWND(lparam.0 as _));
            }
            with(|a| a.command(id, code));
            LRESULT(0)
        }
        WM_TIMER => {
            with(App::tick);
            LRESULT(0)
        }
        // For IsDialogMessage: Enter presses the default button.
        win::DM_GETDEFID => LRESULT((0x534B << 16) | IDOK.0 as isize),
        WM_CLOSE => {
            with(App::shutdown);
            win::destroy(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: the default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// The folder `path` is in, for display.
fn folder(path: &Path) -> String {
    path.parent().map(|p| p.display().to_string()).unwrap_or_default()
}

// ---- formatting ----------------------------------------------------------------------------

fn plural(n: u64, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}

fn percent(done: u64, total: u64) -> f64 {
    done.min(total) as f64 * 100.0 / total.max(1) as f64
}

fn describe(size: Option<u64>, modified: Option<SystemTime>) -> String {
    let size = size.map(human).unwrap_or_else(|| "unknown size".into());
    match modified.and_then(local_time) {
        Some(t) => format!("{size} · modified {t}"),
        None => size,
    }
}

fn local_time(t: SystemTime) -> Option<String> {
    let ts = jiff::Timestamp::try_from(t).ok()?;
    Some(ts.to_zoned(jiff::tz::TimeZone::system()).strftime("%-d %b %Y, %H:%M").to_string())
}

fn clock(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

fn seconds(secs: f64) -> String {
    if secs < 60.0 {
        format!("{secs:.2} s")
    } else {
        clock(Duration::from_secs_f64(secs))
    }
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
