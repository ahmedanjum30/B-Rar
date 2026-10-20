//! The archive browser: look inside an archive, open single files, extract a selection.
//!
//! Extraction runs in a second B-Rar-GUI process (`x ... --items`), so it gets the usual
//! progress window, prompts and result page, and the browser stays responsive.

use std::{
    cmp::Ordering as Cmp,
    collections::{HashMap, HashSet},
    ffi::OsString,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, Mutex},
    time::{Instant, SystemTime},
};

use brar_core::{ConflictPolicy, EntryInfo, Error, Options, Resolution, Selection};
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Key, Layout, PointerButton, Pos2, Rect, Response,
    RichText, Sense, Shape, Stroke, StrokeKind, Ui, Vec2, ViewportCommand, pos2, vec2,
};

use crate::{
    app::{LOGO_PNG, human},
    shell,
    style::{self, Icon, palette, semibold},
};

pub fn run(archive: PathBuf, password: Option<String>) -> ExitCode {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(format!("{} - B-Rar", file_name(&archive)))
        .with_inner_size(vec2(920.0, 600.0))
        .with_min_inner_size(vec2(560.0, 320.0));
    if let Ok(icon) = eframe::icon_data::from_png_bytes(LOGO_PNG) {
        viewport = viewport.with_icon(Arc::new(icon));
    }
    let options = eframe::NativeOptions { viewport, centered: true, ..Default::default() };
    let result = eframe::run_native(
        "B-Rar",
        options,
        Box::new(move |cc| {
            style::install(&cc.egui_ctx);
            Ok(Box::new(Browser::new(&cc.egui_ctx, archive, password)))
        }),
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            shell::error_box("B-Rar", &format!("Could not open the B-Rar window: {e}"));
            ExitCode::FAILURE
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

// ---- the archive as a tree ---------------------------------------------------------------

struct Node {
    name: String,
    /// Archive path with `/`, no leading or trailing slash; empty for the root.
    path: String,
    is_dir: bool,
    /// For folders, totals over everything inside.
    size: u64,
    packed: Option<u64>,
    files: u64,
    modified: Option<SystemTime>,
    parent: usize,
    children: Vec<usize>,
}

struct Tree {
    /// `nodes[0]` is the root; parents always come before their children.
    nodes: Vec<Node>,
}

impl Tree {
    fn build(entries: Vec<EntryInfo>) -> Self {
        let root = Node {
            name: String::new(),
            path: String::new(),
            is_dir: true,
            size: 0,
            packed: Some(0),
            files: 0,
            modified: None,
            parent: 0,
            children: Vec::new(),
        };
        let mut tree = Tree { nodes: vec![root] };
        let mut index: HashMap<String, usize> = HashMap::new();
        for e in entries {
            let path = brar_core::normalize_name(&e.name);
            if path.is_empty() {
                continue;
            }
            let parent = tree.folder(&mut index, path.rsplit_once('/').map_or("", |(p, _)| p));
            match index.get(&path) {
                Some(&i) => {
                    // A folder seen first as a path prefix, or a repeated name (tar appends).
                    let n = &mut tree.nodes[i];
                    n.modified = e.modified.or(n.modified);
                    if !e.is_dir && !n.is_dir {
                        (n.size, n.packed) = (e.size, e.packed);
                    }
                }
                None => {
                    let i = tree.add(parent, &path, e.is_dir);
                    let n = &mut tree.nodes[i];
                    (n.modified, n.size, n.packed, n.files) = (e.modified, e.size, e.packed, u64::from(!e.is_dir));
                    if e.is_dir {
                        (n.size, n.packed, n.files) = (0, Some(0), 0);
                    }
                    index.insert(path, i);
                }
            }
        }
        // Children come after parents, so one backwards pass sums every folder.
        for i in (1..tree.nodes.len()).rev() {
            let (size, packed, files, parent) = {
                let n = &tree.nodes[i];
                (n.size, n.packed, n.files, n.parent)
            };
            let p = &mut tree.nodes[parent];
            p.size += size;
            p.files += files;
            p.packed = p.packed.zip(packed).map(|(a, b)| a + b);
        }
        tree
    }

    /// The folder node for `path`, creating it and its parents as needed.
    fn folder(&mut self, index: &mut HashMap<String, usize>, path: &str) -> usize {
        if path.is_empty() {
            return 0;
        }
        if let Some(&i) = index.get(path) {
            return i;
        }
        let parent = self.folder(index, path.rsplit_once('/').map_or("", |(p, _)| p));
        let i = self.add(parent, path, true);
        self.nodes[i].packed = Some(0);
        index.insert(path.to_owned(), i);
        i
    }

    fn add(&mut self, parent: usize, path: &str, is_dir: bool) -> usize {
        let i = self.nodes.len();
        self.nodes.push(Node {
            name: path.rsplit('/').next().unwrap_or(path).to_owned(),
            path: path.to_owned(),
            is_dir,
            size: 0,
            packed: None,
            files: 0,
            modified: None,
            parent,
            children: Vec::new(),
        });
        self.nodes[parent].children.push(i);
        i
    }

    /// Is `node` inside `folder` (at any depth)?
    fn within(&self, mut node: usize, folder: usize) -> bool {
        while node != 0 {
            node = self.nodes[node].parent;
            if node == folder {
                return true;
            }
        }
        folder == 0
    }
}

// ---- state -----------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Column {
    Name,
    Size,
    Packed,
    Modified,
}

/// What to do once a password is known.
#[derive(Clone, Copy)]
enum Retry {
    List,
    Open(usize),
}

enum Page {
    Loading,
    Ready,
    Password { wrong: bool, retry: Retry },
    Failed(String),
}

enum Action {
    Open(usize),
    Enter(usize),
    Up,
    /// `None`: ask for the folder.
    Extract(Option<PathBuf>),
    Test,
}

#[derive(Default)]
struct Inbox {
    listing: Option<brar_core::Result<Vec<EntryInfo>>>,
    /// Finished "open" jobs: the node and where its file landed.
    opened: Vec<(usize, brar_core::Result<PathBuf>)>,
}

struct Browser {
    archive: PathBuf,
    name: String,
    logo: egui::TextureHandle,
    password: Option<String>,
    inbox: Arc<Mutex<Inbox>>,
    page: Page,
    tree: Tree,
    cwd: usize,
    /// Nodes shown, in display order.
    rows: Vec<usize>,
    stale: bool,
    selected: HashSet<usize>,
    anchor: Option<usize>,
    cursor: Option<usize>,
    reveal_cursor: bool,
    sort: (Column, bool),
    search: String,
    focus_search: bool,
    password_input: String,
    show_password: bool,
    focus_password: bool,
    status: Option<(String, Instant)>,
    opening: usize,
    temp: PathBuf,
    jobs: u32,
}

impl Browser {
    fn new(ctx: &egui::Context, archive: PathBuf, password: Option<String>) -> Self {
        let logo = eframe::icon_data::from_png_bytes(LOGO_PNG)
            .map(|i| egui::ColorImage::from_rgba_unmultiplied([i.width as usize, i.height as usize], &i.rgba))
            .unwrap_or_default();
        let mut b = Self {
            name: file_name(&archive),
            archive,
            logo: ctx.load_texture("logo", logo, egui::TextureOptions::LINEAR),
            password,
            inbox: Arc::default(),
            page: Page::Loading,
            tree: Tree::build(Vec::new()),
            cwd: 0,
            rows: Vec::new(),
            stale: true,
            selected: HashSet::new(),
            anchor: None,
            cursor: None,
            reveal_cursor: false,
            sort: (Column::Name, true),
            search: String::new(),
            focus_search: false,
            password_input: String::new(),
            show_password: false,
            focus_password: false,
            status: None,
            opening: 0,
            temp: std::env::temp_dir().join("B-Rar").join(std::process::id().to_string()),
            jobs: 0,
        };
        b.list(ctx);
        b
    }

    fn options(&self) -> Options {
        Options { password: self.password.clone(), ..Options::default() }
    }

    fn list(&mut self, ctx: &egui::Context) {
        self.page = Page::Loading;
        let (inbox, ctx, archive, opts) = (Arc::clone(&self.inbox), ctx.clone(), self.archive.clone(), self.options());
        std::thread::spawn(move || {
            let result = brar_core::list(&archive, &opts);
            inbox.lock().unwrap().listing = Some(result);
            ctx.request_repaint();
        });
    }

    fn ask_password(&mut self, wrong: bool, retry: Retry) {
        self.password_input.clear();
        self.focus_password = true;
        self.page = Page::Password { wrong, retry };
    }

    fn say(&mut self, message: impl Into<String>) {
        self.status = Some((message.into(), Instant::now()));
    }

    // ---- rows and selection ----

    fn refresh_rows(&mut self) {
        let t = &self.tree;
        let query = self.search.trim().to_lowercase();
        let mut rows: Vec<usize> = if query.is_empty() {
            t.nodes[self.cwd].children.clone()
        } else {
            (1..t.nodes.len())
                .filter(|&i| t.within(i, self.cwd) && t.nodes[i].name.to_lowercase().contains(&query))
                .collect()
        };
        let (column, ascending) = self.sort;
        rows.sort_by(|&a, &b| {
            let (x, y) = (&t.nodes[a], &t.nodes[b]);
            // Folders first, whichever way the column sorts.
            let by = match column {
                Column::Name => natural(&x.name, &y.name),
                Column::Size => x.size.cmp(&y.size),
                Column::Packed => x.packed.cmp(&y.packed),
                Column::Modified => x.modified.cmp(&y.modified),
            };
            let by = if ascending { by } else { by.reverse() };
            y.is_dir.cmp(&x.is_dir).then(by).then_with(|| natural(&x.name, &y.name))
        });
        self.rows = rows;
        let shown: HashSet<usize> = self.rows.iter().copied().collect();
        self.selected.retain(|n| shown.contains(n));
        self.anchor = self.anchor.filter(|n| shown.contains(n));
        self.cursor = self.cursor.filter(|n| shown.contains(n));
        self.stale = false;
    }

    fn row_of(&self, node: usize) -> Option<usize> {
        self.rows.iter().position(|&n| n == node)
    }

    fn enter(&mut self, folder: usize) {
        let from = self.cwd;
        self.cwd = folder;
        self.search.clear();
        self.selected.clear();
        self.refresh_rows();
        // Coming back up, keep the folder we left highlighted.
        let back = std::iter::successors(Some(from), |&n| (n != 0).then(|| self.tree.nodes[n].parent))
            .find(|&n| self.tree.nodes[n].parent == folder && n != folder);
        self.cursor = back.or(self.rows.first().copied());
        self.anchor = self.cursor;
        if let Some(n) = back {
            self.selected.insert(n);
        }
        self.reveal_cursor = true;
    }

    fn click(&mut self, node: usize, ctrl: bool, shift: bool) {
        match (shift, self.anchor.and_then(|a| self.row_of(a)), self.row_of(node)) {
            (true, Some(a), Some(b)) => {
                if !ctrl {
                    self.selected.clear();
                }
                self.selected.extend(&self.rows[a.min(b)..=a.max(b)]);
            }
            _ if ctrl => {
                if !self.selected.remove(&node) {
                    self.selected.insert(node);
                }
                self.anchor = Some(node);
            }
            _ => {
                self.selected.clear();
                self.selected.insert(node);
                self.anchor = Some(node);
            }
        }
        self.cursor = Some(node);
    }

    fn move_cursor(&mut self, to: isize, shift: bool) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as isize - 1;
        let row = self.cursor.and_then(|c| self.row_of(c)).map_or(0, |r| r as isize);
        let next = self.rows[if to == isize::MIN { 0 } else if to == isize::MAX { last as usize } else { (row + to).clamp(0, last) as usize }];
        self.click(next, false, shift);
        self.reveal_cursor = true;
    }

    /// What "extract" acts on: the selection, or everything in view when nothing is selected.
    fn picked(&self) -> Option<Vec<String>> {
        if !self.selected.is_empty() {
            let mut items: Vec<String> = self.rows.iter().filter(|n| self.selected.contains(n)).map(|&n| self.tree.nodes[n].path.clone()).collect();
            items.dedup();
            return Some(items);
        }
        if self.cwd == 0 && self.search.trim().is_empty() {
            return None; // the whole archive: no selection at all, the fastest path
        }
        Some(self.rows.iter().map(|&n| self.tree.nodes[n].path.clone()).collect())
    }

    // ---- actions ----

    fn run(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::Enter(n) => self.enter(n),
            Action::Up => {
                if self.cwd != 0 {
                    self.enter(self.tree.nodes[self.cwd].parent);
                }
            }
            Action::Open(n) if self.tree.nodes[n].is_dir => self.enter(n),
            Action::Open(n) => self.open(ctx, n),
            Action::Extract(dest) => {
                let dest = match dest {
                    Some(d) => d,
                    None => match shell::pick_folder(&self.archive) {
                        Ok(Some(d)) => d,
                        Ok(None) => return,
                        Err(e) => return self.say(e),
                    },
                };
                let mut args: Vec<OsString> = vec!["x".into(), self.archive.clone().into(), "-o".into(), dest.into()];
                if let Some(items) = self.picked() {
                    match self.write_items(&items) {
                        Ok(file) => args.extend(["--items".into(), file.into()]),
                        Err(e) => return self.say(format!("Couldn't start extracting: {e}")),
                    }
                    args.extend(["--base".into(), self.tree.nodes[self.cwd].path.clone().into()]);
                }
                self.spawn(args);
            }
            Action::Test => self.spawn(vec!["t".into(), self.archive.clone().into()]),
        }
    }

    /// Selections can be long, so they go to the child in a file it deletes after reading.
    fn write_items(&mut self, items: &[String]) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(&self.temp)?;
        self.jobs += 1;
        let file = self.temp.join(format!("items-{}.txt", self.jobs));
        std::fs::write(&file, items.join("\n"))?;
        Ok(file)
    }

    fn spawn(&mut self, args: Vec<OsString>) {
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => return self.say(format!("Couldn't start B-Rar: {e}")),
        };
        let mut cmd = std::process::Command::new(exe);
        cmd.args(args);
        if let Some(pw) = &self.password {
            // Not on the command line, where other programs could read it.
            cmd.env(crate::PASSWORD_VAR, pw);
        }
        if let Err(e) = cmd.spawn() {
            self.say(format!("Couldn't start B-Rar: {e}"));
        }
    }

    /// Extracts one file to a temporary folder and opens it with its default app.
    fn open(&mut self, ctx: &egui::Context, node: usize) {
        let n = &self.tree.nodes[node];
        self.jobs += 1;
        let dir = self.temp.join(self.jobs.to_string());
        let parent = n.path.rsplit_once('/').map_or("", |(p, _)| p).to_owned();
        let target = dir.join(&n.name);
        let opts = Options {
            selection: Some(Selection { items: vec![n.path.clone()], base: parent }),
            on_conflict: ConflictPolicy::Always(Resolution::Overwrite),
            ..self.options()
        };
        self.opening += 1;
        self.say(format!("Opening {}...", n.name));
        let (inbox, ctx, archive) = (Arc::clone(&self.inbox), ctx.clone(), self.archive.clone());
        std::thread::spawn(move || {
            let result = brar_core::extract(&archive, &dir, &opts).map(|_| target);
            inbox.lock().unwrap().opened.push((node, result));
            ctx.request_repaint();
        });
    }
}

