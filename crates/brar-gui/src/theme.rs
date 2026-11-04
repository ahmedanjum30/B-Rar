//! The Workbench look (DESIGN.md), in light or dark after Windows' app setting: the palette,
//! and the controls B-Rar paints itself so they take it (buttons, tool buttons, check boxes and
//! radio buttons, status glyphs, the progress bar). Plain GDI; glyphs come from Windows' own
//! icon font (Segoe Fluent Icons, or Segoe MDL2 Assets before Windows 11).

use std::{cell::OnceCell, ffi::c_void};

use windows::{
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::{
            Dwm::DwmSetWindowAttribute,
            Gdi::{
                BeginPaint, CreatePen, CreateSolidBrush, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteObject,
                DrawTextW, Ellipse, EndPaint, FillRect, HBRUSH, HDC, HFONT, HGDIOBJ, InvalidateRect, PAINTSTRUCT, PS_SOLID, RoundRect, SelectObject,
                SetBkMode, SetTextColor, TRANSPARENT,
            },
        },
        System::{
            LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW},
            Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW},
        },
        UI::{
            Controls::SetWindowTheme,
            Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent},
            Shell::{DefSubclassProc, SetWindowSubclass},
            WindowsAndMessaging::*,
        },
    },
    core::{HSTRING, PCSTR, w},
};

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

/// The colours of one theme (COLORREF values).
pub struct Palette {
    pub dark: bool,
    /// Window background, toolbar, status bar.
    pub surface: u32,
    /// Buttons, fields, the list.
    pub field: u32,
    /// Every other list row.
    pub alt: u32,
    pub border: u32,
    pub text: u32,
    pub weak: u32,
    pub accent: u32,
    pub on_accent: u32,
    /// Hovered tool buttons, selected rows.
    pub select: u32,
    pub hover: u32,
    pub pressed: u32,
    pub ok: u32,
    pub error: u32,
    pub warn: u32,
}

const LIGHT: Palette = Palette {
    dark: false,
    surface: rgb(0xF0, 0xF0, 0xF0),
    field: rgb(0xFF, 0xFF, 0xFF),
    alt: rgb(0xF7, 0xF7, 0xF7),
    border: rgb(0xBD, 0xBD, 0xBD),
    text: rgb(0x14, 0x14, 0x14),
    weak: rgb(0x5A, 0x5A, 0x5A),
    accent: rgb(0x1F, 0x6F, 0xEB),
    on_accent: rgb(0xFF, 0xFF, 0xFF),
    select: rgb(0xCC, 0xE3, 0xFF),
    hover: rgb(0xE5, 0xF0, 0xFF),
    pressed: rgb(0xB8, 0xD6, 0xFB),
    ok: rgb(0x10, 0x7C, 0x10),
    error: rgb(0xC4, 0x2B, 0x1C),
    warn: rgb(0xA3, 0x5A, 0x00),
};

const DARK: Palette = Palette {
    dark: true,
    surface: rgb(0x25, 0x25, 0x26),
    field: rgb(0x1E, 0x1E, 0x1E),
    alt: rgb(0x23, 0x23, 0x24),
    border: rgb(0x4A, 0x4A, 0x4A),
    text: rgb(0xE8, 0xE8, 0xE8),
    weak: rgb(0xA0, 0xA0, 0xA0),
    accent: rgb(0x4C, 0x9A, 0xFF),
    on_accent: rgb(0x00, 0x00, 0x00),
    select: rgb(0x26, 0x4F, 0x78),
    hover: rgb(0x2D, 0x3A, 0x4A),
    pressed: rgb(0x1E, 0x40, 0x66),
    ok: rgb(0x6C, 0xCB, 0x5F),
    error: rgb(0xF4, 0x87, 0x71),
    warn: rgb(0xE5, 0xC0, 0x7B),
};

thread_local! {
    static PALETTE: OnceCell<&'static Palette> = const { OnceCell::new() };
    static BRUSHES: OnceCell<Brushes> = const { OnceCell::new() };
    /// For WM_DRAWITEM, which can arrive while a window's state is borrowed (building a page).
    static FONTS: std::cell::Cell<Option<PaintFonts>> = const { std::cell::Cell::new(None) };
}

