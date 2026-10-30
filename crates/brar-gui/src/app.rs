//! The B-Rar window: progress, "file already exists", password and result pages, talking to
//! the extraction (or the archive being created) running on a worker thread; and the "Add to
//! archive" page that comes before creating one.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime},
};

use brar_core::{Conflict, ConflictPolicy, CreateOptions, Decision, Error, Options, PartFinder, Progress, Resolution, Summary};
use eframe::egui::{self, Align, Key, Layout, RichText, TextureHandle, Ui, Vec2, ViewportCommand, vec2};

use crate::{
    Mode, shell,
    style::{self, Icon, palette, semibold},
};

pub const LOGO_PNG: &[u8] = include_bytes!("../../../assets/brar-256.png");

pub fn run(archive: PathBuf, mode: Mode, password: Option<String>) -> ExitCode {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("B-Rar")
        .with_inner_size(vec2(Page::Running.width(), 220.0))
        .with_resizable(false)
        .with_maximize_button(false);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(LOGO_PNG) {
        viewport = viewport.with_icon(Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
        centered: true,
        ..Default::default()
    };
    let succeeded = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&succeeded);
    let result = eframe::run_native(
        "B-Rar",
        options,
        Box::new(move |cc| {
            style::install(&cc.egui_ctx);
            Ok(Box::new(App::new(&cc.egui_ctx, archive, mode, flag, password)))
        }),
    );
    if let Err(e) = result {
        shell::error_box("B-Rar", &format!("Could not open the B-Rar window: {e}"));
        return ExitCode::FAILURE;
    }
    if succeeded.load(Ordering::Relaxed) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
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
    /// Window width; the height follows the content.
    fn width(&self) -> f32 {
        match self {
            Page::Conflict(_) | Page::Add => 540.0,
            Page::Password { .. } => 480.0,
            Page::Running | Page::Done { .. } | Page::Failed(_) => 520.0,
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
    archive: PathBuf,
    name: String,
    mode: Mode,
    logo: TextureHandle,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    succeeded: Arc<AtomicBool>,
    page: Page,
    shown_size: Vec2,
    clock: Clock,
    title: String,
    password: String,
    show_password: bool,
    focus_password: bool,
    apply_to_all: bool,
    /// The "Add to archive" page's fields.
    add: AddForm,
}

/// Compression levels the "Add to archive" page offers: name, Deflate level.
const LEVELS: [(&str, u32); 4] = [("Store", 0), ("Fastest", 1), ("Normal", 6), ("Best", 9)];

#[derive(Default)]
struct AddForm {
    path: String,
    level: u32,
    encrypt: bool,
    password: String,
    confirm: String,
    focus: bool,
}

impl App {
    fn new(ctx: &egui::Context, archive: PathBuf, mode: Mode, succeeded: Arc<AtomicBool>, password: Option<String>) -> Self {
        let logo = eframe::icon_data::from_png_bytes(LOGO_PNG)
            .map(|i| egui::ColorImage::from_rgba_unmultiplied([i.width as usize, i.height as usize], &i.rgba))
            .unwrap_or_default();
        let mut app = Self {
            name: archive.file_name().unwrap_or_default().to_string_lossy().into_owned(),
            archive,
            mode,
            logo: ctx.load_texture("logo", logo, egui::TextureOptions::LINEAR),
            shared: Arc::default(),
            worker: None,
            succeeded,
            page: Page::Running,
            shown_size: Vec2::ZERO,
            clock: Clock::new(),
            title: String::new(),
            password: String::new(),
            show_password: false,
            focus_password: false,
            apply_to_all: false,
            add: AddForm::default(),
        };
        match &app.mode {
            Mode::Create { ask: true, level, .. } => {
                app.add = AddForm {
                    path: app.archive.display().to_string(),
                    level: *level,
                    focus: true,
                    ..Default::default()
                };
                app.page = Page::Add;
            }
            _ => app.start(ctx, password),
        }
        app
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

    /// (Re)starts the job, e.g. after the user typed a password.
    fn start(&mut self, ctx: &egui::Context, password: Option<String>) {
        let progress = Arc::new(Progress::default());
        *self.shared.progress.lock().unwrap() = Arc::clone(&progress);
        self.clock = Clock::new();
        self.page = Page::Running;

        let ask = {
            let (shared, ctx) = (Arc::clone(&self.shared), ctx.clone());
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
                ctx.request_repaint();
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
        let (shared, ctx, archive, mode) = (Arc::clone(&self.shared), ctx.clone(), self.archive.clone(), self.mode.clone());
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
            ctx.request_repaint();
        }));
    }

    fn answer(&mut self, resolution: Resolution) {
        if let Page::Conflict(ask) = std::mem::replace(&mut self.page, Page::Running) {
            let _ = ask.reply.send(Decision {
                resolution,
                apply_to_all: self.apply_to_all,
            });
        }
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
        if let Page::Conflict(_) = self.page {
            self.answer(Resolution::Cancel);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }

    fn set_title(&mut self, ctx: &egui::Context, title: String) {
        if title != self.title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(outcome) = self.shared.outcome.lock().unwrap().take() {
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
            self.page = match outcome {
                Ok(summary) => {
                    self.succeeded.store(true, Ordering::Relaxed);
                    Page::Done {
                        summary,
                        secs: self.clock.active().as_secs_f64(),
                    }
                }
                Err(Error::Cancelled) => {
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                    Page::Running
                }
                Err(e @ (Error::PasswordRequired | Error::WrongPassword)) => {
                    self.password.clear();
                    self.focus_password = true;
                    Page::Password {
                        wrong: matches!(e, Error::WrongPassword),
                    }
                }
                // Denied partway (one protected subfolder, say): the same hand-over to an
                // elevated copy as main.rs does up front.
                Err(e) if e.is_access_denied() && !shell::is_elevated() && self.dest().is_some() => match crate::elevate(&self.archive, &self.mode) {
                    Ok(true) => {
                        self.succeeded.store(true, Ordering::Relaxed);
                        ctx.send_viewport_cmd(ViewportCommand::Close);
                        Page::Running
                    }
                    Ok(false) => Page::Failed(crate::declined(self.dest().unwrap_or(Path::new("")))),
                    Err(message) => Page::Failed(message),
                },
                Err(e) => Page::Failed(e.to_string()),
            };
        }
        if matches!(self.page, Page::Running)
            && let Some(ask) = self.shared.conflict.lock().unwrap().take()
        {
            self.apply_to_all = false;
            self.page = Page::Conflict(ask);
        }

        let title = match &self.page {
            Page::Add => "Add to archive - B-Rar".into(),
            Page::Running => {
                let p = self.progress();
                if !p.is_paused() {
                    self.clock.sample(p.done_bytes());
                }
                ctx.request_repaint_after(Duration::from_millis(100));
                match p.total_bytes() {
                    0 => format!("{} - B-Rar", self.name),
                    total => format!("{:.0}% - {} - B-Rar", percent(p.done_bytes(), total), self.name),
                }
            }
            Page::Conflict(_) => "File already exists - B-Rar".into(),
            Page::Password { .. } => "Password - B-Rar".into(),
            Page::Done { .. } => "Done - B-Rar".into(),
            Page::Failed(_) => "Error - B-Rar".into(),
        };
        self.set_title(ctx, title);

        if ctx.input(|i| i.viewport().close_requested()) {
            self.shutdown();
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let pal = palette(ui);
        // The page lays out at its natural height; the window then takes that height, so text
        // scaling or a long message never cuts off the buttons.
        ui.painter().rect_filled(ui.max_rect(), 0.0, pal.bg);
        let shown = egui::Frame::new().inner_margin(egui::Margin::symmetric(24, 20)).show(ui, |ui| {
            ui.set_width(self.page.width() - 48.0);
            match &self.page {
                Page::Add => self.add_page(ui),
                Page::Running => self.running_page(ui),
                Page::Conflict(_) => self.conflict_page(ui),
                Page::Password { wrong } => {
                    let wrong = *wrong;
                    self.password_page(ui, wrong);
                }
                Page::Done { summary, secs } => {
                    let (summary, secs) = (*summary, *secs);
                    self.done_page(ui, summary, secs);
                }
                Page::Failed(message) => {
                    let message = message.clone();
                    self.failed_page(ui, &message);
                }
            }
        });
        let size = vec2(self.page.width(), shown.response.rect.height().ceil());
        if (size - self.shown_size).length() > 0.5 {
            self.shown_size = size;
            ui.ctx().send_viewport_cmd(ViewportCommand::InnerSize(size));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.shutdown();
    }
}

// ---- pages ---------------------------------------------------------------------------------

impl App {
    fn running_page(&mut self, ui: &mut Ui) {
        let pal = palette(ui);
        let p = self.progress();
        let (title, subtitle) = match (self.dest(), self.creating()) {
            (Some(dest), _) => (format!("Extracting {}", self.name), format!("to {}", dest.display())),
            (None, true) => (format!("Compressing to {}", self.name), format!("in {}", folder(&self.archive))),
            (None, false) => (format!("Testing {}", self.name), "Checking every file for errors".to_owned()),
        };
        header(ui, |ui| logo(ui, &self.logo), &title, &subtitle);
        ui.add_space(18.0);

        let (total, done) = (p.total_bytes(), p.done_bytes());
        let paused = p.is_paused();
        let fraction = (total > 0).then(|| (done.min(total) as f64 / total as f64) as f32);
        style::progress_bar(ui, fraction, if paused { pal.paused } else { pal.accent });
        ui.add_space(8.0);

        let elapsed = self.clock.active();
        let rate = if paused { 0.0 } else { self.clock.rate() };
        ui.horizontal(|ui| {
            let amount = if total > 0 {
                format!("{} of {}", human(done), human(total))
            } else {
                human(done)
            };
            ui.label(RichText::new(amount).color(pal.text));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let status = if paused {
                    "Paused".to_owned()
                } else if total > 0 {
                    format!("{:.0}%", percent(done, total))
                } else {
                    String::new()
                };
                ui.label(RichText::new(status).family(semibold()).color(pal.text));
            });
        });
        ui.horizontal(|ui| {
            let mut stats = format!("{} elapsed", clock(elapsed));
            if !paused && rate > 0.0 {
                stats = format!("{}/s · {stats}", human(rate as u64));
                if total > done
                    && elapsed > Duration::from_secs(2)
                    && let Some(left) = self.clock.left(total - done)
                {
                    stats.push_str(&format!(" · {} left", clock(left)));
                }
            }
            ui.label(RichText::new(stats).color(pal.weak));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(format!("{} files", p.files())).color(pal.weak));
            });
        });
        let current = p.current_file();
        let current = current.rsplit(['/', '\\']).next().unwrap_or_default();
        ui.add(egui::Label::new(RichText::new(current).small().color(pal.weak)).truncate());

        buttons(ui, |ui| {
            if style::button(ui, "Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                p.cancel();
            }
            if style::button(ui, if paused { "Resume" } else { "Pause" }).clicked() {
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
            }
        });
    }

    fn conflict_page(&mut self, ui: &mut Ui) {
        let pal = palette(ui);
        let Page::Conflict(ask) = &self.page else { return };
        let file = ask.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let folder = ask.path.parent().map(|p| p.display().to_string()).unwrap_or_default();
        let rows = [
            ("Existing file", describe(ask.old_size, ask.old_modified)),
            ("From archive", describe(ask.new_size, ask.new_modified)),
        ];
        header(ui, |ui| style::status_icon(ui, Icon::Warning), "Replace existing file?", &file);
        ui.add(egui::Label::new(RichText::new(format!("in {folder}")).small().color(pal.weak)).truncate());
        ui.add_space(10.0);
        egui::Grid::new("conflict").num_columns(2).spacing(vec2(16.0, 4.0)).show(ui, |ui| {
            for (label, value) in rows {
                ui.label(RichText::new(label).color(pal.weak));
                ui.label(RichText::new(value).color(pal.text));
                ui.end_row();
            }
        });
        ui.add_space(6.0);
        ui.checkbox(&mut self.apply_to_all, "Do this for all remaining conflicts");

        let mut choice = None;
        buttons(ui, |ui| {
            if style::primary_button(ui, "Replace", true).clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                choice = Some(Resolution::Overwrite);
            }
            if style::button(ui, "Skip").clicked() {
                choice = Some(Resolution::Skip);
            }
            if style::button(ui, "Keep both").on_hover_text("Extract as \"name (1)\"").clicked() {
                choice = Some(Resolution::Rename);
            }
            ui.add_space(12.0);
            if style::button(ui, "Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                choice = Some(Resolution::Cancel);
            }
        });
        if let Some(resolution) = choice {
            self.answer(resolution);
        }
    }

    fn password_page(&mut self, ui: &mut Ui, wrong: bool) {
        let pal = palette(ui);
        header(
            ui,
            |ui| style::status_icon(ui, Icon::Lock),
            "Password required",
            &format!("{} is encrypted", self.name),
        );
        ui.add_space(14.0);
        let mut submit = false;
        ui.horizontal(|ui| {
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.password)
                    .password(!self.show_password)
                    .hint_text("Enter password")
                    .margin(vec2(8.0, 6.0))
                    .desired_width(ui.available_width() - 72.0),
            );
            if std::mem::take(&mut self.focus_password) {
                field.request_focus();
            }
            submit |= field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            let toggle = if self.show_password { "Hide" } else { "Show" };
            if ui.add(egui::Button::new(toggle).min_size(vec2(64.0, 30.0))).clicked() {
                self.show_password = !self.show_password;
            }
        });
        if wrong {
            ui.label(RichText::new("That password is incorrect. Try again.").color(pal.error));
        }
        let mut cancel = false;
        buttons(ui, |ui| {
            submit |= style::primary_button(ui, "OK", !self.password.is_empty()).clicked();
            cancel = style::button(ui, "Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape));
        });
        if cancel {
            ui.ctx().send_viewport_cmd(ViewportCommand::Close);
        } else if submit && !self.password.is_empty() {
            let password = std::mem::take(&mut self.password);
            self.start(ui.ctx(), Some(password));
        }
    }

    fn done_page(&mut self, ui: &mut Ui, s: Summary, secs: f64) {
        if self.creating() {
            return self.created_page(ui, s, secs);
        }
        let pal = palette(ui);
        let (title, subtitle) = match self.dest() {
            Some(_) => ("Extraction complete".to_owned(), self.name.clone()),
            None => ("No errors found".to_owned(), self.name.clone()),
        };
        header(ui, |ui| style::status_icon(ui, Icon::Ok), &title, &subtitle);
        ui.add_space(12.0);
        let mut line = format!("{} files · {} folders · {}", s.files, s.dirs, human(s.bytes));
        line.push_str(&format!("  in {} ({}/s)", seconds(secs), human((s.bytes as f64 / secs.max(1e-3)) as u64)));
        ui.label(RichText::new(line).color(pal.text));
        let mut notes = Vec::new();
        if s.kept_existing > 0 {
            notes.push(format!("kept {} existing files", s.kept_existing));
        }
        if s.skipped > 0 {
            notes.push(format!("skipped {} links", s.skipped));
        }
        if let Some(dest) = self.dest() {
            notes.insert(0, format!("Saved to {}", dest.display()));
        }
        ui.add(egui::Label::new(RichText::new(notes.join(" · ")).color(pal.weak)).truncate());

        let dest = self.dest().map(Path::to_path_buf);
        buttons(ui, |ui| {
            if let Some(dest) = &dest {
                if style::primary_button(ui, "Open folder", true).clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                    shell::open_path(dest);
                    ui.ctx().send_viewport_cmd(ViewportCommand::Close);
                }
                if style::button(ui, "Close").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    ui.ctx().send_viewport_cmd(ViewportCommand::Close);
                }
            } else if style::primary_button(ui, "Close", true).clicked() || ui.input(|i| i.key_pressed(Key::Enter) || i.key_pressed(Key::Escape)) {
                ui.ctx().send_viewport_cmd(ViewportCommand::Close);
            }
        });
    }

    fn failed_page(&mut self, ui: &mut Ui, message: &str) {
        let pal = palette(ui);
        let verb = match (self.dest(), self.creating()) {
            (Some(_), _) => "extract",
            (None, true) => "create",
            (None, false) => "test",
        };
        header(
            ui,
            |ui| style::status_icon(ui, Icon::Error),
            &format!("Couldn't {verb} the archive"),
            &self.name,
        );
        ui.add_space(12.0);
        ui.add(egui::Label::new(RichText::new(message).color(pal.text)).wrap());
        buttons(ui, |ui| {
            if style::primary_button(ui, "Close", true).clicked() || ui.input(|i| i.key_pressed(Key::Enter) || i.key_pressed(Key::Escape)) {
                ui.ctx().send_viewport_cmd(ViewportCommand::Close);
            }
        });
    }
}

