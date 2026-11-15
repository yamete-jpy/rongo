//! Shortcuts on each OS. The app's labels are written the macOS way
//! ("⇧⌘R"); off macOS they read the Windows way ("Ctrl+Shift+R"). egui
//! already maps ⌘ to Ctrl for the shortcuts themselves. Every label with a
//! shortcut in it goes through `label` (the tests check the source for any
//! that doesn't). `drop_emacs_keys` keeps egui's ⌃-keys for text fields to
//! macOS, where Ctrl isn't the command key.

use std::borrow::Cow;

/// `text` with every shortcut in it written for this OS.
pub fn label(text: &str) -> Cow<'_, str> {
    relabel(text, cfg!(target_os = "macos"))
}

/// macOS: `text` as it is. Elsewhere: each run of ⌃⌥⇧⌘ and the key after it
/// becomes "Ctrl+Alt+Shift+Key", in that order (⌃ and ⌘ are both Ctrl), and
/// the key symbols become names (↩ is "Enter"), with modifiers or alone.
/// An arrow is a key only after a modifier: alone, "→" is prose ("Settings →
/// Collection History"). Modifiers before a space or "-" stay alone
/// ("⇧-click" is "Shift-click").
fn relabel(text: &str, mac: bool) -> Cow<'_, str> {
    if mac || !text.chars().any(|c| MODIFIERS.contains(&c) || key_name(c).is_some()) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if MODIFIERS.contains(&c) {
            let (mut ctrl, mut alt, mut shift) = (false, false, false);
            let mut m = Some(c);
            while let Some(c) = m {
                match c {
                    '⌥' => alt = true,
                    '⇧' => shift = true,
                    _ => ctrl = true,
                }
                m = chars.next_if(|c| MODIFIERS.contains(c));
            }
            let names: Vec<&str> = [(ctrl, "Ctrl"), (alt, "Alt"), (shift, "Shift")].into_iter().filter_map(|(on, name)| on.then_some(name)).collect();
            out.push_str(&names.join("+"));
            if let Some(k) = chars.next_if(|k| !k.is_whitespace() && *k != '-') {
                out.push('+');
                match key_name(k).or_else(|| arrow_name(k)) {
                    Some(name) => out.push_str(name),
                    None => out.push(k),
                }
            }
        } else if let Some(name) = key_name(c) {
            out.push_str(name);
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// ⌃ Control, ⌥ Option, ⇧ Shift, ⌘ Command.
const MODIFIERS: [char; 4] = ['⌃', '⌥', '⇧', '⌘'];

fn key_name(c: char) -> Option<&'static str> {
    Some(match c {
        '↩' | '⏎' => "Enter",
        '⌫' => "Backspace",
        '⌦' => "Delete",
        '⎋' => "Esc",
        '⇥' => "Tab",
        _ => return None,
    })
}

fn arrow_name(c: char) -> Option<&'static str> {
    Some(match c {
        '↑' => "Up",
        '↓' => "Down",
        '←' => "Left",
        '→' => "Right",
        _ => return None,
    })
}

/// egui's text fields take ⌃H ⌃K ⌃U ⌃W as macOS fields do (delete a
/// character, to the line's end, to its start, a word). Off macOS Ctrl is
/// the command key and those are ⌘H ⌘K ⌘U ⌘W: Ctrl+U, Update Dialog in the
/// menus, would clear the query bar where ⌘U does nothing. So while a text
/// field has focus they are dropped off macOS. Called after the app's own
/// shortcuts have had them (Ctrl+W closes the tab), before anything is drawn.
pub fn drop_emacs_keys(ctx: &egui::Context) {
    drop_emacs_keys_if(ctx, cfg!(target_os = "macos"));
}

