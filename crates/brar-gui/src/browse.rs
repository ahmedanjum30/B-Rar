//! The archive browser: look inside an archive, open single files, extract a selection.
//!
//! Extraction runs in a second B-Rar-GUI process (`x ... --items`), so it gets the usual
//! progress window, prompts and result page, and the browser stays responsive.
//!
//! Standard Windows controls: a virtual list view (Explorer's selection, keys and icons), a
//! breadcrumb of links, a search field and a status bar. Anything that opens a dialog or menu
//! runs after the state is released ([`Later`]), so the list can still draw itself meanwhile.

use std::{
    cell::RefCell,
    cmp::Ordering as Cmp,
    collections::{HashMap, HashSet},
    ffi::{OsString, c_void},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Instant, SystemTime},
};

use brar_core::{ConflictPolicy, EntryInfo, Error, Options, PartFinder, Resolution, Selection};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{BeginPaint, DT_LEFT, EndPaint, InvalidateRect, PAINTSTRUCT},
        Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::NMHDR,
            Input::KeyboardAndMouse::{GetFocus, GetKeyState},
            Shell::{
                DragAcceptFiles, DragFinish, DragQueryFileW, HDROP, SHFILEINFOW, SHGFI_SMALLICON, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES,
                SHGetFileInfoW,
            },
            WindowsAndMessaging::*,
        },
    },
    core::{HSTRING, w},
};

use crate::{
    app::human,
    shell,
    theme::{self, Kind, Tone},
    update::{self, Release},
    win::{self, Builder, Fonts, IDCANCEL, IDOK, Role},
};

// Controls.
const ID_LIST: i32 = 200;
const ID_SEARCH: i32 = 201;
const ID_EXTRACT: i32 = 202;
const ID_TEST: i32 = 203;
const ID_UP: i32 = 204;
const ID_CRUMBS: i32 = 205;
const ID_LOGO: i32 = 207;
const ID_OPEN: i32 = 208;
const ID_INFO: i32 = 209;
const ID_UPDATES: i32 = 214;
const ID_PASSWORD: i32 = 210;
const ID_SHOW: i32 = 211;
const ID_CHECK: i32 = 212;
const ID_SUPPORT: i32 = 213;
const ID_BAR_TEXT: i32 = 220;
const ID_BAR_UPDATE: i32 = 221;
const ID_BAR_NOTES: i32 = 222;
const ID_BAR_RETRY: i32 = 223;
const ID_BAR_CLOSE: i32 = 224;
// Context menu.
const MENU_OPEN: usize = 1;
const MENU_EXTRACT: usize = 2;
const TIMER: usize = 1;

// List view, header and status bar messages and flags (commctrl.h).
const LVS_REPORT: u32 = 0x1;
const LVS_SHOWSELALWAYS: u32 = 0x8;
const LVS_SHAREIMAGELISTS: u32 = 0x40;
const LVS_OWNERDATA: u32 = 0x1000;
const LVS_EX_FULLROWSELECT: isize = 0x20;
const LVS_EX_DOUBLEBUFFER: isize = 0x10000;
const LVM_SETIMAGELIST: u32 = 0x1003;
const LVM_GETNEXTITEM: u32 = 0x100C;
const LVM_ENSUREVISIBLE: u32 = 0x1013;
const LVM_SETCOLUMNWIDTH: u32 = 0x101E;
const LVM_GETHEADER: u32 = 0x101F;
const LVM_SETITEMSTATE: u32 = 0x102B;
const LVM_GETITEMSTATE: u32 = 0x102C;
const LVM_SETITEMCOUNT: u32 = 0x102F;
const LVM_SETEXTENDEDLISTVIEWSTYLE: u32 = 0x1036;
const LVM_SETSELECTIONMARK: u32 = 0x1043;
const LVM_INSERTCOLUMNW: u32 = 0x1061;
const LVIF_TEXT: u32 = 0x1;
const LVIF_IMAGE: u32 = 0x2;
const LVIF_STATE: u32 = 0x8;
const LVIS_FOCUSED: u32 = 0x1;
const LVIS_SELECTED: u32 = 0x2;
const LVNI_FOCUSED: usize = 0x1;
const LVNI_SELECTED: usize = 0x2;
const LVCF_FMT: u32 = 0x1;
const LVCF_WIDTH: u32 = 0x2;
const LVCF_TEXT: u32 = 0x4;
const LVCFMT_RIGHT: i32 = 0x1;
const HDM_GETITEMW: u32 = 0x120B;
const HDM_SETITEMW: u32 = 0x120C;
const HDI_FORMAT: u32 = 0x4;
const HDF_SORTDOWN: i32 = 0x200;
const HDF_SORTUP: i32 = 0x400;
const LVM_SETBKCOLOR: u32 = 0x1001;
const LVM_SETTEXTCOLOR: u32 = 0x1024;
const LVM_SETTEXTBKCOLOR: u32 = 0x1026;
const NM_CUSTOMDRAW: u32 = (-12i32) as u32;
const EMF_CENTERED: u32 = 0x1;
const NM_CLICK: u32 = (-2i32) as u32;
const NM_RETURN: u32 = (-4i32) as u32;
const NM_RCLICK: u32 = (-5i32) as u32;
const LVN_ITEMCHANGED: u32 = (-101i32) as u32;
const LVN_COLUMNCLICK: u32 = (-108i32) as u32;
const LVN_ITEMACTIVATE: u32 = (-114i32) as u32;
const LVN_ODSTATECHANGED: u32 = (-115i32) as u32;
const LVN_GETDISPINFOW: u32 = (-177i32) as u32;
const LVN_GETEMPTYMARKUP: u32 = (-187i32) as u32;
const APPCOMMAND_BROWSER_BACKWARD: usize = 1;

#[repr(C)]
struct LvItem {
    mask: u32,
    item: i32,
    sub_item: i32,
    state: u32,
    state_mask: u32,
    text: *mut u16,
    text_max: i32,
    image: i32,
    param: isize,
    indent: i32,
    group_id: i32,
    columns: u32,
    pu_columns: *mut u32,
    pi_col_fmt: *mut i32,
    group: i32,
}

#[repr(C)]
struct LvColumn {
    mask: u32,
    fmt: i32,
    cx: i32,
    text: *const u16,
    text_max: i32,
    sub_item: i32,
    image: i32,
    order: i32,
    cx_min: i32,
    cx_default: i32,
    cx_ideal: i32,
}

#[repr(C)]
struct NmLvDispInfo {
    hdr: NMHDR,
    item: LvItem,
}

#[repr(C)]
struct NmListView {
    hdr: NMHDR,
    item: i32,
    sub_item: i32,
    new_state: u32,
    old_state: u32,
    changed: u32,
    action: POINT,
    param: isize,
}

#[repr(C)]
struct NmLink {
    hdr: NMHDR,
    mask: u32,
    link: i32,
    state: u32,
    state_mask: u32,
    id: [u16; 48],
}

#[repr(C)]
struct NmEmptyMarkup {
    hdr: NMHDR,
    flags: u32,
    markup: [u16; 2084],
}

#[repr(C)]
struct HdItem {
    mask: u32,
    cxy: i32,
    text: *mut u16,
    bitmap: isize,
    text_max: i32,
    fmt: i32,
    param: isize,
    image: i32,
    order: i32,
    kind: u32,
    filter: *mut c_void,
    state: u32,
}

