//! Modal windows: the connection manager, the document editor, confirmations
//! and a one-line prompt.

use egui::{Align, Context, Id, Key, Layout, Modal, RichText, Ui, vec2};
use egui_phosphor::regular as icon;
use rongo_core::bson::{Bson, Document};
use rongo_core::format::{self, Style};
use rongo_core::profile::{Auth, ConnectionProfile, SecretKind, SshAuth, SshConfig, Target, TlsConfig};
use rongo_core::vault::Vault;
use rongo_core::{ConnId, ConnectRequest, Namespace, RequestId};
use uuid::Uuid;

use crate::doc_find::{self, DocFind};
use crate::theme;
use crate::vault_ui::{self, CatchUp};

// ---------------------------------------------------------------- connection manager

#[derive(Clone, Copy, PartialEq, Eq)]
enum FormTab {
    Server,
    Auth,
    Tls,
    Ssh,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetMode {
    Uri,
    Manual,
}

/// Editable copy of a profile, with every field as text.
struct Form {
    id: Uuid,
    is_new: bool,
    name: String,
    mode: TargetMode,
    uri: String,
    hosts: String,
    replica_set: String,
    direct: bool,
    auth_on: bool,
    user: String,
    password: String,
    has_saved_password: bool,
    auth_db: String,
    mechanism: String,
    tls: TlsConfig,
    ca_file: String,
    cert_file: String,
    ssh_on: bool,
    ssh_host: String,
    ssh_port: String,
    ssh_user: String,
    ssh_key_mode: bool,
    ssh_key_path: String,
    ssh_secret: String,
    has_saved_ssh_secret: bool,
    default_db: String,
    color: Option<String>,
    read_only: bool,
    /// Collection History: `None` follows Settings.
    history: Option<bool>,
    tab: FormTab,
    error: Option<String>,
}

impl Form {
    /// The form for a connection not saved yet (new, or a clone): nothing is
    /// saved for it.
    fn new(p: &ConnectionProfile) -> Self {
        Self::build(p, true, |_| false)
    }

    /// A saved connection's form, noting which of its secrets the vault holds.
    fn from_profile(p: &ConnectionProfile, vault: &Vault) -> Self {
        Self::build(p, false, |kind| vault.get(p.id, kind).is_some())
    }

    fn build(p: &ConnectionProfile, is_new: bool, in_vault: impl Fn(SecretKind) -> bool) -> Self {
        let (mode, uri, hosts, replica_set, direct) = match &p.target {
            Target::Uri { uri } => (TargetMode::Uri, uri.clone(), "localhost:27017".into(), String::new(), false),
            Target::Manual { hosts, replica_set, direct } => {
                (TargetMode::Manual, String::new(), hosts.join(", "), replica_set.clone().unwrap_or_default(), *direct)
            }
        };
        let (auth_on, user, auth_db, mechanism) = match &p.auth {
            Auth::None => (false, String::new(), "admin".into(), String::new()),
            Auth::Password { user, auth_db, mechanism } => (true, user.clone(), auth_db.clone(), mechanism.clone().unwrap_or_default()),
        };
        let ssh = p.ssh.clone();
        let uses = |kind| match kind {
            SecretKind::Password => p.uses_password(),
            SecretKind::Ssh => p.ssh.is_some(),
            SecretKind::HistoryKey => false, // not a connection secret
        };
        let saved = |kind| !is_new && uses(kind) && in_vault(kind);
        Self {
            id: p.id,
            is_new,
            name: p.name.clone(),
            mode,
            uri,
            hosts,
            replica_set,
            direct,
            auth_on,
            user,
            password: String::new(),
            has_saved_password: saved(SecretKind::Password),
            auth_db,
            mechanism,
            tls: p.tls.clone(),
            ca_file: p.tls.ca_file.as_ref().map(|f| f.display().to_string()).unwrap_or_default(),
            cert_file: p.tls.cert_key_file.as_ref().map(|f| f.display().to_string()).unwrap_or_default(),
            ssh_on: ssh.is_some(),
            ssh_host: ssh.as_ref().map(|s| s.host.clone()).unwrap_or_default(),
            ssh_port: ssh.as_ref().map_or("22".into(), |s| s.port.to_string()),
            ssh_user: ssh.as_ref().map(|s| s.user.clone()).unwrap_or_default(),
            ssh_key_mode: matches!(ssh.as_ref().map(|s| &s.auth), Some(SshAuth::Key { .. })),
            ssh_key_path: match ssh.as_ref().map(|s| &s.auth) {
                Some(SshAuth::Key { path }) => path.display().to_string(),
                _ => "~/.ssh/id_ed25519".into(),
            },
            ssh_secret: String::new(),
            has_saved_ssh_secret: saved(SecretKind::Ssh),
            default_db: p.default_db.clone().unwrap_or_default(),
            color: p.color.clone(),
            read_only: p.read_only,
            history: p.history,
            tab: FormTab::Server,
            error: None,
        }
    }

    fn to_profile(&self) -> Result<ConnectionProfile, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Give the connection a name".into());
        }
        let target = match self.mode {
            TargetMode::Uri => {
                let uri = self.uri.trim();
                if !(uri.starts_with("mongodb://") || uri.starts_with("mongodb+srv://")) {
                    return Err("The URI must start with mongodb:// or mongodb+srv://".into());
                }
                Target::Uri { uri: uri.to_string() }
            }
            TargetMode::Manual => {
                let hosts: Vec<String> = self.hosts.split([',', '\n']).map(|h| h.trim().to_string()).filter(|h| !h.is_empty()).collect();
                if hosts.is_empty() {
                    return Err("Add at least one host:port".into());
                }
                let rs = self.replica_set.trim();
                Target::Manual { hosts, replica_set: (!rs.is_empty()).then(|| rs.to_string()), direct: self.direct }
            }
        };
        let auth = if self.auth_on {
            if self.user.trim().is_empty() {
                return Err("Enter a username, or turn authentication off".into());
            }
            Auth::Password {
                user: self.user.trim().into(),
                auth_db: if self.auth_db.trim().is_empty() { "admin".into() } else { self.auth_db.trim().into() },
                mechanism: (!self.mechanism.is_empty()).then(|| self.mechanism.clone()),
            }
        } else {
            Auth::None
        };
        let path = |s: &str| (!s.trim().is_empty()).then(|| std::path::PathBuf::from(s.trim()));
        let tls = TlsConfig { ca_file: path(&self.ca_file), cert_key_file: path(&self.cert_file), ..self.tls.clone() };
        let ssh = if self.ssh_on {
            let port: u16 = self.ssh_port.trim().parse().ok().filter(|p| *p > 0).ok_or("SSH port must be a number from 1 to 65535")?;
            if self.ssh_host.trim().is_empty() || self.ssh_user.trim().is_empty() {
                return Err("SSH needs a host and a user".into());
            }
            if self.ssh_key_mode && self.ssh_key_path.trim().is_empty() {
                return Err("Choose the SSH private key file".into());
            }
            Some(SshConfig {
                host: self.ssh_host.trim().into(),
                port,
                user: self.ssh_user.trim().into(),
                auth: if self.ssh_key_mode {
                    SshAuth::Key { path: self.ssh_key_path.trim().into() }
                } else {
                    SshAuth::Password
                },
            })
        } else {
            None
        };
        let default_db = self.default_db.trim();
        if !default_db.is_empty()
            && let Some(why) = crate::app::database_name_error(default_db)
        {
            return Err(format!("Default database '{default_db}' is not a valid name: {why}"));
        }
        Ok(ConnectionProfile {
            id: self.id,
            name: name.into(),
            target,
            auth,
            tls,
            ssh,
            default_db: (!default_db.is_empty()).then(|| default_db.to_string()),
            color: self.color.clone(),
            read_only: self.read_only,
            history: self.history,
        })
    }

    /// The secrets to use: what was typed, or else what the vault holds.
    fn secrets(&self, vault: &Vault) -> (Option<String>, Option<String>) {
        let typed = |s: &String| (!s.is_empty()).then(|| s.clone());
        let pw = typed(&self.password).or_else(|| self.has_saved_password.then(|| vault.get(self.id, SecretKind::Password)).flatten());
        let ssh = typed(&self.ssh_secret).or_else(|| self.has_saved_ssh_secret.then(|| vault.get(self.id, SecretKind::Ssh)).flatten());
        (pw, ssh)
    }
}

pub enum TestState {
    Idle,
    Running(RequestId),
    Ok(String),
    Failed(String),
}

pub enum ConnectAction {
    Connect(Uuid),
    /// Save the profile, with any newly typed secrets.
    Save { profile: ConnectionProfile, password: Option<String>, ssh_secret: Option<String>, then_connect: bool },
    Delete(Uuid),
    /// Dragged to just before another profile, or (`None`) to the end.
    Move { id: Uuid, before: Option<Uuid> },
    SetColor(Uuid, Option<String>),
    Test(Box<ConnectRequest>),
    /// Export Connection… (the toolbar or a row's right-click menu).
    Export(Uuid),
    /// Opening a connection's form found the vault changed in another
    /// rongo window: it must be unlocked again.
    VaultChanged,
    /// Something to tell the user, as an error toast (the dialog has no toasts).
    Notice(String),
    Close,
}

pub struct ConnectDialog {
    /// The highlighted rows; Connect opens all of them.
    selected: Vec<Uuid>,
    /// Where a ⇧-click range starts: the last row clicked without ⇧.
    anchor: Option<Uuid>,
    form: Option<Form>,
    pub test: TestState,
    /// The form's footer (messages and buttons) as last drawn, so the body
    /// can leave exactly that much room under it.
    footer_h: f32,
    /// The list's footer (separator and buttons) as last drawn, for the
    /// same reason.
    list_footer_h: f32,
    /// The form a Save closed, until the next frame: if the save fails (the
    /// vault changed elsewhere) `restore_form` brings it back as typed.
    saved: Option<Form>,
}

impl ConnectDialog {
    pub fn new() -> Self {
        Self { selected: Vec::new(), anchor: None, form: None, test: TestState::Idle, footer_h: 46.0, list_footer_h: 40.0, saved: None }
    }

    /// A Save failed and nothing was written: the form is open again, as it
    /// was typed, with `error` on it.
    pub fn restore_form(&mut self, error: String) {
        if let Some(f) = self.saved.take() {
            self.form = Some(f);
        }
        if let Some(f) = &mut self.form {
            f.error = Some(error);
        }
    }

    pub fn new_connection() -> Self {
        let mut d = Self::new();
        d.form = Some(Form::new(&ConnectionProfile::new("New connection")));
        d
    }

    /// Tests: whether the form is open, and the message on it.
    #[cfg(test)]
    pub fn has_form(&self) -> bool {
        self.form.is_some()
    }

    #[cfg(test)]
    pub fn form_error(&self) -> Option<&str> {
        self.form.as_ref()?.error.as_deref()
    }

    /// Tests: what pressing Save does to the form (closed, kept for `restore_form`).
    #[cfg(test)]
    pub fn close_form_for_save(&mut self) {
        self.saved = self.form.take();
    }

    /// `vault` holds the connections' secrets: the form notes which are
    /// saved, and Test connection uses them.
    pub fn show(&mut self, ctx: &Context, profiles: &[ConnectionProfile], vault: &Vault) -> Vec<ConnectAction> {
        let mut actions = Vec::new();
        // The app has answered the last Save by now.
        self.saved = None;
        // Never wider or taller than the window: a centred dialog wider
        // than the window loses both edges, the checkboxes with them.
        let screen = ctx.content_rect().size();
        let min_h = 420.0f32.min(screen.y - 80.0).max(200.0);
        let modal = Modal::new(Id::new("connect-dialog")).show(ctx, |ui| {
            ui.set_width(760.0f32.min(screen.x - 40.0).max(240.0));
            ui.set_min_height(min_h);
            match &mut self.form {
                None => self.list(ui, min_h, profiles, vault, &mut actions),
                Some(_) => self.edit(ui, min_h, vault, &mut actions),
            }
        });
        // The form, open, holds typing: Cancel goes back to the list.
        if theme::may_close(&modal, self.form.is_some()) {
            actions.push(ConnectAction::Close);
        }
        actions
    }

    /// A click picks just this row, ⌘-click (or its checkbox) adds or
    /// removes it, and ⇧-click picks every row from the anchor to it.
    fn pick(&mut self, id: Uuid, m: egui::Modifiers, profiles: &[ConnectionProfile]) {
        let pos = |id| profiles.iter().position(|p: &ConnectionProfile| p.id == id);
        if m.shift
            && let Some((a, b)) = self.anchor.and_then(pos).zip(pos(id))
        {
            self.selected = profiles[a.min(b)..=a.max(b)].iter().map(|p| p.id).collect();
            return;
        }
        if !m.command {
            self.selected.clear();
        }
        match self.selected.iter().position(|s| *s == id) {
            Some(i) => _ = self.selected.remove(i),
            None => self.selected.push(id),
        }
        self.anchor = Some(id);
    }