/// The fonts [`draw_item`] uses, set once the window's fonts exist.
pub fn set_fonts(f: PaintFonts) {
    FONTS.set(Some(f));
}

struct Brushes {
    surface: HBRUSH,
    field: HBRUSH,
    hover: HBRUSH,
}

/// The palette for Windows' "app mode" (Settings > Personalization > Colors), read once;
/// BRAR_THEME=light or dark overrides it.
pub fn palette() -> &'static Palette {
    PALETTE.with(|p| {
        *p.get_or_init(|| {
            match std::env::var("BRAR_THEME").as_deref() {
                Ok("light") => return &LIGHT,
                Ok("dark") => return &DARK,
                _ => {}
            }
            let mut value = 1u32;
            let mut size = 4u32;
            // SAFETY: a DWORD-sized buffer and its size.
            let found = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
                    w!("AppsUseLightTheme"),
                    RRF_RT_REG_DWORD,
                    None,
                    Some(&mut value as *mut _ as *mut c_void),
                    Some(&mut size),
                )
            };
            if found.is_ok() && value == 0 { &DARK } else { &LIGHT }
        })
    })
}

fn brushes() -> (HBRUSH, HBRUSH, HBRUSH) {
    BRUSHES.with(|b| {
        let b = b.get_or_init(|| {
            let p = palette();
            // SAFETY: brushes kept for the life of the thread.
            unsafe {
                Brushes {
                    surface: CreateSolidBrush(COLORREF(p.surface)),
                    field: CreateSolidBrush(COLORREF(p.field)),
                    hover: CreateSolidBrush(COLORREF(p.hover)),
                }
            }
        });
        (b.surface, b.field, b.hover)
    })
}

pub fn surface_brush() -> HBRUSH {
    brushes().0
}

pub fn field_brush() -> HBRUSH {
    brushes().1
}

/// The update bar's colour.
pub fn hover_brush() -> HBRUSH {
    brushes().2
}

/// The icon font's face: Segoe Fluent Icons on Windows 11, Segoe MDL2 Assets before.
pub fn icon_face() -> &'static str {
    let mut size = 0u32;
    // SAFETY: asking only for the value's size.
    let fluent = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Fonts"),
            w!("Segoe Fluent Icons (TrueType)"),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    }
    .is_ok();
    if fluent { "Segoe Fluent Icons" } else { "Segoe MDL2 Assets" }
}

/// Dark title bar and dark scroll bars where the theme is dark. Call on each top-level window.
pub fn dress_window(hwnd: HWND) {
    if !palette().dark {
        return;
    }
    let on: i32 = 1;
    // SAFETY: DWMWA_USE_IMMERSIVE_DARK_MODE (20) takes a BOOL.
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE(20),
            &on as *const _ as *const c_void,
            4,
        );
    }
}

/// Lets common controls (scroll bars, the list's header) draw dark when asked with
/// [`dark_control`]: uxtheme's SetPreferredAppMode (ordinal 135), as Explorer and Notepad do.
pub fn allow_dark_controls() {
    if !palette().dark {
        return;
    }
    // SAFETY: uxtheme ships with every Windows 10/11; the ordinal's signature is
    // fn(i32) -> i32 (2 = force dark). Missing, nothing happens.
    unsafe {
        if let Ok(lib) = LoadLibraryW(w!("uxtheme.dll"))
            && let Some(f) = GetProcAddress(lib, PCSTR(135 as *const u8))
        {
            let set: extern "system" fn(i32) -> i32 = std::mem::transmute(f);
            set(2);
        }
    }
}

/// Windows' own dark rendering for a control (`theme`: "DarkMode_Explorer" for lists and
/// scroll bars, "DarkMode_CFD" for text fields, "DarkMode_ItemsView" for list headers).
pub fn dark_control(h: HWND, theme: &str) {
    if palette().dark {
        // SAFETY: a live control and a NUL-terminated theme name; uxtheme's
        // AllowDarkModeForWindow (ordinal 133) is fn(HWND, BOOL) -> BOOL, skipped if missing.
        unsafe {
            if let Ok(lib) = LoadLibraryW(w!("uxtheme.dll"))
                && let Some(f) = GetProcAddress(lib, PCSTR(133 as *const u8))
            {
                let allow: extern "system" fn(HWND, i32) -> i32 = std::mem::transmute(f);
                allow(h, 1);
            }
            let _ = SetWindowTheme(h, &HSTRING::from(theme), None);
        }
    }
}

