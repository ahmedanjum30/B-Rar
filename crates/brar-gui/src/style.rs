//! Look and feel: Windows 11 colors and Segoe UI, following the system light/dark theme,
//! plus the few custom-painted widgets the window uses.

use std::sync::Arc;

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Pos2, Rect, Response, RichText, Sense, Shape, Stroke, TextStyle,
    Theme, Ui, Vec2, vec2,
};

pub const ACCENT: Color32 = Color32::from_rgb(37, 99, 235);
const ACCENT_DARK: Color32 = Color32::from_rgb(96, 165, 250);

pub fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

pub struct Palette {
    pub bg: Color32,
    pub text: Color32,
    pub weak: Color32,
    pub track: Color32,
    pub accent: Color32,
    pub ok: Color32,
    pub error: Color32,
    pub warn: Color32,
    pub paused: Color32,
}

pub fn palette(ui: &Ui) -> Palette {
    if ui.visuals().dark_mode {
        Palette {
            bg: Color32::from_rgb(32, 32, 32),
            text: Color32::from_rgb(240, 240, 240),
            weak: Color32::from_rgb(160, 160, 160),
            track: Color32::from_rgb(64, 64, 64),
            accent: ACCENT_DARK,
            ok: Color32::from_rgb(108, 203, 95),
            error: Color32::from_rgb(255, 120, 130),
            warn: Color32::from_rgb(252, 200, 0),
            paused: Color32::from_rgb(252, 200, 0),
        }
    } else {
        Palette {
            bg: Color32::from_rgb(243, 243, 243),
            text: Color32::from_rgb(26, 26, 26),
            weak: Color32::from_rgb(96, 96, 96),
            track: Color32::from_rgb(218, 218, 218),
            accent: ACCENT,
            ok: Color32::from_rgb(15, 123, 15),
            error: Color32::from_rgb(196, 43, 28),
            warn: Color32::from_rgb(157, 93, 0),
            paused: Color32::from_rgb(157, 93, 0),
        }
    }
}

pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);
    for theme in [Theme::Light, Theme::Dark] {
        ctx.style_mut_of(theme, |style| {
            let dark = theme == Theme::Dark;
            style.text_styles.insert(TextStyle::Body, FontId::proportional(14.0));
            style.text_styles.insert(TextStyle::Button, FontId::proportional(14.0));
            style.text_styles.insert(TextStyle::Small, FontId::proportional(12.0));
            style.text_styles.insert(TextStyle::Heading, FontId::new(19.0, semibold()));
            style.spacing.item_spacing = vec2(8.0, 6.0);
            style.spacing.button_padding = vec2(16.0, 6.0);
            style.spacing.interact_size.y = 32.0;

            let v = &mut style.visuals;
            v.panel_fill = if dark {
                Color32::from_rgb(32, 32, 32)
            } else {
                Color32::from_rgb(243, 243, 243)
            };
            v.window_fill = v.panel_fill;
            v.selection.bg_fill = if dark { ACCENT_DARK } else { ACCENT };
            v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
            v.hyperlink_color = v.selection.bg_fill;
            let radius = CornerRadius::same(4);
            let (fill, hover, border) = if dark {
                (
                    Color32::from_rgb(45, 45, 45),
                    Color32::from_rgb(55, 55, 55),
                    Color32::from_rgb(70, 70, 70),
                )
            } else {
                (Color32::WHITE, Color32::from_rgb(249, 249, 249), Color32::from_rgb(208, 208, 208))
            };
            for w in [
                &mut v.widgets.inactive,
                &mut v.widgets.hovered,
                &mut v.widgets.active,
                &mut v.widgets.open,
            ] {
                w.corner_radius = radius;
                w.bg_stroke = Stroke::new(1.0, border);
            }
            v.widgets.inactive.weak_bg_fill = fill;
            v.widgets.inactive.bg_fill = fill;
            v.widgets.hovered.weak_bg_fill = hover;
            v.widgets.hovered.bg_fill = hover;
            v.widgets.active.weak_bg_fill = border;
            v.widgets.active.bg_fill = border;
            v.widgets.hovered.expansion = 0.0;
            v.widgets.active.expansion = 0.0;
            v.extreme_bg_color = if dark { Color32::from_rgb(28, 28, 28) } else { Color32::WHITE };
        });
    }
}

