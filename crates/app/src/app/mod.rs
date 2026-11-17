//! The application: layout, response routing, and the glue between the tree,
//! the tabs and the dialogs.

mod actions;
mod chrome;
mod conns;
mod history;
mod ops;
mod outcome;
mod paste;
mod requests;
mod router;
mod settings_glue;
mod status;
mod tab_ops;
mod update_check;
mod vault;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::Ui;
use rongo_core::profile::{ProfileStore, SecretBackend};
use rongo_core::vault::Vault;
use rongo_core::{ConnId, CoreHandle, RequestId, Waker};

use crate::history_tab::RestoreAsk;
use crate::job::Job;
use crate::dialogs::{Confirm, ConnectDialog, DocDialog, ViewDocDialog};
use crate::net::Net;
use crate::operations::Operations;
use crate::paste::{CollClip, PasteDialog};
use crate::sidebar::{self, OpenConn, Sidebar};
use crate::settings::{ChangePasswordDialog, HistoryFiles, Settings, SettingsDialog};
use crate::tools::ToolDialog;
use crate::tabs::{Tab, TabKind};
use crate::theme;
use crate::unlock::UnlockDialog;
use crate::update::{Hooks, UpdateCheck};

use history::purge_history_at_start;
pub(crate) use paste::database_name_error;
use requests::Requests;
use vault::unlock_dialog;

struct Toast {
    text: String,
    error: bool,
    until: Instant,
}

pub struct RongoApp {
    _rt: tokio::runtime::Runtime,
    requests: Requests,
    waker: Waker,
    ctx: egui::Context,
    store: Option<ProfileStore>,
    conns: BTreeMap<ConnId, OpenConn>,
    tabs: Vec<Tab>,
    active_tab: usize,
    next_tab_id: u64,
    sidebar: Sidebar,
    /// Where the sidebar was last frame, so ⌘F over it goes to its search.
    sidebar_rect: egui::Rect,
    /// The tab last scrolled into view in the tab bar.
    shown_tab: u64,
    connect_dialog: Option<ConnectDialog>,
    export_conn: Option<crate::export_conn::ExportConnDialog>,
    doc_dialog: Option<DocDialog>,
    confirm: Option<Confirm>,
    tool: Option<ToolDialog>,
    /// Collections copied from the tree, waiting to be pasted.
    coll_clip: Option<CollClip>,
    paste_dialog: Option<PasteDialog>,
    view_doc: Option<ViewDocDialog>,
    /// A restore found conflicts: what to do with them.
    restore_ask: Option<RestoreAsk>,
    settings: Settings,
    /// Where Settings are saved (in tests, a temporary folder of each app's own).
    settings_path: Option<PathBuf>,
    settings_dialog: Option<SettingsDialog>,
    toasts: Vec<Toast>,
    /// Where Collection History is kept (`history_at()`); None without a
    /// data folder.
    history_root: Option<PathBuf>,
    /// The open vault: every saved password and SSH secret, and the key that
    /// seals Collection History. None until the master password is typed
    /// (the window shows nothing but the launch dialog until then), and
    /// again once another rongo window changed the vault.
    vault: Option<Arc<Vault>>,
    /// The launch dialog, while there is no vault.
    unlock: Option<UnlockDialog>,
    /// Where the vault is kept: beside profiles.json (in tests, a temporary
    /// folder of each app's own).
    vault_path: Option<PathBuf>,
    /// `--mock`: the demo server opens once the vault is unlocked.
    demo_on_unlock: bool,
    /// Settings → Change Master Password….
    change_password: Option<ChangePasswordDialog>,
    /// The Reset question (Settings → Reset…, or Forgot? at launch). Kept
    /// apart from `confirm`: it is also asked while the vault is locked,
    /// when it stands in for the launch dialog.
    reset_ask: Option<Confirm>,
    /// The Keychain that Reset deletes rongo's items left in (macOS; tests
    /// put one in memory here, never the real one).
    keychain: Option<Arc<dyn SecretBackend>>,
    /// Reset deleting those items, off the UI thread (macOS may ask first).
    keychain_forget: Option<Job<rongo_core::CoreResult<()>>>,
    /// The history files as Settings measures and changes them, off the UI
    /// thread and without the key.
    history_files: HistoryFiles,
    /// Connection names by profile (`refresh_names`), and what they were
    /// made from: a hash of the saved and open connections' ids and names.
    names: HashMap<uuid::Uuid, String>,
    names_from: Option<u64>,
    /// The Connect each connection waits for. Only its answer speaks for
    /// the connection: an earlier attempt's (given up on by Disconnect) is
    /// stale.
    connecting: HashMap<ConnId, RequestId>,
    /// The start-up purge (`purge_history_at_start`), until its outcome is
    /// taken in: a failure is said.
    startup_purge: Option<std::thread::JoinHandle<rongo_core::CoreResult<u64>>>,
    /// The update check: GitHub's latest release, asked at launch, and the
    /// banner offering it (`update_check.rs`).
    update: UpdateCheck,
    /// The Operations panel under the tree: the writes and data moves of
    /// this session (`ops.rs`, `crate::operations`).
    ops: Operations,
    /// Showing an export's file in the file manager, off the UI thread.
    revealing: Option<Job<Result<(), String>>>,
    /// Where the Operations panel began last frame (its header, folded),
    /// while the sidebar shows: the corner floats above it.
    ops_top: Option<f32>,
    /// Tests: the app's own folder (connections, settings, history, vault),
    /// removed with the app.
    #[cfg(test)]
    _temp: crate::test_support::TempDir,
}

