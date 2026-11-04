//! Small helpers over Win32 for B-Rar's two windows (progress and browser): fonts, child
//! controls and their text, icons, a message loop, and [`Builder`], which stacks controls down a
//! page the way a dialog lays them out, in the Workbench look ([`crate::theme`]).

use std::ffi::c_void;

use windows::{
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM},
        Graphics::Gdi::{
            CreateFontIndirectW, DT_CALCRECT, DT_NOPREFIX, DT_WORDBREAK, DeleteObject, DrawTextW, GetDC, GetTextExtentPoint32W, GetTextMetricsW, HDC,
            HFONT, HGDIOBJ, ReleaseDC, SelectObject, SetBkMode, SetTextColor, TEXTMETRICW, TRANSPARENT,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::{
                ICC_BAR_CLASSES, ICC_LINK_CLASS, ICC_LISTVIEW_CLASSES, ICC_PROGRESS_CLASS, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX,
                InitCommonControlsEx,
            },
            HiDpi::GetDpiForSystem,
            Input::KeyboardAndMouse::EnableWindow,
            WindowsAndMessaging::*,
        },
    },
    core::{HSTRING, PCWSTR, w},
};

pub use windows::Win32::UI::WindowsAndMessaging::{IDCANCEL, IDOK};

/// A worker thread has news (posted to the window, which then looks at the shared state).
pub const WM_WAKE: u32 = WM_APP + 1;
/// Answer to IsDialogMessage's DM_GETDEFID: Enter presses IDOK.
pub const DM_GETDEFID: u32 = WM_USER;

pub const BS_OWNERDRAW: u32 = 0xB;
pub const SS_OWNERDRAW: u32 = 0xD;
pub const ES_AUTOHSCROLL: u32 = 0x80;
pub const ES_PASSWORD: u32 = 0x20;
pub const SS_NOPREFIX: u32 = 0x80;
pub const SS_ENDELLIPSIS: u32 = 0x4000;
pub const SS_PATHELLIPSIS: u32 = 0x8000;
pub const SS_ICON: u32 = 0x3;
pub const SS_REALSIZECONTROL: u32 = 0x40;
pub const STM_SETICON: u32 = 0x170;
pub const EM_SETCUEBANNER: u32 = 0x1501;
pub const EM_SETPASSWORDCHAR: u32 = 0xCC;
pub const EM_SETSEL: u32 = 0xB1;
pub const BST_CHECKED: usize = 1;
pub const BN_CLICKED: u32 = 0;
pub const EN_CHANGE: u32 = 0x300;

/// What a static's text looks like ([`Builder::label`]); kept in the control's user data and
/// applied in WM_CTLCOLORSTATIC ([`ctl_color`]).
#[derive(Clone, Copy, PartialEq)]
pub enum Role {
    Normal = 0,
    Weak = 1,
    Error = 2,
}

/// Common Controls v6 (from the manifest) for every control B-Rar uses.
pub fn init_controls() {
    // SAFETY: plain call with a filled-in struct.
    unsafe {
        let _ = InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES | ICC_PROGRESS_CLASS | ICC_LINK_CLASS | ICC_LISTVIEW_CLASSES | ICC_BAR_CLASSES,
        });
    }
    crate::theme::register_bar();
    crate::theme::allow_dark_controls();
}

/// Pixels at 100% scaling to pixels at the system's scaling (the GUI is system DPI aware).
pub fn scale() -> f64 {
    // SAFETY: no arguments.
    (unsafe { GetDpiForSystem() }) as f64 / 96.0
}

/// The fonts both windows use: Windows' message font (Segoe UI), bigger, smaller and semibold
/// kinds, and Windows' icon font for glyphs.
pub struct Fonts {
    pub normal: HFONT,
    pub small: HFONT,
    pub bold: HFONT,
    pub heading: HFONT,
    pub big: HFONT,
    pub glyph: HFONT,
    pub glyph_big: HFONT,
}