    fn list(&mut self, ui: &mut Ui, min_h: f32, profiles: &[ConnectionProfile], vault: &Vault, actions: &mut Vec<ConnectAction>) {
        ui.heading(format!("{}  Connection Manager", icon::PLUGS_CONNECTED));
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            if ui.button(format!("{}  New connection", icon::PLUS)).clicked() {
                self.form = Some(Form::new(&ConnectionProfile::new("New connection")));
                self.test = TestState::Idle;
            }
            // Editing, cloning and deleting work on one connection at a time.
            let picked: Vec<&ConnectionProfile> = profiles.iter().filter(|p| self.selected.contains(&p.id)).collect();
            let sel = match picked.as_slice() {
                [p] => Some(*p),
                _ => None,
            };
            if ui.add_enabled(sel.is_some(), egui::Button::new(format!("{}  Edit", icon::PENCIL_SIMPLE))).clicked()
                && let Some(p) = sel
            {
                // Another rongo window may have saved its passwords since: caught up first.
                match vault_ui::catch_up(vault) {
                    CatchUp::Changed => actions.push(ConnectAction::VaultChanged),
                    fresh => {
                        // Busy or unreadable just now: this window's copy still serves, and a toast says why.
                        if let CatchUp::Failed(e) = fresh {
                            actions.push(ConnectAction::Notice(vault_ui::unreadable(&e)));
                        }
                        self.form = Some(Form::from_profile(p, vault));
                        self.test = TestState::Idle;
                    }
                }
            }
            if ui.add_enabled(sel.is_some(), egui::Button::new(format!("{}  Clone", icon::COPY))).clicked()
                && let Some(p) = sel
            {
                let mut copy = p.clone();
                copy.id = Uuid::new_v4();
                copy.name = format!("{} (copy)", p.name);
                self.form = Some(Form::new(&copy));
            }
            if ui.add_enabled(sel.is_some(), egui::Button::new(format!("{}  Export…", icon::EXPORT))).clicked()
                && let Some(p) = sel
            {
                actions.push(ConnectAction::Export(p.id));
            }
            if ui.add_enabled(sel.is_some(), egui::Button::new(RichText::new(format!("{}  Delete", icon::TRASH)).color(theme::ERROR))).clicked()
                && let Some(p) = sel
            {
                actions.push(ConnectAction::Delete(p.id));
                self.selected.clear();
            }
        });
        ui.separator();
        // The list takes the height the footer leaves, as measured. Not from
        // `available_height`: a modal's room is its own size last frame, so
        // a footer taller than guessed grew the dialog every frame.
        let used = ui.cursor().top() - ui.max_rect().top();
        let list_h = (min_h - used - self.list_footer_h).max(80.0);
        // Both ways: a narrow window scrolls the columns instead of cutting them.
        let scroll = egui::ScrollArea::both().max_height(list_h).min_scrolled_height(list_h).auto_shrink([false, false]);
        scroll.show(ui, |ui| {
            let mut rows = Vec::new();
            egui::Grid::new("profiles").num_columns(5).striped(true).spacing(vec2(16.0, 6.0)).show(ui, |ui| {
                let picked = profiles.iter().filter(|p| self.selected.contains(&p.id)).count();
                let mut all = !profiles.is_empty() && picked == profiles.len();
                let some = picked > 0 && !all;
                let every = egui::Checkbox::without_text(&mut all).indeterminate(some);
                if ui.add_enabled(!profiles.is_empty(), every).on_hover_text("Select all").clicked() {
                    self.selected = if all { profiles.iter().map(|p| p.id).collect() } else { Vec::new() };
                }
                ui.label(RichText::new("Color").strong());
                ui.label(RichText::new("Name").strong());
                ui.label(RichText::new("Server").strong());
                ui.label(RichText::new("Options").strong());
                ui.end_row();
                if profiles.is_empty() {
                    ui.weak("No saved connections yet.");
                    ui.end_row();
                }
                for p in profiles {
                    let selected = self.selected.contains(&p.id);
                    let mut ticked = selected;
                    if ui.add(egui::Checkbox::without_text(&mut ticked)).clicked() {
                        let m = ui.input(|i| i.modifiers);
                        self.pick(p.id, egui::Modifiers { command: true, ..m }, profiles);
                    }
                    let dot = theme::color_button(ui, p.color.as_deref());
                    egui::Popup::menu(&dot).id(Id::new(("profile-color", p.id))).show(|ui| {
                        if let Some(c) = theme::color_swatches(ui, p.color.as_deref()) {
                            actions.push(ConnectAction::SetColor(p.id, c));
                            ui.close();
                        }
                    });
                    // Drawn like the connection in the tree.
                    let tag = theme::conn_color(p.color.as_deref());
                    let r = crate::sidebar::conn_label(ui, icon::PLUGS, theme::MUTED, &p.name, selected, tag, egui::Sense::click_and_drag());
                    if r.clicked() {
                        let m = ui.input(|i| i.modifiers);
                        self.pick(p.id, m, profiles);
                    }
                    if r.drag_started() {
                        egui::DragAndDrop::set_payload(ui.ctx(), p.conn_id());
                    }
                    rows.push((p.conn_id(), r.rect, r.rect));
                    if r.double_clicked() {
                        actions.push(ConnectAction::Connect(p.id));
                    }
                    r.context_menu(|ui| {
                        if ui.button(format!("{}  Export Connection…", icon::EXPORT)).clicked() {
                            actions.push(ConnectAction::Export(p.id));
                            ui.close();
                        }
                    });
                    ui.label(p.summary());
                    let mut opts = Vec::new();
                    if let Auth::Password { user, .. } = &p.auth {
                        opts.push(format!("user {user}"));
                    }
                    if p.tls.enabled {
                        opts.push("TLS".into());
                    }
                    if p.ssh.is_some() {
                        opts.push("SSH".into());
                    }
                    ui.weak(opts.join(" · "));
                    ui.end_row();
                }
            });
            if let Some((id, before)) = crate::sidebar::drop_target(ui, &rows) {
                actions.push(ConnectAction::Move { id: id.0, before: before.map(|b| b.0) });
            }
        });
        let footer_top = ui.cursor().top();
        ui.separator();
        theme::dialog_buttons(ui, |ui| {
            // In list order, whatever order they were picked in.
            let ids: Vec<Uuid> = profiles.iter().filter(|p| self.selected.contains(&p.id)).map(|p| p.id).collect();
            let text = match ids.len() {
                0 | 1 => format!("{}  Connect", icon::PLUG),
                n => format!("{}  Connect {n}", icon::PLUG),
            };
            if ui.add_enabled(!ids.is_empty(), theme::primary_button(text, false)).clicked() {
                actions.extend(ids.into_iter().map(ConnectAction::Connect));
            }
            if ui.button("Close").clicked() {
                actions.push(ConnectAction::Close);
            }
            // The hint gets what the buttons leave, cut short if need be.
            if profiles.len() > 1 {
                theme::footer_note(ui, RichText::new("Tick several to connect them together · drag a name to reorder").weak());
            }
        });
        self.list_footer_h = ui.cursor().top() - footer_top;
    }

    fn edit(&mut self, ui: &mut Ui, min_h: f32, vault: &Vault, actions: &mut Vec<ConnectAction>) {
        let Some(f) = &mut self.form else { return };
        ui.heading(if f.is_new { "New connection" } else { "Edit connection" });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Name");
            let w = (ui.available_width() - 60.0).clamp(120.0, 320.0);
            ui.add(egui::TextEdit::singleline(&mut f.name).desired_width(w));
            ui.add_space(16.0);
            ui.label("Color");
            if let Some(c) = theme::color_swatches(ui, f.color.as_deref()) {
                f.color = c;
            }
            ui.add_space(16.0);
            ui.checkbox(&mut f.read_only, "Read-only (refuse every write)");
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            for (tab, label) in [(FormTab::Server, "Server"), (FormTab::Auth, "Authentication"), (FormTab::Tls, "TLS"), (FormTab::Ssh, "SSH Tunnel")] {
                ui.selectable_value(&mut f.tab, tab, label);
            }
        });
        ui.separator();
        // The body takes the height the footer leaves, so the buttons sit at
        // the bottom of the dialog rather than above an empty strip. Not from
        // `available_height`: a modal's room is its own size last frame, so
        // any shortfall in the footer's height grew the dialog every frame
        // until it filled the window.
        let body_h = (min_h - ui.min_rect().height() - self.footer_h).max(250.0);
        ui.allocate_ui(vec2(ui.available_width(), body_h), |ui| {
            ui.set_min_height(body_h);
            match f.tab {
                FormTab::Server => server_tab(ui, f),
                FormTab::Auth => auth_tab(ui, f),
                FormTab::Tls => tls_tab(ui, f),
                FormTab::Ssh => ssh_tab(ui, f),
            }
        });
        let footer_top = ui.min_rect().bottom();
        ui.separator();
        // A failed test can be a paragraph (every server of a replica set),
        // so messages scroll after a few lines instead of pushing the
        // buttons off the window.
        egui::ScrollArea::vertical().id_salt("form-notes").max_height(90.0).auto_shrink([false, true]).show(ui, |ui| {
            if let Some(e) = &f.error {
                ui.colored_label(theme::ERROR, e);
            }
            match &self.test {
                TestState::Idle => {}
                TestState::Running(..) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Testing connection…");
                    });
                }
                TestState::Ok(m) => {
                    ui.colored_label(theme::OK, format!("{} {m}", icon::CHECK_CIRCLE));
                }
                TestState::Failed(m) => {
                    ui.colored_label(theme::ERROR, format!("{} {m}", icon::X_CIRCLE));
                }
            }
        });
        let form_id = f.id;
        let mut close = false;
        let mut saving = false;
        ui.horizontal(|ui| {
            let testing = matches!(self.test, TestState::Running(..));
            if ui.add_enabled(!testing, egui::Button::new(format!("{}  Test connection", icon::PULSE))).clicked() {
                match f.to_profile() {
                    Ok(mut p) => {
                        f.error = None;
                        // Another rongo window may have saved its passwords since: caught up first.
                        match vault_ui::catch_up(vault) {
                            CatchUp::Changed => actions.push(ConnectAction::VaultChanged),
                            fresh => {
                                // Busy or unreadable just now: this window's copy still serves, and a toast says why.
                                if let CatchUp::Failed(e) = fresh {
                                    actions.push(ConnectAction::Notice(vault_ui::unreadable(&e)));
                                }
                                let (password, ssh_secret) = f.secrets(vault);
                                // A throwaway id so testing never disturbs an open connection.
                                p.id = Uuid::new_v4();
                                actions.push(ConnectAction::Test(Box::new(ConnectRequest { profile: p, password, ssh_secret })));
                            }
                        }
                    }
                    Err(e) => f.error = Some(e),
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut save = |then_connect: bool, f: &mut Form| match f.to_profile() {
                    Ok(profile) => {
                        // A secret typed while its method is off is not kept.
                        let typed = |s: &String, on: bool| (on && !s.is_empty()).then(|| s.clone());
                        actions.push(ConnectAction::Save {
                            profile,
                            password: typed(&f.password, f.auth_on),
                            ssh_secret: typed(&f.ssh_secret, f.ssh_on),
                            then_connect,
                        });
                        true
                    }
                    Err(e) => {
                        f.error = Some(e);
                        false
                    }
                };
                if ui.add(theme::primary_button(format!("{}  Save & Connect", icon::PLUG), false)).clicked() {
                    close = save(true, f);
                    saving = close;
                }
                if ui.button("Save").clicked() {
                    close = save(false, f);
                    saving = close;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        self.footer_h = ui.min_rect().bottom() - footer_top;
        if close {
            self.selected = vec![form_id];
            self.anchor = Some(form_id);
            let form = self.form.take();
            // Kept until the app has answered the Save (see `restore_form`).
            self.saved = form.filter(|_| saving);
            self.test = TestState::Idle;
        }
    }
}

fn field(ui: &mut Ui, label: &str, value: &mut String, hint: &str, password: bool) {
    ui.label(label);
    ui.add(egui::TextEdit::singleline(value).hint_text(hint).password(password).desired_width(420.0));
    ui.end_row();
}

fn file_field(ui: &mut Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(value).desired_width(360.0));
        if ui.button(icon::FOLDER_OPEN).clicked()
            && let Some(p) = rfd::FileDialog::new().pick_file()
        {
            *value = p.display().to_string();
        }
    });
    ui.end_row();
}

fn server_tab(ui: &mut Ui, f: &mut Form) {
    ui.horizontal(|ui| {
        ui.radio_value(&mut f.mode, TargetMode::Manual, "Host / port");
        ui.radio_value(&mut f.mode, TargetMode::Uri, "Connection string (URI)");
    });
    ui.add_space(6.0);
    egui::Grid::new("server").num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
        match f.mode {
            TargetMode::Uri => {
                field(ui, "URI", &mut f.uri, "mongodb+srv://user@cluster.example.net/?retryWrites=true", false);
                ui.label("");
                ui.weak("A password in the URI is moved to the vault when you save.");
                ui.end_row();
            }
            TargetMode::Manual => {
                field(ui, "Hosts", &mut f.hosts, "db1.example.net:27017, db2.example.net:27017", false);
                field(ui, "Replica set", &mut f.replica_set, "rs0 (optional)", false);
                ui.label("");
                ui.checkbox(&mut f.direct, "Direct connection (talk to the first host only)");
                ui.end_row();
            }
        }
        field(ui, "Default database", &mut f.default_db, "used for new IntelliShell tabs", false);
        ui.label("Collection History");
        let name = |v: Option<bool>| match v {
            None => "Use Settings",
            Some(true) => "On",
            Some(false) => "Off",
        };
        egui::ComboBox::from_id_salt("conn-history")
            .selected_text(name(f.history))
            .show_ui(ui, |ui| {
                for v in [None, Some(true), Some(false)] {
                    ui.selectable_value(&mut f.history, v, name(v));
                }
            })
            .response
            .on_hover_text("Keep the documents rongo deletes or changes on this connection, on this computer, so they can be restored");
        ui.end_row();
    });
}