/// Column headings in the theme's text colour (the header tells the list, not the window,
/// how it draws: hence a subclass on the list).
pub fn header_text(list: HWND) {
    if palette().dark {
        // SAFETY: a subclass on a live control, removed when it's destroyed.
        unsafe {
            let _ = SetWindowSubclass(list, Some(list_subclass), 2, 0);
        }
    }
}

unsafe extern "system" fn list_subclass(h: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, _data: usize) -> LRESULT {
    const NM_CUSTOMDRAW: u32 = (-12i32) as u32;
    if msg == WM_NOTIFY {
        #[repr(C)]
        struct CustomDraw {
            hdr: windows::Win32::UI::Controls::NMHDR,
            stage: u32,
            hdc: HDC,
        }
        // SAFETY: WM_NOTIFY's lParam starts with an NMHDR; NM_CUSTOMDRAW's with NMCUSTOMDRAW.
        let cd = unsafe { &*(lparam.0 as *const CustomDraw) };
        if cd.hdr.code == NM_CUSTOMDRAW {
            match cd.stage {
                0x1 => return LRESULT(0x20), // prepaint: tell me about each item
                0x10001 => {
                    // SAFETY: the item's DC.
                    unsafe {
                        SetTextColor(cd.hdc, COLORREF(palette().text));
                    }
                    return LRESULT(0x2); // CDRF_NEWFONT: draw with what was set
                }
                _ => {}
            }
        }
    }
    // SAFETY: the default handling.
    unsafe { DefSubclassProc(h, msg, wparam, lparam) }
}

// ---- painted controls ----------------------------------------------------------------------

/// What an owner-drawn control is; kept with its state in the control's user data.
#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Button = 1,
    Primary = 2,
    /// Glyph over a label (the browser's toolbar).
    Tool = 3,
    /// A glyph alone (Up).
    Icon = 4,
    Check = 5,
    Radio = 6,
    /// A status glyph (static).
    Glyph = 7,
}

/// Colour of a [`Kind::Glyph`].
#[derive(Clone, Copy)]
pub enum Tone {
    Accent = 0,
    Ok = 1,
    Error = 2,
    Warn = 3,
}

// User data layout: kind (bits 0-7), glyph (8-23), tone (24-27), checked (32), hot (33),
// radio group (40-47).
const CHECKED: isize = 1 << 32;
const WM_MOUSELEAVE: u32 = 0x02A3;
const HOT: isize = 1 << 33;

pub fn pack(kind: Kind, glyph: u16, tone: u8, group: u8) -> isize {
    kind as isize | (glyph as isize) << 8 | (tone as isize) << 24 | (group as isize) << 40
}

fn data(h: HWND) -> isize {
    // SAFETY: plain call.
    unsafe { GetWindowLongPtrW(h, GWLP_USERDATA) }
}

fn set_data(h: HWND, v: isize) {
    // SAFETY: B-Rar's own controls' user data.
    unsafe {
        SetWindowLongPtrW(h, GWLP_USERDATA, v);
    }
}

pub fn kind_of(h: HWND) -> Option<Kind> {
    Some(match data(h) & 0xFF {
        1 => Kind::Button,
        2 => Kind::Primary,
        3 => Kind::Tool,
        4 => Kind::Icon,
        5 => Kind::Check,
        6 => Kind::Radio,
        7 => Kind::Glyph,
        _ => return None,
    })
}

pub fn is_checked(h: HWND) -> bool {
    data(h) & CHECKED != 0
}

pub fn set_checked(h: HWND, on: bool) {
    let d = data(h);
    set_data(h, if on { d | CHECKED } else { d & !CHECKED });
    invalidate(h);
}

fn invalidate(h: HWND) {
    // SAFETY: plain call.
    unsafe {
        let _ = InvalidateRect(Some(h), None, true);
    }
}

