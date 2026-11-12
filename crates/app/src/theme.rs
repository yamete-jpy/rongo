//! Dark theme close to the reference screenshot, type colours, and fonts.

use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Theme};
use rongo_core::format::TypeKind;

pub const BG: Color32 = Color32::from_rgb(0x1e, 0x1f, 0x22);
pub const PANEL: Color32 = Color32::from_rgb(0x2b, 0x2d, 0x30);
pub const RAISED: Color32 = Color32::from_rgb(0x35, 0x37, 0x3b);
pub const BORDER: Color32 = Color32::from_rgb(0x43, 0x45, 0x4a);
pub const TEXT: Color32 = Color32::from_rgb(0xdf, 0xe1, 0xe5);
pub const MUTED: Color32 = Color32::from_rgb(0x8c, 0x8f, 0x94);
pub const ACCENT: Color32 = Color32::from_rgb(0x3d, 0x8b, 0xfd);
pub const SELECTION: Color32 = Color32::from_rgb(0x2e, 0x43, 0x6e);
pub const CURRENT_LINE: Color32 = Color32::from_rgb(0x32, 0x31, 0x2a);
pub const OK: Color32 = Color32::from_rgb(0x5f, 0xb8, 0x65);
pub const ERROR: Color32 = Color32::from_rgb(0xf2, 0x6d, 0x6d);
pub const WARN: Color32 = Color32::from_rgb(0xe5, 0xb5, 0x67);

pub const STRING: Color32 = Color32::from_rgb(0x6a, 0xab, 0x73);
pub const NUMBER: Color32 = Color32::from_rgb(0x56, 0xa8, 0xf5);
pub const CONTAINER: Color32 = Color32::from_rgb(0xcf, 0x8e, 0x6d);
pub const DATE: Color32 = Color32::from_rgb(0xc7, 0x7d, 0xbb);
pub const KEYWORD: Color32 = Color32::from_rgb(0xcf, 0x8e, 0x6d);
pub const FUNCTION: Color32 = Color32::from_rgb(0xe8, 0xc4, 0x6b);
pub const COMMENT: Color32 = Color32::from_rgb(0x7a, 0x7e, 0x85);

/// Connection colours, by the name saved in the profile.
pub const CONN_COLORS: [(&str, Color32); 8] = [
    ("red", Color32::from_rgb(0xe5, 0x48, 0x4d)),
    ("orange", Color32::from_rgb(0xf0, 0x7c, 0x2e)),
    ("yellow", Color32::from_rgb(0xe8, 0xc2, 0x3f)),
    ("green", Color32::from_rgb(0x46, 0xa7, 0x58)),
    ("teal", Color32::from_rgb(0x1f, 0xa8, 0x9a)),
    ("blue", Color32::from_rgb(0x3d, 0x8b, 0xfd)),
    ("purple", Color32::from_rgb(0x9b, 0x6b, 0xd6)),
    ("pink", Color32::from_rgb(0xd9, 0x5c, 0xa8)),
];

pub fn conn_color(name: Option<&str>) -> Option<Color32> {
    let name = name?;
    CONN_COLORS.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

/// A row of colour swatches, the first meaning "no colour". Returns the
/// choice when one is clicked.
pub fn color_swatches(ui: &mut egui::Ui, current: Option<&str>) -> Option<Option<String>> {
    let mut chosen = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let none = std::iter::once((None, MUTED));
        for (name, color) in none.chain(CONN_COLORS.iter().map(|(n, c)| (Some(*n), *c))) {
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::click());
            let painter = ui.painter();
            let center = rect.center();
            if name.is_some() {
                painter.circle_filled(center, 8.0, color);
            } else {
                painter.circle_stroke(center, 7.5, Stroke::new(1.0, color));
                painter.line_segment([center + egui::vec2(-5.0, 5.0), center + egui::vec2(5.0, -5.0)], Stroke::new(1.0, color));
            }
            if name == current || resp.hovered() {
                painter.circle_stroke(center, 10.0, Stroke::new(1.5, if name == current { TEXT } else { MUTED }));
            }
            if resp.on_hover_text(name.unwrap_or("No color")).clicked() {
                chosen = Some(name.map(str::to_string));
            }
        }
    });
    chosen
}

