//! The right-click menus, and the texts their Copy items make.

use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use rongo_core::bson::{Bson, Document, doc};
use rongo_core::format::{self, Style};
use rongo_core::transfer;

use super::drill::{Row, Rows, compute_columns, join};
use super::{ResultView, Source, ViewAction};
use crate::net::Net;
use crate::{shortcut, theme};

/// The hover text of anything a locked connection turns off.
pub const LOCKED: &str = "Read-only connection";

/// `r` (a disabled widget) says why on hover when the view is locked.
pub(super) fn locked_why(locked: bool, r: egui::Response) -> egui::Response {
    if locked { r.on_disabled_hover_text(LOCKED) } else { r }
}

/// The cell a right-click menu is for.
pub(super) struct CellCtx<'a> {
    /// Its row at the drill level, and the row of the document it is in.
    pub(super) row: usize,
    pub(super) origin: usize,
    /// Its column (`None` in the Tree view).
    pub(super) col: Option<&'a str>,
    /// The row's dotted path from the document root.
    pub(super) path: &'a str,
    /// Its value, if the row has one in that column.
    pub(super) cell: Option<&'a Bson>,
}

/// What a cell's menu works out once per frame, for its submenus.
struct CellMenu {
    /// The row's document and its `_id`.
    doc: Option<Document>,
    id: Option<Bson>,
    /// The cell's dotted path from the document root (None: the root).
    field_path: Option<String>,
    /// The view may write (`editable`); `can_write`: and the document has
    /// an `_id` to write it by.
    can: bool,
    can_write: bool,
    locked: bool,
}

