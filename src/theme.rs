//! Visual language, modelled on Spotifast's `src/theme.rs`.
//!
//! Same foundation it uses: Inter at four weights instead of egui's default
//! face, a neutral blue-grey surface ramp rather than pure black, 8px radii,
//! sentence-case section titles, and a restrained type scale (11.5 → 22). The
//! accent stays RustTube red; Spotifast's green is one constant away.
//!
//! Everything is drawn by egui itself: no icon font, no webview, no images
//! beyond the thumbnails RustTube fetches.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Rect,
    Response, RichText, Sense, Stroke, TextStyle, Ui, Vec2,
};
use std::sync::Arc;

// ------------------------------------------------------------------- palette

pub const ACCENT: Color32 = Color32::from_rgb(0xf2, 0x3b, 0x33);
pub const ACCENT_SOFT: Color32 = Color32::from_rgb(0xff, 0x74, 0x66);
pub const ON_ACCENT: Color32 = Color32::from_rgb(0x16, 0x08, 0x07);

/// Window behind everything.
pub const BG: Color32 = Color32::from_rgb(0x0f, 0x11, 0x14);
/// Sidebar, player bar, dialogs.
pub const PANEL: Color32 = Color32::from_rgb(0x15, 0x18, 0x1c);
/// Cards, inputs, chips.
pub const PANEL_2: Color32 = Color32::from_rgb(0x1d, 0x21, 0x27);
/// Hover / selected.
pub const PANEL_3: Color32 = Color32::from_rgb(0x26, 0x2b, 0x33);
pub const SURFACE_ACTIVE: Color32 = Color32::from_rgb(0x2f, 0x35, 0x3f);
pub const LINE: Color32 = Color32::from_rgb(0x2a, 0x30, 0x38);
pub const TEXT: Color32 = Color32::from_rgb(0xf2, 0xf4, 0xf6);
pub const DIM: Color32 = Color32::from_rgb(0xa9, 0xb1, 0xbc);
pub const DIM_2: Color32 = Color32::from_rgb(0x6e, 0x77, 0x84);
pub const DANGER: Color32 = Color32::from_rgb(0xf5, 0x71, 0x7f);
pub const OK: Color32 = Color32::from_rgb(0x1e, 0xd7, 0x60);

// ------------------------------------------------------------------- metrics

pub const RADIUS: u8 = 8;
pub const RADIUS_SMALL: u8 = 4;
pub const TOP_BAR_HEIGHT: f32 = 56.0;
pub const SIDEBAR_WIDTH: f32 = 240.0;

// --------------------------------------------------------------------- fonts

const INTER_400: &[u8] = include_bytes!("../assets/fonts/Inter-400.ttf");
const INTER_500: &[u8] = include_bytes!("../assets/fonts/Inter-500.ttf");
const INTER_600: &[u8] = include_bytes!("../assets/fonts/Inter-600.ttf");
const INTER_700: &[u8] = include_bytes!("../assets/fonts/Inter-700.ttf");

fn family(name: &str) -> FontFamily {
    FontFamily::Name(name.into())
}

pub fn regular(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

pub fn medium(size: f32) -> FontId {
    FontId::new(size, family("Inter Medium"))
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, family("Inter SemiBold"))
}

pub fn bold(size: f32) -> FontId {
    FontId::new(size, family("Inter Bold"))
}

/// Inter for every weight, with egui's own faces kept behind it so scripts and
/// emoji the Latin subset lacks still render.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    for (key, bytes) in [
        ("inter-400", INTER_400),
        ("inter-500", INTER_500),
        ("inter-600", INTER_600),
        ("inter-700", INTER_700),
    ] {
        fonts
            .font_data
            .insert(key.to_string(), Arc::new(FontData::from_static(bytes)));
    }

    let fallback = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "inter-400".to_string());

    for (name, key) in [
        ("Inter Medium", "inter-500"),
        ("Inter SemiBold", "inter-600"),
        ("Inter Bold", "inter-700"),
    ] {
        let mut list = vec![key.to_string()];
        list.extend(fallback.iter().cloned());
        fonts.families.insert(family(name), list);
    }

    ctx.set_fonts(fonts);
}

// --------------------------------------------------------------------- style

pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);

    let mut style = (*ctx.style()).clone();
    style.visuals = visuals();
    style.text_styles = [
        (TextStyle::Small, regular(11.5)),
        (TextStyle::Body, regular(14.0)),
        (TextStyle::Button, regular(14.0)),
        (TextStyle::Heading, bold(22.0)),
        (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
    ]
    .into();
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(12.0, 6.0);
    style.spacing.interact_size = Vec2::new(40.0, 28.0);

    let mut scroll = egui::style::ScrollStyle::floating();
    scroll.bar_width = 8.0;
    scroll.floating_width = 6.0;
    scroll.handle_min_length = 28.0;
    style.spacing.scroll = scroll;

    style.animation_time = 0.12;
    ctx.set_style(style);
}

fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = PANEL_3;
    v.extreme_bg_color = PANEL_2;
    v.faint_bg_color = PANEL_2;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0_f32, ACCENT);

    let radius = CornerRadius::same(RADIUS);
    for widget in [
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        widget.corner_radius = radius;
        widget.bg_stroke = Stroke::NONE;
        widget.fg_stroke = Stroke::new(1.0_f32, TEXT);
        widget.expansion = 0.0;
    }
    v.widgets.noninteractive.corner_radius = radius;
    v.widgets.noninteractive.bg_fill = PANEL;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, LINE);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.inactive.bg_fill = PANEL_2;
    v.widgets.inactive.weak_bg_fill = PANEL_2;
    v.widgets.hovered.bg_fill = PANEL_3;
    v.widgets.hovered.weak_bg_fill = PANEL_3;
    v.widgets.active.bg_fill = SURFACE_ACTIVE;
    v.widgets.active.weak_bg_fill = SURFACE_ACTIVE;
    v.widgets.open.bg_fill = PANEL_3;
    v.widgets.open.weak_bg_fill = PANEL_3;

    v.window_corner_radius = CornerRadius::same(RADIUS);
    v.menu_corner_radius = CornerRadius::same(RADIUS);
    v
}

// -------------------------------------------------------------------- blocks

pub fn h1(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(bold(22.0)).color(TEXT));
}

pub fn sub(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(regular(13.0)).color(DIM));
}

/// Sentence-case section heading, as Spotifast titles its rows.
pub fn section(ui: &mut Ui, text: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(text).font(bold(17.0)).color(TEXT));
    ui.add_space(4.0);
}

pub fn panel_frame() -> egui::Frame {
    egui::Frame::NONE
        .fill(PANEL)
        .stroke(Stroke::new(1.0_f32, LINE))
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::same(16))
}

pub fn card_frame() -> egui::Frame {
    egui::Frame::NONE
        .fill(PANEL_2)
        .stroke(Stroke::NONE)
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::symmetric(16, 14))
}

// ------------------------------------------------------------------- buttons

pub fn accent_button(ui: &mut Ui, label: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(label).font(semibold(13.0)).color(ON_ACCENT))
            .fill(ACCENT)
            .stroke(Stroke::NONE)
            .corner_radius(CornerRadius::same(RADIUS)),
    )
}

/// Outlined button, the secondary action beside an accent one.
pub fn quiet_button(ui: &mut Ui, label: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(label).font(medium(13.0)).color(TEXT))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0_f32, LINE))
            .corner_radius(CornerRadius::same(RADIUS)),
    )
}

pub fn danger_button(ui: &mut Ui, label: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(label).font(medium(12.5)).color(DANGER))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0_f32, DANGER.gamma_multiply(0.6)))
            .corner_radius(CornerRadius::same(RADIUS)),
    )
}

/// Sidebar navigation row: rounded fill on hover, filled when active.
pub fn nav_row(ui: &mut Ui, label: &str, active: bool) -> Response {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 36.0), Sense::click());
    let hovered = response.hovered();

    if active || hovered {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(RADIUS),
            if active { PANEL_3 } else { PANEL_2 },
        );
    }
    ui.painter().text(
        egui::pos2(rect.left() + 12.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        if active {
            semibold(13.5)
        } else {
            medium(13.5)
        },
        if active || hovered { TEXT } else { DIM },
    );
    response
}

// --------------------------------------------------------------------- icons

/// Small monochrome transport/status icons painted with shapes.
pub enum Icon {
    Play,
    Pause,
    Stop,
    Back,
}

pub fn icon_button(ui: &mut Ui, icon: Icon, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let color = if response.hovered() { TEXT } else { DIM };
    if response.hovered() {
        ui.painter()
            .circle_filled(rect.center(), size * 0.46, PANEL_3);
    }
    paint_icon(ui.painter(), rect, &icon, color);
    response
}