impl App {
    fn add_page(&mut self, ui: &mut Ui) {
        let pal = palette(ui);
        let Mode::Create { inputs, .. } = &self.mode else { return };
        let what = match inputs.as_slice() {
            [one] => one.file_name().unwrap_or_default().to_string_lossy().into_owned(),
            many => format!("{} items", many.len()),
        };
        header(ui, |ui| logo(ui, &self.logo), "Add to archive", &what);
        ui.add_space(14.0);

        ui.label(RichText::new("Archive").color(pal.weak));
        let field = ui.add(
            egui::TextEdit::singleline(&mut self.add.path)
                .margin(vec2(8.0, 6.0))
                .desired_width(ui.available_width()),
        );
        if std::mem::take(&mut self.add.focus) {
            field.request_focus();
        }
        ui.add_space(10.0);
        ui.label(RichText::new("Compression").color(pal.weak));
        ui.horizontal(|ui| {
            for (name, level) in LEVELS {
                ui.radio_value(&mut self.add.level, level, name);
            }
        });
        ui.add_space(10.0);
        ui.checkbox(&mut self.add.encrypt, "Encrypt with a password (AES-256)");
        if self.add.encrypt {
            for (value, hint) in [(&mut self.add.password, "Password"), (&mut self.add.confirm, "Type it again")] {
                ui.add(
                    egui::TextEdit::singleline(value)
                        .password(true)
                        .hint_text(hint)
                        .margin(vec2(8.0, 6.0))
                        .desired_width(ui.available_width()),
                );
            }
        }

        // What's wrong with the form, if anything (shown under it, and Create stays off).
        let path = PathBuf::from(self.add.path.trim());
        let problem = if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")) {
            Some("The archive's name must end in .zip.")
        } else if self.add.encrypt && self.add.password.is_empty() {
            Some("Enter a password.")
        } else if self.add.encrypt && self.add.password != self.add.confirm {
            Some("The two passwords don't match.")
        } else {
            None
        };
        let exists = path.exists();
        if let Some(problem) = problem.filter(|p| !p.starts_with("Enter") && !(p.starts_with("The two") && self.add.confirm.is_empty())) {
            ui.label(RichText::new(problem).color(pal.error));
        } else if exists {
            ui.label(RichText::new("An archive with this name exists; it will be replaced.").color(pal.weak));
        }