impl ResultView {
    /// Menu for an empty result: inserting and pasting still make sense.
    pub(super) fn empty_menu(&self, ui: &mut Ui, actions: &mut Vec<ViewAction>) {
        if self.source_ns().is_none() {
            return;
        }
        let locked = self.read_only;
        if locked_why(locked, ui.add_enabled(!locked, egui::Button::new("Insert Document…").shortcut_text(shortcut::label("⌘D")))).clicked()
            && let Some(a) = self.insert()
        {
            actions.push(a);
            ui.close();
        }
        if locked_why(locked, ui.add_enabled(!locked, egui::Button::new("Paste Document(s)…").shortcut_text(shortcut::label("⌘V")))).clicked() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestPaste);
            ui.close();
        }
    }

    /// Right-click under the rows: the menu of the anchor row (`current_row`;
    /// its selected cell, if any), or what makes sense with no row.
    pub(super) fn background_menu(&mut self, ui: &mut Ui, net: &mut Net, rows: &Rows, actions: &mut Vec<ViewAction>) {
        if let Some(i) = self.current_row()
            && let Some(row @ Row { origin, .. }) = rows.get(i)
        {
            let col = self.selected_column().map(str::to_string);
            let cell = col.as_deref().and_then(|c| rows.cell(row, c));
            self.cell_menu(ui, net, CellCtx { row: i, origin, col: col.as_deref(), path: &rows.path(i), cell }, actions);
            return;
        }
        self.empty_menu(ui, actions);
        ui.separator();
        if ui.add(egui::Button::new("Refresh View").shortcut_text(shortcut::label("⌘R"))).clicked() {
            self.fetch(net);
            ui.close();
        }
        let can = self.editable().is_some();
        let locked = self.read_only;
        if locked_why(locked, ui.add_enabled(can, egui::Button::new(format!("{}  Update Dialog…", icon::PENCIL_LINE)).shortcut_text(shortcut::label("⌘U")))).clicked()
            && let Some(a) = self.bulk(false, Vec::new(), None)
        {
            actions.push(a);
            ui.close();
        }
        if locked_why(locked, ui.add_enabled(can, egui::Button::new(RichText::new(format!("{}  Delete Dialog…", icon::BROOM)).color(theme::ERROR)).shortcut_text("F8"))).clicked()
            && let Some(a) = self.bulk(true, Vec::new(), None)
        {
            actions.push(a);
            ui.close();
        }
        ui.separator();
        if ui.add_enabled(self.source_ns().is_some(), egui::Button::new(format!("{}  Export Documents…", icon::EXPORT))).clicked()
            && let Source::Find { conn, spec, .. } = &self.source
        {
            actions.push(ViewAction::Export { conn: *conn, spec: spec.clone() });
            ui.close();
        }
    }

    /// The right-click menu for a cell (`col` is `None` in the Tree view).
    pub(super) fn cell_menu(&mut self, ui: &mut Ui, net: &mut Net, at: CellCtx, actions: &mut Vec<ViewAction>) {
        let CellCtx { row, origin, col, path: path_prefix, cell } = at;
        // Opened on a row of a page that has since changed (another page
        // landed, the rows moved): the row may hold another document now.
        if self.menu_gen != Some(self.rows_gen) {
            ui.close();
            return;
        }
        let doc = self.rows.doc(origin).cloned();
        let id = doc.as_ref().and_then(|d| d.get("_id")).cloned();
        let can = self.editable().is_some();
        // `DOC_ID` is the document's `_id`, not a field of the row's value.
        let field_path = col.filter(|c| !self.is_doc_id(c)).map(|c| if c == "(value)" { path_prefix.to_string() } else { join(path_prefix, c) }).filter(|p| !p.is_empty());
        let can_write = can && id.is_some();
        let locked = self.read_only;
        let m = CellMenu { doc, id, field_path, can, can_write, locked };

        ui.menu_button(format!("{}  Field", icon::TEXT_COLUMNS), |ui| self.field_menu(ui, &m, cell, actions));
        ui.menu_button(format!("{}  Document", icon::FILE_TEXT), |ui| self.document_menu(ui, &m, actions));
        if let Some(c) = col {
            ui.menu_button(format!("{}  Column", icon::COLUMNS), |ui| self.column_menu(ui, c, actions));
        }
        ui.menu_button(format!("{}  Copy", icon::CLIPBOARD_TEXT), |ui| self.copy_menu(ui, &m, cell));
        let CellMenu { id, field_path, can, locked, .. } = m;
        if locked_why(locked, ui.add_enabled(can, egui::Button::new(format!("{}  Paste Document(s)…", icon::CLIPBOARD)).shortcut_text(shortcut::label("⌘V")))).clicked() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestPaste);
            ui.close();
        }
        ui.separator();
        if let Some(c) = col.filter(|c| *c != "(value)") {
            let can_step = !self.is_doc_id(c) && matches!(cell, Some(Bson::Document(_) | Bson::Array(_)));
            if ui.add_enabled(can_step, egui::Button::new(format!("{}  Step Into Column", icon::TABLE)).shortcut_text(shortcut::label("↩"))).clicked() {
                self.step_into(row, c, false);
                ui.close();
            }
            if ui.add_enabled(can_step, egui::Button::new(format!("{}  Step Into Cell", icon::TABLE)).shortcut_text(shortcut::label("⇧↩"))).clicked() {
                self.step_into(row, c, true);
                ui.close();
            }
            ui.separator();
        }
        if ui.add(egui::Button::new("Refresh View").shortcut_text(shortcut::label("⌘R"))).clicked() {
            self.fetch(net);
            ui.close();
        }
        if ui.add_enabled(self.source_ns().is_some() && id.is_some(), egui::Button::new("Refresh Document").shortcut_text(shortcut::label("⇧⌘R"))).clicked() {
            self.refresh_doc(net, origin);
            ui.close();
        }
        ui.separator();
        if locked_why(locked, ui.add_enabled(can, egui::Button::new(format!("{}  Update Dialog…", icon::PENCIL_LINE)).shortcut_text(shortcut::label("⌘U")))).clicked()
            && let Some(a) = self.bulk(false, self.selected_ids(), field_path.clone().filter(|p| p != "_id").zip(cell.cloned()))
        {
            actions.push(a);
            ui.close();
        }
        if locked_why(locked, ui.add_enabled(can, egui::Button::new(RichText::new(format!("{}  Delete Dialog…", icon::BROOM)).color(theme::ERROR)).shortcut_text("F8"))).clicked()
            && let Some(a) = self.bulk(true, self.selected_ids(), None)
        {
            actions.push(a);
            ui.close();
        }
        ui.separator();
        if ui.add_enabled(self.source_ns().is_some(), egui::Button::new(format!("{}  Export Documents…", icon::EXPORT))).clicked()
            && let Source::Find { conn, spec, .. } = &self.source
        {
            actions.push(ViewAction::Export { conn: *conn, spec: spec.clone() });
            ui.close();
        }
    }

    /// The cell menu's Field submenu: Edit Value, Set to null, Remove Field.
    fn field_menu(&self, ui: &mut Ui, m: &CellMenu, cell: Option<&Bson>, actions: &mut Vec<ViewAction>) {
        let CellMenu { ref id, ref field_path, can_write, locked, .. } = *m;
        let enabled = can_write && field_path.as_deref().is_some_and(|p| p != "_id");
        if locked_why(locked, ui.add_enabled(enabled, egui::Button::new("Edit Value…"))).clicked()
            && let (Some(id), Some(path)) = (id.clone(), field_path.clone())
            && let Some(a) = self.edit_value(id, path, cell.cloned().unwrap_or(Bson::Null))
        {
            actions.push(a);
            ui.close();
        }
        if locked_why(locked, ui.add_enabled(enabled, egui::Button::new("Set to null"))).clicked()
            && let (Some(id), Some(path)) = (id.clone(), field_path.clone())
            && let Some(a) = self.update_one(id, doc! { "$set": { path: Bson::Null } })
        {
            actions.push(a);
            ui.close();
        }
        if locked_why(locked, ui.add_enabled(enabled && cell.is_some(), egui::Button::new(RichText::new("Remove Field").color(theme::ERROR)))).clicked()
            && let (Some(id), Some(path)) = (id.clone(), field_path.clone())
            && let Some(a) = self.update_one(id, doc! { "$unset": { path: "" } })
        {
            actions.push(a);
            ui.close();
        }
    }

    /// The cell menu's Document submenu: insert, delete, view, edit, clone.
    fn document_menu(&self, ui: &mut Ui, m: &CellMenu, actions: &mut Vec<ViewAction>) {
        let CellMenu { ref doc, can, can_write, locked, .. } = *m;
        if locked_why(locked, ui.add_enabled(can, egui::Button::new("Insert Document…").shortcut_text(shortcut::label("⌘D")))).clicked()
            && let Some(a) = self.insert()
        {
            actions.push(a);
            ui.close();
        }
        if locked_why(locked, ui.add_enabled(can && !self.selection.selected.is_empty(), egui::Button::new("Delete Document…").shortcut_text(shortcut::label("⇧⌫")))).clicked()
            && let Some(a) = self.delete(self.selected_ids())
        {
            actions.push(a);
            ui.close();
        }
        if ui.add_enabled(doc.is_some(), egui::Button::new("View Document (JSON)…").shortcut_text("F3")).clicked()
            && let Some(d) = doc.clone()
        {
            actions.push(ViewAction::View { doc: d });
            ui.close();
        }
        if locked_why(locked, ui.add_enabled(can_write, egui::Button::new("Edit Document (JSON)…").shortcut_text(shortcut::label("⌘J")))).clicked()
            && let Some(a) = doc.clone().and_then(|d| self.edit(d))
        {
            actions.push(a);
            ui.close();
        }
        if locked_why(locked, ui.add_enabled(can && doc.is_some(), egui::Button::new("Clone Document…"))).clicked()
            && let Some(a) = doc.clone().and_then(|d| self.clone_doc(d))
        {
            actions.push(a);
            ui.close();
        }
    }

    /// The cell menu's Column submenu for column `c`: sorting, hiding and
    /// the column order. `DOC_ID` is neither sorted by nor hidden.
    fn column_menu(&mut self, ui: &mut Ui, c: &str, actions: &mut Vec<ViewAction>) {
        let fixed = self.is_doc_id(c);
        let sort_path = self.shared_prefix().filter(|_| c != "(value)" && !fixed).map(|p| join(&p, c));
        let can_sort = sort_path.is_some();
        // A header click sorts the page on screen; for a query, the
        // menu sorts the whole collection, on the server.
        let query = matches!(self.source, Source::Find { .. });
        let (up, down) = if query { ("Sort Collection Ascending", "Sort Collection Descending") } else { ("Sort Ascending", "Sort Descending") };
        let why = "Runs the query again, sorted by this field. Click the column header to sort only the rows shown.";
        for (label, glyph, dir) in [(up, icon::SORT_ASCENDING, 1), (down, icon::SORT_DESCENDING, -1)] {
            let mut item = ui.add_enabled(can_sort, egui::Button::new(format!("{glyph}  {label}")));
            if query {
                item = item.on_hover_text(why);
            }
            if item.clicked()
                && let Some(p) = sort_path.clone()
            {
                if query { self.sort_query(p, dir, actions) } else { self.sort_by(p, Some(dir)) }
                ui.close();
            }
        }
        ui.separator();
        if ui.add_enabled(!fixed, egui::Button::new("Hide Column")).clicked() {
            self.hidden_mut().insert(c.to_string());
            // The selected cell was there: it is no longer on screen.
            if self.selection.selected_col.as_deref() == Some(c) {
                self.selection.selected_col = None;
            }
            ui.close();
        }
        if ui.add_enabled(!self.hidden().is_empty(), egui::Button::new("Show All Columns")).clicked() {
            self.hidden_mut().clear();
            ui.close();
        }
        let key = self.order_key();
        if ui.add_enabled(self.cols.order.contains_key(&key), egui::Button::new("Reset Column Order")).on_hover_text("Drag a column header to move it").clicked() {
            self.cols.order.remove(&key);
            ui.close();
        }
    }

    /// The cell menu's Copy submenu.
    fn copy_menu(&self, ui: &mut Ui, m: &CellMenu, cell: Option<&Bson>) {
        let CellMenu { ref id, ref field_path, .. } = *m;
        if ui.add_enabled(cell.is_some(), egui::Button::new("Copy Value")).clicked()
            && let Some(v) = cell
        {
            let text = match v {
                Bson::String(s) => s.clone(),
                other => format::value_to_shell_pretty(other, Style::Display),
            };
            ui.ctx().copy_text(text);
            ui.close();
        }
        if ui.add_enabled(field_path.is_some(), egui::Button::new("Copy Field Path")).clicked()
            && let Some(p) = &field_path
        {
            ui.ctx().copy_text(p.clone());
            ui.close();
        }
        if ui.add(egui::Button::new("Copy Document(s)").shortcut_text(shortcut::label("⌘C"))).clicked() {
            ui.ctx().copy_text(self.copy_text());
            ui.close();
        }
        if ui.add_enabled(id.is_some(), egui::Button::new("Copy _id")).clicked()
            && let Some(i) = &id
        {
            // Reads back as the same `_id`: a whole double stays a double.
            ui.ctx().copy_text(format::to_shell(i, Style::Editable));
            ui.close();
        }
        ui.separator();
        let any = !self.selection.selected.is_empty();
        for (label, sep, header) in [("Selected as CSV", ',', true), ("Selected as CSV (no header)", ',', false), ("Selected for Excel", '\t', true), ("Selected for Excel (no header)", '\t', false)] {
            if ui.add_enabled(any, egui::Button::new(label)).clicked() {
                ui.ctx().copy_text(self.selection_text(sep, header));
                ui.close();
            }
        }
    }

    /// Selected documents as shell text: one document, or an array of them, so
    /// pasting the text back yields the same documents. Editable, so a whole
    /// double stays a double (`Double(5)`, where reading shows `5.0`).
    ///
    /// Each document is parsed, formatted, written out and dropped in turn
    /// (drilled in, the rows are parsed already), so ⌘A ⌘C neither keeps
    /// the page parsed nor copies it into one array first. The array is
    /// written as `value_to_shell_pretty` writes one: each document a level
    /// in, which only indents its lines (a string never holds a raw line
    /// break: it is written escaped).
    pub(super) fn copy_text(&self) -> String {
        let one = |o: usize| -> Option<String> {
            if self.drill.is_empty() {
                let d = Document::try_from(self.rows.raw_doc(o)?.as_ref()).ok()?;
                Some(format::doc_to_shell(&d, Style::Editable))
            } else {
                self.rows.doc(o).map(|d| format::doc_to_shell(d, Style::Editable))
            }
        };
        let mut texts = self.selected_origins().into_iter().filter_map(one);
        let Some(first) = texts.next() else { return String::new() };
        let Some(second) = texts.next() else { return first };
        let mut out = String::from("[");
        for (i, text) in [first, second].into_iter().chain(texts).enumerate() {
            if i > 0 {
                out.push(',');
            }
            for line in text.split('\n') {
                out.push_str("\n    ");
                out.push_str(line);
            }
        }
        out.push_str("\n]");
        out
    }

    /// The selected rows as delimited text, in the columns the table shows
    /// (drill level, `DOC_ID` first, column order, hidden columns applied),
    /// one line per row in table order; `header` puts the column names
    /// first. Cells are written as in the CSV export.
    pub(super) fn selection_text(&self, sep: char, header: bool) -> String {
        let rows = self.resolved();
        let all = self.cols.cache.clone().unwrap_or_else(|| compute_columns(&rows));
        let columns = self.shown_columns(all);
        let mut lines = Vec::new();
        if header {
            // Field names come from the data too: defuse them as the export does.
            let names: Vec<String> = columns.iter().map(|c| transfer::defuse_formula(c)).collect();
            lines.push(transfer::delimited_line(names.iter().map(String::as_str), sep));
        }
        for i in &self.selection.selected {
            let Some(row) = rows.get(*i) else { continue };
            let cells: Vec<String> = columns.iter().map(|c| transfer::csv_value(rows.cell(row, c))).collect();
            lines.push(transfer::delimited_line(cells.iter().map(String::as_str), sep));
        }
        lines.join("\n")
    }
}