/// A click on a painted check box or radio button: toggle it, or select it and clear the
/// rest of its group. Call from WM_COMMAND before anything reads the state.
pub fn clicked(parent: HWND, h: HWND) {
    match kind_of(h) {
        Some(Kind::Check) => set_checked(h, !is_checked(h)),
        Some(Kind::Radio) => {
            let group = (data(h) >> 40) & 0xFF;
            // SAFETY: walking this window's children.
            unsafe {
                let mut c = GetWindow(parent, GW_CHILD).ok();
                while let Some(child) = c {
                    if kind_of(child) == Some(Kind::Radio) && (data(child) >> 40) & 0xFF == group {
                        set_checked(child, child == h);
                    }
                    c = GetWindow(child, GW_HWNDNEXT).ok();
                }
            }
        }
        _ => {}
    }
}

/// Hover tracking and double-click-as-click for painted buttons.
pub fn track(h: HWND) {
    // SAFETY: a subclass on a live control, removed when it's destroyed.
    unsafe {
        let _ = SetWindowSubclass(h, Some(subclass), 1, 0);
    }
}

unsafe extern "system" fn subclass(h: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, _data: usize) -> LRESULT {
    match msg {
        WM_MOUSEMOVE if data(h) & HOT == 0 => {
            set_data(h, data(h) | HOT);
            let mut t = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: h,
                dwHoverTime: 0,
            };
            // SAFETY: the struct is the size it says.
            unsafe {
                let _ = TrackMouseEvent(&mut t);
            }
            invalidate(h);
        }
        WM_MOUSELEAVE => {
            set_data(h, data(h) & !HOT);
            invalidate(h);
        }
        // Owner-drawn buttons get double-clicks as such; make the second one a click too.
        WM_LBUTTONDBLCLK => {
            // SAFETY: forwarding as a button press.
            return unsafe { DefSubclassProc(h, WM_LBUTTONDOWN, wparam, lparam) };
        }
        _ => {}
    }
    // SAFETY: the default handling.
    unsafe { DefSubclassProc(h, msg, wparam, lparam) }
}

#[repr(C)]
struct DrawItem {
    ctl_type: u32,
    ctl_id: u32,
    item_id: u32,
    item_action: u32,
    item_state: u32,
    hwnd: HWND,
    hdc: HDC,
    rc: RECT,
    item_data: usize,
}

const ODS_SELECTED: u32 = 0x1;
const ODS_DISABLED: u32 = 0x4;
const ODS_FOCUS: u32 = 0x10;
const ODS_NOFOCUSRECT: u32 = 0x200;

/// The fonts painted controls use: text, small text, and the icon font at two sizes.
#[derive(Clone, Copy)]
pub struct PaintFonts {
    pub text: HFONT,
    pub small: HFONT,
    pub glyph: HFONT,
    pub glyph_big: HFONT,
}