/// A small dropdown showing a connection's colour (a square swatch, or a
/// struck-out one for none) and a caret, framed like a button so it reads as
/// something to click rather than a status light.
pub fn color_button(ui: &mut egui::Ui, current: Option<&str>) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(36.0, 20.0), egui::Sense::click());
    let v = ui.style().interact(&resp);
    let painter = ui.painter();
    painter.rect(rect, 4.0, v.weak_bg_fill, v.bg_stroke, egui::StrokeKind::Inside);
    let swatch = egui::Rect::from_center_size(egui::pos2(rect.left() + 12.0, rect.center().y), egui::vec2(12.0, 12.0));
    match conn_color(current) {
        Some(c) => _ = painter.rect_filled(swatch, 3.0, c),
        None => {
            painter.rect_stroke(swatch, 3.0, Stroke::new(1.0, MUTED), egui::StrokeKind::Inside);
            painter.line_segment([swatch.left_bottom() + egui::vec2(2.0, -2.0), swatch.right_top() + egui::vec2(-2.0, 2.0)], Stroke::new(1.0, MUTED));
        }
    }
    let caret = egui::pos2(rect.right() - 10.0, rect.center().y);
    painter.text(caret, egui::Align2::CENTER_CENTER, egui_phosphor::regular::CARET_DOWN, FontId::proportional(11.0), v.text_color());
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Color")
}

pub fn type_color(kind: TypeKind) -> Color32 {
    match kind {
        TypeKind::String | TypeKind::Code => STRING,
        TypeKind::Int32 | TypeKind::Int64 | TypeKind::Double | TypeKind::Decimal => NUMBER,
        TypeKind::Array | TypeKind::Object => CONTAINER,
        TypeKind::Date | TypeKind::Timestamp => DATE,
        TypeKind::Bool => KEYWORD,
        TypeKind::Null | TypeKind::Other => MUTED,
        TypeKind::ObjectId | TypeKind::Binary | TypeKind::Regex => TEXT,
    }
}

pub fn type_icon(kind: TypeKind) -> &'static str {
    use egui_phosphor::regular as i;
    match kind {
        TypeKind::ObjectId => i::IDENTIFICATION_CARD,
        TypeKind::String => i::TEXT_AA,
        TypeKind::Int32 | TypeKind::Int64 | TypeKind::Double | TypeKind::Decimal => i::HASH,
        TypeKind::Bool => i::TOGGLE_LEFT,
        TypeKind::Date | TypeKind::Timestamp => i::CALENDAR_BLANK,
        TypeKind::Null => i::PROHIBIT,
        TypeKind::Array => i::BRACKETS_SQUARE,
        TypeKind::Object => i::BRACKETS_CURLY,
        TypeKind::Binary => i::BINARY,
        TypeKind::Regex => i::ASTERISK,
        TypeKind::Code => i::CODE,
        TypeKind::Other => i::QUESTION,
    }
}

pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

/// macOS fonts for scripts egui's own fonts lack, tried in this order after
/// them. Missing files are skipped.
const FALLBACK_FONTS: &[&str] = &[
    "/System/Library/Fonts/Supplemental/Thonburi.ttc",
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    "/System/Library/Fonts/AppleSDGothicNeo.ttc",
    "/System/Library/Fonts/Supplemental/Lao Sangam MN.ttf",
    "/System/Library/Fonts/Supplemental/Khmer Sangam MN.ttf",
    "/System/Library/Fonts/Supplemental/Myanmar Sangam MN.ttc",
    "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
];

