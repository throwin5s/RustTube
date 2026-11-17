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
use std::time::Duration;

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
    set_scrollbar_opacity(&mut scroll, 0.0);
    style.spacing.scroll = scroll;

    style.animation_time = 0.12;
    ctx.set_style(style);
}

/// Reveal scrollbars only while the user is actively scrolling, then let them
/// disappear again. egui's floating style normally fades one in whenever the
/// pointer enters the scroll area, which leaves the bar visible while merely
/// reading or moving across a page.
pub fn update_scrollbars(ctx: &egui::Context) {
    // Hold the bar just long enough to read as responsive after the last scroll,
    // then ease it out rather than snapping it away.
    const VISIBLE_FOR: f32 = 0.35;
    const FADE_OUT: f32 = 0.25;

    let since_scroll = ctx.input(|input| input.time_since_last_scroll());
    let fade = ((since_scroll - VISIBLE_FOR) / FADE_OUT).clamp(0.0, 1.0);
    // Smoothstep, so there is no visible step where the hold ends or the fade
    // finishes.
    let eased = fade * fade * (3.0 - 2.0 * fade);
    let alpha = 1.0 - eased;

    ctx.style_mut(|style| set_scrollbar_opacity(&mut style.spacing.scroll, alpha));

    if fade >= 1.0 {
        return;
    }
    if fade > 0.0 {
        // Animation frames while the bar is fading out.
        ctx.request_repaint_after(Duration::from_millis(16));
    } else {
        // Asleep while the bar is held, wake once to start the fade.
        ctx.request_repaint_after(Duration::from_secs_f32(VISIBLE_FOR - since_scroll));
    }
}

fn set_scrollbar_opacity(scroll: &mut egui::style::ScrollStyle, alpha: f32) {
    let (background, handle) = (0.4 * alpha, 0.6 * alpha);
    scroll.dormant_background_opacity = 0.0;
    scroll.dormant_handle_opacity = 0.0;
    scroll.active_background_opacity = background;
    scroll.active_handle_opacity = handle;
    scroll.interact_background_opacity = background;
    scroll.interact_handle_opacity = handle;
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
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Outlined button, the secondary action beside an accent one.
pub fn quiet_button(ui: &mut Ui, label: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(label).font(medium(13.0)).color(TEXT))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0_f32, LINE))
            .corner_radius(CornerRadius::same(RADIUS)),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn danger_button(ui: &mut Ui, label: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(label).font(medium(12.5)).color(DANGER))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0_f32, DANGER.gamma_multiply(0.6)))
            .corner_radius(CornerRadius::same(RADIUS)),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
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
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

// --------------------------------------------------------------------- icons

/// Small monochrome transport/status icons painted with shapes.
pub enum Icon {
    Play,
    Pause,
    Back,
    Forward,
    Sidebar,
    Volume,
    VolumeMuted,
    Fullscreen,
    ExitFullscreen,
}