pub fn paint_icon(painter: &egui::Painter, rect: Rect, icon: &Icon, color: Color32) {
    let c = rect.center();
    let s = rect.width().min(rect.height()) * 0.5;
    match icon {
        Icon::Play => {
            let points = vec![
                egui::pos2(c.x - s * 0.32, c.y - s * 0.44),
                egui::pos2(c.x - s * 0.32, c.y + s * 0.44),
                egui::pos2(c.x + s * 0.46, c.y),
            ];
            painter.add(egui::Shape::convex_polygon(points, color, Stroke::NONE));
        }
        Icon::Pause => {
            let w = s * 0.18;
            painter.rect_filled(
                Rect::from_min_size(egui::pos2(c.x - s * 0.34, c.y - s * 0.42), Vec2::new(w, s * 0.84)),
                CornerRadius::same(RADIUS_SMALL),
                color,
            );
            painter.rect_filled(
                Rect::from_min_size(egui::pos2(c.x + s * 0.16, c.y - s * 0.42), Vec2::new(w, s * 0.84)),
                CornerRadius::same(RADIUS_SMALL),
                color,
            );
        }
        Icon::Stop => {
            painter.rect_filled(
                Rect::from_center_size(c, Vec2::splat(s * 0.72)),
                CornerRadius::same(RADIUS_SMALL),
                color,
            );
        }
        Icon::Back => {
            painter.add(egui::Shape::line(
                vec![
                    egui::pos2(c.x + s * 0.25, c.y - s * 0.4),
                    egui::pos2(c.x - s * 0.25, c.y),
                    egui::pos2(c.x + s * 0.25, c.y + s * 0.4),
                ],
                Stroke::new(1.8_f32, color),
            ));
        }
    }
}

/// Rounded placeholder used while a thumbnail downloads (or when it failed).
pub fn placeholder_thumbnail(painter: &egui::Painter, rect: Rect) {
    painter.rect_filled(rect, CornerRadius::same(RADIUS), PANEL_2);
    paint_icon(
        painter,
        Rect::from_center_size(rect.center(), Vec2::splat(28.0)),
        &Icon::Play,
        DIM_2,
    );
}

/// Progress bar with rounded ends; returns a clickable response so callers can seek.
pub fn progress(ui: &mut Ui, fraction: f32, height: f32, hovered_fill: Color32) -> (Response, Rect) {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height * 3.0), Sense::click());
    let line = Rect::from_center_size(rect.center(), Vec2::new(width, height));
    let radius = CornerRadius::same(RADIUS);
    ui.painter().rect_filled(line, radius, PANEL_3);
    let filled = Rect::from_min_size(
        line.min,
        Vec2::new(width * fraction.clamp(0.0, 1.0), height),
    );
    if fraction > 0.0 {
        ui.painter().rect_filled(filled, radius, hovered_fill);
    }
    if response.hovered() {
        let knob = Rect::from_center_size(
            egui::pos2(filled.right().max(line.left() + 3.0), line.center().y),
            Vec2::splat(height * 1.9),
        );
        ui.painter()
            .circle_filled(knob.center(), height * 0.95, TEXT);
    }
    (response, line)
}

// ----------------------------------------------------------------- formatting

pub fn format_clock(total: f64) -> String {
    let value = total.max(0.0).round() as i64;
    let h = value / 3600;
    let m = (value % 3600) / 60;
    let s = value % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

pub fn format_long(seconds: i64) -> String {
    let value = seconds.max(0);
    let h = value / 3600;
    let m = (value % 3600) / 60;
    if h >= 100 {
        format!("{h}h")
    } else if h > 0 {
        if m > 0 {
            format!("{h}h {m}m")
        } else {
            format!("{h}h")
        }
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{value}s")
    }
}

pub fn format_compact(value: u64) -> String {
    if value >= 1_000_000_000 {
        format!("{:.1}B", value as f64 / 1_000_000_000.0)
    } else if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

pub fn format_relative(ms: i64) -> String {
    if ms <= 0 {
        return "unknown".into();
    }
    let diff = (chrono::Utc::now().timestamp_millis() - ms).max(0);
    let minutes = diff / 60_000;
    if minutes < 1 {
        return "just now".into();
    }
    if minutes < 60 {
        return format!("{minutes}m ago");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{days}d ago");
    }
    let months = days / 30;
    if months < 12 {
        return format!("{months}mo ago");
    }
    format!("{}y ago", months / 12)
}

pub fn initials(name: &str) -> String {
    name.split_whitespace()
        .take(2)
        .filter_map(|part| part.chars().next())
        .collect::<String>()
        .to_uppercase()
}

/// Circular initials avatar drawn without any network round trip.
pub fn avatar(ui: &mut Ui, name: &str, diameter: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(diameter), Sense::hover());
    ui.painter().circle_filled(rect.center(), diameter * 0.5, PANEL_2);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        initials(name),
        semibold(diameter * 0.36),
        DIM,
    );
}
