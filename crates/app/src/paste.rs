//! The Paste dialog: what to do with collections copied from the tree.

use std::collections::{HashMap, HashSet};

use egui::{Align, Id, Layout, Modal, RichText, Ui};
use egui_phosphor::regular as icon;
use rongo_core::{Command, ConnId, CopyItem, CopyOptions, IfExists, Namespace, OnDuplicateId};

use crate::theme;

/// Collections copied from the tree, waiting to be pasted.
pub struct CollClip {
    pub conn: ConnId,
    pub db: String,
    pub colls: Vec<String>,
}

pub struct PasteRow {
    pub src: Namespace,
    /// The new name as typed; spaces around it don't count (`name`).
    pub target: String,
    pub mode: IfExists,
}

impl PasteRow {
    /// The collection this row writes: the typed name without the spaces
    /// around it (pasting a name often brings one along).
    pub fn name(&self) -> &str {
        self.target.trim()
    }
}

pub struct TargetInfo<'a> {
    pub name: &'a str,
    pub color: Option<egui::Color32>,
    pub read_only: bool,
}

pub enum PasteAction {
    Send(Command),
    Close,
}

pub struct PasteDialog {
    pub rows: Vec<PasteRow>,
    pub indexes: bool,
    pub collection_options: bool,
    /// The confirmation typed for a Replace.
    pub typed: String,
    /// The Replace rows' targets `typed` was typed for: when they change,
    /// what was typed confirms nothing and is cleared.
    typed_for: Vec<String>,
    to: ConnId,
    to_db: String,
    from_conn: ConnId,
    from_db: String,
    from_name: String,
    /// The target database's collections, once known. Until then nobody
    /// can say which rows land on an existing collection, so Paste waits.
    /// A set: every row looks itself up in it each frame.
    existing: Option<HashSet<String>>,
    /// Why the target database's collections couldn't be listed.
    listing_error: Option<String>,
    /// A name or the confirmation was typed (or a row renamed): a click
    /// outside or Esc no longer closes the dialog.
    edited: bool,
    /// A row's height as last drawn: only the rows in view are laid out.
    row_h: f32,
}

impl PasteDialog {
    /// One row per copied collection, named as in the source and appended
    /// to, skipping duplicate `_id`s; indexes and options come along.
    pub fn new(clip: &CollClip, to: ConnId, to_db: String) -> Self {
        let rows = clip
            .colls
            .iter()
            .map(|c| PasteRow { src: Namespace::new(clip.db.clone(), c.clone()), target: c.clone(), mode: IfExists::Append(OnDuplicateId::default()) })
            .collect();
        Self { rows, indexes: true, collection_options: true, typed: String::new(), typed_for: Vec::new(), to, to_db, from_conn: clip.conn, from_db: clip.db.clone(), from_name: String::new(), existing: None, listing_error: None, edited: false, row_h: 0.0 }
    }

    /// The connection pasted into.
    pub fn to(&self) -> ConnId {
        self.to
    }

    /// The database pasted into.
    pub fn to_db(&self) -> &str {
        &self.to_db
    }

    /// The connection the collections were copied from.
    pub fn from(&self) -> ConnId {
        self.from_conn
    }

    /// The name of the source connection, for the header.
    pub fn set_source_name(&mut self, name: impl Into<String>) {
        self.from_name = name.into();
    }

    /// The target database's collections: rows that land on one of them
    /// offer Append / Replace.
    pub fn set_existing(&mut self, names: Vec<String>) {
        self.existing = Some(names.into_iter().collect());
        self.listing_error = None;
    }

    /// The target database's collections couldn't be listed: Paste stays
    /// off and says why.
    pub fn set_listing_failed(&mut self, why: String) {
        self.listing_error = Some(why);
    }

    fn exists(&self, row: &PasteRow) -> bool {
        self.existing.as_ref().is_some_and(|e| e.contains(row.name()))
    }

    /// Forget a confirmation typed for another set of Replace rows
    /// (`replaces`, by index).
    fn sync_typed(&mut self, replaces: &[usize]) {
        let same = self.typed_for.len() == replaces.len() && self.typed_for.iter().zip(replaces).all(|(t, &i)| t == self.rows[i].name());
        if !same {
            self.typed.clear();
            self.typed_for = replaces.iter().map(|&i| self.rows[i].name().to_string()).collect();
        }
    }