        let mut create = false;
        let mut cancel = false;
        buttons(ui, |ui| {
            create = style::primary_button(ui, if exists { "Replace" } else { "Create" }, problem.is_none()).clicked()
                || (problem.is_none() && ui.input(|i| i.key_pressed(Key::Enter)));
            cancel = style::button(ui, "Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape));
        });
        if cancel {
            ui.ctx().send_viewport_cmd(ViewportCommand::Close);
        } else if create {
            let path = std::path::absolute(&path).unwrap_or(path);
            if let Mode::Create {
                level, password, overwrite, ..
            } = &mut self.mode
            {
                *level = self.add.level;
                *password = self.add.encrypt.then(|| std::mem::take(&mut self.add.password));
                *overwrite = exists;
            }
            self.add.confirm.clear();
            self.name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            self.archive = path;
            self.start(ui.ctx(), None);
        }
    }

    fn created_page(&mut self, ui: &mut Ui, s: Summary, secs: f64) {
        let pal = palette(ui);
        header(ui, |ui| style::status_icon(ui, Icon::Ok), "Archive created", &self.name);
        ui.add_space(12.0);
        let packed = std::fs::metadata(&self.archive).map(|m| m.len()).unwrap_or(0);
        let ratio = if s.bytes > 0 { packed as f64 * 100.0 / s.bytes as f64 } else { 100.0 };
        let line = format!(
            "{} files · {} folders · {} to {} ({ratio:.0}%)  in {}",
            s.files,
            s.dirs,
            human(s.bytes),
            human(packed),
            seconds(secs)
        );
        ui.label(RichText::new(line).color(pal.text));
        let mut notes = vec![format!("Saved in {}", folder(&self.archive))];
        if s.skipped > 0 {
            notes.push(format!("skipped {} links", s.skipped));
        }
        ui.add(egui::Label::new(RichText::new(notes.join(" · ")).color(pal.weak)).truncate());
        let archive = self.archive.clone();
        buttons(ui, |ui| {
            if style::primary_button(ui, "Show in folder", true).clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                shell::show_in_folder(&archive);
                ui.ctx().send_viewport_cmd(ViewportCommand::Close);
            }
            if style::button(ui, "Close").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                ui.ctx().send_viewport_cmd(ViewportCommand::Close);
            }
        });
    }
}