impl Fonts {
    pub fn new() -> Self {
        // SAFETY: the struct is the size it says; the fonts are freed in Drop.
        unsafe {
            let mut ncm = NONCLIENTMETRICSW {
                cbSize: size_of::<NONCLIENTMETRICSW>() as u32,
                ..Default::default()
            };
            let _ = SystemParametersInfoW(
                SPI_GETNONCLIENTMETRICS,
                ncm.cbSize,
                Some(&mut ncm as *mut _ as *mut c_void),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
            let base = ncm.lfMessageFont;
            let with = |size: f64, weight: i32| {
                let mut f = base;
                f.lfHeight = (base.lfHeight as f64 * size).round() as i32;
                f.lfWeight = weight;
                CreateFontIndirectW(&f)
            };
            let icons = |px: f64| {
                let mut f = base;
                f.lfHeight = -(px * scale()).round() as i32;
                f.lfWeight = 400;
                let face: Vec<u16> = crate::theme::icon_face().encode_utf16().collect();
                f.lfFaceName = [0; 32];
                f.lfFaceName[..face.len()].copy_from_slice(&face);
                CreateFontIndirectW(&f)
            };
            Fonts {
                normal: CreateFontIndirectW(&base),
                small: with(0.9, 400),
                bold: with(1.0, 600),
                heading: with(1.15, 600),
                big: with(2.0, 600),
                glyph: icons(14.0),
                glyph_big: icons(24.0),
            }
        }
    }
}

impl Fonts {
    /// The fonts painted controls draw with.
    pub fn paint(&self) -> crate::theme::PaintFonts {
        crate::theme::PaintFonts {
            text: self.normal,
            small: self.small,
            glyph: self.glyph,
            glyph_big: self.glyph_big,
        }
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        // SAFETY: fonts this struct created; no control uses them once the window is gone.
        unsafe {
            for f in [self.normal, self.small, self.bold, self.heading, self.big, self.glyph, self.glyph_big] {
                let _ = DeleteObject(HGDIOBJ(f.0));
            }
        }
    }
}

/// A child control. `style` adds to WS_CHILD | WS_VISIBLE.
pub fn child(parent: HWND, class: PCWSTR, text: &str, style: u32, ex: u32, id: i32, font: HFONT) -> HWND {
    // SAFETY: a parent window and a NUL-terminated class and text that outlive the call.
    unsafe {
        let style = WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style);
        let menu = HMENU(id as isize as _);
        let h = CreateWindowExW(
            WINDOW_EX_STYLE(ex),
            class,
            &HSTRING::from(text),
            style,
            0,
            0,
            0,
            0,
            Some(parent),
            Some(menu),
            None,
            None,
        )
        .unwrap_or_default();
        SendMessageW(h, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
        h
    }
}

/// A control's ID.
pub fn id_of(h: HWND) -> i32 {
    // SAFETY: plain call.
    unsafe { GetDlgCtrlID(h) }
}

pub fn item(parent: HWND, id: i32) -> HWND {
    // SAFETY: plain call.
    unsafe { GetDlgItem(Some(parent), id).unwrap_or_default() }
}

pub fn text(h: HWND) -> String {
    // SAFETY: a buffer of the length passed.
    unsafe {
        let len = GetWindowTextLengthW(h).max(0) as usize;
        let mut buf = vec![0u16; len + 1];
        let n = GetWindowTextW(h, &mut buf);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

/// Sets a control's text, only if it changed (no flicker on the progress page's timer).
pub fn set_text(h: HWND, s: &str) {
    if text(h) != s {
        // SAFETY: a NUL-terminated string that outlives the call.
        unsafe {
            let _ = SetWindowTextW(h, &HSTRING::from(s));
        }
    }
}

pub fn checked(h: HWND) -> bool {
    if crate::theme::kind_of(h).is_some() {
        return crate::theme::is_checked(h);
    }
    // SAFETY: plain message.
    unsafe { SendMessageW(h, BM_GETCHECK, None, None).0 as usize == BST_CHECKED }
}

pub fn set_check(h: HWND, on: bool) {
    if crate::theme::kind_of(h).is_some() {
        return crate::theme::set_checked(h, on);
    }
    // SAFETY: plain message.
    unsafe {
        SendMessageW(h, BM_SETCHECK, Some(WPARAM(on as usize)), None);
    }
}

pub fn enable(h: HWND, on: bool) {
    // SAFETY: plain call.
    unsafe {
        let _ = EnableWindow(h, on);
    }
}

pub fn send(h: HWND, msg: u32, w: usize, l: isize) -> isize {
    // SAFETY: the caller passes what `msg` expects.
    unsafe { SendMessageW(h, msg, Some(WPARAM(w)), Some(LPARAM(l))).0 }
}

pub fn focus(h: HWND) {
    // SAFETY: plain call.
    unsafe {
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(h));
    }
}

pub fn destroy(h: HWND) {
    // SAFETY: plain call.
    unsafe {
        let _ = DestroyWindow(h);
    }
}

pub fn set_title(hwnd: HWND, title: &str) {
    set_text(hwnd, title);
}

/// A text field's grey hint ("Search this folder").
pub fn cue(edit: HWND, hint: &str) {
    let hint: Vec<u16> = hint.encode_utf16().chain(Some(0)).collect();
    send(edit, EM_SETCUEBANNER, 1, hint.as_ptr() as isize);
}

/// Shows or hides a password field's characters.
pub fn reveal(edit: HWND, show: bool) {
    send(edit, EM_SETPASSWORDCHAR, if show { 0 } else { 0x25CF }, 0);
    // SAFETY: plain call.
    unsafe {
        let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(edit), None, true);
    }
}

/// B-Rar's icon (the program's icon resource) at `size` pixels.
pub fn app_icon(size: i32) -> HICON {
    // SAFETY: resource 1 is the icon build.rs links in.
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        LoadImageW(Some(instance.into()), PCWSTR(1 as _), IMAGE_ICON, size, size, LR_DEFAULTCOLOR)
            .map(|h| HICON(h.0))
            .unwrap_or_default()
    }
}