impl RongoApp {
    #[cfg(not(test))]
    pub fn new(cc: &eframe::CreationContext<'_>, rt: tokio::runtime::Runtime, mock: bool) -> Self {
        Self::start(cc, rt, mock, Hooks::system())
    }

    /// Tests: an app with a folder of its own (connections, settings,
    /// history, vault), removed with it. It never asks GitHub (no request
    /// to make) nor opens a browser.
    #[cfg(test)]
    pub fn new(cc: &eframe::CreationContext<'_>, rt: tokio::runtime::Runtime, mock: bool) -> Self {
        Self::start(cc, rt, mock, Hooks::none(), crate::test_support::TempDir::new())
    }

    /// Tests: an app on a folder the test filled first (a settings file, say).
    #[cfg(test)]
    pub fn new_in(cc: &eframe::CreationContext<'_>, rt: tokio::runtime::Runtime, mock: bool, temp: crate::test_support::TempDir) -> Self {
        Self::start(cc, rt, mock, Hooks::none(), temp)
    }

    /// Tests: `new_in`, with a stub GitHub and browser.
    #[cfg(test)]
    pub fn with_hooks(cc: &eframe::CreationContext<'_>, rt: tokio::runtime::Runtime, mock: bool, temp: crate::test_support::TempDir, hooks: Hooks) -> Self {
        Self::start(cc, rt, mock, hooks, temp)
    }

    fn start(cc: &eframe::CreationContext<'_>, rt: tokio::runtime::Runtime, mock: bool, hooks: Hooks, #[cfg(test)] temp: crate::test_support::TempDir) -> Self {
        theme::install(&cc.egui_ctx);
        let ctx = cc.egui_ctx.clone();
        let waker: Waker = Arc::new(move || ctx.request_repaint());
        let core = CoreHandle::start(rt.handle().clone(), waker.clone());
        let mut toasts = Vec::new();
        #[cfg(not(test))]
        let paths = AppPaths::new();
        #[cfg(test)]
        let paths = AppPaths::new(&temp);
        let (settings, settings_notice) = paths.settings.as_deref().map(Settings::load).unwrap_or_default();
        if let Some(text) = settings_notice {
            toasts.push(Toast { text, error: true, until: Instant::now() + Duration::from_secs(15) });
        }
        let store = match paths.profiles.clone().map(ProfileStore::load) {
            Some(Ok(s)) => Some(s),
            Some(Err(e)) => {
                toasts.push(Toast { text: format!("Could not load saved connections: {e}"), error: true, until: Instant::now() + Duration::from_secs(15) });
                None
            }
            None => None,
        };
        let (vault, unlock) = initial_vault(&paths, store.as_ref());
        // Entries past the settings' age and size go now (by file names and
        // sizes: no key needed). After the look for history to move above.
        let startup_purge = paths.history.as_ref().and_then(|root| purge_history_at_start(root.clone(), &settings.history, waker.clone()));
        let mut app = Self {
            _rt: rt,
            requests: Requests::new(core),
            waker,
            ctx: cc.egui_ctx.clone(),
            store,
            conns: BTreeMap::new(),
            tabs: vec![Tab { id: 1, kind: TabKind::Welcome }],
            active_tab: 0,
            next_tab_id: 1,
            sidebar: Sidebar::default(),
            sidebar_rect: egui::Rect::NOTHING,
            shown_tab: u64::MAX,
            connect_dialog: None,
            export_conn: None,
            doc_dialog: None,
            confirm: None,
            tool: None,
            coll_clip: None,
            paste_dialog: None,
            view_doc: None,
            restore_ask: None,
            settings,
            settings_path: paths.settings,
            settings_dialog: None,
            toasts,
            history_files: HistoryFiles::new(paths.history.clone().unwrap_or_default()),
            names: HashMap::new(),
            names_from: None,
            connecting: HashMap::new(),
            startup_purge,
            history_root: paths.history,
            vault,
            unlock,
            vault_path: paths.vault,
            demo_on_unlock: false,
            change_password: None,
            reset_ask: None,
            keychain: crate::unlock::keychain(),
            keychain_forget: None,
            update: UpdateCheck::new(hooks),
            ops: Operations::default(),
            revealing: None,
            ops_top: None,
            #[cfg(test)]
            _temp: temp,
        };
        app.settings.apply();
        // In the background: the launch dialog shows meanwhile.
        app.start_update_check();
        if mock {
            if app.vault.is_some() {
                app.open_demo();
            } else {
                app.demo_on_unlock = true;
            }
        }
        app
    }

