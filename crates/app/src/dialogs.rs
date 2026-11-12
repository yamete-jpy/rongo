//! Modal windows: the connection manager, the document editor, confirmations
//! and a one-line prompt.

use egui::{Align, Context, Id, Key, Layout, Modal, RichText, Ui, vec2};
use egui_phosphor::regular as icon;
use rongo_core::bson::{Bson, Document};
use rongo_core::format::{self, Style};
use rongo_core::profile::{self, Auth, ConnectionProfile, SecretKind, SshAuth, SshConfig, Target, TlsConfig};
use rongo_core::{ConnId, ConnectRequest, Namespace, RequestId};
use uuid::Uuid;

use crate::{highlight, theme};

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
    tab: FormTab,
    error: Option<String>,
}

impl Form {
    fn from_profile(p: &ConnectionProfile, is_new: bool) -> Self {
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
        };
        let saved = |kind| !is_new && uses(kind) && profile::get_secret(p.id, kind).ok().flatten().is_some();
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
            let port: u16 = self.ssh_port.trim().parse().map_err(|_| "SSH port must be a number".to_string())?;
            if self.ssh_host.trim().is_empty() || self.ssh_user.trim().is_empty() {
                return Err("SSH needs a host and a user".into());
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
        Ok(ConnectionProfile {
            id: self.id,
            name: name.into(),
            target,
            auth,
            tls,
            ssh,
            default_db: (!self.default_db.trim().is_empty()).then(|| self.default_db.trim().to_string()),
            color: self.color.clone(),
        })
    }

    /// The secrets to use: what was typed, or else what the keychain holds.
    fn secrets(&self) -> (Option<String>, Option<String>) {
        let typed = |s: &String| (!s.is_empty()).then(|| s.clone());
        let pw = typed(&self.password).or_else(|| self.has_saved_password.then(|| profile::get_secret(self.id, SecretKind::Password).ok().flatten()).flatten());
        let ssh = typed(&self.ssh_secret).or_else(|| self.has_saved_ssh_secret.then(|| profile::get_secret(self.id, SecretKind::Ssh).ok().flatten()).flatten());
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
    Close,
}

pub struct ConnectDialog {
    /// The highlighted rows; Connect opens all of them.
    selected: Vec<Uuid>,
    /// Where a ⇧-click range starts: the last row clicked without ⇧.
    anchor: Option<Uuid>,
    form: Option<Form>,
    pub test: TestState,
}

impl ConnectDialog {
    pub fn new() -> Self {
        Self { selected: Vec::new(), anchor: None, form: None, test: TestState::Idle }
    }

    pub fn new_connection() -> Self {
        let mut d = Self::new();
        d.form = Some(Form::from_profile(&ConnectionProfile::new("New connection"), true));
        d
    }