fn auth_tab(ui: &mut Ui, f: &mut Form) {
    ui.checkbox(&mut f.auth_on, "Username / password authentication");
    ui.add_space(6.0);
    ui.add_enabled_ui(f.auth_on, |ui| {
        egui::Grid::new("auth").num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
            field(ui, "Username", &mut f.user, "", false);
            let hint = if f.has_saved_password { "saved — type to replace" } else { "" };
            field(ui, "Password", &mut f.password, hint, true);
            field(ui, "Auth database", &mut f.auth_db, "admin", false);
            ui.label("Mechanism");
            egui::ComboBox::from_id_salt("mech")
                .selected_text(if f.mechanism.is_empty() { "Default" } else { f.mechanism.as_str() })
                .show_ui(ui, |ui| {
                    for (v, l) in [("", "Default"), ("SCRAM-SHA-256", "SCRAM-SHA-256"), ("SCRAM-SHA-1", "SCRAM-SHA-1")] {
                        ui.selectable_value(&mut f.mechanism, v.to_string(), l);
                    }
                });
            ui.end_row();
        });
    });
}

fn tls_tab(ui: &mut Ui, f: &mut Form) {
    ui.checkbox(&mut f.tls.enabled, "Use TLS/SSL");
    ui.add_space(6.0);
    ui.add_enabled_ui(f.tls.enabled, |ui| {
        egui::Grid::new("tls").num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
            file_field(ui, "CA certificate", &mut f.ca_file);
            file_field(ui, "Client certificate + key (PEM)", &mut f.cert_file);
            ui.label("");
            ui.checkbox(&mut f.tls.allow_invalid_certs, "Allow invalid certificates (not recommended)");
            ui.end_row();
        });
    });
}

fn ssh_tab(ui: &mut Ui, f: &mut Form) {
    ui.checkbox(&mut f.ssh_on, "Connect through an SSH tunnel");
    ui.add_space(6.0);
    ui.add_enabled_ui(f.ssh_on, |ui| {
        egui::Grid::new("ssh").num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
            field(ui, "SSH host", &mut f.ssh_host, "bastion.example.net", false);
            field(ui, "SSH port", &mut f.ssh_port, "22", false);
            field(ui, "SSH user", &mut f.ssh_user, "ec2-user", false);
            ui.label("Authentication");
            ui.horizontal(|ui| {
                ui.radio_value(&mut f.ssh_key_mode, false, "Password");
                ui.radio_value(&mut f.ssh_key_mode, true, "Private key");
            });
            ui.end_row();
            if f.ssh_key_mode {
                file_field(ui, "Private key", &mut f.ssh_key_path);
            }
            let label = if f.ssh_key_mode { "Passphrase" } else { "Password" };
            let hint = if f.has_saved_ssh_secret { "saved — type to replace" } else { "" };
            field(ui, label, &mut f.ssh_secret, hint, true);
        });
        ui.add_space(4.0);
        ui.weak("The SSH host key must already be in ~/.ssh/known_hosts. Replica set members are reached through the first host only.");
    });
}

// ---------------------------------------------------------------- document editor

pub enum DocMode {
    Edit { id: Bson },
    Insert,
}

pub struct DocDialog {
    pub conn: ConnId,
    pub ns: Namespace,
    pub mode: DocMode,
    pub view: Option<u64>,
    text: String,
    /// Counts the edits of `text`: the find bar searches it again.
    version: u64,
    layout: theme::CodeLayout,
    /// The find bar (⌘F), while it is open.
    find: Option<DocFind>,
    /// Something was typed: a click outside or Esc no longer closes it.
    typed: bool,
    pub error: Option<String>,
    pub saving: Option<RequestId>,
}

pub enum DocAction {
    Save(Document),
    Close,
}

impl DocDialog {
    pub fn edit(conn: ConnId, ns: Namespace, doc: &Document, view: Option<u64>) -> Self {
        let id = doc.get("_id").cloned().unwrap_or(Bson::Null);
        let text = format::doc_to_shell(doc, Style::Editable);
        Self { conn, ns, mode: DocMode::Edit { id }, view, text, version: 0, layout: Default::default(), find: None, typed: false, error: None, saving: None }
    }

    #[cfg(test)]
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn insert(conn: ConnId, ns: Namespace, template: Option<&Document>, view: Option<u64>) -> Self {
        let text = match template {
            Some(d) => format::doc_to_shell(d, Style::Editable),
            None => "{\n    \n}".to_string(),
        };
        Self { conn, ns, mode: DocMode::Insert, view, text, version: 0, layout: Default::default(), find: None, typed: false, error: None, saving: None }
    }

    pub fn show(&mut self, ctx: &Context) -> Option<DocAction> {
        let mut action = None;
        let title = match self.mode {
            DocMode::Edit { .. } => format!("{}  Edit document — {}", icon::PENCIL_SIMPLE, self.ns),
            DocMode::Insert => format!("{}  Add document — {}", icon::FILE_PLUS, self.ns),
        };
        // The editor gives up height on a short window so the buttons and a
        // few lines of error still fit. From the window, not the dialog's
        // own last size, which would feed back into itself.
        let window_h = (ctx.content_rect().height() - 300.0).clamp(160.0, 440.0);
        let editor = Id::new("doc-dialog").with("text");
        let modal = Modal::new(Id::new("doc-dialog")).show(ctx, |ui| {
            ui.set_width(720.0);
            ui.heading(title);
            ui.weak("Shell syntax: ObjectId(\"…\"), ISODate(\"…\"), NumberLong(\"…\"), NumberDecimal(\"…\"), Double(5).");
            ui.add_space(4.0);
            doc_find::bar(&mut self.find, ui, editor, "Find in this document", doc_find::Keys::Dialog, &self.text, self.version);
            // The find bar's height comes off the editor's: the dialog keeps its size.
            let edit_h = window_h - if self.find.is_some() { doc_find::BAR_H } else { 0.0 };
            let layout = &mut self.layout;
            let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| layout.galley(ui, buf.as_str(), theme::mono(13.0), wrap);
            egui::Frame::new().fill(theme::BG).inner_margin(6.0).corner_radius(4.0).show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(edit_h).min_scrolled_height(edit_h.min(300.0)).show(ui, |ui| {
                    let marks = ui.painter().add(egui::Shape::Noop);
                    // Where the selection began before this frame's typing: the text before it stays.
                    let selected_from = doc_find::edit_from(&self.find, ui.ctx(), editor);
                    let edit = egui::TextEdit::multiline(&mut self.text)
                        .id(editor)
                        .code_editor()
                        .frame(egui::Frame::NONE)
                        .desired_width(f32::INFINITY)
                        .desired_rows(18)
                        .layouter(&mut layouter)
                        .show(ui);
                    if edit.response.changed() {
                        self.typed = true;
                        self.version += 1;
                        // Found again at once: the marks below are for this text.
                        doc_find::edited(&mut self.find, ui.ctx(), &self.text, self.version, selected_from, &edit);
                    }
                    doc_find::paint(&mut self.find, ui, marks, &edit);
                });
            });
            if let Some(e) = &self.error {
                // A failed validation can list every rule it broke.
                egui::ScrollArea::vertical().id_salt("doc-error").max_height(72.0).show(ui, |ui| {
                    ui.colored_label(theme::ERROR, e);
                });
            }
            ui.horizontal(|ui| {
                if self.saving.is_some() {
                    ui.spinner();
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let save = ui.add_enabled(self.saving.is_none(), theme::primary_button("Save", false));
                    // ⌘⇧S is another shortcut: taken first, as ⌘S would match it too.
                    let shortcut = ui.input_mut(|i| {
                        i.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, Key::S);
                        i.consume_key(egui::Modifiers::COMMAND, Key::S)
                    });
                    if save.clicked() || (shortcut && self.saving.is_none()) {
                        match rongo_shell::parse_editor_document(&self.text) {
                            Ok(doc) => {
                                self.error = None;
                                action = Some(DocAction::Save(doc));
                            }
                            Err(e) => self.error = Some(e.to_string()),
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        action = Some(DocAction::Close);
                    }
                });
            });
        });
        if theme::may_close(&modal, self.typed) && action.is_none() && self.saving.is_none() {
            action = Some(DocAction::Close);
        }
        action
    }
}

/// Read-only JSON view of one document (View Document, F3).
pub struct ViewDocDialog {
    text: String,
    layout: theme::CodeLayout,
    /// The find bar (⌘F), while it is open.
    find: Option<DocFind>,
}

impl ViewDocDialog {
    pub fn new(doc: &Document) -> Self {
        Self { text: format::doc_to_shell(doc, Style::Display), layout: Default::default(), find: None }
    }

    /// Returns true when the dialog should close.
    pub fn show(&mut self, ctx: &Context) -> bool {
        let mut close = false;
        let viewer = Id::new("view-doc").with("text");
        let modal = Modal::new(Id::new("view-doc")).show(ctx, |ui| {
            ui.set_width(720.0);
            ui.heading(format!("{}  View Document", icon::FILE_TEXT));
            ui.add_space(4.0);
            // The text never changes: one version.
            doc_find::bar(&mut self.find, ui, viewer, "Find in this document", doc_find::Keys::Dialog, &self.text, 0);
            // The find bar's height comes off the text's: the dialog keeps its size.
            let view_h = 480.0 - if self.find.is_some() { doc_find::BAR_H } else { 0.0 };
            let layout = &mut self.layout;
            let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| layout.galley(ui, buf.as_str(), theme::mono(13.0), wrap);
            egui::Frame::new().fill(theme::BG).inner_margin(6.0).corner_radius(4.0).show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(view_h).show(ui, |ui| {
                    let marks = ui.painter().add(egui::Shape::Noop);
                    let mut view = self.text.as_str();
                    let out = egui::TextEdit::multiline(&mut view).id(viewer).code_editor().frame(egui::Frame::NONE).desired_width(f32::INFINITY).layouter(&mut layouter).show(ui);
                    doc_find::paint(&mut self.find, ui, marks, &out);
                });
            });
            theme::dialog_buttons(ui, |ui| {
                if ui.button("Close").clicked() {
                    close = true;
                }
                if ui.button(format!("{}  Copy", icon::CLIPBOARD_TEXT)).clicked() {
                    ui.ctx().copy_text(self.text.clone());
                }
            });
        });
        close || theme::may_close(&modal, false)
    }
}

// ---------------------------------------------------------------- confirm & prompt

pub enum Pending {
    Delete { conn: ConnId, ns: Namespace, ids: Vec<Bson>, view: Option<u64> },
    DropCollection { conn: ConnId, ns: Namespace },
    DropDatabase { conn: ConnId, db: String },
    CreateCollection { conn: ConnId, db: String },
    RenameCollection { conn: ConnId, ns: Namespace },
    DuplicateCollection { conn: ConnId, ns: Namespace },
    ClearCollection { conn: ConnId, ns: Namespace },
    DropIndex { conn: ConnId, ns: Namespace, name: String },
    /// Lift a connection's read-only lock.
    AllowWrites { conn: ConnId },
    /// Insert documents from the clipboard; the checkbox strips their `_id`s.
    Paste { conn: ConnId, ns: Namespace, docs: Vec<Document>, view: Option<u64> },
    /// The name for a new database to paste collections into.
    PasteAsDatabase { conn: ConnId },
    /// A write whose Collection History can't be kept (too large, or its
    /// key unavailable): sent as `cmd`, without history, once `name` is typed.
    WithoutHistory { origin: crate::net::Origin, conn: ConnId, cmd: Box<rongo_core::Command>, name: String },
    /// Remove one Collection History entry; the History tab `tab` lists again.
    RemoveHistory { tab: u64, entry: rongo_core::history::EntryId },
    /// Remove every Collection History entry of a collection recorded by
    /// `before` (ms, when it was asked), off the UI thread; the History
    /// tabs list again when it is done.
    EmptyHistory { profile: uuid::Uuid, ns: Namespace, before: i64 },
    /// Settings → Delete…: remove one collection's Collection History, as
    /// recorded by `before` (ms, when it was asked).
    DeleteHistory { profile: uuid::Uuid, ns: Namespace, before: i64 },
    /// Settings → Clear All History…: remove every profile's.
    ClearHistory,
    /// Settings → Save with lower Collection History limits, which remove
    /// history at once.
    SaveSettings(Box<crate::settings::Settings>),
    /// Delete a saved connection. The checkbox, there when the profile has
    /// Collection History, deletes that too.
    DeleteProfile { id: uuid::Uuid },
    /// Reset the master password (Settings → Reset…, or Forgot? at launch).
    /// The checkbox, there when there is Collection History, deletes it too.
    ResetVault,
    /// Close these tabs (a tab menu's close, with scripts among them);
    /// `focus` is shown if the active tab closes.
    CloseTabs { ids: Vec<u64>, focus: Option<u64> },
}

pub struct Confirm {
    pub title: String,
    pub message: String,
    pub ok_label: String,
    pub danger: bool,
    pub pending: Pending,
    /// For prompts: the text the user types.
    pub input: Option<String>,
    /// An optional checkbox: (label, ticked).
    pub checkbox: Option<(String, bool)>,
    /// The text that must be typed (exactly) before OK can be pressed,
    /// such as a database's name before dropping it.
    pub must_type: Option<String>,
}

/// Whether `typed` confirms `want`, a name typed to go ahead with something
/// that can't be undone: spaces around either don't count.
pub fn confirms(typed: &str, want: &str) -> bool {
    typed.trim() == want.trim()
}

pub enum ConfirmResult {
    Yes,
    No,
}

impl Confirm {
    /// A question titled `title`, saying `message`; its OK button (labelled
    /// `ok_label`) does `pending`.
    pub fn new(title: &str, message: String, ok_label: &str, pending: Pending) -> Self {
        Confirm { title: title.into(), message, ok_label: ok_label.into(), danger: false, pending, input: None, checkbox: None, must_type: None }
    }