/// Colours a static by its [`Role`] on the window background (WM_CTLCOLORSTATIC).
pub fn ctl_color(wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let p = crate::theme::palette();
    // SAFETY: WM_CTLCOLORSTATIC's arguments: the control's DC and handle.
    unsafe {
        let hdc = HDC(wparam.0 as _);
        SetBkMode(hdc, TRANSPARENT);
        let role = GetWindowLongPtrW(HWND(lparam.0 as _), GWLP_USERDATA);
        let color = match role {
            1 => p.weak,
            2 => p.error,
            _ => p.text,
        };
        SetTextColor(hdc, COLORREF(color));
        LRESULT(crate::theme::surface_brush().0 as isize)
    }
}

/// Text fields in the theme's colours (WM_CTLCOLOREDIT).
pub fn edit_color(wparam: WPARAM) -> LRESULT {
    let p = crate::theme::palette();
    // SAFETY: WM_CTLCOLOREDIT's DC.
    unsafe {
        let hdc = HDC(wparam.0 as _);
        SetTextColor(hdc, COLORREF(p.text));
        windows::Win32::Graphics::Gdi::SetBkColor(hdc, COLORREF(p.field));
        LRESULT(crate::theme::field_brush().0 as isize)
    }
}

pub fn set_role(h: HWND, role: Role) {
    // SAFETY: statics' user data is B-Rar's own.
    unsafe {
        SetWindowLongPtrW(h, GWLP_USERDATA, role as isize);
        let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(h), None, true);
    }
}

/// The text's width and line height in `font`.
pub fn measure(font: HFONT, text: &str) -> (i32, i32) {
    // SAFETY: a screen DC, selected font restored before release.
    unsafe {
        let dc = GetDC(None);
        let old = SelectObject(dc, HGDIOBJ(font.0));
        let wide: Vec<u16> = text.encode_utf16().collect();
        let mut size = SIZE::default();
        let _ = GetTextExtentPoint32W(dc, &wide, &mut size);
        let mut tm = TEXTMETRICW::default();
        let _ = GetTextMetricsW(dc, &mut tm);
        SelectObject(dc, old);
        ReleaseDC(None, dc);
        (size.cx, tm.tmHeight)
    }
}

/// The height `text` takes in `font`, wrapped to `width`.
pub fn wrapped_height(font: HFONT, text: &str, width: i32) -> i32 {
    // SAFETY: as in `measure`.
    unsafe {
        let dc = GetDC(None);
        let old = SelectObject(dc, HGDIOBJ(font.0));
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: 0,
        };
        DrawTextW(dc, &mut wide, &mut rect, DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX);
        SelectObject(dc, old);
        ReleaseDC(None, dc);
        rect.bottom
    }
}