/// "file2" before "file10", ignoring case.
fn natural(a: &str, b: &str) -> Cmp {
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, _) => return Cmp::Less,
            (_, None) => return Cmp::Greater,
            (Some(p), Some(q)) if p.is_ascii_digit() && q.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut s = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        s.push(c);
                        it.next();
                    }
                    s
                };
                let (m, n) = (take(&mut x), take(&mut y));
                let (m, n) = (m.trim_start_matches('0'), n.trim_start_matches('0'));
                match m.len().cmp(&n.len()).then_with(|| m.cmp(n)) {
                    Cmp::Equal => {}
                    other => return other,
                }
            }
            (Some(p), Some(q)) => {
                match p.to_lowercase().cmp(q.to_lowercase()) {
                    Cmp::Equal => {}
                    other => return other,
                }
                x.next();
                y.next();
            }
        }
    }
}

// ---- egui ------------------------------------------------------------------------------------

const ROW: f32 = 26.0;

impl eframe::App for Browser {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let (listing, opened) = {
            let mut inbox = self.inbox.lock().unwrap();
            (inbox.listing.take(), std::mem::take(&mut inbox.opened))
        };
        match listing {
            Some(Ok(entries)) => {
                self.tree = Tree::build(entries);
                self.cwd = 0;
                self.page = Page::Ready;
                self.refresh_rows();
                self.cursor = self.rows.first().copied();
            }
            Some(Err(e @ (Error::PasswordRequired | Error::WrongPassword))) => {
                self.ask_password(matches!(e, Error::WrongPassword), Retry::List);
            }
            Some(Err(e)) => self.page = Page::Failed(e.to_string()),
            None => {}
        }
        for (node, result) in opened {
            self.opening -= 1;
            match result {
                Ok(file) => {
                    self.status = None;
                    shell::open_path(&file);
                }
                Err(e @ (Error::PasswordRequired | Error::WrongPassword)) => {
                    self.ask_password(matches!(e, Error::WrongPassword), Retry::Open(node));
                }
                Err(Error::Cancelled) => {}
                Err(e) => self.say(format!("Couldn't open {}: {e}", self.tree.nodes[node].name)),
            }
        }
        if self.status.as_ref().is_some_and(|(_, at)| at.elapsed().as_secs() > 8) && self.opening == 0 {
            self.status = None;
        }
        if self.stale && matches!(self.page, Page::Ready) {
            self.refresh_rows();
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let pal = palette(ui);
        match &self.page {
            Page::Ready => self.browser_page(ui),
            Page::Loading => centered(ui, pal.bg, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(20.0));
                    ui.label(RichText::new(format!("Reading {}...", self.name)).color(pal.text));
                });
            }),
            Page::Password { wrong, retry } => {
                let (wrong, retry) = (*wrong, *retry);
                centered(ui, pal.bg, |ui| self.password_page(ui, wrong, retry));
            }
            Page::Failed(message) => {
                let message = message.clone();
                centered(ui, pal.bg, |ui| {
                    ui.set_max_width(460.0);
                    ui.horizontal(|ui| {
                        style::status_icon(ui, Icon::Error);
                        ui.vertical(|ui| {
                            ui.label(RichText::new("Couldn't open the archive").heading().color(pal.text));
                            ui.add(egui::Label::new(RichText::new(&self.name).color(pal.weak)).truncate());
                        });
                    });
                    ui.add_space(10.0);
                    ui.add(egui::Label::new(RichText::new(message).color(pal.text)).wrap());
                    ui.add_space(14.0);
                    if style::primary_button(ui, "Close", true).clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        ui.ctx().send_viewport_cmd(ViewportCommand::Close);
                    }
                });
            }
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Best effort: files still open in another app stay until the next cleanup.
        let _ = std::fs::remove_dir_all(&self.temp);
    }
}