/// The folder `path` is in, for display.
fn folder(path: &Path) -> String {
    path.parent().map(|p| p.display().to_string()).unwrap_or_default()
}

// ---- layout helpers ------------------------------------------------------------------------

fn header(ui: &mut Ui, icon: impl FnOnce(&mut Ui), title: &str, subtitle: &str) {
    let pal = palette(ui);
    ui.horizontal(|ui| {
        icon(ui);
        ui.add_space(8.0);
        ui.vertical(|ui| {
            ui.add_space(1.0);
            ui.add(egui::Label::new(RichText::new(title).heading().color(pal.text)).truncate());
            ui.add(egui::Label::new(RichText::new(subtitle).color(pal.weak)).truncate());
        });
    });
}

fn logo(ui: &mut Ui, texture: &TextureHandle) {
    ui.add(egui::Image::new(egui::load::SizedTexture::new(texture.id(), Vec2::splat(40.0))));
}

/// The right-aligned button row that ends every page; add buttons right to left. The window is
/// sized to the page's content (see `ui`), so this just follows the content above it.
fn buttons(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.add_space(16.0);
    let row = vec2(ui.available_width(), ui.spacing().interact_size.y);
    ui.allocate_ui_with_layout(row, Layout::right_to_left(Align::Center), add);
}

// ---- formatting ----------------------------------------------------------------------------

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