    /// The rows that will drop an existing collection, by index.
    fn replaces(&self) -> Vec<usize> {
        (0..self.rows.len()).filter(|&i| self.rows[i].mode == IfExists::Replace && self.exists(&self.rows[i])).collect()
    }

    /// What must be typed before pasting: the collection's name for one
    /// Replace, the database's for several.
    fn confirmation(&self, replaces: &[usize]) -> Option<String> {
        match replaces {
            [] => None,
            [one] => Some(self.rows[*one].name().to_string()),
            _ => Some(self.to_db.clone()),
        }
    }

    /// How many rows paste into each name.
    fn targets(&self) -> HashMap<&str, usize> {
        let mut counts = HashMap::with_capacity(self.rows.len());
        for r in &self.rows {
            *counts.entry(r.name()).or_insert(0) += 1;
        }
        counts
    }

    /// A free name for row `i` ending in `_copy`: not in the target
    /// database and not another row's target (`targets`).
    fn copy_name(&self, i: usize, targets: &HashMap<&str, usize>) -> String {
        let own = self.rows[i].name();
        let base = format!("{own}_copy");
        let others = |n: &str| targets.get(n).copied().unwrap_or(0).saturating_sub(usize::from(n == own));
        let taken = |n: &str| self.existing.as_ref().is_some_and(|e| e.contains(n)) || others(n) > 0;
        let mut name = base.clone();
        let mut n = 2;
        while taken(&name) {
            name = format!("{base}{n}");
            n += 1;
        }
        name
    }

    /// Tests: `check`, working out the confirmation the Replace rows ask for.
    #[cfg(test)]
    pub fn ready(&self, target: &TargetInfo) -> Result<(), String> {
        self.check(target, self.confirmation(&self.replaces()).as_deref())
    }

    /// Why Paste is not possible yet; the dialog shows it under the button.
    /// Core refuses the same things again per collection, so this is the
    /// first line, not the only one. `confirmation`: what the Replace rows
    /// ask to be typed, if anything.
    fn check(&self, target: &TargetInfo, confirmation: Option<&str>) -> Result<(), String> {
        if target.read_only {
            return Err(format!("{} is read-only", target.name));
        }
        if self.rows.is_empty() {
            return Err("Nothing to paste".into());
        }
        if let Some(why) = &self.listing_error {
            return Err(format!("Couldn't see what {} holds: {why}", self.to_db));
        }
        if self.existing.is_none() {
            return Err(format!("Checking what {} holds…", self.to_db));
        }
        let same_db = self.from_conn == self.to && self.from_db == self.to_db;
        // Looked up, not searched for: a paste can hold thousands of rows.
        let sources: HashMap<&str, usize> = if same_db { self.rows.iter().enumerate().map(|(i, r)| (r.src.coll.as_str(), i)).collect() } else { HashMap::new() };
        let mut first_as: HashMap<&str, usize> = HashMap::with_capacity(self.rows.len());
        for (i, row) in self.rows.iter().enumerate() {
            let name = &row.src.coll;
            let t = row.name();
            if same_db && name == t {
                return Err(format!("{name} can't be pasted into itself"));
            }
            if t.is_empty() {
                return Err(format!("{name}: the new name is empty"));
            }
            if rongo_core::names::bad_collection_name(t) {
                return Err(format!("{name}: '{t}' is not a valid collection name"));
            }
            // Within one database a row's target may be another row's source:
            // writing it first would change what that row then copies.
            if let Some(&j) = sources.get(t)
                && j != i
            {
                return Err(format!("{name} would overwrite {}, which is also being pasted", self.rows[j].src.coll));
            }
            if let Some(&first) = first_as.get(t) {
                return Err(format!("{} and {name} would both be pasted as {t}", self.rows[first].src.coll));
            }
            first_as.insert(t, i);
        }
        if let Some(want) = confirmation
            && !crate::dialogs::confirms(&self.typed, want)
        {
            return Err(format!("Type {want} to confirm replacing what is there"));
        }
        Ok(())
    }

    pub fn build(&self) -> Command {
        let items = self
            .rows
            .iter()
            .map(|r| CopyItem { src: r.src.clone(), dst: Namespace::new(self.to_db.clone(), r.name()), if_exists: r.mode })
            .collect();
        Command::CopyCollections { to: self.to, items, options: CopyOptions { indexes: self.indexes, collection_options: self.collection_options, history: None } }
    }