    /// OK in the danger colour.
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    /// A prompt, with `text` typed in it to begin with.
    pub fn input(mut self, text: String) -> Self {
        self.input = Some(text);
        self
    }

    /// A checkbox labelled `label`, ticked or not.
    pub fn checkbox(mut self, label: impl Into<String>, ticked: bool) -> Self {
        self.checkbox = Some((label.into(), ticked));
        self
    }

    /// OK waits until exactly `name` is typed in the prompt.
    pub fn must_type(mut self, name: String) -> Self {
        self.must_type = Some(name);
        self
    }

    /// OK waits for typed text, and for exactly the right text when a
    /// name must be typed (spaces around either don't count).
    pub fn ok_enabled(&self) -> bool {
        match (&self.input, &self.must_type) {
            (Some(s), Some(want)) => confirms(s, want),
            (Some(s), None) => !s.trim().is_empty(),
            (None, _) => true,
        }
    }

    pub fn show(&mut self, ctx: &Context) -> Option<ConfirmResult> {
        let mut result = None;
        let modal = Modal::new(Id::new("confirm")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.heading(&self.title);
            ui.add_space(4.0);
            ui.label(&self.message);
            if let Some(input) = &mut self.input {
                let r = ui.add(egui::TextEdit::singleline(input).desired_width(f32::INFINITY));
                // ↩ (which takes the field's focus) goes ahead on the rule OK
                // follows. Read before the focus is asked back: `lost_focus`
                // looks at the focus as it is now, so after `request_focus`
                // it was never true and ↩ did nothing.
                let entered = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                r.request_focus();
                if entered && self.ok_enabled() {
                    result = Some(ConfirmResult::Yes);
                }
            }
            if let Some((label, ticked)) = &mut self.checkbox {
                ui.add_space(4.0);
                ui.checkbox(ticked, label.as_str());
            }
            ui.add_space(8.0);
            theme::dialog_buttons(ui, |ui| {
                let ok_enabled = self.ok_enabled();
                if ui.add_enabled(ok_enabled, theme::primary_button(self.ok_label.clone(), self.danger)).clicked() {
                    result = Some(ConfirmResult::Yes);
                }
                if ui.button("Cancel").clicked() {
                    result = Some(ConfirmResult::No);
                }
            });
        });
        if modal.should_close() && result.is_none() {
            result = Some(ConfirmResult::No);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::test_support::{find_count, find_marks, flat_shapes, press, temp_vault, type_in};

    /// Editing a connection keeps its lock: the form carries `read_only`
    /// through (it used to build every profile unlocked), and its checkbox
    /// is what changes it.
    #[test]
    fn form_keeps_the_read_only_flag() {
        let mut p = ConnectionProfile::new("locked");
        p.read_only = true;
        let (_dir, vault) = temp_vault();
        let form = Form::from_profile(&p, &vault);
        assert!(form.to_profile().unwrap().read_only, "saving a locked profile keeps it locked");
        p.read_only = false;
        assert!(!Form::from_profile(&p, &vault).to_profile().unwrap().read_only);
    }

    /// The form says in one line what the driver or the tunnel would only
    /// refuse later, less plainly: SSH port 0, a private key with no file,
    /// a default database name MongoDB doesn't allow.
    #[test]
    fn form_refuses_what_the_driver_would() {
        let (_dir, vault) = temp_vault();
        let p = ConnectionProfile::new("p");
        let form = || Form::from_profile(&p, &vault);
        let ssh = || {
            let mut f = form();
            (f.ssh_on, f.ssh_host, f.ssh_user) = (true, "bastion".into(), "me".into());
            f
        };
        assert!(ssh().to_profile().is_ok(), "a whole SSH setup passes");
        let mut f = ssh();
        f.ssh_port = "0".into();
        assert!(f.to_profile().unwrap_err().contains("SSH port"), "port 0");
        let mut f = ssh();
        f.ssh_key_mode = true;
        f.ssh_key_path = "  ".into();
        assert!(f.to_profile().unwrap_err().contains("private key"), "no key file");
        f.ssh_key_path = "~/.ssh/id_ed25519".into();
        assert!(f.to_profile().is_ok());
        for bad in ["a.b", "with space", "a/b", "x".repeat(64).as_str()] {
            let mut f = form();
            f.default_db = bad.into();
            let e = f.to_profile().unwrap_err();
            assert!(e.contains("Default database"), "{bad:?}: {e}");
        }
        let mut f = form();
        f.default_db = " shop ".into();
        assert_eq!(f.to_profile().unwrap().default_db.as_deref(), Some("shop"));
    }

    /// The connection form offers the lock, ticked for a locked profile.
    #[test]
    fn form_shows_the_read_only_checkbox() {
        let (_dir, vault) = temp_vault();
        let ctx = Context::default();
        theme::install(&ctx);
        let mut dialog = ConnectDialog::new_connection();
        let mut seen = false;
        for _ in 0..3 {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1000.0, 800.0))), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| _ = dialog.show(ui.ctx(), &[], &vault));
            out.textures_delta.clear();
            seen |= out.shapes.iter().any(|s| matches!(&s.shape, egui::Shape::Text(t) if t.galley.text() == "Read-only (refuse every write)"));
        }
        assert!(seen, "the checkbox is drawn");
    }

    /// The form carries a connection's Collection History choice through
    /// (use Settings / on / off) and shows it on the Server tab.
    #[test]
    fn form_keeps_the_history_override() {
        let (_dir, vault) = temp_vault();
        let mut p = ConnectionProfile::new("h");
        for v in [None, Some(true), Some(false)] {
            p.history = v;
            assert_eq!(Form::from_profile(&p, &vault).to_profile().unwrap().history, v);
        }
        let ctx = Context::default();
        theme::install(&ctx);
        let mut dialog = ConnectDialog::new_connection();
        let mut texts = Vec::new();
        for _ in 0..3 {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1000.0, 800.0))), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| _ = dialog.show(ui.ctx(), &[], &vault));
            out.textures_delta.clear();
            texts = out.shapes.iter().filter_map(|s| match &s.shape {
                egui::Shape::Text(t) => Some(t.galley.text().to_string()),
                _ => None,
            }).collect::<Vec<_>>();
        }
        assert!(texts.iter().any(|t| t == "Collection History"), "{texts:?}");
        assert!(texts.iter().any(|t| t == "Use Settings"), "a new connection follows Settings: {texts:?}");
    }

    /// The form notes which secrets the vault holds for a saved connection
    /// (none for a new one or a clone), and Test connection uses them
    /// unless others are typed.
    #[test]
    fn form_reads_saved_secrets_from_the_vault() {
        let (_dir, vault) = temp_vault();
        let mut p = ConnectionProfile::new("saved");
        p.auth = Auth::Password { user: "u".into(), auth_db: "admin".into(), mechanism: None };
        p.ssh = Some(SshConfig { host: "h".into(), port: 22, user: "u".into(), auth: SshAuth::Password });
        vault.set(p.id, SecretKind::Password, "pw").unwrap();
        let mut f = Form::from_profile(&p, &vault);
        assert!(f.has_saved_password && !f.has_saved_ssh_secret);
        assert_eq!(f.secrets(&vault), (Some("pw".to_string()), None));
        f.password = "typed".into();
        f.ssh_secret = "ssh typed".into();
        assert_eq!(f.secrets(&vault), (Some("typed".to_string()), Some("ssh typed".to_string())));
        let new = Form::new(&p);
        assert!(!new.has_saved_password && !new.has_saved_ssh_secret);
        assert_eq!(new.secrets(&vault), (None, None));
    }

    /// Edit catches up with passwords another rongo window saved since this
    /// one unlocked; a vault whose password was changed there asks to unlock
    /// again instead of opening the form.
    #[test]
    fn edit_sees_secrets_saved_in_another_window() {
        use egui_kittest::Harness;
        use egui_kittest::kittest::Queryable;
        struct State {
            dialog: ConnectDialog,
            profiles: Vec<ConnectionProfile>,
            vault: std::sync::Arc<Vault>,
            actions: Vec<ConnectAction>,
        }
        let (_dir, vault) = temp_vault();
        let mut p = ConnectionProfile::new("p");
        p.auth = Auth::Password { user: "u".into(), auth_db: "admin".into(), mechanism: None };
        let other = Vault::unlock(vault.path().to_path_buf(), crate::test_support::TEST_PASSWORD).unwrap();
        other.set(p.id, SecretKind::Password, "from the other window").unwrap();
        assert_eq!(vault.get(p.id, SecretKind::Password), None, "not seen yet");
        let mut dialog = ConnectDialog::new();
        dialog.selected = vec![p.id];
        let state = State { dialog, profiles: vec![p.clone()], vault: vault.clone(), actions: Vec::new() };
        let mut h = Harness::builder().with_size(vec2(1000.0, 800.0)).build_ui_state(
            |ui, s: &mut State| {
                theme::install(ui.ctx());
                let actions = s.dialog.show(ui.ctx(), &s.profiles, &s.vault);
                s.actions.extend(actions);
            },
            state,
        );
        h.run_steps(3);
        h.get_by_label_contains("Edit").click();
        h.run_steps(3);
        let f = h.state().dialog.form.as_ref().expect("the form opens");
        assert!(f.has_saved_password);
        assert_eq!(f.secrets(&vault).0.as_deref(), Some("from the other window"));

        h.state_mut().dialog.form = None;
        h.run_steps(2);
        other.change_password(crate::test_support::TEST_PASSWORD, "another password").unwrap();
        h.get_by_label_contains("Edit").click();
        h.run_steps(3);
        assert!(h.state().dialog.form.is_none());
        assert!(matches!(h.state().actions.last(), Some(ConnectAction::VaultChanged)));
    }

    /// Test connection catches up with a password another rongo window saved
    /// after the form opened, and uses it; a vault whose password was changed
    /// there asks to unlock again and tests nothing.
    #[test]
    fn test_connection_uses_secrets_saved_in_another_window() {
        use egui_kittest::Harness;
        use egui_kittest::kittest::Queryable;
        struct State {
            dialog: ConnectDialog,
            vault: std::sync::Arc<Vault>,
            actions: Vec<ConnectAction>,
        }
        let (_dir, vault) = temp_vault();
        let mut p = ConnectionProfile::new("p");
        p.auth = Auth::Password { user: "u".into(), auth_db: "admin".into(), mechanism: None };
        vault.set(p.id, SecretKind::Password, "old").unwrap();
        let mut dialog = ConnectDialog::new();
        dialog.form = Some(Form::from_profile(&p, &vault));
        let state = State { dialog, vault: vault.clone(), actions: Vec::new() };
        let mut h = Harness::builder().with_size(vec2(1000.0, 800.0)).build_ui_state(
            |ui, s: &mut State| {
                theme::install(ui.ctx());
                let actions = s.dialog.show(ui.ctx(), &[], &s.vault);
                s.actions.extend(actions);
            },
            state,
        );
        h.run_steps(3);
        let other = Vault::unlock(vault.path().to_path_buf(), crate::test_support::TEST_PASSWORD).unwrap();
        other.set(p.id, SecretKind::Password, "new").unwrap();
        h.get_by_label_contains("Test connection").click();
        h.run_steps(2);
        assert!(matches!(h.state().actions.last(), Some(ConnectAction::Test(req)) if req.password.as_deref() == Some("new")), "the password saved since");

        h.state_mut().actions.clear();
        other.change_password(crate::test_support::TEST_PASSWORD, "another password").unwrap();
        h.get_by_label_contains("Test connection").click();
        h.run_steps(2);
        assert!(matches!(h.state().actions.as_slice(), [ConnectAction::VaultChanged]), "asks to unlock again, tests nothing");
    }

    /// Edit while another window holds the vault's lock: the form still
    /// opens from this window's copy, and an error toast says why.
    #[test]
    fn edit_with_a_busy_vault_says_so() {
        use egui_kittest::Harness;
        use egui_kittest::kittest::Queryable;
        struct State {
            dialog: ConnectDialog,
            profiles: Vec<ConnectionProfile>,
            vault: std::sync::Arc<Vault>,
            actions: Vec<ConnectAction>,
        }
        let (_dir, vault) = temp_vault();
        let p = ConnectionProfile::new("p");
        let mut dialog = ConnectDialog::new();
        dialog.selected = vec![p.id];
        let state = State { dialog, profiles: vec![p], vault: vault.clone(), actions: Vec::new() };
        let mut h = Harness::builder().with_size(vec2(1000.0, 800.0)).build_ui_state(
            |ui, s: &mut State| {
                theme::install(ui.ctx());
                let actions = s.dialog.show(ui.ctx(), &s.profiles, &s.vault);
                s.actions.extend(actions);
            },
            state,
        );
        h.run_steps(3);
        let held = std::fs::File::options().read(true).write(true).create(true).truncate(false).open(vault.path().with_extension("lock")).unwrap();
        held.lock().unwrap();
        h.get_by_label_contains("Edit").click();
        h.run_steps(3);
        drop(held);
        assert!(h.state().dialog.has_form(), "opens from this window's copy");
        let notices: Vec<&str> = h.state().actions.iter().filter_map(|a| if let ConnectAction::Notice(t) = a { Some(t.as_str()) } else { None }).collect();
        assert_eq!(notices.len(), 1, "one toast");
        assert!(notices[0].contains("busy"), "{notices:?}");
        assert!(!h.state().actions.iter().any(|a| matches!(a, ConnectAction::VaultChanged)));
    }

    /// Save closes the form, but a Save that failed because the vault was
    /// changed elsewhere brings it back as typed, secrets included, with
    /// the message on it. A Save that went through lets it go.
    #[test]
    fn a_failed_save_brings_the_form_back_as_typed() {
        use egui_kittest::Harness;
        use egui_kittest::kittest::Queryable;
        struct State {
            dialog: ConnectDialog,
            profiles: Vec<ConnectionProfile>,
            vault: std::sync::Arc<Vault>,
            actions: Vec<ConnectAction>,
        }
        let (_dir, vault) = temp_vault();
        let mut dialog = ConnectDialog::new_connection();
        {
            let f = dialog.form.as_mut().unwrap();
            (f.auth_on, f.user) = (true, "u".into());
            f.password = "typed secret".into();
            f.ssh_secret = "typed ssh secret".into();
        }
        let state = State { dialog, profiles: Vec::new(), vault, actions: Vec::new() };
        let mut h = Harness::builder().with_size(vec2(1000.0, 800.0)).build_ui_state(
            |ui, s: &mut State| {
                theme::install(ui.ctx());
                let actions = s.dialog.show(ui.ctx(), &s.profiles, &s.vault);
                s.actions.extend(actions);
            },
            state,
        );
        h.run_steps(3);
        h.get_by_label("Save").click();
        h.step();
        assert!(matches!(h.state().actions.last(), Some(ConnectAction::Save { password: Some(p), .. }) if p == "typed secret"));
        assert!(!h.state().dialog.has_form(), "Save closes the form");

        h.state_mut().dialog.restore_form("The vault was changed".into());
        assert!(h.state().dialog.has_form());
        assert_eq!(h.state().dialog.form_error(), Some("The vault was changed"));
        let f = h.state().dialog.form.as_ref().unwrap();
        assert_eq!((f.password.as_str(), f.ssh_secret.as_str()), ("typed secret", "typed ssh secret"));

        // Saved for good: the form is gone, and a late restore finds nothing to bring back.
        h.run_steps(3);
        assert!(h.query_by_label_contains("The vault was changed").is_some(), "the message is on the form");
        h.get_by_label("Save").click();
        h.run_steps(3);
        assert!(!h.state().dialog.has_form());
        assert!(h.state().dialog.saved.is_none(), "not kept past the next frame");
        h.state_mut().dialog.restore_form("late".into());
        assert!(!h.state().dialog.has_form());
    }

    /// A secret typed while its method is off (password authentication, the
    /// SSH tunnel) is not sent with Save; with the method on, it is. (Turned
    /// off, the saved one is then deleted by the store, in the same write.)
    #[test]
    fn save_sends_no_secret_for_a_method_that_is_off() {
        use egui_kittest::Harness;
        use egui_kittest::kittest::Queryable;
        struct State {
            dialog: ConnectDialog,
            vault: std::sync::Arc<Vault>,
            actions: Vec<ConnectAction>,
        }
        let (_dir, vault) = temp_vault();
        let mut p = ConnectionProfile::new("p");
        p.auth = Auth::Password { user: "u".into(), auth_db: "admin".into(), mechanism: None };
        p.ssh = Some(SshConfig { host: "h".into(), port: 22, user: "u".into(), auth: SshAuth::Password });
        let mut dialog = ConnectDialog::new();
        dialog.form = Some(Form::from_profile(&p, &vault));
        let state = State { dialog, vault, actions: Vec::new() };
        let mut h = Harness::builder().with_size(vec2(1000.0, 800.0)).build_ui_state(
            |ui, s: &mut State| {
                theme::install(ui.ctx());
                let actions = s.dialog.show(ui.ctx(), &[], &s.vault);
                s.actions.extend(actions);
            },
            state,
        );
        let save = |h: &mut Harness<'_, State>, auth_on: bool, ssh_on: bool| {
            let mut f = Form::from_profile(&p, &h.state().vault);
            (f.password, f.ssh_secret, f.auth_on, f.ssh_on) = ("typed pw".into(), "typed ssh".into(), auth_on, ssh_on);
            h.state_mut().dialog.form = Some(f);
            h.run_steps(3);
            h.get_by_label("Save").click();
            h.run_steps(2);
            match h.state_mut().actions.pop() {
                Some(ConnectAction::Save { profile, password, ssh_secret, .. }) => (profile.needs_password(), profile.ssh.is_some(), password, ssh_secret),
                _ => panic!("no Save"),
            }
        };
        assert_eq!(save(&mut h, true, true), (true, true, Some("typed pw".into()), Some("typed ssh".into())));
        assert_eq!(save(&mut h, false, true), (false, true, None, Some("typed ssh".into())));
        assert_eq!(save(&mut h, true, false), (true, false, Some("typed pw".into()), None));
        assert_eq!(save(&mut h, false, false), (false, false, None, None));
    }

    /// In a window narrower than the dialog, the dialog shrinks to fit:
    /// every row's checkbox and the Connect button stay on screen and work,
    /// and the columns scroll instead of being cut.
    #[test]
    fn manager_fits_a_narrow_window() {
        let (_dir, vault) = temp_vault();
        let ctx = Context::default();
        theme::install(&ctx);
        let names = ["alpha-with-a-very-long-connection-name-1", "bravo-with-a-very-long-connection-name-2", "charlie-3"];
        let profiles = names.map(ConnectionProfile::new);
        let mut dialog = ConnectDialog::new();
        let width = 380.0;
        let frame = |dialog: &mut ConnectDialog, events: Vec<egui::Event>| {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(width, 700.0))), events, ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| _ = dialog.show(ui.ctx(), &profiles, &vault));
            out.textures_delta.clear();
            let boxes: Vec<egui::Rect> = out.shapes.iter().filter_map(|s| match &s.shape {
                egui::Shape::Rect(r) if (13.5..=16.5).contains(&r.rect.width()) && (r.rect.width() - r.rect.height()).abs() < 0.5 => Some(r.rect),
                _ => None,
            }).collect();
            let connect = out.shapes.iter().find_map(|s| match &s.shape {
                egui::Shape::Text(t) if t.galley.text().ends_with("Connect") => Some(t.galley.rect.translate(t.pos.to_vec2())),
                _ => None,
            });
            (boxes, connect)
        };
        for _ in 0..30 {
            frame(&mut dialog, vec![]);
        }
        let (boxes, connect) = frame(&mut dialog, vec![]);
        assert_eq!(boxes.len(), names.len() + 1, "a checkbox per row and one in the header: {boxes:?}");
        assert!(boxes.iter().all(|b| b.left() >= 0.0 && b.right() <= width), "checkboxes on screen: {boxes:?}");
        let connect = connect.expect("Connect button");
        assert!(connect.left() >= 0.0 && connect.right() <= width, "Connect on screen: {connect:?}");

        // Tick the first row.
        let first = boxes.iter().filter(|b| b.top() > boxes.iter().map(|b| b.top()).fold(f32::MAX, f32::min) + 1.0).min_by(|a, b| a.top().total_cmp(&b.top())).unwrap().center();
        let press = |pressed| egui::Event::PointerButton { pos: first, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        frame(&mut dialog, vec![egui::Event::PointerMoved(first)]);
        frame(&mut dialog, vec![press(true)]);
        frame(&mut dialog, vec![press(false)]);
        assert_eq!(dialog.selected, vec![profiles[0].id]);
    }

    /// ⌘-click or a row's checkbox adds rows, ⇧-click picks a range, the
    /// header checkbox picks all or none, and Connect opens every picked row
    /// in list order; Edit needs a single row.
    #[test]
    fn manager_connects_several() {
        let (_dir, vault) = temp_vault();
        let ctx = Context::default();
        theme::install(&ctx);
        let names = ["alpha", "bravo", "charlie", "delta"];
        let profiles = names.map(ConnectionProfile::new);
        let id = |n: &str| profiles[names.iter().position(|x| *x == n).unwrap()].id;
        let mut dialog = ConnectDialog::new();
        let frame = |dialog: &mut ConnectDialog, mut events: Vec<egui::Event>, modifiers| {
            events.insert(0, egui::Event::ModifiersChanged(modifiers));
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1400.0, 900.0))),
                events,
                ..Default::default()
            };
            let mut actions = Vec::new();
            let mut out = ctx.run_ui(input, |ui| actions = dialog.show(ui.ctx(), &profiles, &vault));
            out.textures_delta.clear();
            let texts: HashMap<String, egui::Pos2> = out
                .shapes
                .iter()
                .filter_map(|s| match &s.shape {
                    egui::Shape::Text(t) => Some((t.galley.text().trim_start_matches(|c: char| !c.is_ascii()).trim().to_string(), t.galley.rect.translate(t.pos.to_vec2()).center())),
                    _ => None,
                })
                .collect();
            // Checkbox frames: squares of the style's icon size.
            let boxes: Vec<egui::Pos2> = out
                .shapes
                .iter()
                .filter_map(|s| match &s.shape {
                    egui::Shape::Rect(r) if (13.5..=16.5).contains(&r.rect.width()) && (r.rect.width() - r.rect.height()).abs() < 0.5 => Some(r.rect.center()),
                    _ => None,
                })
                .collect();
            (actions, texts, boxes)
        };
        // The text `label`, or with `tick` the checkbox at the start of its row.
        let find = |dialog: &mut ConnectDialog, label: &str, tick: bool| {
            let (_, texts, boxes) = frame(dialog, vec![], egui::Modifiers::NONE);
            let at = *texts.get(label).unwrap_or_else(|| panic!("{label:?} in {:?}", texts.keys()));
            if !tick {
                return at;
            }
            *boxes.iter().filter(|b| (b.y - at.y).abs() < 4.0 && b.x < at.x).min_by(|a, b| a.x.total_cmp(&b.x)).unwrap_or_else(|| panic!("a checkbox on {label:?}'s row"))
        };
        let press = |dialog: &mut ConnectDialog, at: egui::Pos2, m: egui::Modifiers| {
            let button = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: m };
            frame(dialog, vec![egui::Event::PointerMoved(at)], m);
            frame(dialog, vec![button(true)], m);
            let actions = frame(dialog, vec![button(false)], m).0;
            // Past the double-click window, so the next click is a new one.
            ctx.input_mut(|i| i.pointer = Default::default());
            actions
        };
        let click = |dialog: &mut ConnectDialog, label: &str, m: egui::Modifiers| {
            let at = find(dialog, label, false);
            press(dialog, at, m)
        };
        let tick = |dialog: &mut ConnectDialog, label: &str| {
            let at = find(dialog, label, true);
            press(dialog, at, egui::Modifiers::NONE)
        };
        let connects = |actions: Vec<ConnectAction>| actions.into_iter().filter_map(|a| if let ConnectAction::Connect(id) = a { Some(id) } else { None }).collect::<Vec<_>>();
        for _ in 0..30 {
            frame(&mut dialog, vec![], egui::Modifiers::NONE);
        }

        click(&mut dialog, "charlie", egui::Modifiers::NONE);
        click(&mut dialog, "alpha", egui::Modifiers::COMMAND);
        assert_eq!(connects(click(&mut dialog, "Connect 2", egui::Modifiers::NONE)), [id("alpha"), id("charlie")]);

        // Edit does nothing with two rows picked.
        click(&mut dialog, "Edit", egui::Modifiers::NONE);
        assert!(dialog.form.is_none(), "edit opened with two rows picked");

        // ⌘-click again drops a row.
        click(&mut dialog, "alpha", egui::Modifiers::COMMAND);
        assert_eq!(connects(click(&mut dialog, "Connect", egui::Modifiers::NONE)), [id("charlie")]);

        click(&mut dialog, "delta", egui::Modifiers::NONE);
        click(&mut dialog, "bravo", egui::Modifiers::SHIFT);
        assert_eq!(connects(click(&mut dialog, "Connect 3", egui::Modifiers::NONE)), [id("bravo"), id("charlie"), id("delta")]);

        // Checkboxes add and drop rows without holding a key.
        click(&mut dialog, "alpha", egui::Modifiers::NONE);
        tick(&mut dialog, "charlie");
        assert_eq!(connects(click(&mut dialog, "Connect 2", egui::Modifiers::NONE)), [id("alpha"), id("charlie")]);
        tick(&mut dialog, "alpha");
        assert_eq!(connects(click(&mut dialog, "Connect", egui::Modifiers::NONE)), [id("charlie")]);

        // The header checkbox picks every row, then none.
        tick(&mut dialog, "Name");
        assert_eq!(connects(click(&mut dialog, "Connect 4", egui::Modifiers::NONE)), names.map(id));
        tick(&mut dialog, "Name");
        assert!(dialog.selected.is_empty());
        assert!(connects(click(&mut dialog, "Connect", egui::Modifiers::NONE)).is_empty(), "nothing to connect");

        // A plain click goes back to one row, which Edit opens.
        click(&mut dialog, "bravo", egui::Modifiers::NONE);
        click(&mut dialog, "Edit", egui::Modifiers::NONE);
        assert!(dialog.form.as_ref().is_some_and(|f| f.id == id("bravo")), "edit opens the one picked row");
    }

    /// Export… (with one connection picked) and a row's right-click menu
    /// both ask to export that connection.
    #[test]
    fn manager_exports_a_connection() {
        let (_dir, vault) = temp_vault();
        let ctx = Context::default();
        theme::install(&ctx);
        let profiles = ["alpha", "bravo"].map(ConnectionProfile::new);
        let mut dialog = ConnectDialog::new();
        let frame = |dialog: &mut ConnectDialog, events: Vec<egui::Event>| {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1200.0, 800.0))), events, ..Default::default() };
            let mut actions = Vec::new();
            let mut out = ctx.run_ui(input, |ui| actions = dialog.show(ui.ctx(), &profiles, &vault));
            out.textures_delta.clear();
            let texts: HashMap<String, egui::Pos2> = out
                .shapes
                .iter()
                .filter_map(|s| match &s.shape {
                    egui::Shape::Text(t) => Some((t.galley.text().trim_start_matches(|c: char| !c.is_ascii()).trim().to_string(), t.galley.rect.translate(t.pos.to_vec2()).center())),
                    _ => None,
                })
                .collect();
            (actions, texts)
        };
        let click = |dialog: &mut ConnectDialog, label: &str, button: egui::PointerButton| {
            let mut texts = HashMap::new();
            for _ in 0..3 {
                texts = frame(dialog, vec![]).1;
            }
            let at = *texts.get(label).unwrap_or_else(|| panic!("{label:?} in {:?}", texts.keys()));
            let press = |pressed| egui::Event::PointerButton { pos: at, button, pressed, modifiers: egui::Modifiers::NONE };
            let mut actions = frame(dialog, vec![egui::Event::PointerMoved(at)]).0;
            actions.extend(frame(dialog, vec![press(true)]).0);
            actions.extend(frame(dialog, vec![press(false)]).0);
            actions
        };
        let exports = |actions: Vec<ConnectAction>| actions.into_iter().filter_map(|a| if let ConnectAction::Export(id) = a { Some(id) } else { None }).collect::<Vec<_>>();
        use egui::PointerButton::{Primary, Secondary};
        assert!(exports(click(&mut dialog, "Export…", Primary)).is_empty(), "nothing picked");
        click(&mut dialog, "bravo", Primary);
        assert_eq!(exports(click(&mut dialog, "Export…", Primary)), [profiles[1].id]);
        assert!(exports(click(&mut dialog, "alpha", Secondary)).is_empty());
        assert_eq!(exports(click(&mut dialog, "Export Connection…", Primary)), [profiles[0].id]);
    }

    /// In the Connection Manager a row looks like the connection in the tree
    /// (tint and bar in its colour), dragging a row asks to move it, and the
    /// colour button opens the colours.
    #[test]
    fn manager_reorders_and_colours() {
        let (_dir, vault) = temp_vault();
        let ctx = Context::default();
        theme::install(&ctx);
        let [alpha, bravo, charlie] = ["alpha", "bravo", "charlie"].map(ConnectionProfile::new);
        let alpha = ConnectionProfile { color: Some("blue".into()), ..alpha };
        let profiles = [alpha.clone(), bravo.clone(), charlie.clone()];
        let [red, blue] = ["red", "blue"].map(|n| theme::conn_color(Some(n)).unwrap());
        let mut dialog = ConnectDialog::new();
        struct Seen {
            actions: Vec<ConnectAction>,
            names: HashMap<String, egui::Rect>,
            circles: Vec<egui::epaint::CircleShape>,
            rects: Vec<egui::epaint::RectShape>,
            carets: Vec<egui::Pos2>,
        }
        let mut frame = |events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1400.0, 900.0))),
                events,
                ..Default::default()
            };
            let mut actions = Vec::new();
            let mut out = ctx.run_ui(input, |ui| actions = dialog.show(ui.ctx(), &profiles, &vault));
            out.textures_delta.clear();
            let mut seen = Seen { actions, names: HashMap::new(), circles: Vec::new(), rects: Vec::new(), carets: Vec::new() };
            for s in &out.shapes {
                match &s.shape {
                    egui::Shape::Text(t) => {
                        let rect = t.galley.rect.translate(t.pos.to_vec2());
                        if t.galley.text() == icon::CARET_DOWN {
                            seen.carets.push(rect.center());
                        } else if let Some(n) = ["alpha", "bravo", "charlie"].into_iter().find(|n| t.galley.text().ends_with(&format!("  {n}"))) {
                            seen.names.insert(n.to_string(), rect);
                        }
                    }
                    egui::Shape::Circle(c) => seen.circles.push(*c),
                    egui::Shape::Rect(r) => seen.rects.push(r.clone()),
                    _ => {}
                }
            }
            seen
        };
        let button = |pos, pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        let click = |frame: &mut dyn FnMut(Vec<egui::Event>) -> Seen, at| {
            frame(vec![egui::Event::PointerMoved(at)]);
            frame(vec![button(at, true)]);
            frame(vec![button(at, false)]).actions
        };

        // Let the dialog finish fading in.
        for _ in 0..30 {
            frame(vec![]);
        }
        let Seen { names, circles, rects, carets, .. } = frame(vec![]);
        let row = |n: &str| names[n];
        let alpha_y = row("alpha").center().y;
        let at_alpha = |r: &egui::Rect| (r.center().y - alpha_y).abs() < 4.0;
        assert!(rects.iter().any(|r| r.fill == blue.gamma_multiply(0.22) && r.rect.contains(row("alpha").center())), "alpha's row is tinted, as in the tree");
        assert!(rects.iter().any(|r| r.fill == blue && r.rect.width() == 3.0 && at_alpha(&r.rect)), "and has the bar at its left");
        assert!(rects.iter().any(|r| r.fill == blue && r.rect.width() == 12.0 && at_alpha(&r.rect) && r.rect.right() < row("alpha").left()), "the colour button shows blue");
        assert!(!circles.iter().any(|c| c.fill == blue), "no round dot that looks like a status light");

        // Drag charlie above alpha.
        let (from, to) = (row("charlie").center(), row("alpha").center() - vec2(0.0, 4.0));
        frame(vec![egui::Event::PointerMoved(from)]);
        frame(vec![button(from, true)]);
        let mut actions = Vec::new();
        for i in 1..=10 {
            actions.extend(frame(vec![egui::Event::PointerMoved(from.lerp(to, i as f32 / 10.0))]).actions);
        }
        actions.extend(frame(vec![button(to, false)]).actions);
        let moves: Vec<_> = actions.iter().filter_map(|a| if let ConnectAction::Move { id, before } = a { Some((*id, *before)) } else { None }).collect();
        assert_eq!(moves, [(charlie.id, Some(alpha.id))]);

        // Bravo's colour button (by its caret), then red.
        let caret = *carets.iter().find(|c| (c.y - row("bravo").center().y).abs() < 4.0 && c.x < row("bravo").left()).expect("bravo's colour button");
        click(&mut frame, caret);
        for _ in 0..30 {
            frame(vec![]);
        }
        let swatch = frame(vec![]).circles.into_iter().find(|c| c.fill == red).expect("the colours opened").center;
        let actions = click(&mut frame, swatch);
        assert!(actions.iter().any(|a| matches!(a, ConnectAction::SetColor(id, Some(c)) if *id == bravo.id && c == "red")));
    }

    /// A failed test against a replica set reports every server, a message
    /// many lines long. The form keeps a steady height through repaints and
    /// its buttons stay on screen, instead of growing a little every frame
    /// until it fills the window.
    #[test]
    fn form_keeps_its_height_after_a_long_error() {
        let (_dir, vault) = temp_vault();
        let ctx = Context::default();
        theme::install(&ctx);
        let mut dialog = ConnectDialog::new_connection();
        dialog.test = TestState::Failed(crate::test_support::long_driver_error());
        let screen = vec2(1200.0, 1000.0);
        let mut bottom = || {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, screen)), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| _ = dialog.show(ui.ctx(), &[], &vault));
            out.textures_delta.clear();
            out.shapes.iter().find_map(|s| match &s.shape {
                egui::Shape::Text(t) if t.galley.text().ends_with("Save & Connect") => Some(t.pos.y + t.galley.rect.bottom()),
                _ => None,
            })
        };
        // The first frame only measures the dialog.
        let settled = (0..5).filter_map(|_| bottom()).last().expect("Save & Connect");
        let later = (0..60).filter_map(|_| bottom()).last().expect("Save & Connect");
        assert!((later - settled).abs() < 1.0, "the footer moved from {settled} to {later}");
        assert!(later < screen.y - 150.0, "the dialog reached the bottom of the window: footer at {later}");
    }

    /// The Connection Manager keeps a steady height through repaints, also
    /// when its footer is taller than usual (bigger buttons): the list is
    /// sized from the window and the footer as measured, not from the room
    /// the dialog had last frame, which grew it every frame.
    #[test]
    fn manager_keeps_its_height_with_a_tall_footer() {
        let (_dir, vault) = temp_vault();
        let profiles: Vec<ConnectionProfile> = (0..30).map(|i| ConnectionProfile::new(format!("connection {i}"))).collect();
        for button_h in [None, Some(44.0)] {
            let ctx = Context::default();
            theme::install(&ctx);
            if let Some(h) = button_h {
                ctx.all_styles_mut(|s| s.spacing.interact_size.y = h);
            }
            let mut dialog = ConnectDialog::new();
            let screen = vec2(1200.0, 900.0);
            let mut bottom = || {
                let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, screen)), ..Default::default() };
                let mut out = ctx.run_ui(input, |ui| _ = dialog.show(ui.ctx(), &profiles, &vault));
                out.textures_delta.clear();
                out.shapes.iter().find_map(|s| match &s.shape {
                    egui::Shape::Text(t) if t.galley.text().ends_with("Connect") => Some(t.pos.y + t.galley.rect.bottom()),
                    _ => None,
                })
            };
            let settled = (0..5).filter_map(|_| bottom()).last().expect("Connect");
            let later = (0..60).filter_map(|_| bottom()).last().expect("Connect");
            assert!((later - settled).abs() < 1.0, "{button_h:?}: the footer moved from {settled} to {later}");
            assert!(later < screen.y - 100.0, "{button_h:?}: the dialog reached the bottom of the window: footer at {later}");
        }
    }

    /// On the smallest window the app allows, a big document with a long
    /// validation error still leaves Save and Cancel on screen, with the
    /// find bar open too; and View Document's Close stays on screen.
    #[test]
    fn doc_dialog_fits_a_short_window() {
        let mut big = Document::new();
        for i in 0..80 {
            big.insert(format!("field_{i}"), format!("value {i}"));
        }
        let screen = vec2(960.0, 600.0);
        let drawn = |ctx: &Context, draw: &mut dyn FnMut(&Context), labels: &[&str]| {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, screen)), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| draw(ui.ctx()));
            out.textures_delta.clear();
            let find = |label: &str| out.shapes.iter().find_map(|s| match &s.shape {
                egui::Shape::Text(t) if t.galley.text() == label => Some(t.galley.rect.translate(t.pos.to_vec2())),
                _ => None,
            });
            labels.iter().map(|l| find(l)).collect::<Option<Vec<_>>>()
        };
        for find_open in [false, true] {
            let ctx = Context::default();
            theme::install(&ctx);
            let conn = ConnectionProfile::mock().conn_id();
            let mut dialog = DocDialog::insert(conn, Namespace::new("db", "c"), Some(&big), None);
            dialog.error = Some(format!("Saving the document failed: server error 121: Document failed validation {}", crate::test_support::long_driver_error().repeat(3)));
            dialog.find = find_open.then(DocFind::default);
            let rects = (0..30).filter_map(|_| drawn(&ctx, &mut |ctx| _ = dialog.show(ctx), &["Save", "Cancel"])).last().expect("Save and Cancel drawn");
            for (name, r) in ["Save", "Cancel"].iter().zip(rects) {
                assert!(r.bottom() <= screen.y - 4.0 && r.top() >= 0.0, "find bar open: {find_open}: {name} off screen: {r:?}");
            }

            let ctx = Context::default();
            theme::install(&ctx);
            let mut view = ViewDocDialog::new(&big);
            view.find = find_open.then(DocFind::default);
            let rects = (0..30).filter_map(|_| drawn(&ctx, &mut |ctx| _ = view.show(ctx), &["Close"])).last().expect("Close drawn");
            assert!(rects[0].bottom() <= screen.y - 4.0 && rects[0].top() >= 0.0, "View Document, find bar open: {find_open}: Close off screen: {:?}", rects[0]);
        }
    }

    /// The find bar while `doc_frame_ms` times the dialogs: closed, open on
    /// a needle, or typed into every frame (each frame another needle, so a
    /// search of the whole text, as a keystroke in the field costs).
    #[derive(Clone, Copy)]
    enum BarIs {
        Closed,
        Open(&'static str),
        Typing(&'static str),
    }

    /// Milliseconds per frame of the document editor and of View Document
    /// showing a document of `fields` fields, with no input but `bar`'s.
    fn doc_frame_ms(fields: usize, bar: BarIs) -> (f64, f64) {
        let ctx = Context::default();
        theme::install(&ctx);
        let mut doc = Document::new();
        for i in 0..fields {
            doc.insert(format!("field_{i}"), format!("value number {i} of the document"));
        }
        let conn = ConnectionProfile::mock().conn_id();
        let mut edit = DocDialog::edit(conn, Namespace::new("db", "c"), &doc, None);
        let mut view = ViewDocDialog::new(&doc);
        if let BarIs::Open(needle) | BarIs::Typing(needle) = bar {
            edit.find = Some(DocFind::typed(needle));
            view.find = Some(DocFind::typed(needle));
        }
        // Typing: the needle, then it without its last letter, by turns.
        let typing = |find: &mut Option<DocFind>, frame: usize| {
            if let (BarIs::Typing(needle), Some(f)) = (bar, find) {
                let typed = f.needle_mut();
                typed.clear();
                typed.push_str(if frame.is_multiple_of(2) { needle } else { &needle[..needle.len() - 1] });
            }
        };
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1200.0, 900.0));
        let time = |draw: &mut dyn FnMut(&Context, usize)| {
            let mut n = 0;
            let mut frame = || {
                n += 1;
                let mut out = ctx.run_ui(egui::RawInput { screen_rect: Some(screen), ..Default::default() }, |ui| draw(ui.ctx(), n));
                out.textures_delta.clear();
            };
            frame();
            frame();
            // The fastest of several rounds: on a loaded machine another
            // process can slow any one round, but not every one.
            (0..5)
                .map(|_| {
                    let t = std::time::Instant::now();
                    for _ in 0..4 {
                        frame();
                    }
                    t.elapsed().as_secs_f64() * 1000.0 / 4.0
                })
                .fold(f64::INFINITY, f64::min)
        };
        let edit_ms = time(&mut |ctx, n| {
            typing(&mut edit.find, n);
            _ = edit.show(ctx);
        });
        let view_ms = time(&mut |ctx, n| {
            typing(&mut view.find, n);
            _ = view.show(ctx);
        });
        (edit_ms, view_ms)
    }

    /// Edit Document and View Document with a 90 KB document cost about
    /// what they do with a small one per frame, the find bar closed or
    /// open: the text isn't tokenized and laid out again while it is
    /// unchanged. (Too coarse to see a search of 90 KB a frame:
    /// `find_follows_the_text_and_runs_only_on_a_change` counts searches,
    /// and `find_marks_only_the_rows_in_view` counts the rows looked at.)
    #[test]
    fn doc_dialogs_frame_cost_does_not_grow_with_the_document() {
        for bar in [BarIs::Closed, BarIs::Open("number 1")] {
            let (small_edit, small_view) = doc_frame_ms(20, bar);
            let (big_edit, big_view) = doc_frame_ms(2_000, bar);
            let open = matches!(bar, BarIs::Open(_));
            assert!(big_edit < small_edit * 3.0 + 5.0, "Edit Document, find bar open: {open}: {big_edit:.1} ms per frame for 2,000 fields vs {small_edit:.1} ms for 20");
            assert!(big_view < small_view * 3.0 + 5.0, "View Document, find bar open: {open}: {big_view:.1} ms per frame for 2,000 fields vs {small_view:.1} ms for 20");
        }
    }

    /// Prints the document dialogs' frame times, with the find bar closed,
    /// open, and typed into each frame (each frame a search of the whole
    /// text). Run with `cargo test -p rongo --release -- --ignored
    /// --nocapture report_doc_dialog_frame_times`.
    #[test]
    #[ignore = "prints timings"]
    fn report_doc_dialog_frame_times() {
        for fields in [20, 2_000, 4_000, 40_000, 200_000] {
            let mut doc = Document::new();
            for i in 0..fields {
                doc.insert(format!("field_{i}"), format!("value number {i} of the document"));
            }
            let kb = format::doc_to_shell(&doc, Style::Editable).len() / 1024;
            // Only the last field matches: the view scrolls to the end, and
            // each frame walks every row above it to place the marks.
            let last: &'static str = Box::leak(format!("number {} of", fields - 1).into_boxed_str());
            let bars = [
                (BarIs::Closed, "find bar closed"),
                (BarIs::Open("number 12"), "find bar open"),
                (BarIs::Open(last), "find bar open at the end"),
                (BarIs::Typing("number 12"), "typing a needle"),
                (BarIs::Typing("ümber 12"), "typing a non-ASCII needle"),
            ];
            for (bar, name) in bars {
                let (edit, view) = doc_frame_ms(fields, bar);
                println!("{fields} fields ({kb} KB), {name}: Edit Document {edit:.2} ms, View Document {view:.2} ms per frame");
            }
        }
    }

    /// The document editor in a harness, noting when it asks to close and
    /// what it saves.
    struct DocProbe {
        dialog: DocDialog,
        closed: bool,
        saved: Vec<Document>,
    }

    fn doc_harness(doc: &Document) -> egui_kittest::Harness<'static, DocProbe> {
        let conn = ConnectionProfile::mock().conn_id();
        let dialog = DocDialog::edit(conn, Namespace::new("db", "c"), doc, None);
        let mut h = egui_kittest::Harness::builder().with_size(vec2(1000.0, 800.0)).build_ui_state(
            |ui, p: &mut DocProbe| {
                theme::install(ui.ctx());
                match p.dialog.show(ui.ctx()) {
                    Some(DocAction::Close) => p.closed = true,
                    Some(DocAction::Save(d)) => p.saved.push(d),
                    None => {}
                }
            },
            DocProbe { dialog, closed: false, saved: Vec::new() },
        );
        h.run_steps(3);
        h
    }

    /// ↩ in a question's field goes ahead on the rule its OK button follows:
    /// text typed, and exactly the name when one must be typed. (It did
    /// nothing: the field asked for its focus back before ↩ was looked at.)
    #[test]
    fn enter_in_a_question_follows_its_ok_button() {
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;
        let enter = |c: Confirm, typed: &str| {
            let mut h = crate::test_support::harness(egui::vec2(800.0, 600.0), 2, (c, None), |ctx, (c, yes): &mut (Confirm, Option<bool>)| {
                if let Some(r) = c.show(ctx) {
                    yes.get_or_insert(matches!(r, ConfirmResult::Yes));
                }
            });
            if !typed.is_empty() {
                h.get_by_role(Role::TextInput).type_text(typed);
                h.run_steps(2);
            }
            let ok = h.state().0.ok_enabled();
            h.key_press(Key::Enter);
            h.run_steps(2);
            assert_eq!(h.state().1 == Some(true), ok, "typed {typed:?}");
            ok
        };
        let name = || Confirm::new("Drop Database", "Type shop".into(), "Drop", Pending::DropDatabase { conn: ConnId(uuid::Uuid::nil()), db: "shop".into() }).input(String::new()).must_type("shop".into());
        assert!(!enter(name(), ""));
        assert!(!enter(name(), "sh"));
        assert!(enter(name(), "shop"));
        let named = || Confirm::new("Add Collection", "Name:".into(), "Create", Pending::CreateCollection { conn: ConnId(uuid::Uuid::nil()), db: "d".into() }).input(String::new());
        assert!(!enter(named(), "   "));
        assert!(enter(named(), "c"));
    }

    /// A click outside the document editor, or Esc, closes it only while
    /// nothing was typed into it; after typing only Cancel closes it, so
    /// minutes of editing aren't lost to a stray click.
    #[test]
    fn doc_dialog_keeps_typed_work_on_an_outside_click_or_esc() {
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;
        let doc = rongo_core::bson::doc! { "_id": 1, "name": "a" };
        let mut h = doc_harness(&doc);
        crate::test_support::click_outside(&mut h);
        assert!(h.state().closed, "nothing typed: a click outside closes it");
        let mut h = doc_harness(&doc);
        h.key_press(Key::Escape);
        h.run_steps(2);
        assert!(h.state().closed, "nothing typed: Esc closes it");

        let mut h = doc_harness(&doc);
        h.get_by_role(Role::MultilineTextInput).focus();
        h.run_steps(2);
        h.get_by_role(Role::MultilineTextInput).type_text("x");
        h.run_steps(2);
        assert!(h.state().dialog.text().contains('x'), "typed: {}", h.state().dialog.text());
        crate::test_support::click_outside(&mut h);
        assert!(!h.state().closed, "a click outside keeps typed work");
        h.key_press(Key::Escape);
        h.run_steps(2);
        assert!(!h.state().closed, "Esc keeps typed work");
        h.get_by_label("Cancel").click();
        h.run_steps(2);
        assert!(h.state().closed, "Cancel closes it");
    }

    /// ⌘S saves the document; ⌘⇧S is another shortcut and saves nothing.
    #[test]
    fn doc_dialog_saves_on_cmd_s_but_not_cmd_shift_s() {
        let doc = rongo_core::bson::doc! { "_id": 1, "name": "a" };
        let mut h = doc_harness(&doc);
        h.key_press_modifiers(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, Key::S);
        h.run_steps(2);
        assert!(h.state().saved.is_empty(), "⌘⇧S saved the document");
        h.key_press_modifiers(egui::Modifiers::COMMAND, Key::S);
        h.run_steps(2);
        assert_eq!(h.state().saved, [doc]);
    }

    /// "apple" three times ignoring case, twice matching it.
    fn apples() -> Document {
        rongo_core::bson::doc! { "_id": 1, "a": "apple", "b": "Apple pie", "c": "pineapple" }
    }

    /// View Document in a harness, noting when it asks to close.
    struct ViewProbe {
        dialog: ViewDocDialog,
        closed: bool,
    }

    fn view_harness(doc: &Document) -> egui_kittest::Harness<'static, ViewProbe> {
        let probe = ViewProbe { dialog: ViewDocDialog::new(doc), closed: false };
        crate::test_support::harness(vec2(1000.0, 800.0), 3, probe, |ctx, p: &mut ViewProbe| p.closed |= p.dialog.show(ctx))
    }

    /// ⌘F in the document editor opens a find bar above the text with its
    /// field focused. Typing finds: "1 of 3", every match marked behind
    /// the text and the current one apart; ↩ and ⇧↩ step round while the
    /// field keeps its focus, Aa matches case, and ⌘F again selects what
    /// was typed, so typing replaces it.
    #[test]
    fn cmd_f_finds_in_the_document_editor() {
        use egui::Modifiers as M;
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;
        let mut h = doc_harness(&apples());
        assert!(h.query_by_role(Role::TextInput).is_none(), "no find bar before ⌘F");
        press(&mut h, M::COMMAND, Key::F);
        assert!(h.get_by_role(Role::TextInput).is_focused(), "⌘F focused the find field");
        assert_eq!(find_count(&h), None, "nothing typed, nothing said");
        type_in(&mut h, "apple");
        assert_eq!(find_count(&h).as_deref(), Some("1 of 3"));
        let (others, current) = find_marks(&h);
        assert_eq!((others.len(), current.len()), (2, 1), "every match marked, the current one apart");
        assert!(others.iter().all(|(o, _)| o.top() > current[0].0.top()), "the first match is the current one");
        for (modifiers, want) in [(M::NONE, "2 of 3"), (M::NONE, "3 of 3"), (M::NONE, "1 of 3"), (M::SHIFT, "3 of 3")] {
            press(&mut h, modifiers, Key::Enter);
            assert_eq!(find_count(&h).as_deref(), Some(want), "after {modifiers:?} ↩");
            assert!(h.get_by_role(Role::TextInput).is_focused(), "the field kept its focus");
        }
        h.get_by_label("Aa").click();
        h.run_steps(2);
        assert_eq!(find_count(&h).as_deref(), Some("1 of 2"), "matching case leaves \"Apple\" out");
        press(&mut h, M::COMMAND, Key::F);
        assert!(h.get_by_role(Role::TextInput).is_focused());
        type_in(&mut h, "pear");
        assert_eq!(h.get_by_role(Role::TextInput).value().as_deref(), Some("pear"), "⌘F selected what was typed");
        assert_eq!(find_count(&h).as_deref(), Some("No matches"));
        assert_eq!(find_marks(&h), (vec![], vec![]));
        press(&mut h, M::COMMAND, Key::A);
        press(&mut h, M::NONE, Key::Backspace);
        assert_eq!(find_count(&h), None, "an empty field says nothing");
    }

    /// Esc closes the find bar, never the dialog, also with nothing typed
    /// into the document; with the bar closed, Esc closes the dialog by
    /// the usual rule. Closing the bar (Esc or ✕) gives the editor its
    /// focus with the current match selected, so typing goes on there.
    #[test]
    fn esc_closes_the_find_bar_not_the_dialog() {
        use egui::Modifiers as M;
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;
        let mut h = doc_harness(&apples());
        press(&mut h, M::COMMAND, Key::F);
        press(&mut h, M::NONE, Key::Escape);
        assert!(h.query_by_role(Role::TextInput).is_none(), "Esc closed the find bar");
        assert!(!h.state().closed, "and not the dialog");
        press(&mut h, M::NONE, Key::Escape);
        assert!(h.state().closed, "with the bar closed and nothing typed, Esc closes the dialog");

        let mut h = doc_harness(&apples());
        press(&mut h, M::COMMAND, Key::F);
        type_in(&mut h, "pie");
        assert_eq!(find_count(&h).as_deref(), Some("1 of 1"));
        press(&mut h, M::NONE, Key::Escape);
        assert!(h.query_by_role(Role::TextInput).is_none() && !h.state().closed);
        assert!(h.get_by_role(Role::MultilineTextInput).is_focused(), "the editor has the focus");
        type_in(&mut h, "tart");
        assert!(h.state().dialog.text().contains("\"Apple tart\""), "typing replaced the match: {}", h.state().dialog.text());

        press(&mut h, M::COMMAND, Key::F);
        h.get_by_label(icon::X).click();
        h.run_steps(2);
        assert!(h.query_by_role(Role::TextInput).is_none() && !h.state().closed, "✕ closed the bar only");
    }

    /// Editing the text with the bar open finds again, so the count follows
    /// the text and the current match stays the one it was; a search runs
    /// only when the text, the needle or match case changes, not each frame.
    #[test]
    fn find_follows_the_text_and_runs_only_on_a_change() {
        use egui::Modifiers as M;
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;
        let searches = |h: &egui_kittest::Harness<'_, DocProbe>| h.state().dialog.find.as_ref().map_or(0, |f| f.searches);
        let mut h = doc_harness(&apples());
        press(&mut h, M::COMMAND, Key::F);
        type_in(&mut h, "apple");
        assert_eq!(find_count(&h).as_deref(), Some("1 of 3"));
        let n = searches(&h);
        assert!(n > 0);
        h.run_steps(5);
        assert_eq!(searches(&h), n, "nothing changed, nothing searched");
        h.get_by_role(Role::MultilineTextInput).focus();
        h.run_steps(2);
        assert_eq!(searches(&h), n, "the editor's focus is no change");
        // Typed at the start, before the current match.
        caret_after(&mut h, "");
        type_in(&mut h, "apple, ");
        assert!(h.state().dialog.text().starts_with("apple, "), "{}", h.state().dialog.text());
        assert_eq!(find_count(&h).as_deref(), Some("2 of 4"), "the count follows the text, the current match stays");
        assert_eq!(searches(&h), n + 1, "one search for the edit");
        let (others, current) = find_marks(&h);
        assert_eq!((others.len(), current.len()), (3, 1));
        h.get_by_label("Aa").click();
        h.run_steps(2);
        assert_eq!(searches(&h), n + 2, "one for match case");
        assert!(find_count(&h).is_some_and(|c| c.ends_with(" of 3")), "{:?}", find_count(&h));
    }

    /// Focuses the document editor with its caret just after the first
    /// `after` in its text ("" for the start).
    fn caret_after(h: &mut egui_kittest::Harness<'_, DocProbe>, after: &str) {
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;
        h.get_by_role(Role::MultilineTextInput).focus();
        h.run_steps(2);
        let text = h.state().dialog.text();
        let at = text.find(after).unwrap_or_else(|| panic!("{after:?} in {text}")) + after.len();
        let chars = text[..at].chars().count();
        let editor = Id::new("doc-dialog").with("text");
        let mut state = egui::TextEdit::load_state(&h.ctx, editor).expect("the editor's state");
        state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(chars))));
        state.store(&h.ctx, editor);
    }

    /// Esc pressed while typing in the document closes the bar and leaves
    /// the caret where it was, so the next keystroke goes there. (It put
    /// the selection on the current match, and the "0" meant for `_id`
    /// replaced "apple".)
    #[test]
    fn esc_from_the_text_keeps_its_caret() {
        use egui::Modifiers as M;
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;
        let mut h = doc_harness(&apples());
        press(&mut h, M::COMMAND, Key::F);
        type_in(&mut h, "apple");
        assert_eq!(find_count(&h).as_deref(), Some("1 of 3"));
        caret_after(&mut h, "\"_id\" : 1");
        type_in(&mut h, "0");
        press(&mut h, M::NONE, Key::Escape);
        assert!(h.query_by_role(Role::TextInput).is_none() && !h.state().closed, "Esc closed the bar only");
        assert!(h.get_by_role(Role::MultilineTextInput).is_focused(), "the text kept the focus");
        type_in(&mut h, "0");
        let text = h.state().dialog.text();
        assert!(text.contains("\"_id\" : 100,") && text.contains("\"a\" : \"apple\""), "the keystroke went to the caret: {text}");
    }

    /// After an edit the current match stays the one it was: an edit before
    /// it (a deletion too) moves it with the text, one after it leaves its
    /// place in the count, and one inside it, which unmakes it, moves on to
    /// the first match from the caret.
    #[test]
    fn the_current_match_stays_through_edits_around_it() {
        use egui::Modifiers as M;
        let mut h = doc_harness(&apples());
        press(&mut h, M::COMMAND, Key::F);
        type_in(&mut h, "apple");
        press(&mut h, M::NONE, Key::Enter);
        assert_eq!(find_count(&h).as_deref(), Some("2 of 3"), "on \"Apple pie\"");
        let between = |h: &egui_kittest::Harness<'_, DocProbe>| {
            let (others, current) = find_marks(h);
            let y = current.first().expect("a current match").0.top();
            others.iter().any(|(o, _)| o.top() < y) && others.iter().any(|(o, _)| o.top() > y)
        };
        assert!(between(&h));
        // Before it: the `1` of `_id` deleted.
        caret_after(&mut h, "\"_id\" : 1");
        press(&mut h, M::NONE, Key::Backspace);
        assert!(h.state().dialog.text().contains("\"_id\" : ,"), "{}", h.state().dialog.text());
        assert_eq!(find_count(&h).as_deref(), Some("2 of 3"), "a deletion before the current match keeps it");
        assert!(between(&h), "still \"Apple pie\"");
        // After it: another match at the end.
        caret_after(&mut h, "\"pineapple\"");
        type_in(&mut h, " apple");
        assert_eq!(find_count(&h).as_deref(), Some("2 of 4"), "an edit after the current match keeps its place");
        // Inside it: "Ap|ple" typed into is no match any more.
        caret_after(&mut h, "\"Ap");
        type_in(&mut h, "x");
        assert!(h.state().dialog.text().contains("\"Apxple pie\""), "{}", h.state().dialog.text());
        assert_eq!(find_count(&h).as_deref(), Some("2 of 3"), "the first match from the caret: \"pineapple\"");
    }

    /// With the bar open, a frame looks only at the rows in view: where each
    /// row starts is worked out once per layout of the text, not walked from
    /// the top each frame (on 11.5 MB scrolled to the end that walk cost
    /// about 2 ms a frame).
    #[test]
    fn find_marks_only_the_rows_in_view() {
        use egui::Modifiers as M;
        let mut doc = Document::new();
        for i in 0..3_000 {
            doc.insert(format!("field_{i}"), format!("value {i}"));
        }
        doc.insert("last", "zebra");
        let mut h = doc_harness(&doc);
        press(&mut h, M::COMMAND, Key::F);
        type_in(&mut h, "zebra");
        h.run_steps(3);
        assert_eq!(find_count(&h).as_deref(), Some("1 of 1"));
        assert_eq!(find_marks(&h).1.len(), 1, "scrolled to the match at the end");
        let find = |h: &egui_kittest::Harness<'_, DocProbe>| {
            let f = h.state().dialog.find.as_ref().expect("the bar is open");
            (f.row_scans, f.rows_looked_at)
        };
        let (scans, _) = find(&h);
        h.run_steps(5);
        let (scans_after, looked_at) = find(&h);
        assert_eq!(scans_after, scans, "the rows were walked again while the layout stayed");
        assert!(looked_at < 100, "{looked_at} rows looked at in a frame, of {} lines", doc.len() + 2);
    }

    /// Characters whose lowercase is longer (İ) or is ASCII (the Kelvin
    /// sign) don't shift a match's mark or the selection left on closing:
    /// both cover exactly the matched characters.
    #[test]
    fn find_marks_and_selects_the_matched_characters() {
        use egui::Modifiers as M;
        let doc = rongo_core::bson::doc! { "_id": 1, "tr": "İzmir İstanbul", "k": "273 \u{212A}x" };
        for (needle, want) in [("İstanbul", "İstanbul"), ("i\u{307}zmir", "İzmir"), ("kx", "\u{212A}x")] {
            let mut h = doc_harness(&doc);
            press(&mut h, M::COMMAND, Key::F);
            type_in(&mut h, needle);
            assert_eq!(find_count(&h).as_deref(), Some("1 of 1"), "{needle}");
            let (_, current) = find_marks(&h);
            let [(mark, _)] = current[..] else { panic!("{needle}: one current mark, {current:?}") };
            // Where egui itself puts those characters in the text.
            let (pos, galley) = flat_shapes(&h)
                .into_iter()
                .find_map(|(_, s)| match s {
                    egui::Shape::Text(t) if t.galley.text().contains("\"_id\"") => Some((t.pos, t.galley)),
                    _ => None,
                })
                .expect("the document's text drawn");
            let text = galley.text();
            let start = text[..text.find(want).unwrap_or_else(|| panic!("{want} in {text}"))].chars().count();
            let at = |c: usize| galley.pos_from_cursor(egui::text::CCursor::new(c)).translate(pos.to_vec2());
            let (a, b) = (at(start), at(start + want.chars().count()));
            assert!((mark.left() - a.left()).abs() < 1.0 && (mark.right() - b.left()).abs() < 1.0 && (mark.top() - a.top()).abs() < 1.0, "{needle}: marked {mark:?}, the characters are {a:?}..{b:?}");
            press(&mut h, M::NONE, Key::Escape);
            h.event(egui::Event::Copy);
            h.run_steps(1);
            let copied = h.output().platform_output.commands.iter().find_map(|c| match c {
                egui::OutputCommand::CopyText(t) => Some(t.clone()),
                _ => None,
            });
            assert_eq!(copied.as_deref(), Some(want), "{needle}: the selection left on closing");
        }
    }

    /// ⌘S still saves with the find bar open and its field focused.
    #[test]
    fn cmd_s_saves_with_the_find_bar_open() {
        use egui::Modifiers as M;
        let mut h = doc_harness(&apples());
        press(&mut h, M::COMMAND, Key::F);
        type_in(&mut h, "apple");
        press(&mut h, M::COMMAND, Key::S);
        assert_eq!(h.state().saved, [apples()]);
    }

    /// The current match is scrolled into view: in a long document,
    /// stepping to a match at the bottom and round to the top shows each.
    #[test]
    fn the_current_match_is_scrolled_into_view() {
        use egui::Modifiers as M;
        let mut doc = rongo_core::bson::doc! { "first": "zebra" };
        for i in 0..300 {
            doc.insert(format!("field_{i}"), format!("value {i}"));
        }
        doc.insert("last", "zebra");
        let mut h = doc_harness(&doc);
        press(&mut h, M::COMMAND, Key::F);
        type_in(&mut h, "zebra");
        for (step, want) in [(false, "1 of 2"), (true, "2 of 2"), (true, "1 of 2")] {
            if step {
                press(&mut h, M::NONE, Key::Enter);
            }
            h.run_steps(3);
            assert_eq!(find_count(&h).as_deref(), Some(want));
            let (_, current) = find_marks(&h);
            let [(rect, clip)] = current[..] else { panic!("{want}: the current match is on screen: {current:?}") };
            assert!(clip.contains_rect(rect), "{want}: {rect:?} in view ({clip:?})");
        }
    }

    /// View Document finds the same way: ⌘F, typing, ↩, and Esc closing
    /// only the bar, leaving the match selected (⌘C copies it); Esc then
    /// closes the dialog.
    #[test]
    fn view_document_finds_too() {
        use egui::Modifiers as M;
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;
        let mut h = view_harness(&apples());
        press(&mut h, M::COMMAND, Key::F);
        assert!(h.get_by_role(Role::TextInput).is_focused(), "⌘F focused the find field");
        type_in(&mut h, "apple");
        assert_eq!(find_count(&h).as_deref(), Some("1 of 3"));
        let (others, current) = find_marks(&h);
        assert_eq!((others.len(), current.len()), (2, 1));
        press(&mut h, M::NONE, Key::Enter);
        assert_eq!(find_count(&h).as_deref(), Some("2 of 3"));
        press(&mut h, M::NONE, Key::Escape);
        assert!(h.query_by_role(Role::TextInput).is_none() && !h.state().closed, "Esc closed only the bar");
        h.event(egui::Event::Copy);
        h.run_steps(1);
        let copied = h.output().platform_output.commands.iter().find_map(|c| match c {
            egui::OutputCommand::CopyText(t) => Some(t.clone()),
            _ => None,
        });
        assert_eq!(copied.as_deref(), Some("Apple"), "the current match was selected");
        press(&mut h, M::NONE, Key::Escape);
        assert!(h.state().closed, "with the bar closed, Esc closes View Document");
    }

    /// With a question over the editor (one asked on Save), Esc is the
    /// question's: it answers it, and the editor's find bar stays open.
    #[test]
    fn esc_is_left_to_a_dialog_on_top() {
        struct Probe {
            doc: DocDialog,
            confirm: Option<Confirm>,
        }
        let conn = ConnectionProfile::mock().conn_id();
        let mut doc = DocDialog::edit(conn, Namespace::new("db", "c"), &apples(), None);
        doc.find = Some(DocFind::default());
        let confirm = Confirm::new("Question", "Go on?".into(), "OK", Pending::ClearHistory);
        let mut h = crate::test_support::harness(vec2(1000.0, 800.0), 3, Probe { doc, confirm: Some(confirm) }, |ctx, p: &mut Probe| {
            _ = p.doc.show(ctx);
            if p.confirm.as_mut().is_some_and(|c| c.show(ctx).is_some()) {
                p.confirm = None;
            }
        });
        press(&mut h, egui::Modifiers::NONE, Key::Escape);
        assert!(h.state().confirm.is_none(), "Esc answered the question");
        assert!(h.state().doc.find.is_some(), "the find bar under it stayed");
    }

    /// The connection manager closes on a click outside; its form, open,
    /// never does (Cancel goes back to the list).
    #[test]
    fn connection_form_keeps_its_work_on_an_outside_click() {
        struct Probe {
            dialog: ConnectDialog,
            vault: std::sync::Arc<Vault>,
            closed: bool,
        }
        let (_dir, vault) = temp_vault();
        let open = |dialog: ConnectDialog| {
            let mut h = egui_kittest::Harness::builder().with_size(vec2(1000.0, 800.0)).build_ui_state(
                |ui, p: &mut Probe| {
                    theme::install(ui.ctx());
                    let acts = p.dialog.show(ui.ctx(), &[], &p.vault);
                    p.closed |= acts.iter().any(|a| matches!(a, ConnectAction::Close));
                },
                Probe { dialog, vault: vault.clone(), closed: false },
            );
            h.run_steps(3);
            h
        };
        let mut h = open(ConnectDialog::new());
        crate::test_support::click_outside(&mut h);
        assert!(h.state().closed, "the list closes");
        let mut h = open(ConnectDialog::new_connection());
        crate::test_support::click_outside(&mut h);
        h.key_press(Key::Escape);
        h.run_steps(2);
        assert!(!h.state().closed && h.state().dialog.has_form(), "the form stays");
    }
}
