//! The view's keyboard shortcuts.

use egui::{Event, Key, Modifiers, Ui};
use rongo_core::format::Style;

use super::json_text::Build;
use super::{Mode, ResultView, ViewAction};
use crate::net::Net;

impl ResultView {
    pub(super) fn keyboard(&mut self, ui: &mut Ui, net: &mut Net, actions: &mut Vec<ViewAction>) {
        if ui.memory(|m| m.focused().is_some()) {
            return;
        }
        // ⌘C and ⌘V arrive as Copy/Paste events, not as key presses.
        // ⌥⌘C belongs to the tree (Copy Name).
        let alt = ui.input(|i| i.modifiers.alt);
        for e in ui.input(|i| i.events.clone()) {
            match e {
                // The view's text writes whole doubles as `5.0`, which pastes
                // back as an integer: the clipboard gets the editable form,
                // built off the UI thread like the view's text (a page like
                // the owner's takes a moment), and copied once it is done.
                Event::Copy if self.json.all && !alt && !self.rows.is_empty() => {
                    self.json.copy = Some(Build::start(self.rows.clone(), self.paging.offset as usize, Style::Editable, ui.ctx().clone()));
                }
                Event::Copy if !self.selection.selected.is_empty() && !alt => ui.ctx().copy_text(self.copy_text()),
                Event::Paste(text) => actions.extend(self.paste(text)),
                _ => {}
            }
        }
        let key = |ui: &mut Ui, m: Modifiers, k: Key| ui.input_mut(|i| i.consume_key(m, k));
        if key(ui, Modifiers::COMMAND | Modifiers::SHIFT, Key::R) {
            if let [o] = self.selected_origins().as_slice() {
                self.refresh_doc(net, *o);
            }
        } else if key(ui, Modifiers::COMMAND, Key::R) {
            self.fetch(net);
        }
        if key(ui, Modifiers::COMMAND, Key::A) {
            if self.mode == Mode::Json {
                // Lines off screen aren't laid out, so egui can't select them:
                // select-all is ours, and a drag selection would fight it.
                self.json.all = true;
                ui.ctx().with_plugin(|s: &mut egui::text_selection::LabelSelectionState| s.clear_selection());
            } else {
                self.selection.selected = (0..self.resolved().len()).collect();
                self.selection.anchor = Some(0);
            }
        }
        if self.json.all && ui.input(|i| i.key_pressed(Key::Escape)) {
            self.json.all = false;
        } else if key(ui, Modifiers::NONE, Key::Escape) && !self.drill.is_empty() {
            let mut d = self.drill.clone();
            d.pop();
            self.set_drill(d);
        }
        if self.editable().is_none() {
            if key(ui, Modifiers::NONE, Key::F3)
                && let Some(d) = self.single_selected().cloned()
            {
                actions.push(ViewAction::View { doc: d });
            }
            // A locked find view still steps into values; a static result's
            // keys stay as they were.
            if self.read_only && self.source_ns().is_some() {
                self.step_keys(ui);
            }
            return;
        }
        if key(ui, Modifiers::COMMAND, Key::D) {
            actions.extend(self.insert());
        }
        if key(ui, Modifiers::COMMAND, Key::U) {
            actions.extend(self.bulk(false, self.selected_ids(), self.selected_field()));
        }
        if key(ui, Modifiers::NONE, Key::F8) {
            actions.extend(self.bulk(true, self.selected_ids(), None));
        }
        if self.selection.selected.is_empty() {
            return;
        }
        if key(ui, Modifiers::SHIFT, Key::Backspace) || key(ui, Modifiers::NONE, Key::Delete) {
            let ids = self.selected_ids();
            if !ids.is_empty() {
                actions.extend(self.delete(ids));
            }
        }
        // The selected document is copied only for the key pressed.
        if key(ui, Modifiers::NONE, Key::F3)
            && let Some(d) = self.single_selected().cloned()
        {
            actions.push(ViewAction::View { doc: d });
        }
        if key(ui, Modifiers::COMMAND, Key::J)
            && let Some(d) = self.single_selected().cloned()
        {
            actions.extend(self.edit(d));
        }
        self.step_keys(ui);
    }

    /// ⏎ and ⇧⏎ step into the selected cell (of the anchor row, `current_row`).
    fn step_keys(&mut self, ui: &mut Ui) {
        let key = |ui: &mut Ui, m: Modifiers, k: Key| ui.input_mut(|i| i.consume_key(m, k));
        let row = self.current_row();
        if let (Some(row), Some(col)) = (row, self.selection.selected_col.clone()) {
            if key(ui, Modifiers::SHIFT, Key::Enter) {
                self.step_into(row, &col, true);
            } else if key(ui, Modifiers::NONE, Key::Enter) {
                self.step_into(row, &col, false);
            }
        }
    }
}