    pub fn show(&mut self, ctx: &egui::Context, target: &TargetInfo) -> Option<PasteAction> {
        let mut action = None;
        // A collection that isn't there has nothing to replace.
        let existing = &self.existing;
        for row in &mut self.rows {
            if !existing.as_ref().is_some_and(|e| e.contains(row.name())) {
                row.mode = IfExists::Append(OnDuplicateId::default());
            }
        }
        self.sync_typed(&self.replaces());
        // Never wider or taller than the window, like the connection manager.
        let screen = ctx.content_rect().size();
        let list_h = (screen.y - 340.0).clamp(120.0, 360.0);
        let edits = theme::text_edits(ctx);
        let modal = Modal::new(Id::new("paste-dialog")).show(ctx, |ui| {
            ui.set_width(720.0f32.min(screen.x - 40.0).max(240.0));
            let n = self.rows.len();
            ui.heading(format!("{}  Paste {n} collection{}", icon::CLIPBOARD_TEXT, if n == 1 { "" } else { "s" }));
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                let from = if self.from_name.is_empty() { String::new() } else { format!("{} · ", self.from_name) };
                ui.label(RichText::new(format!("from {from}{}  into", self.from_db)).color(theme::MUTED));
                let lock = if target.read_only { format!("  {}", icon::LOCK_SIMPLE) } else { String::new() };
                ui.label(RichText::new(format!("{}{lock}", target.name)).strong().color(target.color.unwrap_or(theme::TEXT)));
                ui.label(RichText::new(format!("· {}", self.to_db)).strong());
            });
            ui.separator();
            self.rows_ui(ui, list_h);
            ui.separator();
            ui.checkbox(&mut self.indexes, "Copy indexes");
            ui.checkbox(&mut self.collection_options, "Copy collection options and validator");
            // Worked out once, after the rows (which may have changed).
            let replaces = self.replaces();
            let want = self.confirmation(&replaces);
            if let Some(want) = &want {
                ui.add_space(4.0);
                ui.label(RichText::new(format!("Replacing drops what is there first. Type {want} to confirm.")).color(theme::WARN));
                ui.add(egui::TextEdit::singleline(&mut self.typed).hint_text(want).desired_width(f32::INFINITY));
            }
            ui.add_space(8.0);
            // A row changed above: what was typed may be for other rows.
            self.sync_typed(&replaces);
            let ready = self.check(target, want.as_deref());
            theme::dialog_buttons(ui, |ui| {
                let danger = !replaces.is_empty();
                if ui.add_enabled(ready.is_ok(), theme::primary_button("Paste", danger)).clicked() {
                    action = Some(PasteAction::Send(self.build()));
                }
                if ui.button("Cancel").clicked() {
                    action = Some(PasteAction::Close);
                }
            });
            if let Err(why) = &ready {
                ui.add_space(2.0);
                // Wrapped (and cut, whole on hover) to the dialog's width: a
                // long reason such as a driver error must not widen the
                // dialog and push Paste off a narrow window.
                let width = ui.available_width();
                ui.with_layout(Layout::top_down(Align::Max), |ui| {
                    theme::clipped_text(ui, why, theme::WARN, 3, width);
                });
            }
        });
        self.edited |= theme::text_edits(ctx) > edits;
        if theme::may_close(&modal, self.edited) && action.is_none() {
            action = Some(PasteAction::Close);
        }
        action
    }

    /// The rows, one line each. Only those in view are laid out (a paste
    /// of a whole database can hold thousands); a line wider than the
    /// dialog scrolls sideways rather than wrapping, so every line has the
    /// same height.
    fn rows_ui(&mut self, ui: &mut Ui, list_h: f32) {
        if self.row_h <= 0.0 {
            self.row_h = ui.spacing().interact_size.y;
        }
        let row_h = self.row_h;
        let mut tallest = 0.0f32;
        // Its own height from the window (CLAUDE.md: a modal's room is its size
        // last frame): a long list takes `list_h` from the first frame on.
        let scroll = egui::ScrollArea::both().max_height(list_h).min_scrolled_height(list_h).auto_shrink([false, true]);
        scroll.show_rows(ui, row_h, self.rows.len(), |ui, range| {
            // The source name gives way to the rest of the line (whole on hover).
            let name_w = (ui.available_width() * 0.3).clamp(80.0, 240.0);
            let copies: Vec<Option<String>> = {
                let targets = self.targets();
                range.clone().map(|i| self.exists(&self.rows[i]).then(|| self.copy_name(i, &targets))).collect()
            };
            for (i, copy_name) in range.zip(copies) {
                let line = ui.horizontal(|ui| {
                    ui.set_min_height(row_h);
                    self.row(ui, i, copy_name, name_w);
                });
                tallest = tallest.max(line.response.rect.height());
            }
        });
        if tallest > 0.0 {
            self.row_h = tallest;
        }
    }

    /// One collection: its source name, the editable new name, and, when
    /// that name is taken, what to do about it (`copy_name`: a free name
    /// to offer).
    fn row(&mut self, ui: &mut Ui, i: usize, copy_name: Option<String>, name_w: f32) {
        let exists = copy_name.is_some();
        let row = &mut self.rows[i];
        let edited = &mut self.edited;
        ui.scope(|ui| {
            ui.set_max_width(name_w);
            ui.add(egui::Label::new(RichText::new(&row.src.coll).monospace()).truncate());
        });
        ui.label(RichText::new(icon::ARROW_RIGHT).color(theme::MUTED));
        ui.add(egui::TextEdit::singleline(&mut row.target).desired_width(200.0).font(theme::mono(13.0)));
        if !exists {
            return;
        }
        let append = matches!(row.mode, IfExists::Append(_));
        if ui.selectable_label(append, "Append").clicked() && !append {
            row.mode = IfExists::Append(OnDuplicateId::default());
        }
        if ui.selectable_label(!append, "Replace").clicked() && append {
            row.mode = IfExists::Replace;
        }
        if let IfExists::Append(dup) = &mut row.mode {
            let name = |d: OnDuplicateId| match d {
                OnDuplicateId::Skip => "Skip existing _ids",
                OnDuplicateId::Overwrite => "Overwrite existing _ids",
                OnDuplicateId::Stop => "Stop at an existing _id",
            };
            egui::ComboBox::from_id_salt(("paste-dup", i)).selected_text(name(*dup)).show_ui(ui, |ui| {
                for d in [OnDuplicateId::Skip, OnDuplicateId::Overwrite, OnDuplicateId::Stop] {
                    ui.selectable_value(dup, d, name(d));
                }
            });
        }
        if let Some(copy) = &copy_name
            && ui.button(format!("Rename to {copy}")).clicked()
        {
            row.target = copy.clone();
            *edited = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    use super::*;

    fn conn(n: u128) -> ConnId {
        ConnId(uuid::Uuid::from_u128(n))
    }

    fn clip(colls: &[&str]) -> CollClip {
        CollClip { conn: conn(1), db: "sit".into(), colls: colls.iter().map(|s| s.to_string()).collect() }
    }

    fn target(read_only: bool) -> TargetInfo<'static> {
        TargetInfo { name: "UAT", color: None, read_only }
    }

    /// A dialog whose target database is known to be empty.
    fn dialog(colls: &[&str]) -> PasteDialog {
        known(PasteDialog::new(&clip(colls), conn(2), "uat".into()))
    }

    fn known(mut d: PasteDialog) -> PasteDialog {
        d.set_existing(Vec::new());
        d
    }

    #[test]
    fn paste_builds_the_copy_command() {
        let mut d = dialog(&["orders", "users"]);
        d.rows[1].target = "users_2".into();
        d.indexes = false;
        let Command::CopyCollections { to, items, options } = d.build() else { panic!("a CopyCollections command") };
        assert_eq!(to, conn(2));
        let item = |src: &str, dst: &str| CopyItem { src: Namespace::new("sit", src), dst: Namespace::new("uat", dst), if_exists: IfExists::Append(OnDuplicateId::Skip) };
        assert_eq!(items, [item("orders", "orders"), item("users", "users_2")]);
        assert_eq!(options, CopyOptions { indexes: false, collection_options: true, history: None });
        assert_eq!(d.ready(&target(false)), Ok(()));
    }

    /// A dialog on screen for a few frames, as the app drives it.
    fn run(d: PasteDialog, frames: usize) -> Harness<'static, PasteDialog> {
        crate::test_support::harness(egui::vec2(900.0, 700.0), frames, d, |ctx, d| {
            _ = d.show(ctx, &target(false));
        })
    }

    #[test]
    fn existing_targets_offer_a_copy_name() {
        let mut d = dialog(&["orders", "users"]);
        d.set_existing(vec!["orders".into()]);
        let mut h = run(d, 4);
        assert!(h.query_by_label("Append").is_some() && h.query_by_label("Replace").is_some(), "the existing target offers both modes");
        assert!(h.query_by_label("Rename to users_copy").is_none(), "a free name offers no rename");
        h.get_by_label("Rename to orders_copy").click();
        h.run_steps(3);
        assert_eq!(h.state().rows[0].target, "orders_copy");
        assert_eq!(h.state().rows[1].target, "users");
        assert!(h.query_by_label("Rename to orders_copy").is_none(), "no longer clashing");
    }

    #[test]
    fn replace_needs_the_name_typed() {
        let mut d = dialog(&["orders"]);
        d.set_existing(vec!["orders".into()]);
        assert_eq!(d.ready(&target(false)), Ok(()), "append needs no confirmation");
        d.rows[0].mode = IfExists::Replace;
        assert!(d.ready(&target(false)).is_err());
        d.typed = "order".into();
        assert!(d.ready(&target(false)).is_err());
        d.typed = "orders".into();
        assert_eq!(d.ready(&target(false)), Ok(()));

        let mut d = dialog(&["orders", "users"]);
        d.set_existing(vec!["orders".into(), "users".into()]);
        d.rows[0].mode = IfExists::Replace;
        d.rows[1].mode = IfExists::Replace;
        d.typed = "orders".into();
        assert!(d.ready(&target(false)).is_err(), "two replaces want the database name");
        d.typed = "uat".into();
        assert_eq!(d.ready(&target(false)), Ok(()));
    }

    /// Until the target database's collections are known, nobody can say
    /// which rows land on an existing collection (and should offer Replace),
    /// so Paste waits; a failed listing keeps it off and says why.
    #[test]
    fn paste_waits_for_the_target_listing() {
        let mut d = PasteDialog::new(&clip(&["orders"]), conn(2), "uat".into());
        assert_eq!(d.ready(&target(false)), Err("Checking what uat holds…".to_string()));
        d.set_listing_failed("server error 13: not authorized on uat".into());
        assert_eq!(d.ready(&target(false)), Err("Couldn't see what uat holds: server error 13: not authorized on uat".to_string()));
        d.set_existing(vec!["orders".into()]);
        assert_eq!(d.ready(&target(false)), Ok(()), "now known: orders exists, appended to by default");
        assert!(d.exists(&d.rows[0]));
    }

    /// Text typed to confirm one set of Replace rows never confirms another,
    /// even when the name asked for (the database's) stays the same.
    #[test]
    fn typed_confirmation_resets_when_the_replace_rows_change() {
        let mut d = dialog(&["a", "b", "c"]);
        d.set_existing(vec!["a".into(), "b".into(), "c".into()]);
        d.rows[0].mode = IfExists::Replace;
        d.rows[1].mode = IfExists::Replace;
        let mut h = run(d, 3);
        h.state_mut().typed = "uat".into();
        h.run_steps(2);
        assert_eq!(h.state().ready(&target(false)), Ok(()), "a and b confirmed");
        assert_eq!(h.state().typed, "uat");
        h.state_mut().rows[1].mode = IfExists::Append(OnDuplicateId::Skip);
        h.state_mut().rows[2].mode = IfExists::Replace;
        h.run_steps(2);
        assert_eq!(h.state().typed, "", "a and c were never confirmed");
        assert!(h.state().ready(&target(false)).is_err());
        // One Replace row asks for its own name; typed for it, then the rows change again.
        h.state_mut().rows[2].mode = IfExists::Append(OnDuplicateId::Skip);
        h.run_steps(2);
        h.state_mut().typed = "a".into();
        h.run_steps(2);
        assert_eq!(h.state().ready(&target(false)), Ok(()), "a alone, confirmed by its name");
        h.state_mut().rows[1].mode = IfExists::Replace;
        h.run_steps(2);
        assert_eq!(h.state().typed, "");
    }

    /// In the narrowest windows the app allows, a long reason under Paste
    /// wraps inside the dialog instead of widening it: Paste, Cancel and the
    /// reason all stay on screen.
    #[test]
    fn narrow_windows_keep_paste_and_its_reason_on_screen() {
        let long = "x".repeat(70);
        let failed = || {
            let mut d = PasteDialog::new(&clip(&["orders", "users"]), conn(2), "uat".into());
            d.set_listing_failed(crate::test_support::long_driver_error());
            d
        };
        let clash = || {
            let names: Vec<String> = (0..12).map(|i| format!("{long}_{i}")).collect();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            let mut d = known(PasteDialog::new(&clip(&names), conn(1), "sit".into()));
            d.rows[0].target = d.rows[1].src.coll.clone();
            d.rows[1].target = format!("{long}_new");
            d
        };
        for (w, h) in [(960.0, 600.0), (760.0, 600.0)] {
            for (case, d, reason) in [("failed listing", failed(), "Couldn't see what uat holds"), ("clash", clash(), "which is also being pasted")] {
                let mut harness = Harness::builder().with_size(egui::vec2(w, h)).build_ui_state(
                    |ui, d: &mut PasteDialog| {
                        crate::theme::install(ui.ctx());
                        _ = d.show(ui.ctx(), &target(false));
                    },
                    d,
                );
                harness.run_steps(4);
                let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w, h));
                for label in ["Paste", "Cancel"] {
                    let r = harness.get_by_label(label).rect();
                    assert!(screen.contains_rect(r), "{w}x{h} {case}: {label} at {r:?}");
                }
                let shown: Vec<egui::Rect> = harness
                    .output()
                    .shapes
                    .iter()
                    .filter_map(|c| match &c.shape {
                        egui::Shape::Text(t) if t.galley.text().contains(reason) => Some(t.visual_bounding_rect().intersect(c.clip_rect)),
                        _ => None,
                    })
                    .collect();
                assert_eq!(shown.len(), 1, "{w}x{h} {case}: the reason is drawn once");
                let r = shown[0];
                assert!(screen.contains_rect(r) && r.width() > 100.0, "{w}x{h} {case}: the reason is on screen: {r:?}");
            }
        }
    }

    /// A click outside the Paste dialog, or Esc, closes it only while
    /// nothing was typed (a new name, the confirmation); then only Cancel
    /// closes it.
    #[test]
    fn typed_names_are_kept_on_an_outside_click_or_esc() {
        use egui::accesskit::Role;
        struct Probe {
            dialog: PasteDialog,
            closed: bool,
        }
        let open = || {
            let mut h = Harness::builder().with_size(egui::vec2(900.0, 700.0)).build_ui_state(
                |ui, p: &mut Probe| {
                    crate::theme::install(ui.ctx());
                    if let Some(PasteAction::Close) = p.dialog.show(ui.ctx(), &target(false)) {
                        p.closed = true;
                    }
                },
                Probe { dialog: dialog(&["orders", "users"]), closed: false },
            );
            h.run_steps(3);
            h
        };
        let mut h = open();
        crate::test_support::click_outside(&mut h);
        assert!(h.state().closed, "nothing typed: a click outside closes it");
        let mut h = open();
        h.key_press(egui::Key::Escape);
        h.run_steps(2);
        assert!(h.state().closed, "nothing typed: Esc closes it");

        let mut h = open();
        h.query_all_by_role(Role::TextInput).next().expect("a name field").focus();
        h.run_steps(2);
        h.query_all_by_role(Role::TextInput).next().unwrap().type_text("_2");
        h.run_steps(2);
        assert_eq!(h.state().dialog.rows[0].target, "orders_2");
        crate::test_support::click_outside(&mut h);
        h.key_press(egui::Key::Escape);
        h.run_steps(2);
        assert!(!h.state().closed, "the typed name is kept");
        h.get_by_label("Cancel").click();
        h.run_steps(2);
        assert!(h.state().closed, "Cancel closes it");
    }

    /// Spaces around a new name (common when pasting one) don't count: the
    /// collection is created without them, a name that is only spaces is
    /// empty, and the confirmation for a Replace compares names without
    /// them on both sides.
    #[test]
    fn spaces_around_a_new_name_do_not_count() {
        let mut d = dialog(&["orders"]);
        d.rows[0].target = " orders_2 ".into();
        assert_eq!(d.ready(&target(false)), Ok(()));
        let Command::CopyCollections { items, .. } = d.build() else { panic!("a CopyCollections command") };
        assert_eq!(items[0].dst, Namespace::new("uat", "orders_2"));
        d.rows[0].target = "   ".into();
        assert!(d.ready(&target(false)).unwrap_err().contains("empty"));

        // "orders " lands on the existing orders: Append / Replace apply.
        let mut d = dialog(&["orders"]);
        d.set_existing(vec!["orders".into()]);
        d.rows[0].target = "orders ".into();
        assert!(d.exists(&d.rows[0]), "the existing orders");
        d.rows[0].mode = IfExists::Replace;
        assert!(d.ready(&target(false)).is_err(), "Replace waits for the name");
        d.typed = " orders ".into();
        assert_eq!(d.ready(&target(false)), Ok(()));
    }

    /// Milliseconds per frame of the Paste dialog for `n` collections into a
    /// database that already holds each of them (and as many others).
    fn paste_frame_ms(n: usize) -> f64 {
        let names: Vec<String> = (0..n).map(|i| format!("collection_{i:05}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut d = PasteDialog::new(&clip(&refs), conn(2), "uat".into());
        d.set_existing(names.iter().cloned().chain((0..n).map(|i| format!("other_{i:05}"))).collect());
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut frame = || {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 900.0))), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| _ = d.show(ui.ctx(), &target(false)));
            out.textures_delta.clear();
        };
        frame();
        frame();
        (0..5)
            .map(|_| {
                let t = std::time::Instant::now();
                for _ in 0..3 {
                    frame();
                }
                t.elapsed().as_secs_f64() * 1000.0 / 3.0
            })
            .fold(f64::INFINITY, f64::min)
    }

    /// Pasting 2,000 collections into a database of 4,000 costs about what
    /// 20 does per frame: names are looked up, not searched for, and only
    /// the rows in view are laid out.
    #[test]
    fn a_paste_of_2000_collections_stays_cheap() {
        let small = paste_frame_ms(20);
        let big = paste_frame_ms(2_000);
        assert!(big < small * 3.0 + 5.0, "{big:.1} ms per frame for 2,000 collections vs {small:.1} ms for 20");
    }

    /// Prints the Paste dialog's frame times (run with --release --ignored --nocapture).
    #[test]
    #[ignore = "prints timings"]
    fn report_paste_frame_times() {
        for n in [20, 2_000] {
            println!("Paste of {n} collections: {:.2} ms per frame", paste_frame_ms(n));
        }
    }

    /// With a long list the dialog has its full height from its first drawn
    /// frame on and keeps it: the list's scroll area is sized from the
    /// window, not from the room the dialog had the frame before.
    #[test]
    fn a_long_list_keeps_the_dialog_height() {
        let names: Vec<String> = (0..300).map(|i| format!("collection_{i:03}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        for (w, h) in [(1200.0, 900.0), (760.0, 600.0)] {
            let ctx = egui::Context::default();
            crate::theme::install(&ctx);
            let mut d = known(PasteDialog::new(&clip(&refs), conn(2), "uat".into()));
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w, h));
            let mut paste_button = || {
                let mut out = ctx.run_ui(egui::RawInput { screen_rect: Some(screen), ..Default::default() }, |ui| _ = d.show(ui.ctx(), &target(false)));
                out.textures_delta.clear();
                out.shapes.iter().find_map(|s| match &s.shape {
                    egui::Shape::Text(t) if t.galley.text() == "Paste" => Some(t.galley.rect.translate(t.pos.to_vec2())),
                    _ => None,
                })
            };
            // The first frame only measures the dialog.
            let first = (0..2).filter_map(|_| paste_button()).last().expect("Paste drawn");
            let later = (0..60).filter_map(|_| paste_button()).last().expect("Paste drawn");
            assert!((later.top() - first.top()).abs() < 1.0, "{w}x{h}: Paste moved from {first:?} to {later:?}");
            assert!(screen.contains_rect(later), "{w}x{h}: Paste on screen: {later:?}");
        }
    }

    /// With many rows the list scrolls, and a row further down is still
    /// edited, renamed and switched to Replace like the first.
    #[test]
    fn rows_further_down_the_list_work() {
        use egui::accesskit::Role;
        let names: Vec<String> = (0..200).map(|i| format!("c{i:03}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut d = PasteDialog::new(&clip(&refs), conn(2), "uat".into());
        d.set_existing(vec!["c150".into()]);
        let mut h = run(d, 3);
        assert!(h.query_by_label("Rename to c150_copy").is_none(), "far down: not laid out");
        // Scroll the list down to row 150.
        for _ in 0..40 {
            h.event(egui::Event::PointerMoved(egui::pos2(450.0, 250.0)));
            h.event(egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: egui::vec2(0.0, -120.0), phase: egui::TouchPhase::Move, modifiers: egui::Modifiers::NONE });
            h.run_steps(1);
            if h.query_by_label("Rename to c150_copy").is_some() {
                break;
            }
        }
        h.run_steps(10);
        h.get_by_label("Replace").click();
        h.run_steps(2);
        assert_eq!(h.state().rows[150].mode, IfExists::Replace);
        h.get_by_label("Rename to c150_copy").click();
        h.run_steps(2);
        assert_eq!(h.state().rows[150].target, "c150_copy");
        assert_eq!(h.state().rows[150].mode, IfExists::Append(OnDuplicateId::Skip), "a free name has nothing to replace");
        let fields = h.query_all_by_role(Role::TextInput).count();
        assert!(fields < 40, "only the rows in view have a field: {fields}");
    }

    #[test]
    fn read_only_target_is_refused() {
        let d = dialog(&["orders"]);
        assert_eq!(d.ready(&TargetInfo { name: "PROD", color: None, read_only: true }), Err("PROD is read-only".to_string()));
    }

    #[test]
    fn bad_or_clashing_names_are_refused() {
        for bad in ["", "a$b", "system.x"] {
            let mut d = dialog(&["orders"]);
            d.rows[0].target = bad.into();
            let err = d.ready(&target(false)).unwrap_err();
            assert!(err.contains("orders"), "{bad:?}: the message names the row: {err}");
        }
        let mut d = dialog(&["orders", "users"]);
        d.rows[1].target = "orders".into();
        let err = d.ready(&target(false)).unwrap_err();
        assert!(err.contains("orders") && err.contains("users"), "{err}");
    }

    /// `a → b` with `b → b2` in the same database writes into b before it is
    /// copied, whichever mode: refused. Another database has no such clash.
    #[test]
    fn writing_into_another_rows_source_is_refused() {
        for mode in [IfExists::Replace, IfExists::Append(OnDuplicateId::Skip), IfExists::Append(OnDuplicateId::Overwrite)] {
            let mut d = known(PasteDialog::new(&clip(&["a", "b"]), conn(1), "sit".into()));
            d.set_existing(vec!["a".into(), "b".into()]);
            d.rows[0].target = "b".into();
            d.rows[1].target = "b2".into();
            d.rows[0].mode = mode;
            d.typed = "b".into();
            assert_eq!(d.ready(&target(false)), Err("a would overwrite b, which is also being pasted".to_string()), "{mode:?}");
        }
        let mut d = known(PasteDialog::new(&clip(&["a", "b"]), conn(1), "uat".into()));
        d.rows[0].target = "b".into();
        d.rows[1].target = "b2".into();
        assert_eq!(d.ready(&target(false)), Ok(()), "another database: b is not the source");
        let mut d = known(PasteDialog::new(&clip(&["a", "b"]), conn(2), "sit".into()));
        d.rows[0].target = "b".into();
        d.rows[1].target = "b2".into();
        assert_eq!(d.ready(&target(false)), Ok(()), "another connection: b is not the source");
    }

    #[test]
    fn pasting_into_itself_is_refused() {
        let d = known(PasteDialog::new(&clip(&["orders"]), conn(1), "sit".into()));
        assert_eq!(d.ready(&target(false)), Err("orders can't be pasted into itself".to_string()));
        // Another database on the same connection is a copy, and so is renaming.
        assert_eq!(known(PasteDialog::new(&clip(&["orders"]), conn(1), "uat".into())).ready(&target(false)), Ok(()));
        let mut d = known(PasteDialog::new(&clip(&["orders"]), conn(1), "sit".into()));
        d.rows[0].target = "orders_2".into();
        assert_eq!(d.ready(&target(false)), Ok(()));
    }
}