/// Opens the browser on `archive`, or on the home page (opened from the Start menu).
pub fn run(archive: Option<PathBuf>, password: Option<String>) -> ExitCode {
    win::init_controls();
    win::register(w!("BRarBrowser"), Some(wndproc));
    let s = win::scale();
    let px = |v: f64| (v * s).round() as i32;
    // SAFETY: a registered class; the window's state lives in BROWSER until WM_DESTROY.
    let hwnd = unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        CreateWindowExW(
            WS_EX_CONTROLPARENT,
            w!("BRarBrowser"),
            w!("B-Rar"),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            px(920.0),
            px(600.0),
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
    let (cw, ch) = win::client_size(hwnd);
    win::fit_client(hwnd, cw, ch, true);
    theme::dress_window(hwnd);
    let b = Browser::new(hwnd, password);
    theme::set_fonts(b.fonts.paint());
    BROWSER.with(|cell| *cell.borrow_mut() = Some(b));
    // SAFETY: the window just created.
    unsafe {
        DragAcceptFiles(hwnd, true);
        SetTimer(Some(hwnd), TIMER, 250, None);
    }
    with(|b| {
        b.build_bar();
        match archive {
            Some(archive) => b.open_archive(archive),
            None => b.set_page(Page::Home),
        }
        b.check_for_updates(false);
    });
    // SAFETY: the window just created.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    win::run_loop(hwnd, |msg| key(hwnd, msg));
    ExitCode::SUCCESS
}

thread_local! {
    static BROWSER: RefCell<Option<Browser>> = const { RefCell::new(None) };
}

/// Runs `f` on the window's state, unless it's already in use further up the stack.
fn with<R>(f: impl FnOnce(&mut Browser) -> R) -> Option<R> {
    BROWSER.with(|b| b.try_borrow_mut().ok()?.as_mut().map(f))
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

const COLUMNS: [Column; 4] = [Column::Name, Column::Size, Column::Packed, Column::Modified];

/// What to do once a password is known.
#[derive(Clone, Copy)]
enum Retry {
    List,
    Open(usize),
}

enum Page {
    /// No archive yet: "Open archive...", or drop one on the window.
    Home,
    Loading,
    Ready,
    Password {
        wrong: bool,
        retry: Retry,
    },
    Failed(String),
}

/// Work that opens a dialog or a menu: done once the state is released (see the module docs).
enum Later {
    PickArchive,
    /// The Info button's summary, in a message box.
    Info(String),
    /// Pick a folder, then extract the selection there.
    Extract,
    /// The list's right-click menu, for the row under the pointer if any.
    Menu(Option<usize>),
}

/// Where people can support B-Rar.
const SUPPORT_URL: &str = "https://www.patreon.com/cw/ahmedanjum30";

/// Where this window is with updates (see [`update`]).
enum Updates {
    Idle,
    Checking {
        manual: bool,
    },
    /// Only after a check that was asked for.
    UpToDate,
    Available(Release),
    Downloading {
        release: Release,
        done: Arc<AtomicU64>,
        total: Arc<AtomicU64>,
    },
    Failed(String),
    /// Closed by the user: not shown again in this window.
    Dismissed,
}

#[derive(Default)]
struct Inbox {
    listing: Option<brar_core::Result<Vec<EntryInfo>>>,
    /// Finished "open" jobs: the node and where its file landed.
    opened: Vec<(usize, brar_core::Result<PathBuf>)>,
}

/// The browsing controls, while an archive is shown.
struct Ready {
    /// Extract to, Test, Open, Info, Updates.
    tools: [HWND; 5],
    search: HWND,
    up: HWND,
    logo: HWND,
    crumbs: HWND,
    list: HWND,
}

/// Where the browser's self-painted parts are (set by `layout`).
#[derive(Default)]
struct Chrome {
    /// The toolbar's bottom edge.
    toolbar: i32,
    /// The path box around the logo and the breadcrumb.
    path: RECT,
    status: RECT,
}

#[repr(C)]
struct NmLvCustomDraw {
    hdr: NMHDR,
    stage: u32,
    hdc: windows::Win32::Graphics::Gdi::HDC,
    rc: RECT,
    item_spec: usize,
    item_state: u32,
    item_param: isize,
    text: u32,
    text_bk: u32,
    sub_item: i32,
}

struct Browser {
    hwnd: HWND,
    fonts: Fonts,
    s: f64,
    archive: PathBuf,
    name: String,
    password: Option<String>,
    /// Where parts of a multi-part archive turned up, if not next to the first.
    parts: PartFinder,
    inbox: Arc<Mutex<Inbox>>,
    page: Page,
    /// The centred page's controls (home, loading, password, failed).
    built: win::Page,
    ready: Option<Ready>,
    /// The update bar's height when shown (0 when not).
    bar: i32,
    chrome: Chrome,
    /// The status bar's two fields.
    status_text: (String, String),
    tree: Tree,
    cwd: usize,
    /// Nodes shown, in display order.
    rows: Vec<usize>,
    sort: (Column, bool),
    /// Clearing the search field programmatically: don't refresh twice.
    quiet: bool,
    status: Option<(String, Instant)>,
    opening: usize,
    temp: PathBuf,
    jobs: u32,
    /// System image list indexes by extension (and for folders).
    icons: HashMap<String, i32>,
    /// How this copy updates (or doesn't: the Store's, a development build).
    update_kind: update::Kind,
    updates: Arc<Mutex<Updates>>,
}

impl Browser {
    fn new(hwnd: HWND, password: Option<String>) -> Self {
        Self {
            hwnd,
            fonts: Fonts::new(),
            s: win::scale(),
            archive: PathBuf::new(),
            name: String::new(),
            password,
            parts: PartFinder::new(shell::pick_part),
            inbox: Arc::default(),
            page: Page::Home,
            built: win::Page::default(),
            ready: None,
            bar: 0,
            chrome: Chrome::default(),
            status_text: (String::new(), String::new()),
            tree: Tree::build(Vec::new()),
            cwd: 0,
            rows: Vec::new(),
            sort: (Column::Name, true),
            quiet: false,
            status: None,
            opening: 0,
            temp: std::env::temp_dir().join("B-Rar").join(std::process::id().to_string()),
            jobs: 0,
            icons: HashMap::new(),
            update_kind: update::kind(),
            updates: Arc::new(Mutex::new(Updates::Idle)),
        }
    }

    fn px(&self, v: i32) -> i32 {
        (v as f64 * self.s).round() as i32
    }

    fn handle(&self) -> isize {
        self.hwnd.0 as isize
    }

    fn updatable(&self) -> bool {
        matches!(
            self.update_kind,
            update::Kind::Installed | update::Kind::Portable | update::Kind::Sideloaded
        )
    }

    // ---- updates ----

    /// Looks for a newer release in the background. A check that wasn't asked for fails
    /// quietly (offline, say) and says nothing when there's no update.
    fn check_for_updates(&mut self, manual: bool) {
        if !self.updatable() {
            return;
        }
        let mut state = self.updates.lock().unwrap();
        if matches!(*state, Updates::Checking { .. } | Updates::Downloading { .. }) {
            return;
        }
        *state = Updates::Checking { manual };
        drop(state);
        self.refresh_bar();
        let (updates, hwnd, kind) = (Arc::clone(&self.updates), self.handle(), self.update_kind);
        std::thread::spawn(move || {
            let result = update::check(kind);
            *updates.lock().unwrap() = match result {
                Ok(Some(release)) => Updates::Available(release),
                Ok(None) if manual => Updates::UpToDate,
                Err(e) if manual => Updates::Failed(e),
                Ok(None) | Err(_) => Updates::Idle,
            };
            win::post(hwnd, win::WM_WAKE);
        });
    }

    /// Downloads the available update, checks it and installs it; closes the window once the
    /// installer (or the new B-Rar) has started.
    fn start_update(&mut self) {
        let mut state = self.updates.lock().unwrap();
        let Updates::Available(release) = &*state else { return };
        let (release, done, total) = (release.clone(), Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        *state = Updates::Downloading {
            release: release.clone(),
            done: Arc::clone(&done),
            total: Arc::clone(&total),
        };
        drop(state);
        self.refresh_bar();
        let (updates, hwnd, kind) = (Arc::clone(&self.updates), self.handle(), self.update_kind);
        std::thread::spawn(move || {
            let result = update::download(&release, &done, &total).and_then(|file| update::install(kind, &file));
            match result {
                Ok(()) => win::post(hwnd, WM_CLOSE),
                Err(e) => {
                    *updates.lock().unwrap() = Updates::Failed(e);
                    win::post(hwnd, win::WM_WAKE);
                }
            }
        });
    }

    /// The update bar's controls, hidden until there's something to say.
    fn build_bar(&mut self) {
        let f = self.fonts.normal;
        let text = win::child(self.hwnd, win::STATIC, "", win::SS_NOPREFIX | win::SS_ENDELLIPSIS, 0, ID_BAR_TEXT, f);
        win::show(text, false);
        for (id, label) in [
            (ID_BAR_UPDATE, "Update"),
            (ID_BAR_NOTES, "What's new"),
            (ID_BAR_RETRY, "Try again"),
            (ID_BAR_CLOSE, "Close"),
        ] {
            let b = win::child(self.hwnd, win::BUTTON, label, WS_TABSTOP.0, 0, id, f);
            win::show(b, false);
        }
    }

    /// Shows, updates or hides the update bar for the current state.
    fn refresh_bar(&mut self) {
        let (text, role, buttons): (String, Role, &[i32]) = match &*self.updates.lock().unwrap() {
            Updates::Idle | Updates::Dismissed | Updates::Checking { manual: false } => (String::new(), Role::Normal, &[]),
            Updates::Checking { manual: true } => ("Checking for updates...".into(), Role::Normal, &[]),
            Updates::UpToDate => (
                format!("You have the latest version ({}).", env!("CARGO_PKG_VERSION")),
                Role::Normal,
                &[ID_BAR_CLOSE],
            ),
            Updates::Available(r) => (
                format!("B-Rar {} is available.", r.version),
                Role::Normal,
                &[ID_BAR_UPDATE, ID_BAR_NOTES, ID_BAR_CLOSE],
            ),
            Updates::Downloading { release, done, total } => {
                let (done, total) = (done.load(Ordering::Relaxed), total.load(Ordering::Relaxed));
                let percent = (done * 100).checked_div(total).unwrap_or(0);
                (format!("Downloading B-Rar {}... {percent}%", release.version), Role::Normal, &[])
            }
            Updates::Failed(e) => (format!("Update failed: {e}"), Role::Error, &[ID_BAR_RETRY, ID_BAR_CLOSE]),
        };
        let shown = !text.is_empty();
        let label = win::item(self.hwnd, ID_BAR_TEXT);
        win::set_text(label, &text);
        win::set_role(label, role);
        win::show(label, shown);
        for id in [ID_BAR_UPDATE, ID_BAR_NOTES, ID_BAR_RETRY, ID_BAR_CLOSE] {
            win::show(win::item(self.hwnd, id), buttons.contains(&id));
        }
        let height = if shown { self.px(44) } else { 0 };
        if height != self.bar {
            self.bar = height;
            self.layout();
            // SAFETY: repaint the window (the bar's background).
            unsafe {
                let _ = InvalidateRect(Some(self.hwnd), None, true);
            }
        } else {
            self.layout_bar();
        }
    }

    fn layout_bar(&self) {
        if self.bar == 0 {
            return;
        }
        let (w, _) = win::client_size(self.hwnd);
        let bh = self.fonts_button_height();
        let y = (self.bar - bh) / 2;
        let mut x = w - self.px(12);
        for id in [ID_BAR_CLOSE, ID_BAR_RETRY, ID_BAR_NOTES, ID_BAR_UPDATE] {
            let h = win::item(self.hwnd, id);
            // SAFETY: plain call.
            if unsafe { IsWindowVisible(h) }.as_bool() {
                let bw = (win::measure(self.fonts.normal, &win::text(h)).0 + self.px(28)).max(self.px(80));
                x -= bw;
                win::place(h, x, y, bw, bh);
                x -= self.px(8);
            }
        }
        let line = win::measure(self.fonts.normal, "Ag").1;
        win::place(
            win::item(self.hwnd, ID_BAR_TEXT),
            self.px(16),
            (self.bar - line) / 2,
            (x - self.px(24)).max(0),
            line,
        );
    }

    fn fonts_button_height(&self) -> i32 {
        win::measure(self.fonts.normal, "Ag").1 + self.px(14)
    }

    // ---- pages ----

    fn set_page(&mut self, page: Page) {
        let was_ready = matches!(self.page, Page::Ready);
        self.page = page;
        self.built.clear();
        if !matches!(self.page, Page::Ready) && was_ready {
            self.drop_ready();
        }
        match &self.page {
            Page::Ready => {
                if self.ready.is_none() {
                    self.build_ready();
                }
            }
            Page::Home => self.built = self.home_page(),
            Page::Loading => {
                let mut b = Builder::new(self.hwnd, &self.fonts, 440);
                b.centered(&format!("Reading {}...", self.name), self.fonts.normal, Role::Normal);
                b.space(12);
                b.progress(0, true);
                self.built = b.finish();
            }
            Page::Password { wrong, .. } => {
                let mut b = Builder::new(self.hwnd, &self.fonts, 440);
                b.header_glyph(0xE72E, Tone::Accent, "Password required", &format!("{} is encrypted", self.name));
                b.space(14);
                b.edit(ID_PASSWORD, "", win::ES_PASSWORD, "Enter password", 0);
                b.space(6);
                b.check(ID_SHOW, "Show password", false);
                if *wrong {
                    b.space(4);
                    b.label(0, "That password is incorrect. Try again.", Role::Error, false);
                }
                b.space(14);
                b.buttons(&[(IDOK.0, "OK"), (IDCANCEL.0, "Cancel")]);
                win::enable(win::item(self.hwnd, IDOK.0), false);
                self.built = b.finish();
            }
            Page::Failed(message) => {
                let mut b = Builder::new(self.hwnd, &self.fonts, 460);
                b.header_glyph(0xEB90, Tone::Error, "Couldn't open the archive", &self.name);
                b.space(10);
                b.label(0, message, Role::Normal, true);
                b.space(14);
                b.buttons(&[(IDOK.0, "Open another..."), (IDCANCEL.0, "Close")]);
                self.built = b.finish();
            }
        }
        self.layout();
        let focus = match self.page {
            Page::Ready => self.ready.as_ref().map(|r| r.list),
            Page::Password { .. } => Some(win::item(self.hwnd, ID_PASSWORD)),
            _ => Some(win::item(self.hwnd, IDOK.0)),
        };
        if let Some(h) = focus.filter(|h| !h.is_invalid()) {
            win::focus(h);
        }
    }

    fn home_page(&self) -> win::Page {
        let mut b = Builder::new(self.hwnd, &self.fonts, 440);
        b.icon(win::app_icon(b.px(96)), 96);
        b.space(10);
        b.centered("B-Rar", self.fonts.big, Role::Normal);
        b.centered("Open an archive to look inside and extract it.", self.fonts.normal, Role::Weak);
        b.space(18);
        b.buttons_centered(&[(IDOK.0, "Open archive...")]);
        b.space(14);
        b.centered("Or drop an archive on this window.", self.fonts.normal, Role::Weak);
        b.centered(
            "In File Explorer, right-click any archive for B-Rar's menu.",
            self.fonts.normal,
            Role::Weak,
        );
        if let Some((message, _)) = &self.status {
            b.space(10);
            b.centered(message, self.fonts.normal, Role::Error);
        }
        b.space(18);
        b.centered(&format!("Version {}", env!("CARGO_PKG_VERSION")), self.fonts.normal, Role::Weak);
        if self.updatable() {
            b.space(2);
            b.link(ID_CHECK, "<a>Check for updates</a>", "Check for updates");
        }
        b.space(2);
        b.link(ID_SUPPORT, "<a>Support B-Rar</a>", "Support B-Rar");
        b.finish()
    }

    fn build_ready(&mut self) {
        let (hwnd, f, small) = (self.hwnd, self.fonts.normal, self.fonts.small);
        let tool = |id, glyph, label| win::painted(hwnd, label, Kind::Tool, glyph, 0, id, small);
        let tools = [
            tool(ID_EXTRACT, 0xF012, "Extract to"),
            tool(ID_TEST, 0xE9D5, "Test"),
            tool(ID_OPEN, 0xE8E5, "Open"),
            tool(ID_INFO, 0xE946, "Info"),
            tool(ID_UPDATES, 0xE895, "Updates"),
        ];
        let search = win::child(hwnd, win::EDIT, "", win::ES_AUTOHSCROLL | WS_TABSTOP.0, WS_EX_CLIENTEDGE.0, ID_SEARCH, f);
        theme::dark_control(search, "DarkMode_CFD");
        win::cue(search, "Search this folder");
        let up = win::painted(hwnd, "Up", Kind::Icon, 0xE74A, 0, ID_UP, f);
        let logo = win::child(hwnd, win::STATIC, "", win::SS_ICON | win::SS_REALSIZECONTROL, 0, ID_LOGO, f);
        win::send(logo, win::STM_SETICON, win::app_icon(self.px(16)).0 as usize, 0);
        let crumbs = win::child(hwnd, win::LINK, "", WS_TABSTOP.0 | 0x1 /* LWS_TRANSPARENT */, 0, ID_CRUMBS, f);
        let list = win::child(
            hwnd,
            w!("SysListView32"),
            "",
            LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS | LVS_SHAREIMAGELISTS | WS_TABSTOP.0,
            0,
            ID_LIST,
            f,
        );
        let p = theme::palette();
        // Workbench colours (rows are tinted per item in NM_CUSTOMDRAW); in dark, Windows' dark
        // scroll bars and header.
        theme::dark_control(list, "DarkMode_Explorer");
        win::send(list, LVM_SETBKCOLOR, 0, p.field as isize);
        win::send(list, LVM_SETTEXTBKCOLOR, 0, p.field as isize);
        win::send(list, LVM_SETTEXTCOLOR, 0, p.text as isize);
        win::send(list, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER);
        win::send(list, LVM_SETIMAGELIST, 1 /* LVSIL_SMALL */, system_image_list());
        for (i, column) in COLUMNS.iter().enumerate() {
            let (title, width) = match column {
                Column::Name => ("Name", 300),
                Column::Size => ("Size", 92),
                Column::Packed => ("Packed", 92),
                Column::Modified => ("Modified", 150),
            };
            let title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
            let col = LvColumn {
                mask: LVCF_FMT | LVCF_WIDTH | LVCF_TEXT,
                fmt: if matches!(column, Column::Size | Column::Packed) {
                    LVCFMT_RIGHT
                } else {
                    0
                },
                cx: self.px(width),
                text: title.as_ptr(),
                text_max: 0,
                sub_item: 0,
                image: 0,
                order: 0,
                cx_min: 0,
                cx_default: 0,
                cx_ideal: 0,
            };
            win::send(list, LVM_INSERTCOLUMNW, i, &col as *const _ as isize);
        }
        theme::dark_control(HWND(win::send(list, LVM_GETHEADER, 0, 0) as _), "DarkMode_ItemsView");
        theme::header_text(list);
        self.ready = Some(Ready {
            tools,
            search,
            up,
            logo,
            crumbs,
            list,
        });
    }

    fn drop_ready(&mut self) {
        if let Some(r) = self.ready.take() {
            for h in r.tools.into_iter().chain([r.search, r.up, r.logo, r.crumbs, r.list]) {
                win::destroy(h);
            }
        }
        self.chrome = Chrome::default();
    }

    /// Places everything for the window's size.
    fn layout(&mut self) {
        let (w, h) = win::client_size(self.hwnd);
        self.layout_bar();
        let top = self.bar;
        if let Some(r) = &self.ready {
            // Toolbar: big icon buttons, the search field on the right.
            let (tw, th, gap) = (self.px(70), self.px(56), self.px(2));
            let mut x = self.px(6);
            let y1 = top + self.px(5);
            for &t in &r.tools {
                win::place(t, x, y1, tw, th);
                x += tw + gap;
            }
            let eh = win::measure(self.fonts.normal, "Ag").1 + self.px(10);
            let sw = self.px(240).min(w - x - self.px(16)).max(self.px(80));
            win::place(r.search, w - self.px(10) - sw, y1 + (th - eh) / 2, sw, eh);
            let toolbar = y1 + th + self.px(5);
            // Address row: Up, then the path in a field-coloured box.
            let ah = self.px(26);
            let y2 = toolbar + self.px(5);
            win::place(r.up, self.px(6), y2, ah, ah);
            let path = RECT {
                left: self.px(6) + ah + self.px(6),
                top: y2,
                right: w - self.px(10),
                bottom: y2 + ah,
            };
            let logo = self.px(16);
            win::place(r.logo, path.left + self.px(6), y2 + (ah - logo) / 2, logo, logo);
            let line = win::measure(self.fonts.normal, "Ag").1;
            let cx = path.left + self.px(6) + logo + self.px(8);
            win::place(r.crumbs, cx, y2 + (ah - line) / 2, (path.right - cx - self.px(6)).max(0), line);
            // Status bar along the bottom, painted by the window.
            let sh = self.px(26);
            let status = RECT {
                left: 0,
                top: h - sh,
                right: w,
                bottom: h,
            };
            let ly = y2 + ah + self.px(5);
            win::place(r.list, 0, ly, w, (status.top - ly).max(0));
            // The name column takes what the others leave.
            let others = self.px(92 + 92 + 150);
            // SAFETY: plain call.
            let scroll = unsafe { GetSystemMetrics(SM_CXVSCROLL) };
            let name = (w - others - scroll - self.px(4)).max(self.px(120));
            win::send(r.list, LVM_SETCOLUMNWIDTH, 0, name as isize);
            self.chrome = Chrome { toolbar, path, status };
        } else {
            let (pw, ph) = (self.built.width, self.built.height);
            let x = ((w - pw) / 2).max(self.px(16));
            let y = top + ((h - top - ph) / 2).max(self.px(20));
            self.built.place(x, y);
        }
        // Everything moved: repaint the window and its controls (the window's own parts too:
        // toolbar line, path box, status bar).
        win::redraw(self.hwnd);
    }

    /// The parts of the window it paints itself: the update bar, the toolbar's bottom line, the
    /// path box, the status bar.
    fn paint(&self, dc: windows::Win32::Graphics::Gdi::HDC) {
        let p = theme::palette();
        let (w, _) = win::client_size(self.hwnd);
        if self.bar > 0 {
            theme::fill(
                dc,
                &RECT {
                    left: 0,
                    top: 0,
                    right: w,
                    bottom: self.bar,
                },
                p.hover,
            );
        }
        if self.ready.is_none() {
            return;
        }
        let c = &self.chrome;
        theme::fill(
            dc,
            &RECT {
                left: 0,
                top: c.toolbar,
                right: w,
                bottom: c.toolbar + 1,
            },
            p.border,
        );
        // The box takes the toolbar colour: the breadcrumb (a SysLink) paints its own background.
        theme::round(dc, &c.path, 2, p.surface, p.border);
        let s = c.status;
        theme::fill(dc, &s, p.surface);
        theme::fill(dc, &RECT { bottom: s.top + 1, ..s }, p.border);
        let split = s.left + (s.right - s.left) * 45 / 100;
        theme::fill(
            dc,
            &RECT {
                left: split,
                right: split + 1,
                top: s.top + self.px(5),
                bottom: s.bottom - self.px(5),
            },
            p.border,
        );
        let pad = self.px(10);
        let (left, right) = &self.status_text;
        theme::text(
            dc,
            &RECT {
                left: s.left + pad,
                right: split - pad,
                ..s
            },
            left,
            self.fonts.small,
            p.weak,
            DT_LEFT,
        );
        theme::text(
            dc,
            &RECT {
                left: split + pad,
                right: s.right - pad,
                ..s
            },
            right,
            self.fonts.small,
            p.weak,
            DT_LEFT,
        );
    }

    /// Alternate row tints and Workbench selection colours (NM_CUSTOMDRAW from the list).
    fn custom_draw(&self, cd: &mut NmLvCustomDraw) -> LRESULT {
        const CDDS_PREPAINT: u32 = 0x1;
        const CDDS_ITEMPREPAINT: u32 = 0x10001;
        const CDDS_SUBITEMPREPAINT: u32 = 0x30001;
        const CDRF_NOTIFYITEMDRAW: isize = 0x20;
        const CDRF_NOTIFYSUBITEMDRAW: isize = 0x20;
        const CDIS_SELECTED: u32 = 0x1;
        const CDIS_FOCUS: u32 = 0x10;
        let p = theme::palette();
        match cd.stage {
            CDDS_PREPAINT => LRESULT(CDRF_NOTIFYITEMDRAW),
            CDDS_ITEMPREPAINT | CDDS_SUBITEMPREPAINT => {
                let row = cd.item_spec;
                let selected = self
                    .ready
                    .as_ref()
                    .is_some_and(|r| win::send(r.list, LVM_GETITEMSTATE, row, LVIS_SELECTED as isize) as u32 & LVIS_SELECTED != 0);
                // Draw selection ourselves, in the theme's colour.
                cd.item_state &= !(CDIS_SELECTED | CDIS_FOCUS);
                cd.text_bk = if selected {
                    p.select
                } else if row % 2 == 1 {
                    p.alt
                } else {
                    p.field
                };
                let weak = cd.stage == CDDS_SUBITEMPREPAINT && cd.sub_item >= 2 && !selected;
                cd.text = if weak { p.weak } else { p.text };
                LRESULT(if cd.stage == CDDS_ITEMPREPAINT { CDRF_NOTIFYSUBITEMDRAW } else { 0 })
            }
            _ => LRESULT(0),
        }
    }

    /// A summary of the archive (the Info button).
    fn info(&self) -> String {
        let root = &self.tree.nodes[0];
        let folders = self.tree.nodes.iter().skip(1).filter(|n| n.is_dir).count();
        let format = Path::new(&self.name)
            .extension()
            .map(|e| e.to_string_lossy().to_uppercase())
            .unwrap_or_default();
        let mut lines = vec![
            format!("{}", self.archive.display()),
            String::new(),
            format!("Format: {format}"),
            format!("Files: {}", root.files),
            format!("Folders: {folders}"),
            format!("Size: {}", human(root.size)),
        ];
        if let Some(packed) = root.packed.filter(|&p| p > 0) {
            let ratio = if root.size > 0 {
                packed as f64 * 100.0 / root.size as f64
            } else {
                100.0
            };
            lines.push(format!("Packed: {} ({ratio:.0}%)", human(packed)));
        }
        if let Ok(m) = std::fs::metadata(&self.archive) {
            lines.push(format!("Archive file: {}", human(m.len())));
        }
        if self.password.is_some() {
            lines.push("Encrypted".into());
        }
        lines.join("\n")
    }

    // ---- archives ----

    /// Shows `archive` in this window (from the home page, a drop, or Ctrl+O).
    fn open_archive(&mut self, archive: PathBuf) {
        self.name = file_name(&archive);
        self.archive = archive;
        if !self.name.is_empty() {
            win::set_title(self.hwnd, &format!("{} - B-Rar", self.name));
        }
        self.password = None;
        self.parts = PartFinder::new(shell::pick_part);
        self.tree = Tree::build(Vec::new());
        self.cwd = 0;
        self.rows.clear();
        self.status = None;
        if matches!(self.page, Page::Ready) {
            self.drop_ready();
        }
        self.list();
    }

    fn options(&self) -> Options {
        Options {
            password: self.password.clone(),
            find_part: self.parts.clone(),
            ..Options::default()
        }
    }

    fn list(&mut self) {
        self.set_page(Page::Loading);
        let (inbox, hwnd, archive, opts) = (Arc::clone(&self.inbox), self.handle(), self.archive.clone(), self.options());
        std::thread::spawn(move || {
            let result = brar_core::list(&archive, &opts);
            inbox.lock().unwrap().listing = Some(result);
            win::post(hwnd, win::WM_WAKE);
        });
    }

    /// Finished background work: the listing, opened files.
    fn wake(&mut self) {
        let (listing, opened) = {
            let mut inbox = self.inbox.lock().unwrap();
            (inbox.listing.take(), std::mem::take(&mut inbox.opened))
        };
        match listing {
            Some(Ok(entries)) => {
                self.tree = Tree::build(entries);
                self.cwd = 0;
                self.set_page(Page::Ready);
                self.refresh_rows();
                self.focus_row(0, false);
            }
            Some(Err(e @ (Error::PasswordRequired | Error::WrongPassword))) => {
                self.set_page(Page::Password {
                    wrong: matches!(e, Error::WrongPassword),
                    retry: Retry::List,
                });
            }
            Some(Err(e)) => self.set_page(Page::Failed(e.to_string())),
            None => {}
        }
        for (node, result) in opened {
            self.opening -= 1;
            match result {
                Ok(file) => {
                    self.status = None;
                    self.update_status();
                    shell::open_path(&file);
                }
                Err(e @ (Error::PasswordRequired | Error::WrongPassword)) => {
                    self.set_page(Page::Password {
                        wrong: matches!(e, Error::WrongPassword),
                        retry: Retry::Open(node),
                    });
                }
                Err(Error::Cancelled) => {}
                Err(e) => {
                    let name = self.tree.nodes[node].name.clone();
                    self.say(format!("Couldn't open {name}: {e}"));
                }
            }
        }
        self.refresh_bar();
    }

    fn say(&mut self, message: impl Into<String>) {
        self.status = Some((message.into(), Instant::now()));
        if matches!(self.page, Page::Home) {
            self.set_page(Page::Home);
        } else {
            self.update_status();
        }
    }

    /// Four times a second: the update bar while downloading, old status messages.
    fn tick(&mut self) {
        if matches!(*self.updates.lock().unwrap(), Updates::Downloading { .. }) {
            self.refresh_bar();
        }
        if self.status.as_ref().is_some_and(|(_, at)| at.elapsed().as_secs() > 8) && self.opening == 0 {
            self.status = None;
            self.update_status();
        }
    }

    // ---- rows and selection ----

    fn search_text(&self) -> String {
        self.ready.as_ref().map(|r| win::text(r.search)).unwrap_or_default()
    }

    fn selected_rows(&self) -> Vec<usize> {
        let Some(r) = &self.ready else { return Vec::new() };
        let mut rows = Vec::new();
        let mut i = -1isize;
        loop {
            i = win::send(r.list, LVM_GETNEXTITEM, i as usize, LVNI_SELECTED as isize);
            if i < 0 {
                return rows;
            }
            rows.push(i as usize);
        }
    }

    fn selected(&self) -> Vec<usize> {
        self.selected_rows().into_iter().filter_map(|r| self.rows.get(r).copied()).collect()
    }

    fn set_row_state(&self, row: isize, state: u32, mask: u32) {
        if let Some(r) = &self.ready {
            let item = LvItem {
                mask: LVIF_STATE,
                item: 0,
                sub_item: 0,
                state,
                state_mask: mask,
                text: std::ptr::null_mut(),
                text_max: 0,
                image: 0,
                param: 0,
                indent: 0,
                group_id: 0,
                columns: 0,
                pu_columns: std::ptr::null_mut(),
                pi_col_fmt: std::ptr::null_mut(),
                group: 0,
            };
            win::send(r.list, LVM_SETITEMSTATE, row as usize, &item as *const _ as isize);
        }
    }

    /// Puts the keyboard cursor on `row` (and selects it if asked), scrolled into view.
    fn focus_row(&self, row: usize, select: bool) {
        let Some(r) = &self.ready else { return };
        if row >= self.rows.len() {
            return;
        }
        let state = LVIS_FOCUSED | if select { LVIS_SELECTED } else { 0 };
        self.set_row_state(row as isize, state, state);
        // Where Shift+arrow selections start from.
        win::send(r.list, LVM_SETSELECTIONMARK, 0, row as isize);
        win::send(r.list, LVM_ENSUREVISIBLE, row, 0);
    }

    /// Recomputes the rows (folder, search, sort), keeping the selection where it still shows.
    fn refresh_rows(&mut self) {
        let keep: HashSet<usize> = self.selected().into_iter().collect();
        let t = &self.tree;
        let query = self.search_text().trim().to_lowercase();
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
        if let Some(r) = &self.ready {
            self.set_row_state(-1, 0, LVIS_SELECTED | LVIS_FOCUSED);
            win::send(r.list, LVM_SETITEMCOUNT, self.rows.len(), 0);
            for (row, node) in self.rows.iter().enumerate() {
                if keep.contains(node) {
                    self.set_row_state(row as isize, LVIS_SELECTED, LVIS_SELECTED);
                }
            }
            // SAFETY: redraw the list with the new rows.
            unsafe {
                let _ = InvalidateRect(Some(r.list), None, false);
            }
            win::enable(r.up, self.cwd != 0);
        }
        self.sort_arrows();
        self.update_crumbs();
        self.update_status();
    }

    fn enter(&mut self, folder: usize) {
        let from = self.cwd;
        self.cwd = folder;
        if let Some(r) = &self.ready {
            self.quiet = true;
            win::set_text(r.search, "");
            self.quiet = false;
            self.set_row_state(-1, 0, LVIS_SELECTED | LVIS_FOCUSED);
        }
        self.refresh_rows();
        // Coming back up, keep the folder we left highlighted.
        let back = std::iter::successors(Some(from), |&n| (n != 0).then(|| self.tree.nodes[n].parent))
            .find(|&n| self.tree.nodes[n].parent == folder && n != folder);
        match back.and_then(|n| self.rows.iter().position(|&r| r == n)) {
            Some(row) => self.focus_row(row, true),
            None => self.focus_row(0, false),
        }
    }

    fn up(&mut self) {
        if self.cwd != 0 {
            self.enter(self.tree.nodes[self.cwd].parent);
        }
    }

    /// What "extract" acts on: the selection, or everything in view when nothing is selected.
    fn picked(&self) -> Option<Vec<String>> {
        let selected = self.selected();
        if !selected.is_empty() {
            let mut items: Vec<String> = selected.iter().map(|&n| self.tree.nodes[n].path.clone()).collect();
            items.dedup();
            return Some(items);
        }
        if self.cwd == 0 && self.search_text().trim().is_empty() {
            return None; // the whole archive: no selection at all, the fastest path
        }
        Some(self.rows.iter().map(|&n| self.tree.nodes[n].path.clone()).collect())
    }

    fn sort_arrows(&self) {
        let Some(r) = &self.ready else { return };
        let header = HWND(win::send(r.list, LVM_GETHEADER, 0, 0) as _);
        for (i, column) in COLUMNS.iter().enumerate() {
            // SAFETY: a zeroed HDITEMW is valid; only the format is read and written.
            let mut hd: HdItem = unsafe { std::mem::zeroed() };
            hd.mask = HDI_FORMAT;
            win::send(header, HDM_GETITEMW, i, &mut hd as *mut _ as isize);
            hd.fmt &= !(HDF_SORTUP | HDF_SORTDOWN);
            if *column == self.sort.0 {
                hd.fmt |= if self.sort.1 { HDF_SORTUP } else { HDF_SORTDOWN };
            }
            win::send(header, HDM_SETITEMW, i, &hd as *const _ as isize);
        }
    }

    /// The breadcrumb: archive › folder › folder, all but the last a link.
    fn update_crumbs(&self) {
        let Some(r) = &self.ready else { return };
        let mut chain = vec![self.cwd];
        while let Some(&n) = chain.last().filter(|&&n| n != 0) {
            chain.push(self.tree.nodes[n].parent);
        }
        chain.reverse();
        let mut markup = String::new();
        for (k, &n) in chain.iter().enumerate() {
            if k > 0 {
                markup.push_str("  ›  ");
            }
            let label = if n == 0 { self.name.clone() } else { self.tree.nodes[n].name.clone() };
            // SysLink has no escapes; a '<' in a name would start a tag.
            let label = label.replace('<', "‹");
            if k + 1 == chain.len() {
                markup.push_str(&label);
            } else {
                markup.push_str(&format!("<a id=\"{n}\">{label}</a>"));
            }
        }
        win::set_text(r.crumbs, &markup);
    }

    fn update_status(&mut self) {
        if self.ready.is_none() {
            return;
        }
        let t = &self.tree;
        let selected = self.selected();
        let summary = if selected.is_empty() {
            let (dirs, files) = self
                .rows
                .iter()
                .fold((0, 0), |(d, f), &n| if t.nodes[n].is_dir { (d + 1, f) } else { (d, f + 1) });
            let size: u64 = self.rows.iter().map(|&n| t.nodes[n].size).sum();
            format!("{} · {} · {}", plural(dirs, "folder"), plural(files, "file"), human(size))
        } else {
            let size: u64 = selected.iter().map(|&n| t.nodes[n].size).sum();
            format!("{} selected · {}", selected.len(), human(size))
        };
        let total = &t.nodes[0];
        let format = Path::new(&self.name)
            .extension()
            .map(|e| e.to_string_lossy().to_uppercase())
            .unwrap_or_default();
        let mut right = format!("{format} · {} · {}", plural(total.files as usize, "file"), human(total.size));
        if let Some(packed) = total.packed.filter(|&p| p > 0) {
            right.push_str(&format!(" → {}", human(packed)));
        }
        if let Some((message, _)) = &self.status {
            right = format!("{message}     {right}");
        }
        self.status_text = (summary, right);
        // SAFETY: repaint the status bar.
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), Some(&self.chrome.status), false);
        }
    }

    /// The list's text for one cell, into the buffer the list gave.
    fn display(&mut self, info: &mut NmLvDispInfo) {
        let Some(&node) = self.rows.get(info.item.item as usize) else { return };
        let column = COLUMNS.get(info.item.sub_item as usize).copied().unwrap_or(Column::Name);
        if info.item.mask & LVIF_IMAGE != 0 && column == Column::Name {
            info.item.image = self.icon_of(node);
        }
        if info.item.mask & LVIF_TEXT != 0 && !info.item.text.is_null() && info.item.text_max > 0 {
            let n = &self.tree.nodes[node];
            let text = match column {
                Column::Name => {
                    if self.search_text().trim().is_empty() {
                        n.name.clone()
                    } else {
                        relative(&n.path, &self.tree.nodes[self.cwd].path).to_owned()
                    }
                }
                Column::Size => human(n.size),
                Column::Packed => n.packed.map(human).unwrap_or_default(),
                Column::Modified => n.modified.and_then(short_time).unwrap_or_default(),
            };
            let max = info.item.text_max as usize - 1;
            let wide: Vec<u16> = text.encode_utf16().take(max).collect();
            // SAFETY: the list's buffer holds text_max characters.
            unsafe {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), info.item.text, wide.len());
                *info.item.text.add(wide.len()) = 0;
            }
        }
    }

    /// The system's small icon for a node's type (by extension), cached.
    fn icon_of(&mut self, node: usize) -> i32 {
        let n = &self.tree.nodes[node];
        let key = if n.is_dir {
            "/".to_owned()
        } else {
            Path::new(&n.name)
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        };
        if let Some(&i) = self.icons.get(&key) {
            return i;
        }
        let (name, attr) = if n.is_dir {
            ("folder".to_owned(), FILE_ATTRIBUTE_DIRECTORY)
        } else {
            (format!("file.{key}"), FILE_ATTRIBUTE_NORMAL)
        };
        let mut info = SHFILEINFOW::default();
        // SAFETY: a valid struct of the size passed; the name is NUL-terminated.
        unsafe {
            SHGetFileInfoW(
                &HSTRING::from(name),
                attr,
                Some(&mut info),
                size_of::<SHFILEINFOW>() as u32,
                SHGFI_SYSICONINDEX | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES,
            );
        }
        self.icons.insert(key, info.iIcon);
        info.iIcon
    }

    // ---- actions ----

    fn open(&mut self, node: usize) {
        if self.tree.nodes[node].is_dir {
            return self.enter(node);
        }
        let n = &self.tree.nodes[node];
        self.jobs += 1;
        let dir = self.temp.join(self.jobs.to_string());
        let parent = n.path.rsplit_once('/').map_or("", |(p, _)| p).to_owned();
        let target = dir.join(&n.name);
        let opts = Options {
            selection: Some(Selection {
                items: vec![n.path.clone()],
                base: parent,
            }),
            on_conflict: ConflictPolicy::Always(Resolution::Overwrite),
            ..self.options()
        };
        let name = n.name.clone();
        self.opening += 1;
        self.say(format!("Opening {name}..."));
        let (inbox, hwnd, archive) = (Arc::clone(&self.inbox), self.handle(), self.archive.clone());
        std::thread::spawn(move || {
            let result = brar_core::extract(&archive, &dir, &opts).map(|_| target);
            inbox.lock().unwrap().opened.push((node, result));
            win::post(hwnd, win::WM_WAKE);
        });
    }

    fn open_cursor(&mut self) {
        let Some(r) = &self.ready else { return };
        let row = win::send(r.list, LVM_GETNEXTITEM, usize::MAX, LVNI_FOCUSED as isize);
        if let Some(&node) = usize::try_from(row).ok().and_then(|row| self.rows.get(row)) {
            self.open(node);
        }
    }

    fn extract_to(&mut self, dest: PathBuf) {
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
        match cmd.spawn() {
            // Let the new window come to the front; otherwise Windows only flashes its
            // taskbar button (the red frame).
            Ok(child) => shell::allow_foreground(child.id()),
            Err(e) => self.say(format!("Couldn't start B-Rar: {e}")),
        }
    }

    fn submit_password(&mut self) {
        let Page::Password { retry, .. } = self.page else { return };
        let password = win::text(win::item(self.hwnd, ID_PASSWORD));
        if password.is_empty() {
            return;
        }
        self.password = Some(password);
        match retry {
            Retry::List => self.list(),
            Retry::Open(node) => {
                self.set_page(Page::Ready);
                self.refresh_rows();
                self.open(node);
            }
        }
    }

    fn cancel_password(&mut self) {
        let Page::Password { retry, .. } = self.page else { return };
        match retry {
            Retry::List => win::post(self.handle(), WM_CLOSE),
            Retry::Open(_) => {
                self.set_page(Page::Ready);
                self.refresh_rows();
            }
        }
    }

    /// WM_COMMAND. Returns work that opens a dialog, for after the state is released.
    fn command(&mut self, id: i32, code: u32) -> Option<Later> {
        let (ok, cancel) = (id == IDOK.0, id == IDCANCEL.0);
        match id {
            ID_BAR_UPDATE => self.start_update(),
            ID_BAR_NOTES => {
                if let Updates::Available(r) = &*self.updates.lock().unwrap() {
                    shell::open_path(Path::new(&r.page));
                }
            }
            ID_BAR_RETRY => self.check_for_updates(true),
            ID_BAR_CLOSE => {
                *self.updates.lock().unwrap() = Updates::Dismissed;
                self.refresh_bar();
            }
            ID_EXTRACT => return Some(Later::Extract),
            ID_OPEN => self.open_cursor(),
            ID_INFO => return Some(Later::Info(self.info())),
            ID_UPDATES => match self.update_kind {
                _ if self.updatable() => self.check_for_updates(true),
                update::Kind::Store => self.say("The Microsoft Store keeps B-Rar up to date."),
                _ => self.say("This copy of B-Rar doesn't update itself (a development build)."),
            },
            ID_TEST => self.spawn(vec!["t".into(), self.archive.clone().into()]),
            ID_UP => self.up(),
            ID_SEARCH if code == win::EN_CHANGE && !self.quiet => self.refresh_rows(),
            ID_SHOW => win::reveal(win::item(self.hwnd, ID_PASSWORD), win::checked(win::item(self.hwnd, ID_SHOW))),
            ID_PASSWORD if code == win::EN_CHANGE => {
                let typed = !win::text(win::item(self.hwnd, ID_PASSWORD)).is_empty();
                win::enable(win::item(self.hwnd, IDOK.0), typed);
            }
            _ if ok => match self.page {
                Page::Home | Page::Failed(_) => return Some(Later::PickArchive),
                Page::Password { .. } => self.submit_password(),
                Page::Ready => {
                    // Enter: in the search field, go to the results; in the list, open.
                    let focus = unsafe { GetFocus() };
                    match &self.ready {
                        Some(r) if focus == r.search => win::focus(r.list),
                        Some(_) => self.open_cursor(),
                        None => {}
                    }
                }
                Page::Loading => {}
            },
            _ if cancel => match self.page {
                Page::Password { .. } => self.cancel_password(),
                Page::Failed(_) => win::post(self.handle(), WM_CLOSE),
                Page::Ready => {
                    if let Some(r) = &self.ready {
                        if !self.search_text().is_empty() {
                            win::set_text(r.search, "");
                        } else {
                            self.set_row_state(-1, 0, LVIS_SELECTED);
                            self.update_status();
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
        None
    }

    /// WM_NOTIFY. Returns work that opens a menu, for after the state is released.
    fn notify(&mut self, hdr: &NMHDR, lparam: LPARAM) -> (Option<Later>, LRESULT) {
        let id = hdr.idFrom as i32;
        match (id, hdr.code) {
            (ID_LIST, NM_CUSTOMDRAW) => {
                // SAFETY: NM_CUSTOMDRAW from a list view comes with an NMLVCUSTOMDRAW.
                let cd = unsafe { &mut *(lparam.0 as *mut NmLvCustomDraw) };
                return (None, self.custom_draw(cd));
            }
            (ID_LIST, LVN_GETDISPINFOW) => {
                // SAFETY: LVN_GETDISPINFOW comes with an NMLVDISPINFOW.
                let info = unsafe { &mut *(lparam.0 as *mut NmLvDispInfo) };
                self.display(info);
            }
            (ID_LIST, LVN_ITEMCHANGED | LVN_ODSTATECHANGED) => self.update_status(),
            (ID_LIST, LVN_ITEMACTIVATE) => self.open_cursor(),
            (ID_LIST, LVN_COLUMNCLICK) => {
                // SAFETY: LVN_COLUMNCLICK comes with an NMLISTVIEW.
                let nm = unsafe { &*(lparam.0 as *const NmListView) };
                if let Some(&column) = COLUMNS.get(nm.sub_item as usize) {
                    self.sort = if self.sort.0 == column {
                        (column, !self.sort.1)
                    } else {
                        (column, column == Column::Name)
                    };
                    self.refresh_rows();
                }
            }
            (ID_LIST, LVN_GETEMPTYMARKUP) => {
                // SAFETY: LVN_GETEMPTYMARKUP comes with an NMLVEMPTYMARKUP.
                let nm = unsafe { &mut *(lparam.0 as *mut NmEmptyMarkup) };
                let text = if self.search_text().trim().is_empty() {
                    "This folder is empty"
                } else {
                    "No matches"
                };
                let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
                nm.markup[..wide.len()].copy_from_slice(&wide);
                nm.flags = EMF_CENTERED;
                return (None, LRESULT(1));
            }
            (ID_LIST, NM_RCLICK) => {
                // SAFETY: NM_RCLICK from a list view comes with an NMITEMACTIVATE, whose
                // first fields after the header are the item and subitem, as in NMLISTVIEW.
                let nm = unsafe { &*(lparam.0 as *const NmListView) };
                let row = usize::try_from(nm.item).ok().filter(|&r| r < self.rows.len());
                if let (Some(row), Some(r)) = (row, &self.ready) {
                    let state = win::send(r.list, LVM_GETITEMSTATE, row, LVIS_SELECTED as isize) as u32;
                    if state & LVIS_SELECTED == 0 {
                        self.set_row_state(-1, 0, LVIS_SELECTED);
                        self.set_row_state(row as isize, LVIS_SELECTED | LVIS_FOCUSED, LVIS_SELECTED | LVIS_FOCUSED);
                    }
                }
                return (Some(Later::Menu(row.map(|r| self.rows[r]))), LRESULT(0));
            }
            (ID_CRUMBS, NM_CLICK | NM_RETURN) => {
                // SAFETY: SysLink's NM_CLICK comes with an NMLINK.
                let nm = unsafe { &*(lparam.0 as *const NmLink) };
                let len = nm.id.iter().position(|&c| c == 0).unwrap_or(nm.id.len());
                if let Ok(node) = String::from_utf16_lossy(&nm.id[..len]).parse::<usize>()
                    && node < self.tree.nodes.len()
                {
                    self.enter(node);
                }
            }
            (ID_CHECK, NM_CLICK | NM_RETURN) => self.check_for_updates(true),
            (ID_SUPPORT, NM_CLICK | NM_RETURN) => shell::open_path(Path::new(SUPPORT_URL)),
            _ => {}
        }
        (None, LRESULT(0))
    }
}

/// Dialogs and menus, with the state released (see the module docs).
fn later(hwnd: HWND, work: Later) {
    let owner = hwnd.0 as isize;
    match work {
        Later::PickArchive => match shell::pick_archive(owner) {
            Ok(Some(path)) => {
                with(|b| b.open_archive(path));
            }
            Ok(None) => {}
            Err(e) => {
                with(|b| b.say(e));
            }
        },
        Later::Info(text) => {
            let name = with(|b| b.name.clone()).unwrap_or_default();
            shell::info_box(&name, &text, owner);
        }
        Later::Extract => {
            let Some(archive) = with(|b| b.archive.clone()) else { return };
            match shell::pick_folder(&archive, owner) {
                Ok(Some(dest)) => {
                    with(|b| b.extract_to(dest));
                }
                Ok(None) => {}
                Err(e) => {
                    with(|b| b.say(e));
                }
            }
        }
        Later::Menu(node) => {
            let is_dir = node.and_then(|n| with(|b| b.tree.nodes[n].is_dir));
            // SAFETY: a menu created, shown and destroyed here.
            let chosen = unsafe {
                let Ok(menu) = CreatePopupMenu() else { return };
                if let Some(is_dir) = is_dir {
                    let _ = AppendMenuW(menu, MF_STRING, MENU_OPEN, if is_dir { w!("Open folder") } else { w!("Open") });
                    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
                }
                let _ = AppendMenuW(menu, MF_STRING, MENU_EXTRACT, w!("Extract to..."));
                let mut at = POINT::default();
                let _ = GetCursorPos(&mut at);
                let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, at.x, at.y, Some(0), hwnd, None);
                let _ = DestroyMenu(menu);
                chosen.0 as usize
            };
            match chosen {
                MENU_OPEN => {
                    if let Some(node) = node {
                        with(|b| b.open(node));
                    }
                }
                MENU_EXTRACT => later(hwnd, Later::Extract),
                _ => {}
            }
        }
    }
}

/// Keyboard shortcuts, before IsDialogMessage: Ctrl+O, Ctrl+F, Alt+Up, Backspace, Ctrl+A.
fn key(hwnd: HWND, msg: &MSG) -> bool {
    if msg.message != WM_KEYDOWN && msg.message != WM_SYSKEYDOWN {
        return false;
    }
    let vk = msg.wParam.0 as u16;
    // SAFETY: plain calls.
    let (ctrl, alt) = unsafe { (GetKeyState(0x11) < 0, GetKeyState(0x12) < 0) };
    if ctrl && vk == b'O' as u16 {
        later(hwnd, Later::PickArchive);
        return true;
    }
    with(|b| {
        let Some(r) = &b.ready else { return false };
        // SAFETY: plain call.
        let focus = unsafe { GetFocus() };
        if ctrl && vk == b'F' as u16 {
            win::focus(r.search);
            win::send(r.search, win::EM_SETSEL, 0, -1);
            true
        } else if (alt && vk == 0x26) || (focus == r.list && vk == 0x08) {
            // Alt+Up anywhere, Backspace in the list: the parent folder.
            b.up();
            true
        } else if focus == r.list && ctrl && vk == b'A' as u16 {
            b.set_row_state(-1, LVIS_SELECTED, LVIS_SELECTED);
            b.update_status();
            true
        } else {
            false
        }
    })
    .unwrap_or(false)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CTLCOLORSTATIC => {
            let brush = win::ctl_color(wparam, lparam);
            let id = win::id_of(HWND(lparam.0 as _));
            // The update bar's text sits on the bar's colour.
            if id == ID_BAR_TEXT {
                return LRESULT(theme::hover_brush().0 as isize);
            }
            brush
        }
        WM_CTLCOLOREDIT => win::edit_color(wparam),
        WM_DRAWITEM if theme::draw_item(lparam) => LRESULT(1),
        WM_COMMAND => {
            let (id, code) = ((wparam.0 & 0xFFFF) as i32, ((wparam.0 >> 16) & 0xFFFF) as u32);
            if code == win::BN_CLICKED && lparam.0 != 0 {
                theme::clicked(hwnd, HWND(lparam.0 as _));
            }
            if let Some(Some(work)) = with(|b| b.command(id, code)) {
                later(hwnd, work);
            }
            LRESULT(0)
        }
        WM_NOTIFY => {
            // SAFETY: WM_NOTIFY's lParam is an NMHDR (or starts with one).
            let hdr = unsafe { &*(lparam.0 as *const NMHDR) };
            match with(|b| b.notify(hdr, lparam)) {
                Some((Some(work), result)) => {
                    later(hwnd, work);
                    result
                }
                Some((None, result)) => result,
                None => LRESULT(0),
            }
        }
        win::WM_WAKE => {
            with(Browser::wake);
            LRESULT(0)
        }
        WM_TIMER => {
            with(Browser::tick);
            LRESULT(0)
        }
        WM_SIZE => {
            with(Browser::layout);
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            let s = win::scale();
            // SAFETY: WM_GETMINMAXINFO's lParam is a MINMAXINFO.
            let info = unsafe { &mut *(lparam.0 as *mut MINMAXINFO) };
            info.ptMinTrackSize = POINT {
                x: (560.0 * s) as i32,
                y: (320.0 * s) as i32,
            };
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            // SAFETY: the standard paint pair; the window's own parts.
            unsafe {
                let dc = BeginPaint(hwnd, &mut ps);
                with(|b| b.paint(dc));
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_DROPFILES => {
            let drop = HDROP(wparam.0 as _);
            let mut buf = [0u16; 32768];
            // SAFETY: the dropped files' handle and a buffer of the size passed.
            let path = unsafe {
                let n = DragQueryFileW(drop, 0, Some(&mut buf));
                DragFinish(drop);
                PathBuf::from(String::from_utf16_lossy(&buf[..n as usize]))
            };
            if !path.as_os_str().is_empty() {
                with(|b| b.open_archive(path));
            }
            LRESULT(0)
        }
        WM_APPCOMMAND => {
            // The mouse's back button: up a folder.
            if (lparam.0 as usize >> 16) & 0x0FFF == APPCOMMAND_BROWSER_BACKWARD {
                with(Browser::up);
                return LRESULT(1);
            }
            // SAFETY: the default handling.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        // For IsDialogMessage: Enter presses the default button.
        win::DM_GETDEFID => LRESULT((0x534B << 16) | IDOK.0 as isize),
        WM_DESTROY => {
            if let Some(temp) = with(|b| b.temp.clone()) {
                // Best effort: files still open in another app stay until the next cleanup.
                let _ = std::fs::remove_dir_all(temp);
            }
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: the default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// The system's small icons, shared (the list doesn't own them: LVS_SHAREIMAGELISTS).
fn system_image_list() -> isize {
    let mut info = SHFILEINFOW::default();
    // SAFETY: a valid struct of the size passed.
    unsafe {
        SHGetFileInfoW(
            w!("file.txt"),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut info),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES,
        ) as isize
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

/// `path` relative to the folder being browsed (search results can be deeper).
fn relative<'a>(path: &'a str, folder: &str) -> &'a str {
    if folder.is_empty() {
        return path;
    }
    path.strip_prefix(folder).and_then(|r| r.strip_prefix('/')).unwrap_or(path)
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}

fn short_time(t: SystemTime) -> Option<String> {
    let ts = jiff::Timestamp::try_from(t).ok()?;
    Some(ts.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H:%M").to_string())
}