/// A fixed-width block in the middle of the window.
fn centered(ui: &mut Ui, bg: Color32, add: impl FnOnce(&mut Ui)) {
    egui::Frame::new().fill(bg).inner_margin(24).show(ui, |ui| {
        ui.set_min_size(ui.available_size());
        ui.add_space((ui.available_height() / 2.0 - 90.0).max(0.0));
        ui.vertical_centered(|ui| {
            ui.set_max_width(440.0);
            ui.with_layout(Layout::top_down(Align::Min), add);
        });
    });
}

impl Browser {
    fn password_page(&mut self, ui: &mut Ui, wrong: bool, retry: Retry) {
        let pal = palette(ui);
        ui.horizontal(|ui| {
            style::status_icon(ui, Icon::Lock);
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.label(RichText::new("Password required").heading().color(pal.text));
                ui.add(egui::Label::new(RichText::new(format!("{} is encrypted", self.name)).color(pal.weak)).truncate());
            });
        });
        ui.add_space(14.0);
        let mut submit = false;
        ui.horizontal(|ui| {
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.password_input)
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
        ui.add_space(14.0);
        let mut cancel = false;
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            submit |= style::primary_button(ui, "OK", !self.password_input.is_empty()).clicked();
            cancel = style::button(ui, "Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape));
        });
        if cancel {
            match retry {
                Retry::List => ui.ctx().send_viewport_cmd(ViewportCommand::Close),
                Retry::Open(_) => self.page = Page::Ready,
            }
        } else if submit && !self.password_input.is_empty() {
            self.password = Some(std::mem::take(&mut self.password_input));
            match retry {
                Retry::List => self.list(ui.ctx()),
                Retry::Open(node) => {
                    self.page = Page::Ready;
                    self.open(ui.ctx(), node);
                }
            }
        }
    }

    fn browser_page(&mut self, ui: &mut Ui) {
        let pal = palette(ui);
        let mut action = None;
        self.keyboard(ui, &mut action);

        egui::Panel::top("toolbar")
            .frame(egui::Frame::new().fill(pal.bg).inner_margin(egui::Margin { left: 14, right: 14, top: 12, bottom: 8 }))
            .show_separator_line(false)
            .show(ui, |ui| self.toolbar(ui, &mut action));
        egui::Panel::bottom("status")
            .frame(egui::Frame::new().fill(pal.bg).inner_margin(egui::Margin::symmetric(14, 6)))
            .show_separator_line(false)
            .show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let fill = ui.visuals().extreme_bg_color;
            egui::Frame::new()
                .fill(fill)
                .stroke(Stroke::new(1.0, ui.visuals().widgets.inactive.bg_stroke.color))
                .corner_radius(CornerRadius::same(6))
                .outer_margin(egui::Margin::symmetric(14, 0))
                .inner_margin(egui::Margin::symmetric(0, 4))
                .show(ui, |ui| {
                    ui.set_min_size(ui.available_size());
                    self.table(ui, &mut action);
                });
        });

        if let Some(action) = action {
            self.run(ui.ctx(), action);
        }
    }

    fn keyboard(&mut self, ui: &mut Ui, action: &mut Option<Action>) {
        let typing = ui.ctx().egui_wants_keyboard_input();
        ui.input_mut(|i| {
            if i.consume_key(egui::Modifiers::COMMAND, Key::F) {
                self.focus_search = true;
            }
            if i.pointer.button_pressed(PointerButton::Extra1) || i.consume_key(egui::Modifiers::ALT, Key::ArrowUp) {
                *action = Some(Action::Up);
            }
            if typing {
                return;
            }
            let shift = i.modifiers.shift;
            if i.consume_key(egui::Modifiers::COMMAND, Key::A) {
                self.selected = self.rows.iter().copied().collect();
            }
            if i.key_pressed(Key::ArrowDown) {
                self.move_cursor(1, shift);
            }
            if i.key_pressed(Key::ArrowUp) {
                self.move_cursor(-1, shift);
            }
            if i.key_pressed(Key::PageDown) {
                self.move_cursor(15, shift);
            }
            if i.key_pressed(Key::PageUp) {
                self.move_cursor(-15, shift);
            }
            if i.key_pressed(Key::Home) {
                self.move_cursor(isize::MIN, shift);
            }
            if i.key_pressed(Key::End) {
                self.move_cursor(isize::MAX, shift);
            }
            if i.key_pressed(Key::Enter)
                && let Some(c) = self.cursor
            {
                *action = Some(Action::Open(c));
            }
            if i.key_pressed(Key::Backspace) {
                *action = Some(Action::Up);
            }
            if i.key_pressed(Key::Escape) {
                if !self.search.is_empty() {
                    self.search.clear();
                    self.stale = true;
                } else {
                    self.selected.clear();
                }
            }
        });
    }

    fn toolbar(&mut self, ui: &mut Ui, action: &mut Option<Action>) {
        let pal = palette(ui);
        ui.horizontal(|ui| {
            ui.add(egui::Image::new(egui::load::SizedTexture::new(self.logo.id(), Vec2::splat(28.0))));
            ui.add_space(2.0);
            let count = self.selected.len();
            let what = if count > 0 { format!("{count} selected") } else { "all".to_owned() };
            let dir = brar_core::suggested_dir(&self.archive);
            let folder = file_name(&dir);
            let quick = style::primary_button(ui, &format!("Extract to \"{folder}\\\""), true)
                .on_hover_text(format!("Extract {what} to {}", dir.display()));
            if quick.clicked() {
                *action = Some(Action::Extract(Some(dir)));
            }
            if style::button(ui, "Extract to...").on_hover_text(format!("Pick a folder, then extract {what}")).clicked() {
                *action = Some(Action::Extract(None));
            }
            if style::button(ui, "Test").on_hover_text("Check every file in the archive for errors").clicked() {
                *action = Some(Action::Test);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("Search this folder")
                        .margin(vec2(8.0, 6.0))
                        .desired_width(220.0_f32.min(ui.available_width() - 8.0)),
                );
                if std::mem::take(&mut self.focus_search) {
                    field.request_focus();
                }
                if field.changed() {
                    self.stale = true;
                }
            });
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let up = ui.add_enabled(self.cwd != 0, egui::Button::new("Up").min_size(vec2(48.0, 26.0)));
            if up.on_hover_text("Parent folder (Backspace)").clicked() {
                *action = Some(Action::Up);
            }
            ui.add_space(4.0);
            // Breadcrumb: archive > folder > folder.
            let mut chain = vec![self.cwd];
            while let Some(&n) = chain.last().filter(|&&n| n != 0) {
                chain.push(self.tree.nodes[n].parent);
            }
            chain.reverse();
            ui.spacing_mut().item_spacing.x = 2.0;
            for (k, &n) in chain.iter().enumerate() {
                if k > 0 {
                    ui.label(RichText::new("›").color(pal.weak));
                }
                let label = if n == 0 { self.name.clone() } else { self.tree.nodes[n].name.clone() };
                let last = k + 1 == chain.len();
                let text = RichText::new(label).color(if last { pal.text } else { pal.weak });
                let text = if last { text.family(semibold()) } else { text };
                if ui.add(egui::Button::new(text).frame(false)).clicked() && !last {
                    *action = Some(Action::Enter(n));
                }
            }
        });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let pal = palette(ui);
        ui.horizontal(|ui| {
            let t = &self.tree;
            let summary = if self.selected.is_empty() {
                let (dirs, files) = self.rows.iter().fold((0, 0), |(d, f), &n| if t.nodes[n].is_dir { (d + 1, f) } else { (d, f + 1) });
                let size: u64 = self.rows.iter().map(|&n| t.nodes[n].size).sum();
                format!("{} · {} · {}", plural(dirs, "folder"), plural(files, "file"), human(size))
            } else {
                let size: u64 = self.selected.iter().map(|&n| t.nodes[n].size).sum();
                format!("{} selected · {}", self.selected.len(), human(size))
            };
            ui.label(RichText::new(summary).color(pal.weak));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let total = &t.nodes[0];
                ui.label(RichText::new(format!("{} in archive · {}", plural(total.files as usize, "file"), human(total.size))).color(pal.weak));
                if let Some((message, _)) = &self.status {
                    ui.add_space(16.0);
                    ui.add(egui::Label::new(RichText::new(message).color(pal.text)).truncate());
                    if self.opening > 0 {
                        ui.add(egui::Spinner::new().size(14.0));
                    }
                }
            });
        });
    }

    fn table(&mut self, ui: &mut Ui, action: &mut Option<Action>) {
        let pal = palette(ui);
        let cols = |left: f32, width: f32| {
            let (size, packed, modified) = (92.0, 92.0, 150.0);
            let name = (width - size - packed - modified - 24.0).max(120.0);
            let mut x = left + 12.0;
            let mut next = |w: f32| {
                let r = (x, w);
                x += w;
                r
            };
            [(Column::Name, next(name)), (Column::Size, next(size)), (Column::Packed, next(packed)), (Column::Modified, next(modified))]
        };

        // Header.
        let (head, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW), Sense::hover());
        for (column, (x, w)) in cols(head.left(), head.width()) {
            let rect = Rect::from_min_size(pos2(x, head.top()), vec2(w, ROW));
            let response = ui.interact(rect, ui.scope_id().with(("head", column as u8)), Sense::click());
            let label = match column {
                Column::Name => "Name",
                Column::Size => "Size",
                Column::Packed => "Packed",
                Column::Modified => "Modified",
            };
            let color = if response.hovered() { pal.text } else { pal.weak };
            let right = column != Column::Name && column != Column::Modified;
            let painter = ui.painter_at(rect);
            let text_rect = if right {
                painter.text(pos2(rect.right() - 10.0, rect.center().y), Align2::RIGHT_CENTER, label, FontId::new(13.0, semibold()), color)
            } else {
                painter.text(pos2(rect.left() + 6.0, rect.center().y), Align2::LEFT_CENTER, label, FontId::new(13.0, semibold()), color)
            };
            if self.sort.0 == column {
                let ax = if right { text_rect.left() - 9.0 } else { text_rect.right() + 9.0 };
                let (cy, s) = (rect.center().y, 3.5);
                let tri = if self.sort.1 {
                    vec![pos2(ax - s, cy + s * 0.6), pos2(ax + s, cy + s * 0.6), pos2(ax, cy - s * 0.6)]
                } else {
                    vec![pos2(ax - s, cy - s * 0.6), pos2(ax + s, cy - s * 0.6), pos2(ax, cy + s * 0.6)]
                };
                painter.add(Shape::convex_polygon(tri, pal.weak, Stroke::NONE));
            }
            if response.clicked() {
                self.sort = if self.sort.0 == column { (column, !self.sort.1) } else { (column, column == Column::Name) };
                self.stale = true;
            }
        }
        let line_y = head.bottom();
        ui.painter().hline(head.x_range().shrink(8.0), line_y, Stroke::new(1.0, ui.visuals().widgets.inactive.bg_stroke.color));

        if self.rows.is_empty() {
            let message = if self.search.trim().is_empty() { "This folder is empty" } else { "No matches" };
            ui.add_space(30.0);
            ui.vertical_centered(|ui| ui.label(RichText::new(message).color(pal.weak)));
            return;
        }

        let reveal = std::mem::take(&mut self.reveal_cursor).then(|| self.cursor.and_then(|c| self.row_of(c))).flatten();
        let searching = !self.search.trim().is_empty();
        egui::ScrollArea::vertical().auto_shrink(false).show_viewport(ui, |ui, viewport| {
            let total = self.rows.len();
            let origin = ui.max_rect().min;
            let width = ui.available_width();
            ui.set_min_height(ROW * total as f32);
            // Under the rows, so the rows win; clicking empty space clears the selection.
            let background = ui.interact(ui.max_rect(), ui.scope_id().with("rows-bg"), Sense::click());
            if background.clicked() {
                self.selected.clear();
            }
            if let Some(r) = reveal {
                let rect = Rect::from_min_size(pos2(origin.x, origin.y + r as f32 * ROW), vec2(width, ROW));
                ui.scroll_to_rect(rect, None);
            }
            let first = (viewport.min.y / ROW).floor().max(0.0) as usize;
            let last = ((viewport.max.y / ROW).ceil() as usize + 1).min(total);
            for r in first..last {
                let node = self.rows[r];
                let rect = Rect::from_min_size(pos2(origin.x, origin.y + r as f32 * ROW), vec2(width, ROW));
                let response = ui.interact(rect, ui.scope_id().with(("row", node)), Sense::click());
                self.row(ui, rect, node, &response, searching, &cols);
                let mods = ui.input(|i| i.modifiers);
                if response.clicked() {
                    self.click(node, mods.command, mods.shift);
                }
                if response.double_clicked() {
                    *action = Some(Action::Open(node));
                }
                if response.secondary_clicked() && !self.selected.contains(&node) {
                    self.click(node, false, false);
                }
                response.context_menu(|ui| self.row_menu(ui, node, action));
            }
        });
    }

    fn row(&self, ui: &Ui, rect: Rect, node: usize, response: &Response, searching: bool, cols: &dyn Fn(f32, f32) -> [(Column, (f32, f32)); 4]) {
        let pal = palette(ui);
        let n = &self.tree.nodes[node];
        let painter = ui.painter_at(rect);
        let inner = rect.shrink2(vec2(6.0, 1.0));
        if self.selected.contains(&node) {
            painter.rect_filled(inner, CornerRadius::same(4), pal.accent.gamma_multiply(0.22));
        } else if response.hovered() {
            painter.rect_filled(inner, CornerRadius::same(4), pal.track.gamma_multiply(0.5));
        }
        // In a multi-selection, outline the row the arrow keys move from.
        if self.cursor == Some(node) && self.selected.len() > 1 {
            painter.rect_stroke(inner, CornerRadius::same(4), Stroke::new(1.0, pal.accent.gamma_multiply(0.6)), StrokeKind::Inside);
        }
        let font = FontId::proportional(13.5);
        let cy = rect.center().y;
        for (column, (x, w)) in cols(rect.left(), rect.width()) {
            let cell = Rect::from_min_max(pos2(x, rect.top()), pos2(x + w, rect.bottom()));
            let p = painter.with_clip_rect(cell.shrink2(vec2(2.0, 0.0)).intersect(rect));
            match column {
                Column::Name => {
                    icon(&p, pos2(x + 8.0, cy), n.is_dir, &pal);
                    let label = if searching { relative(&n.path, &self.tree.nodes[self.cwd].path) } else { n.name.as_str() };
                    p.text(pos2(x + 30.0, cy), Align2::LEFT_CENTER, label, font.clone(), pal.text);
                }
                Column::Size => {
                    p.text(pos2(cell.right() - 10.0, cy), Align2::RIGHT_CENTER, human(n.size), font.clone(), pal.text);
                }
                Column::Packed => {
                    let packed = n.packed.map(human).unwrap_or_default();
                    p.text(pos2(cell.right() - 10.0, cy), Align2::RIGHT_CENTER, packed, font.clone(), pal.weak);
                }
                Column::Modified => {
                    let when = n.modified.and_then(short_time).unwrap_or_default();
                    p.text(pos2(x + 6.0, cy), Align2::LEFT_CENTER, when, font.clone(), pal.weak);
                }
            }
        }
    }

    fn row_menu(&self, ui: &mut Ui, node: usize, action: &mut Option<Action>) {
        let is_dir = self.tree.nodes[node].is_dir;
        if ui.button(if is_dir { "Open folder" } else { "Open" }).clicked() {
            *action = Some(Action::Open(node));
            ui.close();
        }
        ui.separator();
        let dir = brar_core::suggested_dir(&self.archive);
        if ui.button(format!("Extract to \"{}\\\"", file_name(&dir))).clicked() {
            *action = Some(Action::Extract(Some(dir)));
            ui.close();
        }
        if ui.button("Extract to...").clicked() {
            *action = Some(Action::Extract(None));
            ui.close();
        }
    }
}