/// WM_DRAWITEM for B-Rar's painted controls. Returns false for anything else.
pub fn draw_item(lparam: LPARAM) -> bool {
    let Some(fonts) = FONTS.get() else { return false };
    let f = &fonts;
    // SAFETY: WM_DRAWITEM's lParam is a DRAWITEMSTRUCT.
    let di = unsafe { &*(lparam.0 as *const DrawItem) };
    let Some(kind) = kind_of(di.hwnd) else { return false };
    let p = palette();
    let (dc, r) = (di.hdc, di.rc);
    let pressed = di.item_state & ODS_SELECTED != 0;
    let disabled = di.item_state & ODS_DISABLED != 0;
    let hot = data(di.hwnd) & HOT != 0;
    let label = crate::win::text(di.hwnd);
    let glyph = ((data(di.hwnd) >> 8) & 0xFFFF) as u16;
    // SAFETY: painting into the DC Windows handed over, objects restored and freed.
    unsafe {
        fill(dc, &r, p.surface);
        match kind {
            Kind::Button | Kind::Primary => {
                let primary = kind == Kind::Primary && !disabled;
                let bg = if primary {
                    p.accent
                } else if pressed {
                    p.pressed
                } else if hot && !disabled {
                    p.hover
                } else {
                    p.field
                };
                let border = if primary || (hot && !disabled) { p.accent } else { p.border };
                round(dc, &r, 3, bg, border);
                let color = if primary {
                    p.on_accent
                } else if disabled {
                    p.weak
                } else {
                    p.text
                };
                text(dc, &r, &label, f.text, color, DT_CENTER);
                if di.item_state & ODS_FOCUS != 0 && di.item_state & ODS_NOFOCUSRECT == 0 {
                    let inner = inset(&r, 3);
                    round(dc, &inner, 2, bg, if primary { p.on_accent } else { p.accent });
                    text(dc, &r, &label, f.text, color, DT_CENTER);
                }
            }
            Kind::Tool | Kind::Icon => {
                if pressed || hot {
                    round(
                        dc,
                        &r,
                        3,
                        if pressed { p.pressed } else { p.hover },
                        if pressed { p.pressed } else { p.hover },
                    );
                }
                let color = if disabled { p.weak } else { p.accent };
                if kind == Kind::Tool {
                    let h = r.bottom - r.top;
                    let top = RECT {
                        bottom: r.top + h * 60 / 100,
                        ..r
                    };
                    glyph_at(dc, &top, glyph, f.glyph_big, color);
                    let bottom = RECT {
                        top: r.top + h * 58 / 100,
                        ..r
                    };
                    text(dc, &bottom, &label, f.small, if disabled { p.weak } else { p.text }, DT_CENTER);
                } else {
                    glyph_at(dc, &r, glyph, f.glyph, color);
                }
            }
            Kind::Check | Kind::Radio => {
                let size = (r.bottom - r.top).min(18);
                let top = r.top + (r.bottom - r.top - size) / 2;
                let b = RECT {
                    left: r.left,
                    top,
                    right: r.left + size,
                    bottom: top + size,
                };
                let on = is_checked(di.hwnd);
                let edge = if disabled {
                    p.border
                } else if hot {
                    p.accent
                } else {
                    p.weak
                };
                if kind == Kind::Check {
                    round(dc, &b, 3, if on { p.accent } else { p.field }, if on { p.accent } else { edge });
                    if on {
                        glyph_at(dc, &b, 0xE73E, f.glyph, p.on_accent);
                    }
                } else {
                    oval(dc, &b, p.field, if on { p.accent } else { edge });
                    if on {
                        let d = inset(&b, size / 4);
                        oval(dc, &d, p.accent, p.accent);
                    }
                }
                let t = RECT { left: b.right + 8, ..r };
                text(dc, &t, &label, f.text, if disabled { p.weak } else { p.text }, DT_LEFT);
                if di.item_state & ODS_FOCUS != 0 && di.item_state & ODS_NOFOCUSRECT == 0 {
                    let _ = windows::Win32::Graphics::Gdi::DrawFocusRect(dc, &t);
                }
            }
            Kind::Glyph => {
                let tone = (data(di.hwnd) >> 24) & 0xF;
                let color = match tone {
                    1 => p.ok,
                    2 => p.error,
                    3 => p.warn,
                    _ => p.accent,
                };
                glyph_at(dc, &r, glyph, f.glyph_big, color);
            }
        }
    }
    true
}

fn inset(r: &RECT, by: i32) -> RECT {
    RECT {
        left: r.left + by,
        top: r.top + by,
        right: r.right - by,
        bottom: r.bottom - by,
    }
}

/// Fills `r` with `color`.
pub fn fill(dc: HDC, r: &RECT, color: u32) {
    // SAFETY: a brush made, used and freed here.
    unsafe {
        let b = CreateSolidBrush(COLORREF(color));
        FillRect(dc, r, b);
        let _ = DeleteObject(HGDIOBJ(b.0));
    }
}

/// A rounded rectangle, filled and outlined.
pub fn round(dc: HDC, r: &RECT, radius: i32, color: u32, border: u32) {
    // SAFETY: a pen and brush made, selected, restored and freed here.
    unsafe {
        let b = CreateSolidBrush(COLORREF(color));
        let pen = CreatePen(PS_SOLID, 1, COLORREF(border));
        let (ob, op) = (SelectObject(dc, HGDIOBJ(b.0)), SelectObject(dc, HGDIOBJ(pen.0)));
        let _ = RoundRect(dc, r.left, r.top, r.right, r.bottom, radius * 2, radius * 2);
        SelectObject(dc, ob);
        SelectObject(dc, op);
        let _ = DeleteObject(HGDIOBJ(b.0));
        let _ = DeleteObject(HGDIOBJ(pen.0));
    }
}

