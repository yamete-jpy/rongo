//! Drawing the window: the toolbar, the tab bar, the Welcome tab, the active tab, toasts, dialogs and shortcuts.

use std::time::Instant;

use egui::{Align, Align2, Key, Layout, Modifiers, RichText, Ui};
use egui_phosphor::regular as icon;
use rongo_core::{FindSpec, Namespace};

use crate::dialogs::{ConfirmResult, DocAction};
use crate::paste::TargetInfo;
use crate::sidebar::{self, TreeAction};
use crate::settings::{ChangeOutcome, HistoryView};
use crate::shortcut;
use crate::tools::{ToolAction, ToolDialog};
use crate::tabs::{Tab, TabKind};
use crate::theme;
use crate::vault_ui;

use super::RongoApp;

impl RongoApp {
    pub(super) fn toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().button_padding = egui::vec2(10.0, 5.0);
            // What the menu picked, done once it is drawn (it reads the saved profiles in place).
            let (mut pick, mut demo) = (None, false);
            ui.menu_button(RichText::new(format!("{}  Connect  {}", icon::PLUGS_CONNECTED, icon::CARET_DOWN)).size(14.0), |ui| {
                if ui.button(format!("{}  Connection Manager…", icon::LIST)).clicked() {
                    self.open_connection_manager(false);
                    ui.close();
                }
                if ui.button(format!("{}  New connection…", icon::PLUS)).clicked() {
                    self.open_connection_manager(true);
                    ui.close();
                }
                ui.separator();
                let profiles = self.store.as_ref().map_or(&[][..], |s| s.profiles());
                for p in profiles {
                    let open = self.conns.contains_key(&p.conn_id());
                    let ico = RichText::new(if open { icon::PLUGS_CONNECTED } else { icon::DATABASE });
                    let ico = match theme::conn_color(p.color.as_deref()) {
                        Some(c) => ico.color(c),
                        None => ico,
                    };
                    let mut label = egui::text::LayoutJob::default();
                    ico.append_to(&mut label, ui.style(), egui::FontSelection::Default, Align::Center);
                    RichText::new(format!("  {}", p.name)).append_to(&mut label, ui.style(), egui::FontSelection::Default, Align::Center);
                    if ui.button(label).on_hover_text(p.summary()).clicked() {
                        pick = Some(p.id);
                        ui.close();
                    }
                }
                if !profiles.is_empty() {
                    ui.separator();
                }
                if ui.button(format!("{}  Demo server (in-memory)", icon::FLASK)).clicked() {
                    demo = true;
                    ui.close();
                }
            });
            if let Some(p) = pick.and_then(|id| self.store.as_ref()?.get(id).cloned()) {
                self.connect(p);
            }
            if demo {
                self.open_demo();
            }
            if ui.button(RichText::new(format!("{}  IntelliShell", icon::TERMINAL_WINDOW)).size(14.0)).clicked() {
                self.open_shell_for_selection();
            }
            ui.add_space(8.0);
            if ui.button(RichText::new(format!("{}  Export", icon::EXPORT)).size(14.0)).on_hover_text("Export the open or selected collection").clicked() {
                match self.target_collection() {
                    Some((conn, db, Some(coll), spec)) => {
                        let from_view = spec.is_some();
                        let spec = spec.unwrap_or_else(|| FindSpec::all(Namespace::new(db, coll), 50));
                        self.tool = Some(ToolDialog::export(conn, spec, from_view));
                    }
                    _ => self.toast("Select a collection in the tree first", true),
                }
            }
            let import_locked = self.target_conn().and_then(|c| self.conns.get(&c)).is_some_and(|c| c.profile.read_only);
            let import = ui.add_enabled(!import_locked, egui::Button::new(RichText::new(format!("{}  Import", icon::DOWNLOAD_SIMPLE)).size(14.0)));
            if import.on_hover_text("Import a JSON or CSV file").on_disabled_hover_text(crate::result_view::LOCKED).clicked() {
                match self.target_collection() {
                    Some((conn, db, coll, _)) => self.tool = Some(ToolDialog::import(conn, db, coll.unwrap_or_default())),
                    None => self.toast("Select a database or collection in the tree first", true),
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button(RichText::new(format!("{}  Settings", icon::GEAR)).size(14.0)).on_hover_text(shortcut::label("Settings (⌘,)")).clicked() {
                    self.open_settings();
                }
                ui.weak(format!("rongo {}", env!("CARGO_PKG_VERSION")));
            });
        });
    }

    pub(super) fn tab_bar(&mut self, ui: &mut Ui) {
        // Nothing below changes `self` until the bar is drawn (clicks are
        // kept in `pick`/`close`), so it reads the connections in place.
        let conns = &self.conns;
        // A tab's title, with a lock after it when its connection is read-only.
        let title_of = |tab: &Tab| {
            let title = tab.title(|c| conns.get(&c).map_or_else(|| "?".into(), |c| c.profile.name.clone()));
            if tab.conn().and_then(|c| conns.get(&c)).is_some_and(|c| c.profile.read_only) { format!("{title}  {}", icon::LOCK_SIMPLE) } else { title }
        };
        let mut close = None;
        let mut scrolled = None;
        let mut pick = None;
        ui.horizontal(|ui| {
        // Room on the right for the list of every tab, when they overflow.
        let list_w = 30.0;
        let bar = egui::ScrollArea::horizontal().id_salt("tabbar").max_width(ui.available_width() - list_w).show(ui, |ui| {
            ui.horizontal(|ui| {
                for (i, tab) in self.tabs.iter().enumerate() {
                    let title = title_of(tab);
                    let active = i == self.active_tab;
                    let fill = if active { theme::BG } else { theme::PANEL };
                    let tag = tab.conn().and_then(|c| conns.get(&c)).and_then(|c| theme::conn_color(c.profile.color.as_deref()));
                    let frame = ui.push_id(tab.id, |ui| egui::Frame::new().fill(fill).corner_radius(egui::CornerRadius { nw: 5, ne: 5, sw: 0, se: 0 }).inner_margin(egui::Margin::symmetric(8, 4)).show(ui, |ui| {
                        let color = if active { theme::TEXT } else { theme::MUTED };
                        let r = ui.add(egui::Label::new(RichText::new(&title).color(color)).sense(egui::Sense::click()).selectable(false));
                        if r.clicked() {
                            pick = Some(i);
                        }
                        if r.middle_clicked() {
                            close = Some(i);
                        }
                        if ui.add(egui::Button::new(RichText::new(icon::X).size(11.0).color(theme::MUTED)).frame(false)).clicked() {
                            close = Some(i);
                        }
                    }));
                    // The connection's colour along the top of the tab.
                    if let Some(c) = tag {
                        let r = frame.inner.response.rect;
                        ui.painter().rect_filled(egui::Rect::from_min_size(r.left_top(), egui::vec2(r.width(), 3.0)), egui::CornerRadius { nw: 5, ne: 5, sw: 0, se: 0 }, c);
                    }
                    // A newly active tab (just opened, or picked from the
                    // list) scrolls into view.
                    if active && tab.id != self.shown_tab {
                        frame.response.scroll_to_me(None);
                        scrolled = Some(tab.id);
                    }
                }
            });
        });
        // Tabs off the edge: ▾ lists them all.
        if bar.content_size.x > bar.inner_rect.width() + 1.0 {
            ui.menu_button(icon::CARET_DOWN, |ui| {
                for (i, tab) in self.tabs.iter().enumerate() {
                    let title = title_of(tab);
                    if ui.selectable_label(i == self.active_tab, title).clicked() {
                        pick = Some(i);
                        ui.close();
                    }
                }
            })
            .response
            .on_hover_text("All tabs");
        }
        });
        if let Some(id) = scrolled {
            self.shown_tab = id;
        }
        if let Some(i) = pick {
            self.active_tab = i;
        }
        if let Some(i) = close {
            self.close_tab(i);
        }
    }

    fn welcome(&mut self, ui: &mut Ui) {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(icon::LEAF).size(48.0).color(theme::OK));
            ui.heading("rongo");
            ui.weak("A MongoDB client in Rust");
            ui.add_space(20.0);
            if ui.button(RichText::new(format!("{}  Connect…", icon::PLUGS_CONNECTED)).size(15.0)).clicked() {
                self.open_connection_manager(false);
            }
            if ui.button(RichText::new(format!("{}  New connection", icon::PLUS)).size(15.0)).clicked() {
                self.open_connection_manager(true);
            }
            if ui.button(RichText::new(format!("{}  Try the demo server", icon::FLASK)).size(15.0)).clicked() {
                self.open_demo();
            }
            let profiles = self.store.as_ref().map_or(&[][..], |s| s.profiles());
            let mut pick = None;
            if !profiles.is_empty() {
                ui.add_space(20.0);
                ui.label(RichText::new("Saved connections").strong());
                for p in profiles.iter().take(8) {
                    if ui.link(format!("{}  {}", icon::DATABASE, p.name)).on_hover_text(p.summary()).clicked() {
                        pick = Some(p.clone());
                    }
                }
            }
            if let Some(p) = pick {
                self.connect(p);
            }
            ui.add_space(20.0);
            ui.weak(shortcut::label("Double-click a collection to browse it · ⌘↩ runs the shell · ⌘W closes a tab"));
        });
    }

    pub(super) fn active_tab_ui(&mut self, ui: &mut Ui) {
        let keys_enabled = !self.modal_open();
        let tz = self.settings.timezone;
        let writing = match self.tabs.get(self.active_tab).map(|t| &t.kind) {
            Some(TabKind::History(h)) => self.history_busy(Some(h.conn.0), Some(&h.ns)),
            _ => false,
        };
        let Some(tab) = self.tabs.get_mut(self.active_tab) else { return };
        let tab_id = tab.id;
        // A view is told its connection's lock before it draws.
        let locked = tab.conn().and_then(|c| self.conns.get(&c)).is_some_and(|c| c.profile.read_only);
        for v in tab.views_mut() {
            v.read_only = locked;
        }
        let mut net = self.requests.net(keys_enabled);
        let actions = match &mut tab.kind {
            TabKind::Welcome => {
                self.welcome(ui);
                return;
            }
            TabKind::Collection(c) => {
                let conn = self.conns.get(&c.conn);
                c.ui(ui, &mut net, conn)
            }
            TabKind::Shell(s) => {
                let conn = self.conns.get(&s.conn);
                let core = net.core;
                let cid = s.conn;
                s.ui(ui, &mut net, conn, || core.session(cid), &self.waker)
            }
            TabKind::Info { conn, view } => {
                sidebar::breadcrumb(ui, self.conns.get(conn), "admin");
                view.ui(ui, &mut net)
            }
            TabKind::Indexes(it) => {
                let conn = self.conns.get(&it.conn);
                let acts = it.ui(ui, &mut net, conn);
                for a in acts {
                    self.index_action(a);
                }
                return;
            }
            TabKind::History(h) => {
                let conn = self.conns.get(&h.conn);
                h.writing = writing;
                let acts = h.ui(ui, &mut net, conn, tz);
                for a in acts {
                    self.history_action(tab_id, a);
                }
                return;
            }
        };
        for (view, a) in actions {
            self.view_action(view, a);
        }
    }

    pub(super) fn toasts(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        self.toasts.retain(|t| t.until > now);
        if self.toasts.is_empty() {
            return;
        }
        let next = self.toasts.iter().map(|t| t.until).min().unwrap_or(now);
        ctx.request_repaint_after(next.saturating_duration_since(now));
        egui::Area::new(egui::Id::new("toasts")).anchor(Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -16.0)).order(egui::Order::Tooltip).show(ctx, |ui| {
            let mut dismiss = None;
            for (i, t) in self.toasts.iter().enumerate() {
                let (ico, color) = if t.error { (icon::WARNING_CIRCLE, theme::ERROR) } else { (icon::CHECK_CIRCLE, theme::OK) };
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    // A plain row never wraps: a failed connect's paragraph
                    // was one line wider than the window, its start cut off.
                    ui.horizontal_top(|ui| {
                        ui.label(RichText::new(ico).color(color));
                        let text_color = ui.visuals().text_color();
                        theme::clipped_text(ui, &t.text, text_color, 4, 380.0);
                        if ui.small_button(icon::X).clicked() {
                            dismiss = Some(i);
                        }
                    });
                });
            }
            if let Some(i) = dismiss {
                self.toasts.remove(i);
            }
        });
    }

    pub(super) fn dialogs(&mut self, ctx: &egui::Context) {
        if let (Some(d), Some(vault)) = (&mut self.connect_dialog, self.vault.clone()) {
            let acts = d.show(ctx, self.store.as_ref().map_or(&[][..], |s| s.profiles()), &vault);
            for a in acts {
                self.connect_action(a);
            }
        }
        // After the manager, so it draws over it.
        if let Some(d) = &mut self.export_conn {
            let close = d.show(ctx);
            if d.vault_changed() {
                self.export_conn = None;
                self.lock_again();
            } else if close {
                self.export_conn = None;
            }
        }
        if let Some(d) = &mut self.doc_dialog {
            match d.show(ctx) {
                Some(DocAction::Close) => self.doc_dialog = None,
                Some(DocAction::Save(doc)) => self.save_doc(doc),
                None => {}
            }
        }
        // Out for the frame, so it can be shown what the app holds, borrowed.
        if let Some(mut dialog) = self.settings_dialog.take() {
            self.refresh_names();
            let in_use: Vec<(uuid::Uuid, &Namespace)> = self.requests.in_use().collect();
            let action = dialog.show(ctx, &HistoryView { files: &self.history_files, names: &self.names, in_use: &in_use });
            self.settings_dialog = Some(dialog);
            if let Some(a) = action {
                self.settings_action(a);
            }
        }
        // After Settings, so it draws over it.
        if let Some(outcome) = self.change_password.as_mut().and_then(|d| d.show(ctx)) {
            self.change_password = None;
            match outcome {
                ChangeOutcome::Changed => self.toast("Master password changed", false),
                ChangeOutcome::Cancel => {}
                ChangeOutcome::VaultChanged => {
                    self.lock_again();
                    self.toast(vault_ui::changed_message("change the master password again"), true);
                }
            }
        }
        if let Some(v) = &mut self.view_doc
            && v.show(ctx)
        {
            self.view_doc = None;
        }
        if let Some(t) = &mut self.tool {
            match t.show(ctx) {
                Some(ToolAction::Close) => self.tool = None,
                Some(ToolAction::Send(cmd)) => self.tool_send(cmd),
                None => {}
            }
        }
        if let Some(d) = &mut self.paste_dialog {
            let target = self.conns.get(&d.to()).map(|c| (c.profile.name.clone(), theme::conn_color(c.profile.color.as_deref()), c.profile.read_only));
            let (name, color, read_only) = target.unwrap_or_else(|| ("(closed)".into(), None, true));
            if let Some(a) = d.show(ctx, &TargetInfo { name: &name, color, read_only }) {
                self.paste_action(a);
            }
        }
        if let Some(go) = self.restore_ask.as_mut().and_then(|a| a.show(ctx)) {
            self.restore_answer(go);
        }
        if let Some(c) = &mut self.confirm {
            match c.show(ctx) {
                Some(ConfirmResult::Yes) => {
                    if let Some(c) = self.confirm.take() {
                        self.confirmed(c);
                    }
                }
                Some(ConfirmResult::No) => self.confirm = None,
                None => {}
            }
        }
        self.reset_dialog(ctx);
    }

    pub(super) fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.modal_open() {
            return;
        }
        // Before ⌘, below: on the Thai layout ⇧] is ",".
        if take_shift_command_at(ctx, Key::OpenBracket) {
            self.active_tab = (self.active_tab + self.tabs.len() - 1) % self.tabs.len();
        }
        if take_shift_command_at(ctx, Key::CloseBracket) {
            self.active_tab = (self.active_tab + 1) % self.tabs.len();
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Comma)) {
            self.open_settings();
        }
        // A result view under the pointer, or with its find bar focused,
        // claimed ⌘F last frame for its own find bar. The sidebar wins when
        // the pointer is over it.
        let view_wants_find = ctx.data_mut(|d| d.remove_temp::<bool>(egui::Id::new(crate::result_view::FIND_CLAIM))).unwrap_or(false);
        let over_sidebar = self.sidebar.open && ctx.pointer_hover_pos().is_some_and(|p| self.sidebar_rect.contains(p));
        if (over_sidebar || !view_wants_find) && ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::F)) {
            self.sidebar.open = true;
            ctx.memory_mut(|m| m.request_focus(egui::Id::new("tree-search")));
        }
        // Over the tree, ⌘R and ⇧⌘R refresh it (as its menus say) rather
        // than the results.
        if over_sidebar {
            if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::R)) {
                self.refresh_selected();
            } else if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::R)) {
                self.tree_action(TreeAction::RefreshAll);
            }
        }
        // ⌘C and ⌘V arrive as Copy/Paste events. Over the tree they copy the
        // selected collection and paste into the selected database; the
        // events are taken so the result view doesn't also act on them.
        // Anywhere else, and in the search box, they are left alone.
        if over_sidebar && !ctx.egui_wants_keyboard_input() {
            let sel = self.sidebar.selection.clone();
            let alt = ctx.input(|i| i.modifiers.alt);
            let take = |keep: fn(&egui::Event) -> bool| ctx.input_mut(|i| i.events.extract_if(.., |e| keep(e)).count() > 0);
            if let (Some(conn), Some(db), Some(coll)) = (sel.conn, sel.db.clone(), sel.coll.clone())
                && !alt
                && take(|e| matches!(e, egui::Event::Copy))
            {
                self.copy_collections(conn, db, vec![coll]);
            }
            if let (Some(conn), Some(db)) = (sel.conn, sel.db)
                && take(|e| matches!(e, egui::Event::Paste(_)))
            {
                self.open_paste(conn, db);
            }
        }
        // ⌥⌘C arrives as a Copy event (like ⌘C); with ⌥ held it copies the
        // selected collection's name.
        let alt_copy = ctx.input(|i| i.modifiers.alt && i.modifiers.command && i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
        if alt_copy && let Some(coll) = self.sidebar.selection.coll.clone() {
            ctx.copy_text(coll);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::W)) {
            self.close_tab(self.active_tab);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::T)) {
            self.open_shell_for_selection();
        }
    }
}

/// Takes a ⇧⌘ press of the key at `place` (a US layout's key there),
/// whatever the layout calls it. With ⇧ held, Windows names a key by its
/// shifted character: ⇧[ is `{` on a US layout and `ฐ` on the Thai one.
fn take_shift_command_at(ctx: &egui::Context, place: Key) -> bool {
    ctx.input_mut(|i| {
        let at = |e: &egui::Event| matches!(e, egui::Event::Key { key, physical_key, pressed: true, modifiers, .. } if (*key == place || *physical_key == Some(place)) && modifiers.matches_logically(Modifiers::COMMAND | Modifiers::SHIFT));
        i.events.extract_if(.., |e| at(e)).count() > 0
    })
}