/// Segoe UI from the Windows fonts folder, with egui's bundled fonts as fallback (symbols).
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let dir = std::path::PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into())).join("Fonts");
    let fallback = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut semibold_family = fallback.clone();
    if let Ok(bytes) = std::fs::read(dir.join("segoeui.ttf")) {
        fonts.font_data.insert("segoe".into(), Arc::new(FontData::from_owned(bytes)));
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "segoe".into());
        semibold_family.insert(0, "segoe".into());
    }
    if let Ok(bytes) = std::fs::read(dir.join("seguisb.ttf")) {
        fonts.font_data.insert("segoe-semibold".into(), Arc::new(FontData::from_owned(bytes)));
        semibold_family.insert(0, "segoe-semibold".into());
    }
    fonts.families.insert(semibold(), semibold_family);
    ctx.set_fonts(fonts);
}

pub fn primary_button(ui: &mut Ui, text: &str, enabled: bool) -> Response {
    let pal = palette(ui);
    let label = RichText::new(text).color(if ui.visuals().dark_mode { Color32::BLACK } else { Color32::WHITE });
    ui.add_enabled(
        enabled,
        egui::Button::new(label).fill(pal.accent).stroke(Stroke::NONE).min_size(vec2(100.0, 32.0)),
    )
}

pub fn button(ui: &mut Ui, text: &str) -> Response {
    ui.add(egui::Button::new(text).min_size(vec2(88.0, 32.0)))
}

/// A thin rounded bar; `None` animates an indeterminate sweep.
pub fn progress_bar(ui: &mut Ui, fraction: Option<f32>, color: Color32) {
    let pal = palette(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let radius = CornerRadius::same(3);
    painter.rect_filled(rect, radius, pal.track);
    match fraction {
        Some(f) => {
            let mut fill = rect;
            fill.set_width(rect.width() * f.clamp(0.0, 1.0));
            if fill.width() > 0.5 {
                painter.rect_filled(fill, radius, color);
            }
        }
        None => {
            let t = ui.input(|i| i.time) as f32;
            let w = rect.width();
            let x = ((t * 0.7).fract() * 1.4 - 0.4) * w;
            let seg = Rect::from_min_size(Pos2::new(rect.left() + x, rect.top()), vec2(w * 0.4, rect.height()));
            painter.rect_filled(seg.intersect(rect), radius, color);
            ui.ctx().request_repaint();
        }
    }
}

#[derive(Clone, Copy)]
pub enum Icon {
    Ok,
    Error,
    Warning,
    Lock,
}

/// A 40 px status badge: colored circle with a white glyph.
pub fn status_icon(ui: &mut Ui, icon: Icon) {
    let pal = palette(ui);
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let c = rect.center();
    let s = rect.width();
    let at = |x: f32, y: f32| Pos2::new(rect.left() + x * s, rect.top() + y * s);
    let color = match icon {
        Icon::Ok => pal.ok,
        Icon::Error => pal.error,
        Icon::Warning => pal.warn,
        Icon::Lock => pal.accent,
    };
    let glyph = if ui.visuals().dark_mode { Color32::BLACK } else { Color32::WHITE };
    painter.circle_filled(c, s * 0.5, color);
    let stroke = Stroke::new(s * 0.075, glyph);
    match icon {
        Icon::Ok => {
            painter.add(Shape::line(vec![at(0.29, 0.52), at(0.44, 0.66), at(0.72, 0.37)], stroke));
        }
        Icon::Error => {
            painter.line_segment([at(0.34, 0.34), at(0.66, 0.66)], stroke);
            painter.line_segment([at(0.66, 0.34), at(0.34, 0.66)], stroke);
        }
        Icon::Warning => {
            painter.line_segment([at(0.5, 0.27), at(0.5, 0.58)], stroke);
            painter.circle_filled(at(0.5, 0.72), s * 0.05, glyph);
        }
        Icon::Lock => {
            let body = Rect::from_min_max(at(0.32, 0.46), at(0.68, 0.72));
            painter.rect_filled(body, CornerRadius::same(2), glyph);
            let (cx, cy, r) = (0.5, 0.46, 0.12);
            let arc: Vec<Pos2> = (0..=16)
                .map(|i| {
                    let a = std::f32::consts::PI * (1.0 + i as f32 / 16.0);
                    at(cx + r * a.cos(), cy + r * a.sin())
                })
                .collect();
            painter.add(Shape::line(arc, Stroke::new(s * 0.06, glyph)));
        }
    }
}