    fn net(&mut self) -> Net<'_> {
        self.requests.net(false)
    }

    fn modal_open(&self) -> bool {
        self.connect_dialog.is_some()
            || self.export_conn.is_some()
            || self.doc_dialog.is_some()
            || self.confirm.is_some()
            || self.tool.is_some()
            || self.paste_dialog.is_some()
            || self.view_doc.is_some()
            || self.restore_ask.is_some()
            || self.settings_dialog.is_some()
            || self.change_password.is_some()
            || self.reset_ask.is_some()
    }

    /// A toast; said while a logged request's answer is handled, it is also
    /// that entry's result in the Operations panel.
    fn toast(&mut self, text: impl Into<String>, error: bool) {
        let text = text.into();
        self.ops.said(&text, error);
        let secs = if error { 8 } else { 4 };
        self.toasts.push(Toast { text, error, until: Instant::now() + Duration::from_secs(secs) });
    }

    fn next_tab(&mut self) -> u64 {
        self.next_tab_id += 1;
        self.next_tab_id
    }
}

/// Where the app keeps its files: the saved connections, the vault beside
/// them, Settings and Collection History. Tests get empty ones of their own,
/// in a folder of each app's own: they must never read or change the real
/// ones.
struct AppPaths {
    profiles: Option<PathBuf>,
    vault: Option<PathBuf>,
    settings: Option<PathBuf>,
    history: Option<PathBuf>,
}

impl AppPaths {
    #[cfg(not(test))]
    fn new() -> Self {
        Self::with(ProfileStore::default_path(), Settings::default_path(), directories::ProjectDirs::from("dev", "rongo", "rongo").map(|d| history_root(&d)))
    }

    #[cfg(test)]
    fn new(temp: &crate::test_support::TempDir) -> Self {
        Self::with(Some(temp.0.join("profiles.json")), Some(temp.0.join("settings.json")), Some(temp.0.join("history")))
    }

    fn with(profiles: Option<PathBuf>, settings: Option<PathBuf>, history: Option<PathBuf>) -> Self {
        let vault = profiles.as_ref().map(|p| p.with_file_name("vault"));
        AppPaths { profiles, vault, settings, history }
    }
}

/// Collection History's folder: in the local data folder, not the roaming
/// one, as it can reach 1 GB and must not sync with a Windows roaming
/// profile (`%LOCALAPPDATA%\rongo\rongo\data\history`). On macOS both are
/// `~/Library/Application Support/dev.rongo.rongo`, so nothing moves there.
fn history_root(dirs: &directories::ProjectDirs) -> PathBuf {
    dirs.data_local_dir().join("history")
}

/// The vault the window starts with, and the launch dialog: the master
/// password is asked first.
#[cfg(not(test))]
fn initial_vault(paths: &AppPaths, store: Option<&ProfileStore>) -> (Option<Arc<Vault>>, Option<UnlockDialog>) {
    let profiles = store.map(|s| s.profiles().to_vec()).unwrap_or_default();
    let moving = match (&paths.vault, &paths.history) {
        // No store here means profiles.json couldn't be read: its connections may have secrets to move.
        (Some(v), Some(root)) => crate::unlock::should_move(v, store.map(|s| s.profiles()), root),
        _ => false,
    };
    (None, Some(unlock_dialog(paths.vault.as_deref(), moving, profiles)))
}

/// Tests start unlocked, with a vault of their own (`TEST_PASSWORD`).
#[cfg(test)]
fn initial_vault(paths: &AppPaths, _store: Option<&ProfileStore>) -> (Option<Arc<Vault>>, Option<UnlockDialog>) {
    let path = paths.vault.clone().expect("a test folder");
    let v = Vault::create(path, crate::test_support::TEST_PASSWORD, rongo_core::vault::KdfParams::TEST, BTreeMap::new()).expect("a test vault");
    (Some(Arc::new(v)), None)
}