fn oval(dc: HDC, r: &RECT, color: u32, border: u32) {
    // SAFETY: as in `round`.
    unsafe {
        let b = CreateSolidBrush(COLORREF(color));
        let pen = CreatePen(PS_SOLID, 1, COLORREF(border));
        let (ob, op) = (SelectObject(dc, HGDIOBJ(b.0)), SelectObject(dc, HGDIOBJ(pen.0)));
        let _ = Ellipse(dc, r.left, r.top, r.right, r.bottom);
        SelectObject(dc, ob);
        SelectObject(dc, op);
        let _ = DeleteObject(HGDIOBJ(b.0));
        let _ = DeleteObject(HGDIOBJ(pen.0));
    }
}

/// One line of text in `r`, vertically centred, `align` horizontally.
pub fn text(dc: HDC, r: &RECT, s: &str, font: HFONT, color: u32, align: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT) {
    // SAFETY: the font is restored after drawing.
    unsafe {
        let old = SelectObject(dc, HGDIOBJ(font.0));
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(color));
        let mut wide: Vec<u16> = s.encode_utf16().collect();
        let mut rr = *r;
        DrawTextW(dc, &mut wide, &mut rr, align | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
        SelectObject(dc, old);
    }
}

fn glyph_at(dc: HDC, r: &RECT, glyph: u16, font: HFONT, color: u32) {
    let s = String::from_utf16_lossy(&[glyph]);
    text(dc, r, &s, font, color, DT_CENTER);
}

// ---- the progress bar ----------------------------------------------------------------------

/// The Workbench progress bar: a bordered field with an accent fill, amber when paused, or a
/// sweeping segment while the total isn't known. Set with [`set_bar`].
pub const BAR_CLASS: windows::core::PCWSTR = w!("BRarBar");
const BAR_PAUSED: isize = 1 << 16;
const BAR_SWEEP: isize = 1 << 17;

pub fn register_bar() {
    // SAFETY: a static class name and a valid window procedure.
    unsafe {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(bar_proc),
            hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
            lpszClassName: BAR_CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
    }
}

/// `pos` out of 10 000; `sweep` animates instead (unknown total).
pub fn set_bar(h: HWND, pos: u32, paused: bool, sweep: bool) {
    let v = pos.min(10_000) as isize | if paused { BAR_PAUSED } else { 0 } | if sweep { BAR_SWEEP } else { 0 };
    if data(h) != v {
        set_data(h, v);
        // SAFETY: plain calls on a live control.
        unsafe {
            if sweep {
                SetTimer(Some(h), 1, 30, None);
            } else {
                let _ = KillTimer(Some(h), 1);
            }
        }
        invalidate(h);
    }
}

unsafe extern "system" fn bar_proc(h: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            invalidate(h);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let p = palette();
            let mut ps = PAINTSTRUCT::default();
            // SAFETY: the standard paint pair.
            unsafe {
                let dc = BeginPaint(h, &mut ps);
                let mut r = RECT::default();
                let _ = GetClientRect(h, &mut r);
                round(dc, &r, 2, p.field, p.border);
                let v = data(h);
                let inner = inset(&r, 2);
                let w = inner.right - inner.left;
                let color = if v & BAR_PAUSED != 0 { p.warn } else { p.accent };
                let span = if v & BAR_SWEEP != 0 {
                    let t = (windows::Win32::System::SystemInformation::GetTickCount() % 1400) as i32;
                    let seg = w * 2 / 5;
                    let x = inner.left - seg + (w + seg) * t / 1400;
                    Some((x.max(inner.left), (x + seg).min(inner.right)))
                } else {
                    let fw = (w as i64 * (v & 0xFFFF) as i64 / 10_000) as i32;
                    (fw > 0).then_some((inner.left, inner.left + fw))
                };
                if let Some((a, b)) = span.filter(|(a, b)| b > a) {
                    let fill_r = RECT { left: a, right: b, ..inner };
                    fill(dc, &fill_r, color);
                }
                let _ = EndPaint(h, &ps);
            }
            LRESULT(0)
        }
        // SAFETY: the default handling.
        _ => unsafe { DefWindowProcW(h, msg, wparam, lparam) },
    }
}