/// The fallback font files, memory-mapped once. The CJK files are 20-55 MB
/// each; mapping them means only the glyphs actually drawn are read into
/// memory, and egui borrows the bytes without copying them.
fn fallback_fonts() -> &'static [(&'static str, &'static [u8])] {
    static FONTS: std::sync::OnceLock<Vec<(&'static str, &'static [u8])>> = std::sync::OnceLock::new();
    FONTS.get_or_init(|| {
        FALLBACK_FONTS
            .iter()
            .filter_map(|path| {
                let file = std::fs::File::open(path).ok()?;
                // SAFETY: system font files are read-only (SIP-protected) and
                // are not modified while the app runs.
                let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
                let bytes: &'static [u8] = Box::leak(Box::new(map));
                Some((*path, bytes))
            })
            .collect()
    })
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    for (path, bytes) in fallback_fonts() {
        fonts.font_data.insert(path.to_string(), std::sync::Arc::new(egui::FontData::from_static(bytes)));
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push(path.to_string());
        }
    }
    ctx.set_fonts(fonts);
    ctx.set_theme(Theme::Dark);
    ctx.style_mut_of(Theme::Dark, |style| {
        style.text_styles.insert(TextStyle::Body, FontId::proportional(13.5));
        style.text_styles.insert(TextStyle::Button, FontId::proportional(13.5));
        style.text_styles.insert(TextStyle::Monospace, mono(13.0));
        style.text_styles.insert(TextStyle::Small, FontId::proportional(11.5));
        style.text_styles.insert(TextStyle::Heading, FontId::proportional(17.0));
        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.spacing.button_padding = egui::vec2(8.0, 3.0);
        style.spacing.interact_size.y = 22.0;

        let v = &mut style.visuals;
        v.dark_mode = true;
        v.override_text_color = Some(TEXT);
        v.panel_fill = PANEL;
        v.window_fill = PANEL;
        v.extreme_bg_color = BG;
        v.faint_bg_color = Color32::from_rgb(0x26, 0x27, 0x2a);
        v.code_bg_color = BG;
        v.window_stroke = Stroke::new(1.0, BORDER);
        v.selection.bg_fill = SELECTION;
        v.selection.stroke = Stroke::new(1.0, TEXT);
        v.hyperlink_color = ACCENT;
        v.window_corner_radius = CornerRadius::same(6);
        v.menu_corner_radius = CornerRadius::same(5);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = PANEL;
        w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        w.inactive.bg_fill = RAISED;
        w.inactive.weak_bg_fill = RAISED;
        w.inactive.bg_stroke = Stroke::new(1.0, BORDER);
        w.hovered.bg_fill = Color32::from_rgb(0x40, 0x43, 0x48);
        w.hovered.weak_bg_fill = Color32::from_rgb(0x40, 0x43, 0x48);
        w.active.bg_fill = Color32::from_rgb(0x4a, 0x4d, 0x53);
        w.active.weak_bg_fill = Color32::from_rgb(0x4a, 0x4d, 0x53);
        for state in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
            state.corner_radius = CornerRadius::same(4);
        }
    });
}

/// Work slower than this shows a busy indicator; quicker work would only flicker.
pub const SHOW_BUSY_AFTER: std::time::Duration = std::time::Duration::from_millis(300);

/// A bar with a block sliding along it: work is going on, length unknown.
pub fn busy_bar(ui: &mut egui::Ui, width: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 6.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, RAISED);
    let seg = (width / 3.0).min(36.0);
    let x = (ui.input(|i| i.time) as f32 * 0.8).fract() * (rect.width() + seg) - seg;
    let block = egui::Rect::from_min_size(egui::pos2(rect.left() + x, rect.top()), egui::vec2(seg, rect.height())).intersect(rect);
    if block.width() > 0.0 {
        painter.rect_filled(block, 3.0, ACCENT);
    }
}

/// "4s", "2m 05s".
pub fn elapsed(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s < 60 { format!("{s}s") } else { format!("{}m {:02}s", s / 60, s % 60) }
}

/// The red ⊗ that discards running work.
pub fn discard_button(ui: &mut egui::Ui) -> bool {
    ui.add(egui::Button::new(egui::RichText::new(egui_phosphor::regular::X_CIRCLE).color(ERROR).size(15.0)).frame(false))
        .on_hover_text("Discard this operation")
        .clicked()
}

/// The button for a dialog's main action: filled with the accent colour,
/// or red when it destroys something, so the eye finds it first.
pub fn primary_button(text: impl Into<String>, danger: bool) -> egui::Button<'static> {
    let fill = if danger { Color32::from_rgb(0xc4, 0x3c, 0x3c) } else { Color32::from_rgb(0x2f, 0x6f, 0xd6) };
    egui::Button::new(egui::RichText::new(text).strong().color(Color32::WHITE)).fill(fill)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Text in the non-Latin scripts the data uses (Thai, Chinese, Lao,
    /// Khmer, Myanmar, Korean, Japanese) has glyphs in both families. Latin
    /// text isn't checked: egui's has_glyph reports every character of the
    /// replacement-glyph font (Hack, for Monospace) as missing.
    #[test]
    fn fonts_cover_the_data_languages() {
        let ctx = egui::Context::default();
        install(&ctx);
        ctx.run_ui(egui::RawInput::default(), |_| {}).textures_delta.clear();
        for family in [FontId::proportional(13.0), mono(13.0)] {
            for text in ["ภาษาไทย", "元素", "元素們", "ພາສາລາວ", "ធាតុ", "မြန်မာ", "한국어", "ひらがな"] {
                let missing: Vec<String> = text.chars().filter(|c| !ctx.fonts_mut(|f| f.has_glyph(&family, *c))).map(|c| format!("{c:?} U+{:04X}", c as u32)).collect();
                assert!(missing.is_empty(), "{family:?} has no glyphs for {missing:?} in {text}");
            }
        }
    }
}