impl eframe::App for RongoApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        // What the last frame's dialogs sent goes into the Operations log.
        self.log_sent();
        while let Some(r) = self.requests.core.try_recv() {
            self.handle(r);
        }
        self.poll_history_files();
        self.poll_keychain_forget();
        self.poll_startup_purge();
        self.poll_update_check();
        self.poll_reveal();
        let ctx = ui.ctx().clone();
        // Until the vault is open the window shows the launch dialog over an
        // empty window: no toolbar, tree or tabs. Toasts still show (a
        // launch's "Could not load saved connections" would expire unseen).
        if self.vault.is_none() {
            crate::shortcut::drop_emacs_keys(&ctx);
            egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::BG)).show(ui, |_| {});
            // Forgot?'s question stands in for the dialog; Cancel brings it back.
            if self.reset_ask.is_some() {
                self.reset_dialog(&ctx);
            } else {
                let dialog = self.unlock.get_or_insert_with(|| unlock_dialog(self.vault_path.as_deref(), false, Vec::new()));
                if let Some(answer) = dialog.show(&ctx) {
                    self.unlock_answer(&ctx, answer);
                }
            }
            self.toasts(&ctx);
            return;
        }
        self.shortcuts(&ctx);
        crate::shortcut::drop_emacs_keys(&ctx);
        // Scripts in tabs not on screen keep reporting: output, results, and
        // finishing (a finished script's tab can then be closed).
        let mut net = self.requests.net(false);
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            if let TabKind::Shell(s) = &mut tab.kind
                && i != self.active_tab
            {
                s.poll(&mut net);
            }
        }

        egui::Panel::top("toolbar")
            .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(egui::Margin::symmetric(10, 6)))
            .show(ui, |ui| self.toolbar(ui));
        self.update_banner(ui);
        egui::Panel::left("strip")
            .exact_size(26.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(theme::PANEL))
            .show(ui, |ui| sidebar::strip(ui, &mut self.sidebar));
        if self.sidebar.open {
            let mut actions = Vec::new();
            let mut op_actions = Vec::new();
            let (now, tz) = (Instant::now(), self.settings.timezone);
            self.sidebar_rect = egui::Panel::left("sidebar")
                .resizable(true)
                .default_size(300.0)
                .size_range(200.0..=600.0)
                .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(egui::Margin::symmetric(6, 6)))
                .show(ui, |ui| {
                    // Exactly the panel's area, the tree above the Operations
                    // panel: nothing in either can widen it.
                    let area = ui.available_rect_before_wrap();
                    ui.allocate_rect(area, egui::Sense::hover());
                    let (tree_area, ops_area) = self.ops.split(area);
                    self.ops_top = Some(ops_area.top());
                    let order: Vec<ConnId> = self.store.as_ref().map(|s| s.profiles().iter().map(|p| p.conn_id()).collect()).unwrap_or_default();
                    self.sidebar.can_paste = self.coll_clip.is_some();
                    let mut net = self.requests.net(false);
                    let mut tree_ui = ui.new_child(egui::UiBuilder::new().max_rect(tree_area).layout(egui::Layout::top_down(egui::Align::Min)));
                    actions = sidebar::tree(&mut tree_ui, &mut self.sidebar, &mut self.conns, &order, &mut net);
                    let mut ops_ui = ui.new_child(egui::UiBuilder::new().max_rect(ops_area).layout(egui::Layout::top_down(egui::Align::Min)));
                    op_actions = crate::operations::panel(&mut ops_ui, &mut self.ops, now, tz);
                })
                .response
                .rect;
            for a in actions {
                self.tree_action(a);
            }
            for a in op_actions {
                self.ops_action(a);
            }
        } else {
            self.ops_top = None;
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(egui::Margin { left: 6, right: 8, top: 6, bottom: 6 }))
            .show(ui, |ui| {
                self.tab_bar(ui);
                egui::Frame::new().fill(theme::BG).inner_margin(8.0).corner_radius(egui::CornerRadius { nw: 0, ne: 5, sw: 5, se: 5 }).show(ui, |ui| {
                    ui.set_min_size(ui.available_size());
                    self.active_tab_ui(ui);
                });
            });

        // Before the corner, which leaves out what the panel lists.
        self.log_sent();
        self.activity(&ctx);
        self.dialogs(&ctx);
        self.toasts(&ctx);
    }
}

#[cfg(test)]
mod tests;
