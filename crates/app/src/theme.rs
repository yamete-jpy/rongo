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

/// A find bar's matches behind the text, and the current one.
pub const FIND_MATCH: Color32 = Color32::from_rgba_premultiplied(0x5a, 0x4a, 0x14, 0xa0);
pub const FIND_CURRENT: Color32 = Color32::from_rgb(0xb0, 0x7a, 0x10);

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

/// A system font file: a full path (macOS), or a file in Windows' Fonts
/// folder (`%WINDIR%\Fonts`; Windows needn't be on C:).
enum FontFile {
    Path(&'static str),
    WindowsFonts(&'static str),
}

/// Fonts for scripts egui's own fonts lack, tried in this order after them.
/// One list for both systems; missing files are skipped. The Windows names
/// are the ones Microsoft's Windows 10 and 11 font lists give; Nirmala UI is
/// also tried as a .ttc, as some Windows 11 builds ship it.
const FALLBACK_FONTS: &[FontFile] = &[
    FontFile::Path("/System/Library/Fonts/Supplemental/Thonburi.ttc"),
    FontFile::Path("/System/Library/Fonts/Hiragino Sans GB.ttc"),
    FontFile::Path("/System/Library/Fonts/AppleSDGothicNeo.ttc"),
    FontFile::Path("/System/Library/Fonts/Supplemental/Lao Sangam MN.ttf"),
    FontFile::Path("/System/Library/Fonts/Supplemental/Khmer Sangam MN.ttf"),
    FontFile::Path("/System/Library/Fonts/Supplemental/Myanmar Sangam MN.ttc"),
    FontFile::Path("/System/Library/Fonts/Supplemental/Arial Unicode.ttf"),
    // Leelawadee UI: Thai, Lao, Khmer.
    FontFile::WindowsFonts("LeelawUI.ttf"),
    // Microsoft YaHei: Chinese (and kana).
    FontFile::WindowsFonts("msyh.ttc"),
    // Malgun Gothic: Korean.
    FontFile::WindowsFonts("malgun.ttf"),
    // Yu Gothic: Japanese.
    FontFile::WindowsFonts("YuGothR.ttc"),
    // Myanmar Text.
    FontFile::WindowsFonts("mmrtext.ttf"),
    // Nirmala UI: the Indic scripts.
    FontFile::WindowsFonts("Nirmala.ttc"),
    FontFile::WindowsFonts("Nirmala.ttf"),
    // Segoe UI Symbol: symbols.
    FontFile::WindowsFonts("seguisym.ttf"),
];

/// The fallback font files to try, in order: the Windows ones only when
/// there is a Windows folder (`windir`, from `%WINDIR%`).
fn fallback_font_paths(windir: Option<&std::path::Path>) -> Vec<std::path::PathBuf> {
    FALLBACK_FONTS
        .iter()
        .filter_map(|f| match f {
            FontFile::Path(path) => Some(std::path::PathBuf::from(path)),
            FontFile::WindowsFonts(name) => windir.map(|w| w.join("Fonts").join(name)),
        })
        .collect()
}

/// The Windows folder: `%WINDIR%`, else `%SystemRoot%`; an empty value is
/// no value.
fn windows_dir(windir: Option<std::ffi::OsString>, system_root: Option<std::ffi::OsString>) -> Option<std::path::PathBuf> {
    [windir, system_root].into_iter().flatten().find(|v| !v.is_empty()).map(std::path::PathBuf::from)
}

/// The fallback font files, memory-mapped once. The CJK files are 20-55 MB
/// each; mapping them means only the glyphs actually drawn are read into
/// memory, and egui borrows the bytes without copying them.
fn fallback_fonts() -> &'static [(String, &'static [u8])] {
    static FONTS: std::sync::OnceLock<Vec<(String, &'static [u8])>> = std::sync::OnceLock::new();
    FONTS.get_or_init(|| {
        let windir = windows_dir(std::env::var_os("WINDIR"), std::env::var_os("SystemRoot"));
        fallback_font_paths(windir.as_deref())
            .into_iter()
            .filter_map(|path| {
                let file = std::fs::File::open(&path).ok()?;
                // SAFETY: system font files are read-only (SIP-protected on
                // macOS, owned by TrustedInstaller on Windows) and are not
                // modified while the app runs.
                let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
                let bytes: &'static [u8] = Box::leak(Box::new(map));
                Some((path.display().to_string(), bytes))
            })
            .collect()
    })
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    for (name, bytes) in fallback_fonts() {
        fonts.font_data.insert(name.clone(), std::sync::Arc::new(egui::FontData::from_static(bytes)));
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push(name.clone());
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

/// A dialog's row of buttons, laid out from the right: the main action
/// first, then Cancel or Close, then the rest.
pub fn dialog_buttons<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.horizontal(|ui| ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add).inner).inner
}

/// In `dialog_buttons`, after the buttons: a line in the room they leave on
/// the left, cut short with "…" rather than widening the dialog.
pub fn footer_note(ui: &mut egui::Ui, text: impl Into<egui::WidgetText>) {
    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.add(egui::Label::new(text).truncate());
    });
}