/// Registers a window class with B-Rar's icon and the window background.
pub fn register(class: PCWSTR, wndproc: WNDPROC) {
    // SAFETY: a static class name and a valid window procedure.
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: wndproc,
            hInstance: instance.into(),
            lpszClassName: class,
            hbrBackground: crate::theme::surface_brush(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: app_icon(32),
            hIconSm: app_icon(16),
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
}

/// Resizes `hwnd` so its client area is `w` × `h`; centred on the screen if `center`, else
/// keeping its centre where it was.
pub fn fit_client(hwnd: HWND, w: i32, h: i32, center: bool) {
    // SAFETY: plain calls on a live window.
    unsafe {
        let style = WINDOW_STYLE(GetWindowLongW(hwnd, GWL_STYLE) as u32);
        let ex = WINDOW_EX_STYLE(GetWindowLongW(hwnd, GWL_EXSTYLE) as u32);
        let mut r = RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        };
        let _ = AdjustWindowRectEx(&mut r, style, false, ex);
        let (ww, wh) = (r.right - r.left, r.bottom - r.top);
        let mut now = RECT::default();
        let _ = GetWindowRect(hwnd, &mut now);
        let (cx, cy) = if center {
            (GetSystemMetrics(SM_CXSCREEN) / 2, GetSystemMetrics(SM_CYSCREEN) / 2)
        } else {
            ((now.left + now.right) / 2, (now.top + now.bottom) / 2)
        };
        if now.right - now.left == ww && now.bottom - now.top == wh && !center {
            return;
        }
        let _ = SetWindowPos(hwnd, None, cx - ww / 2, cy - wh / 2, ww, wh, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

pub fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut r = RECT::default();
    // SAFETY: plain call.
    unsafe {
        let _ = GetClientRect(hwnd, &mut r);
    }
    (r.right, r.bottom)
}

pub fn place(h: HWND, x: i32, y: i32, w: i32, height: i32) {
    // SAFETY: plain call.
    unsafe {
        // No copying of the old pixels: the window is repainted after a layout ([`redraw`]),
        // and copied bits left ghosts of moved controls behind.
        let _ = SetWindowPos(h, None, x, y, w, height, SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOCOPYBITS);
    }
}

/// Repaints the whole window, background and every control, after its controls moved.
pub fn redraw(hwnd: HWND) {
    use windows::Win32::Graphics::Gdi::{RDW_ALLCHILDREN, RDW_ERASE, RDW_INVALIDATE, RedrawWindow};
    // SAFETY: plain call on a live window.
    unsafe {
        let _ = RedrawWindow(Some(hwnd), None, None, RDW_ERASE | RDW_INVALIDATE | RDW_ALLCHILDREN);
    }
}

pub fn show(h: HWND, on: bool) {
    // SAFETY: plain call.
    unsafe {
        let _ = ShowWindow(h, if on { SW_SHOW } else { SW_HIDE });
    }
}

/// Runs the message loop until the window closes. `pre` sees each message first and returns
/// `true` for one it handled (keyboard shortcuts); then IsDialogMessage gives Tab, Enter and
/// Esc their dialog meaning.
pub fn run_loop(hwnd: HWND, mut pre: impl FnMut(&MSG) -> bool) {
    // SAFETY: the standard message loop on this thread's window.
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if pre(&msg) {
                continue;
            }
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

pub fn post(hwnd: isize, msg: u32) {
    // SAFETY: posting to a window handle (gone windows just drop the message).
    unsafe {
        let _ = PostMessageW(Some(HWND(hwnd as _)), msg, WPARAM(0), LPARAM(0));
    }
}

/// A button B-Rar paints itself ([`crate::theme::draw_item`]), with hover tracking.
pub fn painted(parent: HWND, text: &str, kind: crate::theme::Kind, glyph: u16, group: u8, id: i32, font: HFONT) -> HWND {
    let b = child(parent, BUTTON, text, BS_OWNERDRAW | WS_TABSTOP.0, 0, id, font);
    // SAFETY: B-Rar's own control's user data.
    unsafe {
        SetWindowLongPtrW(b, GWLP_USERDATA, crate::theme::pack(kind, glyph, 0, group));
    }
    crate::theme::track(b);
    b
}

pub const STATIC: PCWSTR = w!("STATIC");
pub const BUTTON: PCWSTR = w!("BUTTON");
pub const EDIT: PCWSTR = w!("EDIT");
pub const LINK: PCWSTR = w!("SysLink");

/// Controls stacked down a page of fixed width, dialog style. Positions are relative to the
/// page; [`Page::place`] moves them to where the page sits in the window.
pub struct Builder<'a> {
    pub parent: HWND,
    pub fonts: &'a Fonts,
    pub s: f64,
    pub width: i32,
    pub y: i32,
    items: Vec<(HWND, RECT)>,
}

/// A built page: its controls with their places, and its size.
#[derive(Default)]
pub struct Page {
    items: Vec<(HWND, RECT)>,
    pub width: i32,
    pub height: i32,
}

impl Page {
    pub fn place(&self, x: i32, y: i32) {
        for (h, r) in &self.items {
            place(*h, x + r.left, y + r.top, r.right - r.left, r.bottom - r.top);
        }
    }

    pub fn clear(&mut self) {
        for (h, _) in self.items.drain(..) {
            destroy(h);
        }
    }
}

impl<'a> Builder<'a> {
    pub fn new(parent: HWND, fonts: &'a Fonts, width: i32) -> Self {
        let s = scale();
        Builder {
            parent,
            fonts,
            s,
            width: (width as f64 * s).round() as i32,
            y: 0,
            items: Vec::new(),
        }
    }

    /// `v` pixels at 100% scaling.
    pub fn px(&self, v: i32) -> i32 {
        (v as f64 * self.s).round() as i32
    }

    pub fn add(&mut self, h: HWND, x: i32, y: i32, w: i32, height: i32) -> HWND {
        self.items.push((
            h,
            RECT {
                left: x,
                top: y,
                right: x + w,
                bottom: y + height,
            },
        ));
        h
    }

    pub fn space(&mut self, v: i32) {
        self.y += self.px(v);
    }

    fn line(&self, font: HFONT) -> i32 {
        measure(font, "Ag").1
    }

    /// An icon and two lines beside it: a title, and a subtitle in grey.
    pub fn header(&mut self, icon: HICON, title: &str, subtitle: &str) {
        let size = self.px(40);
        let ic = child(self.parent, STATIC, "", SS_ICON | SS_REALSIZECONTROL, 0, 0, self.fonts.normal);
        send(ic, STM_SETICON, icon.0 as usize, 0);
        self.add(ic, 0, self.y, size, size);
        let x = size + self.px(12);
        let w = self.width - x;
        let (th, sh) = (self.line(self.fonts.heading), self.line(self.fonts.normal));
        let top = self.y + (size - th - sh).max(0) / 2;
        let t = child(self.parent, STATIC, title, SS_NOPREFIX | SS_ENDELLIPSIS, 0, 0, self.fonts.heading);
        self.add(t, x, top, w, th);
        let st = child(self.parent, STATIC, subtitle, SS_NOPREFIX | SS_PATHELLIPSIS, 0, 0, self.fonts.normal);
        set_role(st, Role::Weak);
        self.add(st, x, top + th, w, sh);
        self.y += size.max(th + sh);
    }

    /// A status glyph (Fluent icon font) and two lines beside it: a title, and a subtitle in grey.
    pub fn header_glyph(&mut self, glyph: u16, tone: crate::theme::Tone, title: &str, subtitle: &str) {
        let size = self.px(32);
        let ic = child(self.parent, STATIC, "", SS_OWNERDRAW, 0, 0, self.fonts.normal);
        // SAFETY: B-Rar's own control's user data.
        unsafe {
            SetWindowLongPtrW(ic, GWLP_USERDATA, crate::theme::pack(crate::theme::Kind::Glyph, glyph, tone as u8, 0));
        }
        let (th, sh) = (self.line(self.fonts.heading), self.line(self.fonts.normal));
        let block = if subtitle.is_empty() { th } else { th + sh };
        let height = size.max(block);
        self.add(ic, 0, self.y + (height - size) / 2, size, size);
        let x = size + self.px(12);
        let w = self.width - x;
        let top = self.y + (height - block) / 2;
        let t = child(self.parent, STATIC, title, SS_NOPREFIX | SS_ENDELLIPSIS, 0, 0, self.fonts.heading);
        self.add(t, x, top, w, th);
        if !subtitle.is_empty() {
            let st = child(self.parent, STATIC, subtitle, SS_NOPREFIX | SS_PATHELLIPSIS, 0, 0, self.fonts.normal);
            set_role(st, Role::Weak);
            self.add(st, x, top + th, w, sh);
        }
        self.y += height;
    }

    /// Label-value pairs, one to a row, the values getting the rest of the width.
    pub fn rows(&mut self, rows: &[(&str, &str)]) {
        let h = self.line(self.fonts.normal);
        let label_w = rows.iter().map(|(l, _)| measure(self.fonts.normal, l).0).max().unwrap_or(0) + self.px(16);
        for (label, value) in rows {
            let l = child(self.parent, STATIC, label, SS_NOPREFIX, 0, 0, self.fonts.normal);
            set_role(l, Role::Weak);
            self.add(l, 0, self.y, label_w, h);
            let v = child(self.parent, STATIC, value, SS_NOPREFIX | SS_ENDELLIPSIS, 0, 0, self.fonts.normal);
            self.add(v, label_w, self.y, self.width - label_w, h);
            self.y += h + self.px(4);
        }
    }

    /// Label-value pairs, two to a row (Elapsed 0:02 · Processed 1.2 GB of 4.5 GB). The
    /// values' ids come back in order, for updating them later.
    pub fn grid(&mut self, rows: &[[(&str, i32, &str); 2]]) {
        let h = self.line(self.fonts.normal);
        let label_w = self.px(84);
        let half = self.width / 2;
        for row in rows {
            for (k, (label, id, value)) in row.iter().enumerate() {
                if label.is_empty() {
                    continue;
                }
                let x = k as i32 * half;
                let l = child(self.parent, STATIC, label, SS_NOPREFIX, 0, 0, self.fonts.normal);
                set_role(l, Role::Weak);
                self.add(l, x, self.y, label_w, h);
                let v = child(self.parent, STATIC, value, SS_NOPREFIX | SS_ENDELLIPSIS, 0, *id, self.fonts.normal);
                self.add(v, x + label_w, self.y, half - label_w - self.px(8), h);
            }
            self.y += h + self.px(4);
        }
    }

    /// A line of text, or a wrapped paragraph.
    pub fn label(&mut self, id: i32, text: &str, role: Role, wrap: bool) -> HWND {
        let font = self.fonts.normal;
        let h = if wrap {
            wrapped_height(font, text, self.width).max(self.line(font))
        } else {
            self.line(font)
        };
        let style = SS_NOPREFIX | if wrap { 0 } else { SS_ENDELLIPSIS };
        let l = child(self.parent, STATIC, text, style, 0, id, font);
        set_role(l, role);
        self.add(l, 0, self.y, self.width, h);
        self.y += h;
        l
    }

    pub fn edit_height(&self) -> i32 {
        self.line(self.fonts.normal) + self.px(10)
    }

    pub fn button_height(&self) -> i32 {
        self.line(self.fonts.normal) + self.px(14)
    }

    /// A one-line text field across the page, less `room` on the right.
    pub fn edit(&mut self, id: i32, text: &str, style: u32, hint: &str, room: i32) -> HWND {
        let h = self.edit_height();
        let e = child(
            self.parent,
            EDIT,
            text,
            ES_AUTOHSCROLL | WS_TABSTOP.0 | style,
            WS_EX_CLIENTEDGE.0,
            id,
            self.fonts.normal,
        );
        crate::theme::dark_control(e, "DarkMode_CFD");
        if !hint.is_empty() {
            cue(e, hint);
        }
        self.add(e, 0, self.y, self.width - room, h);
        self.y += h;
        e
    }

    pub fn check(&mut self, id: i32, text: &str, on: bool) -> HWND {
        let h = self.line(self.fonts.normal) + self.px(8);
        let c = painted(self.parent, text, crate::theme::Kind::Check, 0, 0, id, self.fonts.normal);
        set_check(c, on);
        self.add(c, 0, self.y, self.width, h);
        self.y += h;
        c
    }

    /// Radio buttons in a row; `on` is the index of the selected one.
    pub fn radios(&mut self, first_id: i32, labels: &[&str], on: usize) {
        let h = self.line(self.fonts.normal) + self.px(8);
        let mut x = 0;
        for (i, label) in labels.iter().enumerate() {
            let r = painted(
                self.parent,
                label,
                crate::theme::Kind::Radio,
                0,
                first_id as u8,
                first_id + i as i32,
                self.fonts.normal,
            );
            set_check(r, i == on);
            let w = measure(self.fonts.normal, label).0 + self.px(30);
            self.add(r, x, self.y, w, h);
            x += w + self.px(12);
        }
        self.y += h;
    }

    /// The Workbench progress bar ([`crate::theme::set_bar`]); `sweep` while the total is unknown.
    pub fn progress(&mut self, id: i32, sweep: bool) -> HWND {
        let h = self.px(14);
        let p = child(self.parent, crate::theme::BAR_CLASS, "", 0, 0, id, self.fonts.normal);
        crate::theme::set_bar(p, 0, false, sweep);
        self.add(p, 0, self.y, self.width, h);
        self.y += h;
        p
    }

    /// A row of buttons on the right, given left to right; the one with id IDOK is the default.
    pub fn buttons(&mut self, buttons: &[(i32, &str)]) {
        let h = self.button_height();
        let gap = self.px(8);
        let widths: Vec<i32> = buttons
            .iter()
            .map(|(_, t)| (measure(self.fonts.normal, t).0 + self.px(32)).max(self.px(88)))
            .collect();
        let mut x = self.width - widths.iter().sum::<i32>() - gap * (buttons.len() as i32 - 1).max(0);
        for ((id, label), w) in buttons.iter().zip(widths) {
            let kind = if *id == IDOK.0 {
                crate::theme::Kind::Primary
            } else {
                crate::theme::Kind::Button
            };
            let b = painted(self.parent, label, kind, 0, 0, *id, self.fonts.normal);
            self.add(b, x, self.y, w, h);
            x += w + gap;
        }
        self.y += h;
    }

    /// A row of buttons in the middle of the page.
    pub fn buttons_centered(&mut self, buttons: &[(i32, &str)]) {
        let start = self.items.len();
        self.buttons(buttons);
        // Shift the row from the right edge to the middle.
        let left = self.items[start..].iter().map(|(_, r)| r.left).min().unwrap_or(0);
        let shift = left / 2;
        for (_, r) in &mut self.items[start..] {
            r.left -= shift;
            r.right -= shift;
        }
    }

    /// A SysLink ("<a>Check for updates</a>"), centred.
    pub fn link(&mut self, id: i32, markup: &str, plain: &str) -> HWND {
        let (w, h) = measure(self.fonts.normal, plain);
        let w = w + self.px(4);
        let l = child(self.parent, LINK, markup, WS_TABSTOP.0, 0, id, self.fonts.normal);
        self.add(l, (self.width - w) / 2, self.y, w, h);
        self.y += h;
        l
    }

    /// A line of text centred on the page, in `font`.
    pub fn centered(&mut self, text: &str, font: HFONT, role: Role) -> HWND {
        let h = measure(font, "Ag").1;
        let l = child(self.parent, STATIC, text, SS_NOPREFIX | 0x1 /* SS_CENTER */, 0, 0, font);
        set_role(l, role);
        self.add(l, 0, self.y, self.width, h);
        self.y += h;
        l
    }

    /// An icon centred on the page.
    pub fn icon(&mut self, icon: HICON, size: i32) {
        let size = self.px(size);
        let ic = child(self.parent, STATIC, "", SS_ICON | SS_REALSIZECONTROL, 0, 0, self.fonts.normal);
        send(ic, STM_SETICON, icon.0 as usize, 0);
        self.add(ic, (self.width - size) / 2, self.y, size, size);
        self.y += size;
    }

    pub fn finish(self) -> Page {
        Page {
            items: self.items,
            width: self.width,
            height: self.y,
        }
    }
}