pub fn icon_button(ui: &mut Ui, icon: Icon, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let color = if response.hovered() { TEXT } else { DIM };
    if response.hovered() {
        ui.painter()
            .circle_filled(rect.center(), size * 0.46, PANEL_3);
    }
    paint_icon(ui.painter(), rect, &icon, color);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
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
        Icon::Forward => {
            painter.add(egui::Shape::line(
                vec![
                    egui::pos2(c.x - s * 0.25, c.y - s * 0.4),
                    egui::pos2(c.x + s * 0.25, c.y),
                    egui::pos2(c.x - s * 0.25, c.y + s * 0.4),
                ],
                Stroke::new(1.8_f32, color),
            ));
        }
        Icon::Fullscreen => {
            // Four brackets opening outwards, the usual fullscreen glyph.
            let stroke = Stroke::new(1.7_f32, color);
            let reach = s * 0.42;
            let arm = s * 0.24;
            for (sx, sy) in [(-1.0_f32, -1.0_f32), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                let corner = egui::pos2(c.x + sx * reach, c.y + sy * reach);
                painter.add(egui::Shape::line(
                    vec![egui::pos2(corner.x, corner.y - sy * arm), corner],
                    stroke,
                ));
                painter.add(egui::Shape::line(
                    vec![egui::pos2(corner.x - sx * arm, corner.y), corner],
                    stroke,
                ));
            }
        }
        Icon::ExitFullscreen => {
            // Same brackets, turned inwards: the arrows point at the middle.
            let stroke = Stroke::new(1.7_f32, color);
            let reach = s * 0.32;
            let arm = s * 0.24;
            for (sx, sy) in [(-1.0_f32, -1.0_f32), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                let inner = egui::pos2(c.x + sx * reach, c.y + sy * reach);
                painter.add(egui::Shape::line(
                    vec![egui::pos2(inner.x + sx * arm, inner.y), inner],
                    stroke,
                ));
                painter.add(egui::Shape::line(
                    vec![inner, egui::pos2(inner.x, inner.y + sy * arm)],
                    stroke,
                ));
            }
        }
        Icon::Sidebar => {
            for offset in [-0.32, 0.0, 0.32] {
                painter.rect_filled(
                    Rect::from_min_size(
                        egui::pos2(c.x - s * 0.36, c.y + s * offset - s * 0.05),
                        Vec2::new(s * 0.72, s * 0.1),
                    ),
                    CornerRadius::same(1),
                    color,
                );
            }
        }
        Icon::Volume | Icon::VolumeMuted => {
            painter.rect_filled(
                Rect::from_center_size(
                    egui::pos2(c.x - s * 0.26, c.y),
                    Vec2::new(s * 0.30, s * 0.34),
                ),
                CornerRadius::same(1),
                color,
            );
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(c.x - s * 0.12, c.y - s * 0.18),
                    egui::pos2(c.x - s * 0.12, c.y + s * 0.18),
                    egui::pos2(c.x + s * 0.12, c.y + s * 0.38),
                    egui::pos2(c.x + s * 0.12, c.y - s * 0.38),
                ],
                color,
                Stroke::NONE,
            ));
            if matches!(icon, Icon::VolumeMuted) {
                painter.add(egui::Shape::line(
                    vec![
                        egui::pos2(c.x + s * 0.22, c.y - s * 0.22),
                        egui::pos2(c.x + s * 0.48, c.y + s * 0.22),
                    ],
                    Stroke::new(1.8_f32, color),
                ));
                painter.add(egui::Shape::line(
                    vec![
                        egui::pos2(c.x + s * 0.48, c.y - s * 0.22),
                        egui::pos2(c.x + s * 0.22, c.y + s * 0.22),
                    ],
                    Stroke::new(1.8_f32, color),
                ));
            } else {
                painter.add(egui::Shape::line(
                    vec![
                        egui::pos2(c.x + s * 0.28, c.y - s * 0.14),
                        egui::pos2(c.x + s * 0.28, c.y + s * 0.14),
                    ],
                    Stroke::new(1.4_f32, color),
                ));
                painter.add(egui::Shape::line(
                    vec![
                        egui::pos2(c.x + s * 0.42, c.y - s * 0.28),
                        egui::pos2(c.x + s * 0.42, c.y + s * 0.28),
                    ],
                    Stroke::new(1.4_f32, color),
                ));
            }
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

/// Progress bar with rounded ends. Returns the response, track, and the target
/// fraction while the user is pressing or dragging, so callers can seek to an
/// absolute position instead of accumulating relative seeks.
pub fn progress(
    ui: &mut Ui,
    fraction: f32,
    height: f32,
    hovered_fill: Color32,
) -> (Response, Rect, Option<f32>) {
    let width = ui.available_width();
    let thickness = height.max(6.0);
    // Give the bar a generous invisible hit area so it is easy to grab.
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 30.0), Sense::click_and_drag());
    let line = Rect::from_center_size(rect.center(), Vec2::new(width, thickness));
    let target = response.interact_pointer_pos().and_then(|pos| {
        (width > 0.0).then(|| ((pos.x - line.left()) / width).clamp(0.0, 1.0))
    });
    let shown = target.unwrap_or(fraction).clamp(0.0, 1.0);
    let radius = CornerRadius::same((thickness * 0.5).round() as u8);
    ui.painter().rect_filled(line, radius, PANEL_3);
    let filled = Rect::from_min_size(line.min, Vec2::new(width * shown, thickness));
    if shown > 0.0 {
        ui.painter().rect_filled(filled, radius, hovered_fill);
    }
    let knob_radius = if response.hovered() || response.dragged() {
        7.0
    } else {
        5.0
    };
    ui.painter().circle_filled(
        egui::pos2(
            filled.right().max(line.left() + thickness * 0.5),
            line.center().y,
        ),
        knob_radius,
        TEXT,
    );
    (
        response.on_hover_cursor(egui::CursorIcon::ResizeHorizontal),
        line,
        target,
    )
}