    pub fn show(&mut self, ctx: &Context, profiles: &[ConnectionProfile]) -> Vec<ConnectAction> {
        let mut actions = Vec::new();
        // Never wider or taller than the window: a centred dialog wider
        // than the window loses both edges, the checkboxes with them.
        let screen = ctx.content_rect().size();
        let modal = Modal::new(Id::new("connect-dialog")).show(ctx, |ui| {
            ui.set_width(760.0f32.min(screen.x - 40.0).max(240.0));
            ui.set_min_height(420.0f32.min(screen.y - 80.0).max(200.0));
            match &mut self.form {
                None => self.list(ui, profiles, &mut actions),
                Some(_) => self.edit(ui, &mut actions),
            }
        });
        if modal.should_close() && self.form.is_none() {
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

    fn list(&mut self, ui: &mut Ui, profiles: &[ConnectionProfile], actions: &mut Vec<ConnectAction>) {
        ui.heading(format!("{}  Connection Manager", icon::PLUGS_CONNECTED));
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            if ui.button(format!("{}  New connection", icon::PLUS)).clicked() {
                self.form = Some(Form::from_profile(&ConnectionProfile::new("New connection"), true));
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
                self.form = Some(Form::from_profile(p, false));
                self.test = TestState::Idle;
            }
            if ui.add_enabled(sel.is_some(), egui::Button::new(format!("{}  Clone", icon::COPY))).clicked()
                && let Some(p) = sel
            {
                let mut copy = p.clone();
                copy.id = Uuid::new_v4();
                copy.name = format!("{} (copy)", p.name);
                self.form = Some(Form::from_profile(&copy, true));
            }
            if ui.add_enabled(sel.is_some(), egui::Button::new(RichText::new(format!("{}  Delete", icon::TRASH)).color(theme::ERROR))).clicked()
                && let Some(p) = sel
            {
                actions.push(ConnectAction::Delete(p.id));
                self.selected.clear();
            }
        });
        ui.separator();
        let list_h = ui.available_height() - 40.0;
        // Both ways: a narrow window scrolls the columns instead of cutting them.
        egui::ScrollArea::both().max_height(list_h).auto_shrink([false, false]).show(ui, |ui| {
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
        ui.separator();
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
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
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add(egui::Label::new(RichText::new("Tick several to connect them together · drag a name to reorder").weak()).truncate());
                    });
                }
            });
        });
    }

    fn edit(&mut self, ui: &mut Ui, actions: &mut Vec<ConnectAction>) {
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
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            for (tab, label) in [(FormTab::Server, "Server"), (FormTab::Auth, "Authentication"), (FormTab::Tls, "TLS"), (FormTab::Ssh, "SSH Tunnel")] {
                ui.selectable_value(&mut f.tab, tab, label);
            }
        });
        ui.separator();
        // The body takes the height the footer leaves, so the buttons sit at
        // the bottom of the dialog rather than above an empty strip.
        let notes = usize::from(f.error.is_some()) + usize::from(!matches!(self.test, TestState::Idle));
        let body_h = (ui.available_height() - 46.0 - 24.0 * notes as f32).max(250.0);
        ui.allocate_ui(vec2(ui.available_width(), body_h), |ui| {
            ui.set_min_height(body_h);
            match f.tab {
                FormTab::Server => server_tab(ui, f),
                FormTab::Auth => auth_tab(ui, f),
                FormTab::Tls => tls_tab(ui, f),
                FormTab::Ssh => ssh_tab(ui, f),
            }
        });
        ui.separator();
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
        let form_id = f.id;
        let mut close = false;
        ui.horizontal(|ui| {
            let testing = matches!(self.test, TestState::Running(..));
            if ui.add_enabled(!testing, egui::Button::new(format!("{}  Test connection", icon::PULSE))).clicked() {
                match f.to_profile() {
                    Ok(mut p) => {
                        f.error = None;
                        let (password, ssh_secret) = f.secrets();
                        // A throwaway id so testing never disturbs an open connection.
                        p.id = Uuid::new_v4();
                        actions.push(ConnectAction::Test(Box::new(ConnectRequest { profile: p, password, ssh_secret })));
                    }
                    Err(e) => f.error = Some(e),
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut save = |then_connect: bool, f: &mut Form| match f.to_profile() {
                    Ok(profile) => {
                        let typed = |s: &String| (!s.is_empty()).then(|| s.clone());
                        actions.push(ConnectAction::Save {
                            profile,
                            password: typed(&f.password),
                            ssh_secret: typed(&f.ssh_secret),
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
                }
                if ui.button("Save").clicked() {
                    close = save(false, f);
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if close {
            self.selected = vec![form_id];
            self.anchor = Some(form_id);
            self.form = None;
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
                ui.weak("A password in the URI is moved to the Keychain when you save.");
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
    });
}

fn auth_tab(ui: &mut Ui, f: &mut Form) {
    ui.checkbox(&mut f.auth_on, "Username / password authentication");
    ui.add_space(6.0);
    ui.add_enabled_ui(f.auth_on, |ui| {
        egui::Grid::new("auth").num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
            field(ui, "Username", &mut f.user, "", false);
            let hint = if f.has_saved_password { "saved in Keychain — type to replace" } else { "" };
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
            let hint = if f.has_saved_ssh_secret { "saved in Keychain — type to replace" } else { "" };
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
        Self { conn, ns, mode: DocMode::Edit { id }, view, text, error: None, saving: None }
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
        Self { conn, ns, mode: DocMode::Insert, view, text, error: None, saving: None }
    }

    pub fn show(&mut self, ctx: &Context) -> Option<DocAction> {
        let mut action = None;
        let title = match self.mode {
            DocMode::Edit { .. } => format!("{}  Edit document — {}", icon::PENCIL_SIMPLE, self.ns),
            DocMode::Insert => format!("{}  Add document — {}", icon::FILE_PLUS, self.ns),
        };
        let modal = Modal::new(Id::new("doc-dialog")).show(ctx, |ui| {
            ui.set_width(720.0);
            ui.heading(title);
            ui.weak("Shell syntax: ObjectId(\"…\"), ISODate(\"…\"), NumberLong(\"…\"), NumberDecimal(\"…\"), Double(5).");
            ui.add_space(4.0);
            let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
                let job = highlight::layout_job(buf.as_str(), theme::mono(13.0), wrap);
                ui.fonts_mut(|f| f.layout_job(job))
            };
            egui::Frame::new().fill(theme::BG).inner_margin(6.0).corner_radius(4.0).show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(440.0).min_scrolled_height(300.0).show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut self.text)
                            .code_editor()
                            .frame(egui::Frame::NONE)
                            .desired_width(f32::INFINITY)
                            .desired_rows(18)
                            .layouter(&mut layouter),
                    );
                });
            });
            if let Some(e) = &self.error {
                ui.colored_label(theme::ERROR, e);
            }
            ui.horizontal(|ui| {
                if self.saving.is_some() {
                    ui.spinner();
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let save = ui.add_enabled(self.saving.is_none(), theme::primary_button("Save", false));
                    let shortcut = ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::S));
                    if save.clicked() || (shortcut && self.saving.is_none()) {
                        match rongo_shell::parse_document(&self.text) {
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
        if modal.should_close() && action.is_none() && self.saving.is_none() {
            action = Some(DocAction::Close);
        }
        action
    }
}

/// Read-only JSON view of one document (View Document, F3).
pub struct ViewDocDialog {
    text: String,
}

impl ViewDocDialog {
    pub fn new(doc: &Document) -> Self {
        Self { text: format::doc_to_shell(doc, Style::Display) }
    }

    /// Returns true when the dialog should close.
    pub fn show(&mut self, ctx: &Context) -> bool {
        let mut close = false;
        let modal = Modal::new(Id::new("view-doc")).show(ctx, |ui| {
            ui.set_width(720.0);
            ui.heading(format!("{}  View Document", icon::FILE_TEXT));
            ui.add_space(4.0);
            let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
                let job = highlight::layout_job(buf.as_str(), theme::mono(13.0), wrap);
                ui.fonts_mut(|f| f.layout_job(job))
            };
            egui::Frame::new().fill(theme::BG).inner_margin(6.0).corner_radius(4.0).show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(480.0).show(ui, |ui| {
                    let mut view = self.text.as_str();
                    ui.add(egui::TextEdit::multiline(&mut view).code_editor().frame(egui::Frame::NONE).desired_width(f32::INFINITY).layouter(&mut layouter));
                });
            });
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                    if ui.button(format!("{}  Copy", icon::CLIPBOARD_TEXT)).clicked() {
                        ui.ctx().copy_text(self.text.clone());
                    }
                });
            });
        });
        close || modal.should_close()
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
    /// Insert documents from the clipboard; the checkbox strips their `_id`s.
    Paste { conn: ConnId, ns: Namespace, docs: Vec<Document>, view: Option<u64> },
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