fn drop_emacs_keys_if(ctx: &egui::Context, mac: bool) {
    if !mac && ctx.text_edit_focused() {
        ctx.input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Key { key: egui::Key::H | egui::Key::K | egui::Key::U | egui::Key::W, modifiers, .. } if modifiers.ctrl)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// The macOS labels and what they read off macOS.
    const TABLE: &[(&str, &str)] = &[
        ("⌘T", "Ctrl+T"),
        ("⇧⌘R", "Ctrl+Shift+R"),
        ("⌥⌘C", "Ctrl+Alt+C"),
        ("⌘⇧S", "Ctrl+Shift+S"),
        ("⌥⇧⌘K", "Ctrl+Alt+Shift+K"),
        ("⌃Space", "Ctrl+Space"),
        ("⌃⌘F", "Ctrl+F"),
        ("⌘,", "Ctrl+,"),
        ("⌘↩", "Ctrl+Enter"),
        ("⇧⌘↩", "Ctrl+Shift+Enter"),
        ("⇧↩", "Shift+Enter"),
        ("↩", "Enter"),
        ("⏎", "Enter"),
        ("⇧⌫", "Shift+Backspace"),
        ("⌦", "Delete"),
        ("⎋", "Esc"),
        ("⇧⇥", "Shift+Tab"),
        ("⌘↑", "Ctrl+Up"),
        ("⌥←", "Alt+Left"),
        ("F5", "F5"),
        ("Run all or selection (⌘↩ / F5)", "Run all or selection (Ctrl+Enter / F5)"),
        ("Search open connections (⌘F)", "Search open connections (Ctrl+F)"),
        ("Double-click a collection to browse it · ⌘↩ runs the shell · ⌘W closes a tab", "Double-click a collection to browse it · Ctrl+Enter runs the shell · Ctrl+W closes a tab"),
        ("⌘-click adds a row, ⇧-click a range", "Ctrl-click adds a row, Shift-click a range"),
        ("Settings → Collection History", "Settings → Collection History"),
        ("SIT → UAT", "SIT → UAT"),
    ];

    #[test]
    fn labels_read_the_windows_way_off_macos() {
        for (mac, other) in TABLE {
            assert_eq!(relabel(mac, false), *other, "{mac}");
        }
    }

    #[test]
    fn macos_labels_are_unchanged() {
        for (mac, _) in TABLE {
            assert!(matches!(relabel(mac, true), Cow::Borrowed(s) if s == *mac), "{mac}");
        }
        // Text without a shortcut is not copied on either OS.
        assert!(matches!(relabel("Refresh View", false), Cow::Borrowed("Refresh View")));
    }

    /// Off macOS, Ctrl+U in a focused text field (the query bar, the shell)
    /// leaves the text alone, as ⌘U does on macOS; egui's text fields would
    /// take it as ⌃U and delete the line up to the cursor (here after
    /// "db.coll"). Ctrl+H, Ctrl+K and Ctrl+W likewise. On macOS ⌃U still
    /// deletes, as Cocoa fields do.
    #[test]
    fn ctrl_keys_leave_text_fields_alone_off_macos() {
        let after = |mac: bool, key: egui::Key| {
            let ctx = egui::Context::default();
            let mut text = String::from("db.coll.find()");
            let id = egui::Id::new("field");
            for events in [vec![], vec![egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::CTRL | egui::Modifiers::COMMAND }]] {
                let input = egui::RawInput { events, ..Default::default() };
                ctx.run_ui(input, |ui| {
                    drop_emacs_keys_if(ui.ctx(), mac);
                    let r = ui.add(egui::TextEdit::singleline(&mut text).id(id));
                    if !r.has_focus() {
                        r.request_focus();
                        let mut state = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
                        state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(7))));
                        state.store(ui.ctx(), id);
                    }
                })
                .textures_delta
                .clear();
            }
            text
        };
        let keys = [egui::Key::U, egui::Key::H, egui::Key::K, egui::Key::W];
        assert_eq!(keys.map(|k| after(false, k)), ["db.coll.find()"; 4]);
        assert_eq!(after(true, egui::Key::U), ".find()");
    }

    /// Characters that make a string literal a shortcut label. The scan does
    /// not see a label written as a `\u{2318}` escape, one with no glyph at
    /// all ("Cmd+") or an arrow alone; a new label must use the glyphs.
    const GLYPHS: [char; 10] = ['⌃', '⌥', '⇧', '⌘', '↩', '⏎', '⌫', '⌦', '⎋', '⇥'];

    /// One source file, as far as the label check needs it.
    #[derive(Default, Debug)]
    struct Scan {
        /// Its `mod x;` declarations, other than `#[cfg(test)]` ones.
        mods: Vec<String>,
        /// String literals with a shortcut glyph that don't go straight
        /// into `shortcut::label(…)`.
        bare: Vec<String>,
        /// Those that do.
        labelled: usize,
    }

    /// Reads Rust source well enough for the check: comments, strings (raw
    /// and byte ones too) and char literals are told apart from code, and
    /// `#[cfg(test)] mod … { … }` blocks are passed over.
    fn scan(src: &str) -> Scan {
        let c: Vec<char> = src.chars().collect();
        let mut out = Scan::default();
        // The code since the last `;`, `{` or `}`, without comments and with
        // each string literal as `""`.
        let mut stmt = String::new();
        let mut depth = 0usize;
        // Inside a test module: the depth its block closes at.
        let mut skip: Option<usize> = None;
        let mut i = 0;
        while i < c.len() {
            let ident_before = i > 0 && (c[i - 1].is_alphanumeric() || c[i - 1] == '_');
            if c[i] == '/' && c.get(i + 1) == Some(&'/') {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                let mut nest = 0;
                while i < c.len() {
                    if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                        nest += 1;
                        i += 2;
                    } else if c[i] == '*' && c.get(i + 1) == Some(&'/') {
                        nest -= 1;
                        i += 2;
                        if nest == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                continue;
            }
            if let Some((body, end)) = (!ident_before).then(|| string_at(&c, i)).flatten() {
                if skip.is_none() && body.contains(GLYPHS) {
                    if stmt.trim_end().ends_with("shortcut::label(") {
                        out.labelled += 1;
                    } else {
                        out.bare.push(body);
                    }
                }
                stmt.push_str("\"\"");
                i = end;
                continue;
            }
            if c[i] == '\'' {
                if c.get(i + 1) == Some(&'\\') {
                    i += 2;
                    while i < c.len() && c[i] != '\'' {
                        i += 1;
                    }
                    i += 1;
                    continue;
                }
                if c.get(i + 2) == Some(&'\'') {
                    i += 3;
                    continue;
                }
            }
            match c[i] {
                '{' | ';' => {
                    if let Some((test, name)) = module_decl(&stmt) {
                        if c[i] == '{' && test && skip.is_none() {
                            skip = Some(depth);
                        } else if c[i] == ';' && !test {
                            out.mods.push(name);
                        }
                    }
                    if c[i] == '{' {
                        depth += 1;
                    }
                    stmt.clear();
                }
                '}' => {
                    depth = depth.saturating_sub(1);
                    if skip == Some(depth) {
                        skip = None;
                    }
                    stmt.clear();
                }
                ch => stmt.push(ch),
            }
            i += 1;
        }
        out
    }

    /// A string literal starting at `i` (`"…"`, `r#"…"#`, `b"…"`, `br"…"`):
    /// its contents and the index after it.
    fn string_at(c: &[char], i: usize) -> Option<(String, usize)> {
        let mut j = i;
        if c.get(j) == Some(&'b') {
            j += 1;
        }
        let raw = c.get(j) == Some(&'r');
        if raw {
            j += 1;
        }
        let hashes = c[j.min(c.len())..].iter().take_while(|&&h| h == '#').count();
        if !raw && hashes > 0 {
            return None;
        }
        j += hashes;
        if c.get(j) != Some(&'"') {
            return None;
        }
        let start = j + 1;
        let mut k = start;
        while k < c.len() {
            if !raw && c[k] == '\\' {
                k += 2;
                continue;
            }
            if c[k] == '"' && c[k + 1..].iter().take(hashes).filter(|&&h| h == '#').count() == hashes {
                return Some((c[start..k].iter().collect(), k + 1 + hashes));
            }
            k += 1;
        }
        Some((c[start.min(c.len())..].iter().collect(), c.len()))
    }

    /// `[#[attr]…] [pub[(…)]] mod name` (the code before its `;` or `{`;
    /// the first one in a file may follow the file's `#![attr]`s):
    /// whether `#[cfg(test)]` is among the attributes, and the name.
    fn module_decl(stmt: &str) -> Option<(bool, String)> {
        let mut rest = stmt.trim();
        let mut test = false;
        while let Some(attr) = rest.strip_prefix("#[").or_else(|| rest.strip_prefix("#![")) {
            let end = attr.find(']')?;
            test |= attr[..end].trim() == "cfg(test)";
            rest = attr[end + 1..].trim_start();
        }
        if let Some(r) = rest.strip_prefix("pub") {
            rest = r.trim_start();
            if rest.starts_with('(') {
                rest = rest[rest.find(')')? + 1..].trim_start();
            }
        }
        let name = rest.strip_prefix("mod ")?.trim();
        (!name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_')).then(|| (test, name.to_string()))
    }

    /// The file a `mod name;` in `file` refers to.
    fn module_file(file: &Path, name: &str) -> PathBuf {
        let owns_dir = matches!(file.file_name().and_then(|n| n.to_str()), Some("main.rs" | "mod.rs" | "lib.rs"));
        let base = if owns_dir { file.parent().expect("a folder").to_path_buf() } else { file.with_extension("") };
        let flat = base.join(format!("{name}.rs"));
        if flat.exists() { flat } else { base.join(name).join("mod.rs") }
    }

    #[test]
    fn the_scan_reads_rust_well_enough() {
        let src = r##"
            #![cfg_attr(not(test), windows_subsystem = "windows")]
            mod first;
            // "⌘A" in a comment
            /* "⌘B" /* nested */ still a comment */
            const RAW: &str = r#"a "quoted" ⌘C"#;
            fn f() { let q = '"'; let e = '\''; let l: &'static str = "⇧D"; g(shortcut::label("⌘E")); g(shortcut::label(
                r#"⌘F"#)); g(format!("{}", "↩")); }
            mod real;
            #[cfg(test)]
            mod skipped;
            #[cfg(test)]
            mod tests { fn t() { assert_eq!(x, "⌘G"); } }
            pub(crate) mod after;
        "##;
        let s = scan(src);
        assert_eq!(s.bare, ["a \"quoted\" ⌘C", "⇧D", "↩"]);
        assert_eq!(s.labelled, 2);
        assert_eq!(s.mods, ["first", "real", "after"]);
    }

    /// Every label with a shortcut in it goes through `label`: no string
    /// literal with ⌘ ⇧ ⌥ ⌃ or a key symbol is drawn as it is, in any file
    /// the app is built from (test code aside).
    #[test]
    fn every_shortcut_label_goes_through_the_helper() {
        let main = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
        let (mut files, mut todo, mut bare, mut labelled) = (Vec::new(), vec![main], Vec::new(), 0);
        while let Some(file) = todo.pop() {
            let src = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            let s = scan(&src);
            labelled += s.labelled;
            bare.extend(s.bare.into_iter().map(|b| format!("{}: {b:?}", file.display())));
            todo.extend(s.mods.iter().map(|m| module_file(&file, m)));
            files.push(file);
        }
        let has = |end: &str| files.iter().any(|f| f.ends_with(end));
        assert!(files.len() > 30 && has("sidebar.rs") && has("result_view/menus.rs") && has("app/chrome.rs"), "walked {files:?}");
        assert!(!has("result_view/tests.rs") && !has("app/tests/mod.rs"), "test files are left out");
        assert!(bare.is_empty(), "labels that don't go through shortcut::label:\n{}", bare.join("\n"));
        assert!(labelled >= 30, "only {labelled} labels found: the scan misses some");
    }

    /// rongo runs on Windows too: no text it shows says "this Mac" (say "this
    /// computer"). Comments are left out; macOS-only texts (the Keychain move)
    /// name the Keychain, not the Mac. The needle is built here so this file
    /// doesn't contain it.
    #[test]
    fn no_text_says_this_mac() {
        fn walk(dir: &Path, found: &mut Vec<String>, files: &mut usize) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, found, files);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    *files += 1;
                    let needle = ["this", "Mac"].join(" ");
                    for (n, line) in std::fs::read_to_string(&path).unwrap().lines().enumerate() {
                        if !line.trim_start().starts_with("//") && line.contains(&needle) {
                            found.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
                        }
                    }
                }
            }
        }
        let (mut found, mut files) = (Vec::new(), 0);
        for krate in ["src", "../core/src"] {
            walk(&Path::new(env!("CARGO_MANIFEST_DIR")).join(krate), &mut found, &mut files);
        }
        assert!(files > 60, "walked only {files} files");
        assert!(found.is_empty(), "texts that name the Mac:\n{}", found.join("\n"));
    }
}