/// The button for a dialog's main action: filled with the accent colour,
/// or red when it destroys something, so the eye finds it first.
pub fn primary_button(text: impl Into<String>, danger: bool) -> egui::Button<'static> {
    let fill = if danger { Color32::from_rgb(0xc4, 0x3c, 0x3c) } else { Color32::from_rgb(0x2f, 0x6f, 0xd6) };
    egui::Button::new(egui::RichText::new(text).strong().color(Color32::WHITE)).fill(fill)
}

/// A message wrapped at `width` and cut to `rows` lines ending in "…",
/// with all of it on hover. A driver error can be a paragraph (one entry
/// per replica-set member); unwrapped in a row it ran off the window.
pub fn clipped_text(ui: &mut egui::Ui, text: &str, color: Color32, rows: usize, width: f32) -> egui::Response {
    let font = TextStyle::Body.resolve(ui.style());
    let mut job = egui::text::LayoutJob::simple(text.to_owned(), font, color, width.max(40.0));
    job.wrap.max_rows = rows;
    job.wrap.overflow_character = Some('…');
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let cut = galley.elided;
    let r = ui.add(egui::Label::new(galley));
    if cut { r.on_hover_text(text) } else { r }
}

/// Text past this size is shown without colours: tokenizing it (a query
/// listing 50,000 ids, a document of megabytes) costs more than the
/// colours are worth.
pub const MAX_HIGHLIGHT: usize = 100_000;

/// A code field's laid-out text, kept so that a repaint with the same text
/// and width (a caret blink, a spinner, the pointer moving) reuses it
/// instead of tokenizing and laying out all of it again.
#[derive(Clone, Default)]
pub struct CodeLayout {
    last: Option<((u64, u32, u32), std::sync::Arc<egui::Galley>)>,
}

impl CodeLayout {
    /// `text` laid out in `font` and wrapped at `wrap`: coloured up to
    /// `MAX_HIGHLIGHT` bytes, plain beyond. For a `TextEdit` layouter.
    pub fn galley(&mut self, ui: &egui::Ui, text: &str, font: FontId, wrap: f32) -> std::sync::Arc<egui::Galley> {
        // A hash of the text costs far less than tokenizing it. The scale
        // counts too: a galley is laid out for one.
        let key = (egui::Id::new((text, font.size.to_bits())).value(), wrap.to_bits(), ui.ctx().pixels_per_point().to_bits());
        if let Some((k, g)) = &self.last
            && *k == key
        {
            return g.clone();
        }
        let job = if text.len() > MAX_HIGHLIGHT {
            egui::text::LayoutJob::simple(text.to_string(), font, TEXT, wrap)
        } else {
            crate::highlight::layout_job(text, font, wrap)
        };
        let galley = ui.fonts_mut(|f| f.layout_job(job));
        self.last = Some((key, galley.clone()));
        galley
    }
}

/// A dialog's error: a few lines across the dialog, cut with "…" and whole
/// on hover, so a paragraph (a validator listing every rule a document
/// broke) can't push the buttons off a short window.
pub fn error_note(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let width = ui.available_width();
    clipped_text(ui, text, ERROR, 4, width)
}