pub enum ConfirmResult {
    Yes,
    No,
}

impl Confirm {
    /// OK waits for typed text, and for exactly the right text when a
    /// name must be typed.
    pub fn ok_enabled(&self) -> bool {
        match (&self.input, &self.must_type) {
            (Some(s), Some(want)) => s.trim() == want,
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
                r.request_focus();
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && !input.trim().is_empty() && self.must_type.as_ref().is_none_or(|want| input.trim() == want) {
                    result = Some(ConfirmResult::Yes);
                }
            }
            if let Some((label, ticked)) = &mut self.checkbox {
                ui.add_space(4.0);
                ui.checkbox(ticked, label.as_str());
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let ok_enabled = self.ok_enabled();
                    if ui.add_enabled(ok_enabled, theme::primary_button(self.ok_label.clone(), self.danger)).clicked() {
                        result = Some(ConfirmResult::Yes);
                    }
                    if ui.button("Cancel").clicked() {
                        result = Some(ConfirmResult::No);
                    }
                });
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

    /// In a window narrower than the dialog, the dialog shrinks to fit:
    /// every row's checkbox and the Connect button stay on screen and work,
    /// and the columns scroll instead of being cut.
    #[test]
    fn manager_fits_a_narrow_window() {
        let ctx = Context::default();
        theme::install(&ctx);
        let names = ["alpha-with-a-very-long-connection-name-1", "bravo-with-a-very-long-connection-name-2", "charlie-3"];
        let profiles = names.map(ConnectionProfile::new);
        let mut dialog = ConnectDialog::new();
        let width = 380.0;
        let frame = |dialog: &mut ConnectDialog, events: Vec<egui::Event>| {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(width, 700.0))), events, ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| _ = dialog.show(ui.ctx(), &profiles));
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
            let mut out = ctx.run_ui(input, |ui| actions = dialog.show(ui.ctx(), &profiles));
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

    /// In the Connection Manager a row looks like the connection in the tree
    /// (tint and bar in its colour), dragging a row asks to move it, and the
    /// colour button opens the colours.
    #[test]
    fn manager_reorders_and_colours() {
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
            let mut out = ctx.run_ui(input, |ui| actions = dialog.show(ui.ctx(), &profiles));
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
}