/// Compact volume control drawn in the same visual language as the seek bar.
/// Returns the new 0–130 value while the user clicks or drags it.
pub fn volume_slider(ui: &mut Ui, value: f32) -> Option<f32> {
    let width = 140.0;
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(width, 30.0), Sense::click_and_drag());
    let line = Rect::from_center_size(rect.center(), Vec2::new(width, 8.0));
    let radius = CornerRadius::same(4);
    ui.painter().rect_filled(line, radius, PANEL_3);
    let fraction = (value / 130.0).clamp(0.0, 1.0);
    let filled = Rect::from_min_size(line.min, Vec2::new(width * fraction, 8.0));
    if fraction > 0.0 {
        ui.painter().rect_filled(filled, radius, ACCENT);
    }
    ui.painter().circle_filled(
        egui::pos2(line.left() + width * fraction, line.center().y),
        if response.hovered() || response.dragged() {
            7.0
        } else {
            5.0
        },
        TEXT,
    );
    if response.clicked() || response.dragged() {
        if let Some(pos) = response.interact_pointer_pos() {
            let ratio = ((pos.x - line.left()) / width).clamp(0.0, 1.0);
            return Some(ratio * 130.0);
        }
    }
    None
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

// ------------------------------------------------------------------ motion

/// Ease-out curve for the thumbnail-to-player animation: quick departure,
/// long settle, which reads as deliberate rather than mechanical.
pub fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Slow, steady, slow. Unlike `ease_out_cubic` this starts at zero velocity,
/// so a panel that is already large on screen does not jump a quarter of the
/// way open on its very first frame.
pub fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Interpolate both corners, so a rect can grow from a thumbnail to the player.
pub fn lerp_rect(from: Rect, to: Rect, t: f32) -> Rect {
    let t = t.clamp(0.0, 1.0);
    let mix = |a: f32, b: f32| a + (b - a) * t;
    Rect::from_min_max(
        egui::pos2(mix(from.min.x, to.min.x), mix(from.min.y, to.min.y)),
        egui::pos2(mix(from.max.x, to.max.x), mix(from.max.y, to.max.y)),
    )
}

pub fn initials(name: &str) -> String {
    name.split_whitespace()
        .take(2)
        .filter_map(|part| part.chars().next())
        .collect::<String>()
        .to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ease_out_cubic_is_clamped_and_monotonic() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        assert_eq!(ease_out_cubic(-2.0), 0.0);
        assert_eq!(ease_out_cubic(3.0), 1.0);
        let mut previous = 0.0;
        for step in 0..=20 {
            let value = ease_out_cubic(step as f32 / 20.0);
            assert!(value >= previous, "curve must not go backwards");
            previous = value;
        }
        // Fast departure, long settle: halfway through time is most of the way.
        assert!(ease_out_cubic(0.5) > 0.8);
    }

    #[test]
    fn smoothstep_is_symmetric_and_starts_at_rest() {
        assert_eq!(smoothstep(0.0), 0.0);
        assert_eq!(smoothstep(1.0), 1.0);
        assert!((smoothstep(0.5) - 0.5).abs() < 1e-6);
        // Almost nothing happens in the first frame, which is what stops the
        // drawer from looking like it was simply switched on.
        assert!(smoothstep(0.1) < 0.04);
        assert!(smoothstep(0.9) > 0.96);
        let mut previous = 0.0;
        for step in 0..=20 {
            let value = smoothstep(step as f32 / 20.0);
            assert!(value >= previous, "curve must not go backwards");
            previous = value;
        }
    }

    #[test]
    fn lerp_rect_starts_at_the_thumbnail_and_ends_at_the_player() {
        let from = Rect::from_min_size(egui::pos2(10.0, 20.0), Vec2::new(200.0, 112.0));
        let to = Rect::from_min_size(egui::pos2(40.0, 60.0), Vec2::new(800.0, 450.0));
        assert_eq!(lerp_rect(from, to, 0.0), from);
        assert_eq!(lerp_rect(from, to, 1.0), to);
        assert_eq!(lerp_rect(from, to, -1.0), from);
        assert_eq!(lerp_rect(from, to, 9.0), to);
        let mid = lerp_rect(from, to, 0.5);
        assert!(mid.width() > from.width() && mid.width() < to.width());
        assert!(mid.min.x > from.min.x && mid.min.x < to.min.x);
    }


}