/// `path` relative to the folder being browsed (search results can be deeper).
fn relative<'a>(path: &'a str, folder: &str) -> &'a str {
    if folder.is_empty() {
        return path;
    }
    path.strip_prefix(folder).and_then(|r| r.strip_prefix('/')).unwrap_or(path)
}

/// A 16 px folder or page glyph centred vertically at `at` (left edge).
fn icon(p: &egui::Painter, at: Pos2, is_dir: bool, pal: &style::Palette) {
    let top = at.y - 7.0;
    if is_dir {
        let folder = Color32::from_rgb(240, 186, 64);
        let tab = Rect::from_min_size(pos2(at.x, top), vec2(7.0, 4.0));
        let body = Rect::from_min_size(pos2(at.x, top + 2.0), vec2(16.0, 12.0));
        p.rect_filled(tab, CornerRadius::same(1), folder.gamma_multiply(0.85));
        p.rect_filled(body, CornerRadius::same(2), folder);
    } else {
        let page = Rect::from_min_size(pos2(at.x + 2.0, top), vec2(12.0, 15.0));
        p.rect_filled(page, CornerRadius::same(2), pal.track);
        p.rect_stroke(page, CornerRadius::same(2), Stroke::new(1.0, pal.weak.gamma_multiply(0.7)), StrokeKind::Inside);
        for k in 0..3 {
            let y = top + 5.0 + k as f32 * 3.0;
            p.hline(egui::Rangef::new(page.left() + 3.0, page.right() - 3.0), y, Stroke::new(1.0, pal.weak.gamma_multiply(0.6)));
        }
    }
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}

fn short_time(t: SystemTime) -> Option<String> {
    let ts = jiff::Timestamp::try_from(t).ok()?;
    Some(ts.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H:%M").to_string())
}