/// Whether a dialog closes on a click outside it or Esc: only while it
/// holds nothing typed or changed (`dirty`) that closing would throw away.
/// Otherwise only its Cancel / Close buttons close it. Esc is taken either
/// way, so nothing under the dialog acts on it.
pub fn may_close<T>(modal: &egui::ModalResponse<T>, dirty: bool) -> bool {
    modal.should_close() && !dirty
}

/// How many edits text fields have reported so far this frame. Compared
/// before and after a dialog's contents, it tells whether something was
/// typed into the dialog: every text field reports each edit as a
/// `ValueChanged` output event (made for screen readers).
pub fn text_edits(ctx: &egui::Context) -> usize {
    use egui::output::OutputEvent;
    ctx.output(|o| o.events.iter().filter(|e| matches!(e, OutputEvent::ValueChanged(i) if i.typ == egui::WidgetType::TextEdit)).count())
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

    /// Where `%WINDIR%` isn't set (macOS) the list is the macOS files, as
    /// before; on Windows the Windows files follow, from its Fonts folder.
    #[test]
    fn the_fallback_fonts_resolve_per_os() {
        let mac = [
            "/System/Library/Fonts/Supplemental/Thonburi.ttc",
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
            "/System/Library/Fonts/AppleSDGothicNeo.ttc",
            "/System/Library/Fonts/Supplemental/Lao Sangam MN.ttf",
            "/System/Library/Fonts/Supplemental/Khmer Sangam MN.ttf",
            "/System/Library/Fonts/Supplemental/Myanmar Sangam MN.ttc",
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        ]
        .map(std::path::PathBuf::from);
        assert_eq!(fallback_font_paths(None), mac);
        let windir = std::path::Path::new(r"D:\Win 11");
        let windows = ["LeelawUI.ttf", "msyh.ttc", "malgun.ttf", "YuGothR.ttc", "mmrtext.ttf", "Nirmala.ttc", "Nirmala.ttf", "seguisym.ttf"].map(|f| windir.join("Fonts").join(f));
        assert_eq!(fallback_font_paths(Some(windir)), [&mac[..], &windows[..]].concat());
    }

    /// The Windows folder is `%WINDIR%`, or `%SystemRoot%` where that is
    /// unset; empty values count as unset.
    #[test]
    fn the_windows_folder_falls_back_to_systemroot() {
        let os = |s: &str| Some(std::ffi::OsString::from(s));
        assert_eq!(windows_dir(os(r"C:\Windows"), os(r"D:\Other")), Some(std::path::PathBuf::from(r"C:\Windows")));
        assert_eq!(windows_dir(None, os(r"D:\Other")), Some(std::path::PathBuf::from(r"D:\Other")));
        assert_eq!(windows_dir(os(""), os(r"D:\Other")), Some(std::path::PathBuf::from(r"D:\Other")));
        assert_eq!(windows_dir(None, None), None);
        assert_eq!(windows_dir(os(""), os("")), None);
    }

    /// A long message stops at its row limit with "…" and stays inside its
    /// width; a short one is left whole.
    #[test]
    fn clipped_text_keeps_to_its_rows() {
        let ctx = egui::Context::default();
        install(&ctx);
        let long = "timed out: Server selection timeout: No available servers. ".repeat(20);
        let mut shapes = Vec::new();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            clipped_text(ui, &long, ERROR, 3, 300.0);
            clipped_text(ui, "Connection saved", TEXT, 3, 300.0);
        });
        out.textures_delta.clear();
        for s in out.shapes {
            if let egui::epaint::Shape::Text(t) = s.shape {
                shapes.push(t.galley);
            }
        }
        let [long_g, short_g] = &shapes[..] else { panic!("two texts, got {}", shapes.len()) };
        assert_eq!(long_g.rows.len(), 3);
        let last = long_g.rows.last().and_then(|r| r.glyphs.last()).map(|g| g.chr);
        assert!(long_g.elided && last == Some('…'), "ends with {last:?}");
        assert!(long_g.rect.width() <= 300.0, "{}", long_g.rect.width());
        assert!(!short_g.elided);
        assert_eq!(short_g.text(), "Connection saved");
    }
}
