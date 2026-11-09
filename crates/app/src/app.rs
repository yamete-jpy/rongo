//! The application: layout, response routing, and the glue between the tree,
//! the tabs and the dialogs.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Align, Align2, Key, Layout, Modifiers, RichText, Ui};
use egui_phosphor::regular as icon;
use rongo_core::profile::{self, ConnectionProfile, ProfileStore, SecretKind};
use rongo_core::bson::{Document, doc};
use rongo_core::{Command, ConnId, ConnectRequest, CoreError, CoreHandle, Event, FindSpec, Namespace, RequestId, Response, Waker};

use crate::indexes::{IndexAction, IndexTab};
use crate::dialogs::{ConfirmResult, Confirm, ConnectAction, ConnectDialog, DocAction, DocDialog, DocMode, Pending, TestState, ViewDocDialog};
use crate::net::{Net, Origin, ViewReq};
use crate::result_view::{Mode, ResultView, Source, ViewAction, thousands};
use crate::sidebar::{self, CollNode, OpenConn, Sidebar, Status, TreeAction};
use crate::settings::{OpenIn, Settings, SettingsAction, SettingsDialog};
use crate::tools::{ToolAction, ToolDialog};
use crate::tabs::{CollectionTab, ShellTab, Tab, TabKind};
use crate::theme;

struct Toast {
    text: String,
    error: bool,
    until: Instant,
}

pub struct RongoApp {
    _rt: tokio::runtime::Runtime,
    core: CoreHandle,
    waker: Waker,
    store: Option<ProfileStore>,
    conns: BTreeMap<ConnId, OpenConn>,
    tabs: Vec<Tab>,
    active_tab: usize,
    next_tab_id: u64,
    next_view_id: u64,
    pending: HashMap<RequestId, Origin>,
    sidebar: Sidebar,
    /// Where the sidebar was last frame, so ⌘F over it goes to its search.
    sidebar_rect: egui::Rect,
    connect_dialog: Option<ConnectDialog>,
    doc_dialog: Option<DocDialog>,
    confirm: Option<Confirm>,
    tool: Option<ToolDialog>,
    view_doc: Option<ViewDocDialog>,
    settings: Settings,
    settings_dialog: Option<SettingsDialog>,
    toasts: Vec<Toast>,
}

const DEMO_SCRIPT: &str = r#"// IntelliShell runs mongosh-style JavaScript. ⌘↩ runs everything (or the
// selection), ⇧⌘↩ runs the statement under the cursor. A find() you don't
// read opens as a result tab you can page through.
db.getCollection("advertisers").find({})

db.bet_logs.find({ status: "pending", amount: { $gt: 1000 } }).sort({ amount: -1 })

db.bet_logs.aggregate([
    { $group: { _id: "$provider", bets: { $sum: 1 }, total: { $sum: "$amount" } } },
    { $sort: { total: -1 } }
])
"#;

impl RongoApp {
    pub fn new(cc: &eframe::CreationContext<'_>, rt: tokio::runtime::Runtime, mock: bool) -> Self {
        theme::install(&cc.egui_ctx);
        let ctx = cc.egui_ctx.clone();
        let waker: Waker = Arc::new(move || ctx.request_repaint());
        let core = CoreHandle::start(rt.handle().clone(), waker.clone());
        let mut toasts = Vec::new();
        // Tests get an empty list of their own: they must never read or
        // change the real saved connections.
        #[cfg(not(test))]
        let path = ProfileStore::default_path();
        #[cfg(test)]
        let path = Some(std::env::temp_dir().join(format!("rongo-test-{}", uuid::Uuid::new_v4())).join("profiles.json"));
        let store = match path.map(ProfileStore::load) {
            Some(Ok(s)) => Some(s),
            Some(Err(e)) => {
                toasts.push(Toast { text: format!("Could not load saved connections: {e}"), error: true, until: Instant::now() + Duration::from_secs(15) });
                None
            }
            None => None,
        };
        let mut app = Self {
            _rt: rt,
            core,
            waker,
            store,
            conns: BTreeMap::new(),
            tabs: vec![Tab { id: 1, kind: TabKind::Welcome }],
            active_tab: 0,
            next_tab_id: 1,
            next_view_id: 0,
            pending: HashMap::new(),
            sidebar: Sidebar::default(),
            sidebar_rect: egui::Rect::NOTHING,
            connect_dialog: None,
            doc_dialog: None,
            confirm: None,
            tool: None,
            view_doc: None,
            settings: Settings::load(),
            settings_dialog: None,
            toasts,
        };
        app.settings.apply();
        if mock {
            app.open_demo();
        }
        app
    }

    fn net(&mut self) -> Net<'_> {
        Net { core: &self.core, pending: &mut self.pending, next_view_id: &mut self.next_view_id, keys_enabled: false }
    }

    fn modal_open(&self) -> bool {
        self.connect_dialog.is_some()
            || self.doc_dialog.is_some()
            || self.confirm.is_some()
            || self.tool.is_some()
            || self.view_doc.is_some()
            || self.settings_dialog.is_some()
    }

    #[allow(clippy::too_many_arguments)]
    fn ask(&mut self, title: &str, message: String, ok_label: &str, danger: bool, pending: Pending, input: Option<String>, checkbox: Option<(String, bool)>) {
        self.confirm = Some(Confirm { title: title.into(), message, ok_label: ok_label.into(), danger, pending, input, checkbox });
    }

    /// The collection tab's view for `ns`, if one is open.
    fn view_for(&self, conn: ConnId, ns: &Namespace) -> Option<u64> {
        self.tabs.iter().find_map(|t| match &t.kind {
            TabKind::Collection(c) if c.conn == conn && c.ns == *ns => Some(c.view.id),
            _ => None,
        })
    }

    fn open_info(&mut self, conn: ConnId, title: String, doc: Document) {
        let id = self.net().view_id();
        let mut view = ResultView::new(id, title, Source::Static { docs: vec![doc], truncated: false });
        view.mode = Mode::Json;
        let tab_id = self.next_tab();
        self.tabs.push(Tab { id: tab_id, kind: TabKind::Info { conn, view: Box::new(view) } });
        self.active_tab = self.tabs.len() - 1;
    }

    /// Collection the toolbar's Export/Import act on: the active collection
    /// tab, else the tree selection.
    fn target_collection(&self) -> Option<(ConnId, String, Option<String>, Option<FindSpec>)> {
        if let Some(Tab { kind: TabKind::Collection(c), .. }) = self.tabs.get(self.active_tab) {
            let spec = match &c.view.source {
                Source::Find { spec, .. } => Some(spec.clone()),
                Source::Static { .. } => None,
            };
            return Some((c.conn, c.ns.db.clone(), Some(c.ns.coll.clone()), spec));
        }
        let sel = &self.sidebar.selection;
        Some((sel.conn?, sel.db.clone()?, sel.coll.clone(), None))
    }

    fn toast(&mut self, text: impl Into<String>, error: bool) {
        let secs = if error { 8 } else { 4 };
        self.toasts.push(Toast { text: text.into(), error, until: Instant::now() + Duration::from_secs(secs) });
    }

    fn next_tab(&mut self) -> u64 {
        self.next_tab_id += 1;
        self.next_tab_id
    }

    fn open_demo(&mut self) {
        let profile = ConnectionProfile::mock();
        let cid = profile.conn_id();
        if !self.conns.contains_key(&cid) {
            self.connect(profile);
        }
        self.open_shell(cid, Some("demo-games".into()), Some(DEMO_SCRIPT.into()));
        if let Some(TabKind::Shell(s)) = self.tabs.last_mut().map(|t| &mut t.kind) {
            s.run_when_ready = true;
        }
    }

    // ------------------------------------------------------------ connections

    fn connect(&mut self, profile: ConnectionProfile) {
        let cid = profile.conn_id();
        if let Some(c) = self.conns.get(&cid)
            && !matches!(c.status, Status::Failed(_))
        {
            self.sidebar.selection = sidebar::Selection { conn: Some(cid), db: None, coll: None };
            return;
        }
        let (password, ssh_secret) = if profile.is_mock() {
            (None, None)
        } else {
            // Only touch the keychain when the profile has a password: each
            // read may make macOS ask for permission.
            let pw = if profile.uses_password() {
                profile::get_secret(profile.id, SecretKind::Password).unwrap_or_else(|e| {
                    self.toast(e.to_string(), true);
                    None
                })
            } else {
                None
            };
            let ssh = if profile.needs_ssh_secret() { profile::get_secret(profile.id, SecretKind::Ssh).ok().flatten() } else { None };
            (pw, ssh)
        };
        self.conns.insert(cid, OpenConn::new(profile.clone()));
        self.sidebar.open = true;
        self.sidebar.selection = sidebar::Selection { conn: Some(cid), db: None, coll: None };
        let req = ConnectRequest { profile, password, ssh_secret };
        self.net().send(Origin::Connect(cid), None, Command::Connect(Box::new(req)));
    }

    fn disconnect(&mut self, cid: ConnId) {
        self.net().send(Origin::None, Some(cid), Command::Disconnect);
        self.conns.remove(&cid);
        self.tabs.retain(|t| t.conn() != Some(cid));
        if self.tabs.is_empty() {
            let id = self.next_tab();
            self.tabs.push(Tab { id, kind: TabKind::Welcome });
        }
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        if self.sidebar.selection.conn == Some(cid) {
            self.sidebar.selection = sidebar::Selection::default();
        }
    }

    fn refresh_tree(&mut self, cid: ConnId) {
        let Some(conn) = self.conns.get_mut(&cid) else { return };
        if let Some(dbs) = conn.dbs.as_mut() {
            for db in dbs {
                db.colls = None;
                db.loading = false;
            }
        }
        conn.loading_dbs = true;
        self.net().send(Origin::Databases(cid), Some(cid), Command::ListDatabases);
    }

    // ------------------------------------------------------------ tabs

    fn open_collection(&mut self, cid: ConnId, ns: Namespace) {
        if let Some(i) = self.tabs.iter().position(|t| matches!(&t.kind, TabKind::Collection(c) if c.conn == cid && c.ns == ns)) {
            self.active_tab = i;
            return;
        }
        let tab_id = self.next_tab();
        let view_id = self.net().view_id();
        self.tabs.push(Tab { id: tab_id, kind: TabKind::Collection(Box::new(CollectionTab::new(cid, ns, view_id))) });
        self.active_tab = self.tabs.len() - 1;
    }

    /// IntelliShell on the collection's database with a find() already run.
    fn open_collection_shell(&mut self, cid: ConnId, ns: Namespace) {
        let query = format!("db.getCollection({}).find({{}})\n", serde_json::to_string(&ns.coll).unwrap_or_default());
        // Sample the fields now so suggestions are ready when typing starts.
        if self.conns.get(&cid).is_some_and(|c| !c.fields.contains_key(&ns)) {
            self.net().send(Origin::Fields(cid, ns.clone()), Some(cid), Command::Find(FindSpec::all(ns.clone(), 100)));
        }
        self.open_shell(cid, Some(ns.db), Some(query));
        if let Some(TabKind::Shell(s)) = self.tabs.last_mut().map(|t| &mut t.kind) {
            s.run_when_ready = true;
        }
    }

    fn open_shell(&mut self, cid: ConnId, db: Option<String>, text: Option<String>) {
        let db = db
            .or_else(|| self.conns.get(&cid).and_then(|c| c.profile.default_db.clone()))
            .unwrap_or_else(|| "test".into());
        let tab_id = self.next_tab();
        let text = text.unwrap_or_default();
        self.tabs.push(Tab { id: tab_id, kind: TabKind::Shell(Box::new(ShellTab::new(cid, db, tab_id, text))) });
        self.active_tab = self.tabs.len() - 1;
    }

    fn open_shell_for_selection(&mut self) {
        let sel = self.sidebar.selection.clone();
        let cid = sel.conn.or_else(|| self.conns.iter().find(|(_, c)| c.info().is_some()).map(|(id, _)| *id));
        let Some(cid) = cid else {
            self.toast("Connect to a server first", true);
            return;
        };
        let text = sel.coll.as_ref().map(|c| format!("db.getCollection({}).find({{}})\n", serde_json::to_string(c).unwrap_or_default()));
        self.open_shell(cid, sel.db.clone(), text);
    }

    fn close_tab(&mut self, i: usize) {
        if let TabKind::Shell(s) = &self.tabs[i].kind
            && s.is_running()
        {
            self.toast("Stop the running script before closing its tab", true);
            return;
        }
        self.tabs.remove(i);
        if self.tabs.is_empty() {
            let id = self.next_tab();
            self.tabs.push(Tab { id, kind: TabKind::Welcome });
        }
        if self.active_tab >= self.tabs.len() || self.active_tab > i {
            self.active_tab = self.active_tab.saturating_sub(1);
        }
    }

    fn view_mut(&mut self, id: u64) -> Option<&mut ResultView> {
        self.tabs.iter_mut().flat_map(|t| t.views_mut()).find(|v| v.id == id)
    }

    // ------------------------------------------------------------ responses

    fn handle(&mut self, r: Response) {
        let origin = self.pending.remove(&r.id).unwrap_or(Origin::None);
        match (origin, r.event) {
            (Origin::None, _) => {}
            (Origin::Connect(cid), Event::Connected(info)) => {
                if let Some(c) = self.conns.get_mut(&cid) {
                    c.status = Status::Connected(info);
                    c.dbs = None;
                    c.loading_dbs = false;
                } else {
                    // Disconnected while connecting: the core has just
                    // registered it (and maybe an SSH tunnel). Close it.
                    self.core.send(Some(cid), Command::Disconnect);
                }
            }
            (Origin::Connect(cid), Event::Failed(e)) => {
                let name = self.conns.get(&cid).map(|c| c.profile.name.clone()).unwrap_or_default();
                if let Some(c) = self.conns.get_mut(&cid) {
                    c.status = Status::Failed(e.to_string());
                }
                self.toast(format!("{name}: {e}"), true);
            }
            (Origin::Test(cid), event) => {
                // Close the test connection even if the dialog has gone.
                if matches!(event, Event::Connected(_)) {
                    self.core.send(Some(cid), Command::Disconnect);
                }
                if let Some(d) = &mut self.connect_dialog
                    && let TestState::Running(req) = d.test
                    && req == r.id
                {
                    d.test = match event {
                        Event::Connected(info) => {
                            let rs = info.replica_set.map(|s| format!(", replica set {s}")).unwrap_or_default();
                            TestState::Ok(format!("Connected: MongoDB {} on {} ({}{rs})", info.version, info.host, info.role))
                        }
                        Event::Failed(e) => TestState::Failed(e.to_string()),
                        _ => TestState::Idle,
                    };
                }
            }
            (Origin::Databases(cid), Event::Databases(dbs)) => {
                if let Some(c) = self.conns.get_mut(&cid) {
                    c.set_databases(dbs);
                }
            }
            (Origin::Collections(cid, db), Event::Collections { items, .. }) => {
                if let Some(node) = self.conns.get_mut(&cid).and_then(|c| c.db_mut(&db)) {
                    node.colls = Some(items.into_iter().map(CollNode::new).collect());
                    node.loading = false;
                }
            }
            (Origin::Indexes(cid, ns), Event::Indexes { items, .. }) => {
                if let Some(coll) = self
                    .conns
                    .get_mut(&cid)
                    .and_then(|c| c.db_mut(&ns.db))
                    .and_then(|d| d.colls.as_mut())
                    .and_then(|cs| cs.iter_mut().find(|c| c.info.name == ns.coll))
                {
                    coll.indexes = Some(items);
                    coll.loading = false;
                }
            }
            // A failed listing shows as empty (not a spinner forever, and
            // not retried every frame); Refresh tries again.
            (Origin::Databases(cid), Event::Failed(e)) => {
                if let Some(c) = self.conns.get_mut(&cid) {
                    c.loading_dbs = false;
                    if c.dbs.is_none() {
                        c.dbs = Some(Vec::new());
                    }
                }
                self.toast(e.to_string(), true);
            }
            (Origin::Collections(cid, db), Event::Failed(e)) => {
                if let Some(node) = self.conns.get_mut(&cid).and_then(|c| c.db_mut(&db)) {
                    node.loading = false;
                    node.colls.get_or_insert_with(Vec::new);
                }
                self.toast(format!("{db}: {e}"), true);
            }
            (Origin::Indexes(cid, ns), Event::Failed(e)) => {
                if let Some(coll) = self.coll_node(cid, &ns) {
                    coll.loading = false;
                    coll.indexes.get_or_insert_with(Vec::new);
                }
                self.toast(format!("{ns}: {e}"), true);
            }
            (Origin::OpenDoc { conn, ns, view, clone }, Event::Page { docs, .. }) => match docs.into_iter().next().map(Document::try_from) {
                Some(Ok(mut doc)) if clone => {
                    doc.remove("_id");
                    self.doc_dialog = Some(DocDialog::insert(conn, ns, Some(&doc), Some(view)));
                }
                Some(Ok(doc)) => self.doc_dialog = Some(DocDialog::edit(conn, ns, &doc, Some(view))),
                Some(Err(e)) => self.toast(format!("Could not read the document: {e}"), true),
                None => self.toast("The document no longer exists; refresh the view", true),
            },
            (Origin::OpenDoc { .. }, Event::Failed(e)) => self.toast(e.to_string(), true),
            (Origin::View(vid, ViewReq::RefreshDoc(row)), Event::Page { docs, .. }) => {
                if let Some(v) = self.view_mut(vid) {
                    v.on_doc(r.id, row, docs);
                }
            }
            (Origin::View(vid, ViewReq::Page), Event::Page { docs, has_more, elapsed_ms }) => {
                if let Some(v) = self.view_mut(vid) {
                    v.on_page(r.id, docs, has_more, elapsed_ms);
                }
            }
            (Origin::View(vid, ViewReq::Count), Event::Count(n)) => {
                if let Some(v) = self.view_mut(vid) {
                    v.on_count(r.id, n);
                }
            }
            (Origin::View(vid, _), Event::Failed(e)) => {
                if e != CoreError::Cancelled
                    && let Some(v) = self.view_mut(vid)
                {
                    v.on_error(r.id, e.to_string());
                }
            }
            // Replies go to the dialog that sent them; a dialog cancelled
            // while saving may have been replaced by another one since.
            (Origin::Save { view }, Event::Written(_)) => {
                let mine = self.doc_dialog.as_ref().filter(|d| d.saving == Some(r.id));
                let inserted = mine.is_some_and(|d| matches!(d.mode, DocMode::Insert));
                if mine.is_some() {
                    self.doc_dialog = None;
                }
                if let Some(v) = view.and_then(|v| self.view_mut(v)) {
                    v.refresh();
                }
                self.toast(if inserted { "Document added" } else { "Document saved" }, false);
            }
            (Origin::Save { .. }, Event::Failed(e)) => match &mut self.doc_dialog {
                Some(d) if d.saving == Some(r.id) => {
                    d.saving = None;
                    d.error = Some(e.to_string());
                }
                _ => self.toast(format!("Saving the document failed: {e}"), true),
            },
            (Origin::Notify { .. } | Origin::Info { .. }, Event::Failed(e)) => self.toast(e.to_string(), true),
            (Origin::Notify { view, tree }, ev) => {
                self.toast(describe(&ev), false);
                self.after_write(view, tree);
            }
            (Origin::Info { conn, title }, Event::Document(d)) => self.open_info(conn, title, d),
            (Origin::Fields(cid, ns), event) => {
                let fields = match event {
                    Event::Page { docs, .. } => {
                        let docs: Vec<rongo_core::bson::Document> = docs.into_iter().filter_map(|d| d.try_into().ok()).collect();
                        rongo_core::format::field_paths(&docs, 3)
                    }
                    _ => Vec::new(),
                };
                if let Some(t) = &mut self.tool {
                    t.set_known_fields(&ns, &fields);
                }
                if let Some(c) = self.conns.get_mut(&cid) {
                    c.fields.insert(ns, fields);
                }
            }
            (Origin::HoverStats(cid, ns), event) => {
                if let Some(node) = self.coll_node(cid, &ns) {
                    node.stats_loading = false;
                    node.stats = match event {
                        Event::Document(d) => Some(Ok((rongo_core::CollSummary::from_stats(&d), rongo_core::format::local_now()))),
                        Event::Failed(e) => Some(Err(e.to_string())),
                        _ => node.stats.take(),
                    };
                }
            }
            (Origin::IndexTab(tab), event) => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab)
                    && let TabKind::Indexes(it) = &mut t.kind
                {
                    match event {
                        Event::IndexDetails { items, .. } => it.on_details(r.id, items),
                        Event::Failed(e) => it.on_error(r.id, e.to_string()),
                        _ => {}
                    }
                }
            }
            (Origin::Validator, Event::Document(d)) => {
                if let Some(t) = &mut self.tool {
                    t.set_validator(&d);
                }
            }
            (Origin::Validator, Event::Failed(e)) => {
                if let Some(t) = &mut self.tool {
                    t.set_validator(&doc! {});
                    t.error = Some(format!("Could not read the current validator: {e}"));
                }
            }
            (Origin::Tool { .. }, Event::Failed(e)) => match &mut self.tool {
                Some(t) if t.running == Some(r.id) => {
                    t.running = None;
                    t.error = Some(e.to_string());
                }
                _ => self.toast(e.to_string(), true),
            },
            (Origin::Tool { view, tree }, ev) => {
                let mine = self.tool.as_ref().is_some_and(|t| t.running == Some(r.id));
                let what = if mine { self.tool.take().map_or("Done", |t| t.success_message()) } else { "Done" };
                let detail = describe(&ev);
                self.toast(if detail == "Done" { what.to_string() } else { format!("{what}: {detail}") }, false);
                self.after_write(view, tree);
            }
            _ => {}
        }
    }

    fn after_write(&mut self, view: Option<u64>, tree: Option<ConnId>) {
        if let Some(v) = view.and_then(|v| self.view_mut(v)) {
            v.refresh();
        }
        if let Some(cid) = tree {
            self.refresh_tree(cid);
            for t in &mut self.tabs {
                if let TabKind::Indexes(it) = &mut t.kind
                    && it.conn == cid
                {
                    it.refresh();
                }
            }
        }
    }

    fn coll_node(&mut self, cid: ConnId, ns: &Namespace) -> Option<&mut CollNode> {
        self.conns.get_mut(&cid)?.db_mut(&ns.db)?.colls.as_mut()?.iter_mut().find(|c| c.info.name == ns.coll)
    }

    fn open_indexes(&mut self, cid: ConnId, ns: Namespace) {
        if let Some(i) = self.tabs.iter().position(|t| matches!(&t.kind, TabKind::Indexes(it) if it.conn == cid && it.ns == ns)) {
            self.active_tab = i;
            return;
        }
        let tab_id = self.next_tab();
        self.tabs.push(Tab { id: tab_id, kind: TabKind::Indexes(Box::new(IndexTab::new(tab_id, cid, ns))) });
        self.active_tab = self.tabs.len() - 1;
    }

    /// Open Add Index with the collection's field names for the picker,
    /// sampling them first if they are not known yet.
    fn add_index_dialog(&mut self, conn: ConnId, ns: Namespace) {
        let known = self.conns.get(&conn).and_then(|c| c.fields.get(&ns).cloned());
        if known.is_none() {
            self.net().send(Origin::Fields(conn, ns.clone()), Some(conn), Command::Find(FindSpec::all(ns.clone(), 100)));
        }
        self.tool = Some(ToolDialog::index(conn, ns, known.unwrap_or_default()));
    }

    fn index_action(&mut self, a: IndexAction) {
        match a {
            IndexAction::Add(conn, ns) => self.add_index_dialog(conn, ns),
            IndexAction::Drop(conn, ns, name) => {
                let msg = format!("Drop the index {name} on {ns}?");
                self.ask("Drop Index", msg, "Drop", true, Pending::DropIndex { conn, ns, name }, None, None);
            }
            IndexAction::Edit(conn, ns, spec) => self.tool = Some(ToolDialog::edit_index(conn, ns, &spec)),
            IndexAction::View(doc) => self.view_doc = Some(ViewDocDialog::new(&doc)),
            IndexAction::SetHidden(conn, ns, name, hidden) => {
                let cmd = Command::ModifyIndex { ns, name, hidden: Some(hidden), expire_after_seconds: None };
                self.net().send(Origin::Notify { view: None, tree: Some(conn) }, Some(conn), cmd);
            }
        }
    }

    // ------------------------------------------------------------ actions

    fn tree_action(&mut self, a: TreeAction) {
        match a {
            TreeAction::OpenCollection(cid, ns) => match self.settings.open_collection_in {
                OpenIn::IntelliShell => self.open_collection_shell(cid, ns),
                OpenIn::CollectionTab => self.open_collection(cid, ns),
            },
            TreeAction::OpenCollectionShell(cid, ns) => self.open_collection_shell(cid, ns),
            TreeAction::OpenCollectionTab(cid, ns) => self.open_collection(cid, ns),
            TreeAction::OpenShell(cid, db) => self.open_shell(cid, db, None),
            TreeAction::Refresh(cid) => self.refresh_tree(cid),
            TreeAction::Disconnect(cid) => self.disconnect(cid),
            TreeAction::Reconnect(cid) => {
                if let Some(p) = self.conns.get(&cid).map(|c| c.profile.clone()) {
                    self.conns.remove(&cid);
                    self.connect(p);
                }
            }
            TreeAction::RefreshAll => {
                let ids: Vec<ConnId> = self.conns.keys().copied().collect();
                for cid in ids {
                    self.refresh_tree(cid);
                }
            }
            TreeAction::DisconnectOthers(keep) => {
                let ids: Vec<ConnId> = self.conns.keys().copied().filter(|c| *c != keep).collect();
                for cid in ids {
                    self.disconnect(cid);
                }
            }
            TreeAction::DisconnectAll => {
                let ids: Vec<ConnId> = self.conns.keys().copied().collect();
                for cid in ids {
                    self.disconnect(cid);
                }
            }
            TreeAction::CreateCollection(conn, db) => {
                let msg = format!("Name of the new collection in {db}:");
                self.ask("Add Collection", msg, "Create", false, Pending::CreateCollection { conn, db }, Some(String::new()), None);
            }
            TreeAction::DropCollection(conn, ns) => {
                let msg = format!("Drop {ns} and all of its documents and indexes? This cannot be undone.");
                self.ask("Drop Collection", msg, "Drop", true, Pending::DropCollection { conn, ns }, None, None);
            }
            TreeAction::DropDatabase(conn, db) => {
                let msg = format!("Type the database name to drop {db} and everything in it. This cannot be undone.");
                self.ask("Drop Database", msg, "Drop", true, Pending::DropDatabase { conn, db }, Some(String::new()), None);
            }
            TreeAction::Export(conn, ns) => self.tool = Some(ToolDialog::export(conn, FindSpec::all(ns, 50), false)),
            TreeAction::Import(conn, db, coll) => self.tool = Some(ToolDialog::import(conn, db, coll.unwrap_or_default())),
            TreeAction::Rename(conn, ns) => {
                let msg = format!("New name for {ns}:");
                let current = ns.coll.clone();
                self.ask("Rename Collection", msg, "Rename", false, Pending::RenameCollection { conn, ns }, Some(current), None);
            }
            TreeAction::Duplicate(conn, ns) => {
                let msg = format!("Copy {ns} (documents and indexes) into a new collection named:");
                let suggested = format!("{}_copy", ns.coll);
                self.ask("Duplicate Collection", msg, "Duplicate", false, Pending::DuplicateCollection { conn, ns }, Some(suggested), None);
            }
            TreeAction::Clear(conn, ns) => {
                let msg = format!("Delete every document in {ns}? The collection and its indexes are kept. This cannot be undone.");
                self.ask("Clear Collection", msg, "Clear", true, Pending::ClearCollection { conn, ns }, None, None);
            }
            TreeAction::AddIndex(conn, ns) => self.add_index_dialog(conn, ns),
            TreeAction::OpenIndexes(conn, ns) => self.open_indexes(conn, ns),
            TreeAction::DropIndex(conn, ns, name) => {
                let msg = format!("Drop the index {name} on {ns}?");
                self.ask("Drop Index", msg, "Drop", true, Pending::DropIndex { conn, ns, name }, None, None);
            }
            TreeAction::Validator(conn, ns) => {
                self.net().send(Origin::Validator, Some(conn), Command::GetValidator { ns: ns.clone() });
                self.tool = Some(ToolDialog::validator(conn, ns));
            }
            TreeAction::CollStats(conn, ns) => {
                let title = format!("Statistics: {}", ns.coll);
                self.net().send(Origin::Info { conn, title }, Some(conn), Command::CollStats { ns });
            }
            TreeAction::DbStats(conn, db) => {
                let title = format!("Statistics: {db}");
                self.net().send(Origin::Info { conn, title }, Some(conn), Command::RunCommand { db, cmd: doc! { "dbStats": 1 } });
            }
            TreeAction::ServerInfo(conn, kind) => {
                let title = kind.title().to_string();
                self.net().send(Origin::Info { conn, title }, Some(conn), Command::RunCommand { db: "admin".into(), cmd: kind.command() });
            }
            TreeAction::MoveConnection { id, before } => self.move_profile(id.0, before.map(|b| b.0)),
            TreeAction::SetColor(cid, color) => self.set_color(cid.0, color),
        }
    }

    fn move_profile(&mut self, id: uuid::Uuid, before: Option<uuid::Uuid>) {
        if let Some(store) = &mut self.store
            && let Err(e) = store.move_before(id, before)
        {
            self.toast(e.to_string(), true);
        }
    }

    fn set_color(&mut self, id: uuid::Uuid, color: Option<String>) {
        if let Some(c) = self.conns.get_mut(&ConnId(id)) {
            c.profile.color = color.clone();
        }
        // The demo connection isn't saved; its colour lasts until it closes.
        if let Some(store) = &mut self.store
            && let Some(mut p) = store.get(id).cloned()
        {
            p.color = color;
            if let Err(e) = store.upsert(p, None) {
                self.toast(e.to_string(), true);
            }
        }
    }

    /// Open the editor on a document, fetched whole by `_id` first: the row
    /// on screen may be cut down by a projection, and saving that would
    /// replace the document and drop the other fields.
    fn open_doc(&mut self, conn: ConnId, ns: Namespace, doc: &Document, view: u64, clone: bool) {
        let Some(id) = doc.get("_id").cloned() else {
            self.toast("This document has no _id, so it cannot be edited here", true);
            return;
        };
        let spec = FindSpec { filter: doc! { "_id": id }, ..FindSpec::all(ns.clone(), 1) };
        self.net().send(Origin::OpenDoc { conn, ns, view, clone }, Some(conn), Command::Find(spec));
    }

    fn view_action(&mut self, view: u64, a: ViewAction) {
        match a {
            ViewAction::Edit { conn, ns, doc } => self.open_doc(conn, ns, &doc, view, false),
            ViewAction::Clone { conn, ns, doc } => self.open_doc(conn, ns, &doc, view, true),
            ViewAction::Insert { conn, ns, template } => {
                self.doc_dialog = Some(DocDialog::insert(conn, ns, template.as_ref(), Some(view)));
            }
            ViewAction::Delete { conn, ns, ids } => {
                let n = ids.len();
                let msg = format!("Delete {n} document{} from {ns}?", if n == 1 { "" } else { "s" });
                self.ask("Delete Documents", msg, "Delete", true, Pending::Delete { conn, ns, ids, view: Some(view) }, None, None);
            }
            ViewAction::View { doc } => self.view_doc = Some(ViewDocDialog::new(&doc)),
            ViewAction::EditValue { conn, ns, id, path, value } => {
                self.tool = Some(ToolDialog::value(conn, Some(view), ns, id, path, &value));
            }
            ViewAction::UpdateOne { conn, ns, id, update } => {
                let cmd = Command::UpdateMany { ns, filter: doc! { "_id": id }, update };
                self.net().send(Origin::Notify { view: Some(view), tree: None }, Some(conn), cmd);
            }
            ViewAction::Paste { conn, ns, text } => match rongo_shell::parse_documents_text(&text) {
                Ok(docs) => {
                    let n = docs.len();
                    let has_ids = docs.iter().any(|d| d.contains_key("_id"));
                    let msg = format!("Import {n} document{} from the clipboard into {ns}?", if n == 1 { "" } else { "s" });
                    let checkbox = has_ids.then(|| ("Remove _id fields (insert as new documents)".to_string(), false));
                    self.ask("Paste Documents", msg, "OK", false, Pending::Paste { conn, ns, docs, view: Some(view) }, None, checkbox);
                }
                Err(e) => self.toast(format!("Nothing to paste: {}", e.message), true),
            },
            ViewAction::Bulk { conn, ns, delete, selected, query, field } => {
                self.tool = Some(ToolDialog::bulk(conn, ns, Some(view), delete, selected, query, field));
            }
            ViewAction::Export { conn, spec } => self.tool = Some(ToolDialog::export(conn, spec, true)),
            ViewAction::SortChanged(_) => {}
        }
    }

    fn confirmed(&mut self, c: Confirm) {
        let input = c.input.unwrap_or_default().trim().to_string();
        let ticked = c.checkbox.is_some_and(|(_, t)| t);
        let tree = |conn| Origin::Notify { view: None, tree: Some(conn) };
        match c.pending {
            Pending::Delete { conn, ns, ids, view } => {
                self.net().send(Origin::Notify { view, tree: None }, Some(conn), Command::Delete { ns, ids });
            }
            Pending::DropCollection { conn, ns } => {
                self.close_tabs_for(conn, Some(&ns));
                self.net().send(tree(conn), Some(conn), Command::DropCollection { ns });
            }
            Pending::DropDatabase { conn, db } => {
                if input != db {
                    self.toast("The name did not match; nothing was dropped", true);
                    return;
                }
                self.net().send(tree(conn), Some(conn), Command::DropDatabase { db });
            }
            Pending::CreateCollection { conn, db } => {
                let ns = Namespace::new(db, input);
                self.net().send(tree(conn), Some(conn), Command::CreateCollection { ns });
            }
            Pending::RenameCollection { conn, ns } => {
                if input == ns.coll {
                    return;
                }
                self.close_tabs_for(conn, Some(&ns));
                self.net().send(tree(conn), Some(conn), Command::RenameCollection { ns, to: input });
            }
            Pending::DuplicateCollection { conn, ns } => {
                self.net().send(tree(conn), Some(conn), Command::DuplicateCollection { ns, to: input });
            }
            Pending::ClearCollection { conn, ns } => {
                let view = self.view_for(conn, &ns);
                self.net().send(Origin::Notify { view, tree: None }, Some(conn), Command::ClearCollection { ns });
            }
            Pending::DropIndex { conn, ns, name } => {
                self.net().send(tree(conn), Some(conn), Command::DropIndex { ns, name });
            }
            Pending::Paste { conn, ns, mut docs, view } => {
                if ticked {
                    for d in &mut docs {
                        d.remove("_id");
                    }
                }
                self.net().send(Origin::Notify { view, tree: None }, Some(conn), Command::InsertMany { ns, docs });
            }
        }
    }

    fn close_tabs_for(&mut self, conn: ConnId, ns: Option<&Namespace>) {
        let mut i = 0;
        while i < self.tabs.len() {
            let hit = matches!(&self.tabs[i].kind, TabKind::Collection(c) if c.conn == conn && ns.is_none_or(|n| *n == c.ns));
            if hit { self.close_tab(i) } else { i += 1 }
        }
    }

    fn connect_action(&mut self, a: ConnectAction) {
        match a {
            ConnectAction::Close => self.connect_dialog = None,
            ConnectAction::Connect(id) => {
                if let Some(p) = self.store.as_ref().and_then(|s| s.get(id)).cloned() {
                    self.connect_dialog = None;
                    self.connect(p);
                }
            }
            ConnectAction::Move { id, before } => self.move_profile(id, before),
            ConnectAction::SetColor(id, color) => self.set_color(id, color),
            ConnectAction::Delete(id) => {
                if let Some(s) = &mut self.store
                    && let Err(e) = s.remove(id)
                {
                    self.toast(e.to_string(), true);
                }
            }
            ConnectAction::Test(mut req) => {
                // Its own id: under the profile's id the test would replace
                // that profile's open connection, then disconnect it.
                req.profile.id = uuid::Uuid::new_v4();
                let cid = req.profile.conn_id();
                let id = self.net().send(Origin::Test(cid), None, Command::Connect(req));
                if let Some(d) = &mut self.connect_dialog {
                    d.test = TestState::Running(id);
                }
            }
            ConnectAction::Save { profile, password, ssh_secret, then_connect } => {
                let Some(store) = &mut self.store else {
                    self.toast("Saved connections are unavailable", true);
                    return;
                };
                let id = profile.id;
                let mut result = store.upsert(profile, password);
                if result.is_ok()
                    && let Some(s) = ssh_secret
                {
                    result = profile::set_secret(id, SecretKind::Ssh, &s);
                }
                // An open connection shows the new name and colour straight
                // away; the rest applies when it next connects.
                if result.is_ok()
                    && let (Some(p), Some(c)) = (self.store.as_ref().and_then(|s| s.get(id)), self.conns.get_mut(&ConnId(id)))
                {
                    c.profile.name = p.name.clone();
                    c.profile.color = p.color.clone();
                }
                match result {
                    Err(e) => self.toast(e.to_string(), true),
                    Ok(()) if then_connect => {
                        if let Some(p) = self.store.as_ref().and_then(|s| s.get(id)).cloned() {
                            if self.conns.contains_key(&p.conn_id()) {
                                self.disconnect(p.conn_id());
                            }
                            self.connect_dialog = None;
                            self.connect(p);
                        }
                    }
                    Ok(()) => self.toast("Connection saved", false),
                }
            }
        }
    }

    // ------------------------------------------------------------ drawing

    fn toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().button_padding = egui::vec2(10.0, 5.0);
            let profiles: Vec<ConnectionProfile> = self.store.as_ref().map(|s| s.profiles().to_vec()).unwrap_or_default();
            ui.menu_button(RichText::new(format!("{}  Connect  {}", icon::PLUGS_CONNECTED, icon::CARET_DOWN)).size(14.0), |ui| {
                if ui.button(format!("{}  Connection Manager…", icon::LIST)).clicked() {
                    self.connect_dialog = Some(ConnectDialog::new());
                    ui.close();
                }
                if ui.button(format!("{}  New connection…", icon::PLUS)).clicked() {
                    self.connect_dialog = Some(ConnectDialog::new_connection());
                    ui.close();
                }
                ui.separator();
                for p in &profiles {
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
                        self.connect(p.clone());
                        ui.close();
                    }
                }
                if !profiles.is_empty() {
                    ui.separator();
                }
                if ui.button(format!("{}  Demo server (in-memory)", icon::FLASK)).clicked() {
                    self.open_demo();
                    ui.close();
                }
            });
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
            if ui.button(RichText::new(format!("{}  Import", icon::DOWNLOAD_SIMPLE)).size(14.0)).on_hover_text("Import a JSON or CSV file").clicked() {
                match self.target_collection() {
                    Some((conn, db, coll, _)) => self.tool = Some(ToolDialog::import(conn, db, coll.unwrap_or_default())),
                    None => self.toast("Select a database or collection in the tree first", true),
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button(RichText::new(format!("{}  Settings", icon::GEAR)).size(14.0)).on_hover_text("Settings (⌘,)").clicked() {
                    self.settings_dialog = Some(SettingsDialog::new(&self.settings));
                }
                ui.weak(format!("rongo {}", env!("CARGO_PKG_VERSION")));
            });
        });
    }

    fn tab_bar(&mut self, ui: &mut Ui) {
        let names: HashMap<ConnId, String> = self.conns.iter().map(|(id, c)| (*id, c.profile.name.clone())).collect();
        let colors: HashMap<ConnId, egui::Color32> = self.conns.iter().filter_map(|(id, c)| Some((*id, theme::conn_color(c.profile.color.as_deref())?))).collect();
        let mut close = None;
        egui::ScrollArea::horizontal().id_salt("tabbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                for (i, tab) in self.tabs.iter().enumerate() {
                    let title = tab.title(|c| names.get(&c).cloned().unwrap_or_else(|| "?".into()));
                    let active = i == self.active_tab;
                    let fill = if active { theme::BG } else { theme::PANEL };
                    let tag = tab.conn().and_then(|c| colors.get(&c).copied());
                    let frame = ui.push_id(tab.id, |ui| egui::Frame::new().fill(fill).corner_radius(egui::CornerRadius { nw: 5, ne: 5, sw: 0, se: 0 }).inner_margin(egui::Margin::symmetric(8, 4)).show(ui, |ui| {
                        let color = if active { theme::TEXT } else { theme::MUTED };
                        let r = ui.add(egui::Label::new(RichText::new(&title).color(color)).sense(egui::Sense::click()).selectable(false));
                        if r.clicked() {
                            self.active_tab = i;
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
                }
            });
        });
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
                self.connect_dialog = Some(ConnectDialog::new());
            }
            if ui.button(RichText::new(format!("{}  New connection", icon::PLUS)).size(15.0)).clicked() {
                self.connect_dialog = Some(ConnectDialog::new_connection());
            }
            if ui.button(RichText::new(format!("{}  Try the demo server", icon::FLASK)).size(15.0)).clicked() {
                self.open_demo();
            }
            let profiles: Vec<ConnectionProfile> = self.store.as_ref().map(|s| s.profiles().to_vec()).unwrap_or_default();
            if !profiles.is_empty() {
                ui.add_space(20.0);
                ui.label(RichText::new("Saved connections").strong());
                for p in profiles.iter().take(8) {
                    if ui.link(format!("{}  {}", icon::DATABASE, p.name)).on_hover_text(p.summary()).clicked() {
                        self.connect(p.clone());
                    }
                }
            }
            ui.add_space(20.0);
            ui.weak("Double-click a collection to browse it · ⌘↩ runs the shell · ⌘W closes a tab");
        });
    }

    fn active_tab_ui(&mut self, ui: &mut Ui) {
        let keys_enabled = !self.modal_open();
        let Some(tab) = self.tabs.get_mut(self.active_tab) else { return };
        let mut net = Net { core: &self.core, pending: &mut self.pending, next_view_id: &mut self.next_view_id, keys_enabled };
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
                let core = &self.core;
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
        };
        for (view, a) in actions {
            self.view_action(view, a);
        }
    }

    fn toasts(&mut self, ctx: &egui::Context) {
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
                    ui.set_max_width(420.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(ico).color(color));
                        ui.label(&t.text);
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

    fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(d) = &mut self.connect_dialog {
            let profiles: Vec<ConnectionProfile> = self.store.as_ref().map(|s| s.profiles().to_vec()).unwrap_or_default();
            for a in d.show(ctx, &profiles) {
                self.connect_action(a);
            }
        }
        if let Some(d) = &mut self.doc_dialog {
            match d.show(ctx) {
                Some(DocAction::Close) => self.doc_dialog = None,
                Some(DocAction::Save(doc)) => {
                    let (conn, ns, view) = (d.conn, d.ns.clone(), d.view);
                    let cmd = match &d.mode {
                        DocMode::Edit { id } => {
                            if doc.get("_id").is_some_and(|new| new != id) {
                                d.error = Some("The _id cannot be changed. Clone the document instead.".into());
                                return;
                            }
                            Command::Replace { ns, id: id.clone(), doc }
                        }
                        DocMode::Insert => Command::Insert { ns, doc },
                    };
                    let req = Net { core: &self.core, pending: &mut self.pending, next_view_id: &mut self.next_view_id, keys_enabled: false }.send(Origin::Save { view }, Some(conn), cmd);
                    if let Some(d) = &mut self.doc_dialog {
                        d.saving = Some(req);
                    }
                }
                None => {}
            }
        }
        if let Some(d) = &mut self.settings_dialog {
            match d.show(ctx) {
                Some(SettingsAction::Close) => self.settings_dialog = None,
                Some(SettingsAction::Save(s)) => {
                    self.settings_dialog = None;
                    s.apply();
                    let saved = s.save();
                    self.settings = s;
                    // Cached JSON text holds dates in the old zone.
                    for t in &mut self.tabs {
                        for v in t.views_mut() {
                            v.relayout();
                        }
                    }
                    match saved {
                        Ok(()) => self.toast("Settings saved", false),
                        Err(e) => self.toast(format!("Settings applied but not saved: {e}"), true),
                    }
                }
                None => {}
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
                Some(ToolAction::Send(cmd)) => {
                    let (conn, view, tree) = (t.conn, t.view, t.refresh_tree.then_some(t.conn));
                    let mut net = Net { core: &self.core, pending: &mut self.pending, next_view_id: &mut self.next_view_id, keys_enabled: false };
                    let id = net.send(Origin::Tool { view, tree }, Some(conn), cmd);
                    if let Some(t) = &mut self.tool {
                        t.running = Some(id);
                    }
                }
                None => {}
            }
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
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.modal_open() {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Comma)) {
            self.settings_dialog = Some(SettingsDialog::new(&self.settings));
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
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::OpenBracket)) {
            self.active_tab = (self.active_tab + self.tabs.len() - 1) % self.tabs.len();
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::CloseBracket)) {
            self.active_tab = (self.active_tab + 1) % self.tabs.len();
        }
    }
}

impl eframe::App for RongoApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        while let Some(r) = self.core.try_recv() {
            self.handle(r);
        }
        let ctx = ui.ctx().clone();
        self.shortcuts(&ctx);
        // Scripts in tabs not on screen keep reporting: output, results, and
        // finishing (a finished script's tab can then be closed).
        let mut net = Net { core: &self.core, pending: &mut self.pending, next_view_id: &mut self.next_view_id, keys_enabled: false };
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
        egui::Panel::left("strip")
            .exact_size(26.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(theme::PANEL))
            .show(ui, |ui| sidebar::strip(ui, &mut self.sidebar));
        if self.sidebar.open {
            let mut actions = Vec::new();
            self.sidebar_rect = egui::Panel::left("sidebar")
                .resizable(true)
                .default_size(300.0)
                .size_range(200.0..=600.0)
                .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(egui::Margin::symmetric(6, 6)))
                .show(ui, |ui| {
                    let order: Vec<ConnId> = self.store.as_ref().map(|s| s.profiles().iter().map(|p| p.conn_id()).collect()).unwrap_or_default();
                    let mut net = Net { core: &self.core, pending: &mut self.pending, next_view_id: &mut self.next_view_id, keys_enabled: false };
                    actions = sidebar::tree(ui, &mut self.sidebar, &mut self.conns, &order, &mut net);
                })
                .response
                .rect;
            for a in actions {
                self.tree_action(a);
            }
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

        self.dialogs(&ctx);
        self.toasts(&ctx);
    }
}

/// A one-line summary of a write's outcome for a toast.
fn describe(ev: &Event) -> String {
    let plural = |n: u64| if n == 1 { "" } else { "s" };
    match ev {
        Event::Written(w) => {
            let mut parts = Vec::new();
            if w.inserted > 0 {
                parts.push(format!("inserted {} document{}", thousands(w.inserted), plural(w.inserted)));
            }
            if w.matched > 0 || w.modified > 0 {
                parts.push(format!("updated {} of {} matched", thousands(w.modified), thousands(w.matched)));
            }
            if w.deleted > 0 {
                parts.push(format!("deleted {} document{}", thousands(w.deleted), plural(w.deleted)));
            }
            if w.inserted_id.is_some() && w.inserted == 0 {
                parts.push("upserted 1 document".into());
            }
            if parts.is_empty() {
                "No documents matched".into()
            } else {
                let mut s = parts.join(", ");
                s[..1].make_ascii_uppercase();
                s
            }
        }
        Event::Exported { count, path } => format!("Exported {} document{} to {}", thousands(*count), plural(*count), path.display()),
        Event::Document(d) if d.contains_key("createdIndex") => format!("Created index {}", d.get_str("createdIndex").unwrap_or("?")),
        _ => "Done".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_input() -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1440.0, 900.0))),
            ..Default::default()
        }
    }

    /// Colouring and moving a connection save it, and an open connection
    /// shows the colour at once.
    #[test]
    fn colour_and_order_are_saved() {
        let ctx = egui::Context::default();
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = RongoApp::new(&cc, rt, false);
        let store = app.store.as_mut().expect("a store of the test's own");
        assert!(store.profiles().is_empty(), "not the real saved connections");
        let [a, b] = ["a", "b"].map(ConnectionProfile::new);
        store.upsert(a.clone(), None).unwrap();
        store.upsert(b.clone(), None).unwrap();
        app.conns.insert(a.conn_id(), crate::sidebar::OpenConn::new(a.clone()));

        app.tree_action(TreeAction::SetColor(a.conn_id(), Some("red".into())));
        assert_eq!(app.conns[&a.conn_id()].profile.color.as_deref(), Some("red"));
        assert_eq!(app.store.as_ref().unwrap().get(a.id).unwrap().color.as_deref(), Some("red"));

        app.tree_action(TreeAction::MoveConnection { id: b.conn_id(), before: Some(a.conn_id()) });
        let names: Vec<_> = app.store.as_ref().unwrap().profiles().iter().map(|p| p.name.clone()).collect();
        assert_eq!(names, ["b", "a"]);
    }

    /// Connect with several rows picked in the Connection Manager opens them
    /// all, though the first one closes the dialog.
    #[test]
    fn connect_opens_every_picked_profile() {
        let ctx = egui::Context::default();
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = RongoApp::new(&cc, rt, false);
        // In-memory demo servers: nothing goes over the network or to the keychain.
        let [a, b] = [1, 2].map(|n| ConnectionProfile { id: uuid::Uuid::from_u128(n), ..ConnectionProfile::mock() });
        let store = app.store.as_mut().expect("a store of the test's own");
        assert!(store.profiles().is_empty(), "not the real saved connections");
        store.upsert(a.clone(), None).unwrap();
        store.upsert(b.clone(), None).unwrap();
        app.connect_dialog = Some(ConnectDialog::new());
        for action in [ConnectAction::Connect(a.id), ConnectAction::Connect(b.id)] {
            app.connect_action(action);
        }
        assert!(app.connect_dialog.is_none());
        assert!(app.conns.contains_key(&a.conn_id()) && app.conns.contains_key(&b.conn_id()), "both opened");
    }

    /// Run the whole app (demo server, theme, sidebar, shell results, a
    /// collection tab) and check that, with no input, nothing on screen moves
    /// from one frame to the next. Two splitters used to creep a pixel or so
    /// per frame because their size came from their own content.
    #[test]
    fn nothing_moves_on_its_own() {
        let ctx = egui::Context::default();
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = RongoApp::new(&cc, rt, true);
        let mut frame = eframe::Frame::_new_kittest();
        let sidebar_width = |ctx: &egui::Context| egui::containers::panel::PanelState::load(ctx, egui::Id::new("sidebar")).map(|s| s.size().x);
        let mut widths = Vec::new();
        let mut step = |app: &mut RongoApp| {
            let mut out = ctx.run_ui(frame_input(), |ui| eframe::App::ui(app, ui, &mut frame));
            out.textures_delta.clear();
            widths.extend(sidebar_width(&ctx));
            format!("{:?}", out.shapes)
        };
        // Let the demo connect and the sample script run.
        for _ in 0..150 {
            step(&mut app);
            std::thread::sleep(Duration::from_millis(16));
        }
        let cid = ConnectionProfile::mock().conn_id();
        app.tree_action(TreeAction::OpenCollectionTab(cid, Namespace::new("demo-games", "providers")));
        for _ in 0..60 {
            step(&mut app);
            std::thread::sleep(Duration::from_millis(16));
        }
        for tab in [app.tabs.len() - 1, 1] {
            app.active_tab = tab;
            for _ in 0..10 {
                step(&mut app);
            }
            let first = step(&mut app);
            for i in 0..20 {
                assert!(step(&mut app) == first, "tab {tab}: the screen changed {i} frames later without input");
            }
        }
        // The search box sits under the toolbar and the demo connection's tree
        // starts right below it, on screen.
        let search = ctx.read_response(egui::Id::new("tree-search")).expect("search box").rect;
        let tree_top = ctx.read_response(egui::Id::new("tree-first-row")).expect("tree").rect;
        assert!(search.top() < 120.0, "search box at {search:?}");
        assert!(tree_top.top() < search.bottom() + 40.0, "tree starts at {tree_top:?}");
        // Nobody dragged the sidebar, so it must have kept its width the
        // whole time (a creeping panel stops moving once it hits its maximum).
        assert!(widths.len() > 200);
        assert!(widths.windows(2).all(|w| (w[0] - w[1]).abs() < 0.5), "sidebar width moved: first {:?}, last {:?}", widths.first(), widths.last());
    }

    /// A script that finishes while its tab is in the background is seen
    /// as finished, so its tab can be closed.
    #[test]
    fn background_scripts_finish() {
        let ctx = egui::Context::default();
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = RongoApp::new(&cc, rt, true);
        let mut frame = eframe::Frame::_new_kittest();
        let mut run = |app: &mut RongoApp, n: usize| {
            for _ in 0..n {
                let mut out = ctx.run_ui(frame_input(), |ui| eframe::App::ui(app, ui, &mut frame));
                out.textures_delta.clear();
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        run(&mut app, 40);
        let cid = ConnectionProfile::mock().conn_id();
        app.open_shell(cid, Some("demo-games".into()), Some("sleep(200); print('done')".into()));
        let shell_tab = app.tabs.len() - 1;
        if let TabKind::Shell(s) = &mut app.tabs[shell_tab].kind {
            s.run_when_ready = true;
        }
        run(&mut app, 5);
        let running = |app: &RongoApp| matches!(&app.tabs[shell_tab].kind, TabKind::Shell(s) if s.is_running());
        assert!(running(&app), "the script started");
        app.active_tab = 0;
        run(&mut app, 50);
        assert!(!running(&app), "it finished in the background");
    }

    /// A reply from a dialog that was cancelled while busy doesn't close or
    /// take over the dialog opened after it.
    #[test]
    fn late_replies_leave_the_next_dialog_alone() {
        let ctx = egui::Context::default();
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = RongoApp::new(&cc, rt, true);
        let cid = ConnectionProfile::mock().conn_id();
        let ns = Namespace::new("d", "c");
        // An export was started (request 900) and its dialog closed; now an
        // Update dialog is open and busy with request 901.
        let mut update = ToolDialog::bulk(cid, ns.clone(), None, false, Vec::new(), doc! {}, None);
        update.running = Some(901);
        app.tool = Some(update);
        app.pending.insert(900, Origin::Tool { view: None, tree: None });
        app.handle(Response { id: 900, conn: Some(cid), event: Event::Exported { count: 3, path: "/tmp/x.json".into() } });
        assert!(app.tool.as_ref().is_some_and(|t| t.running == Some(901)), "the Update dialog stayed open and busy");
        app.pending.insert(902, Origin::Tool { view: None, tree: None });
        app.handle(Response { id: 902, conn: Some(cid), event: Event::Failed(CoreError::Other("old".into())) });
        assert!(app.tool.as_ref().is_some_and(|t| t.error.is_none()), "an old failure isn't shown in it");

        // Same for the document editor.
        let mut d = DocDialog::edit(cid, ns.clone(), &doc! { "_id": 1 }, None);
        d.saving = Some(911);
        app.doc_dialog = Some(d);
        app.pending.insert(910, Origin::Save { view: None });
        app.handle(Response { id: 910, conn: Some(cid), event: Event::Written(Default::default()) });
        assert!(app.doc_dialog.is_some(), "another document's save didn't close this editor");
        app.pending.insert(911, Origin::Save { view: None });
        app.handle(Response { id: 911, conn: Some(cid), event: Event::Written(Default::default()) });
        assert!(app.doc_dialog.is_none(), "its own save did");
    }

    /// Testing a profile that is already open leaves its connection alone
    /// (the test used to replace it in the core, then disconnect it).
    #[test]
    fn testing_an_open_profile_keeps_its_connection() {
        let ctx = egui::Context::default();
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = RongoApp::new(&cc, rt, true);
        let mut frame = eframe::Frame::_new_kittest();
        let mut run = |app: &mut RongoApp, n: usize| {
            for _ in 0..n {
                let mut out = ctx.run_ui(frame_input(), |ui| eframe::App::ui(app, ui, &mut frame));
                out.textures_delta.clear();
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        run(&mut app, 40);
        let profile = ConnectionProfile::mock();
        let cid = profile.conn_id();
        assert!(matches!(app.conns[&cid].status, Status::Connected(_)));
        app.connect_action(ConnectAction::Test(Box::new(ConnectRequest { profile, password: None, ssh_secret: None })));
        run(&mut app, 30);
        app.conns.get_mut(&cid).unwrap().dbs = None;
        app.refresh_tree(cid);
        run(&mut app, 30);
        let c = &app.conns[&cid];
        assert!(c.dbs.as_ref().is_some_and(|d| !d.is_empty()), "the open connection still answers");
    }

    /// Editing or cloning a row from a projected result edits the whole
    /// document: saving the projected row would drop the other fields.
    #[test]
    fn edit_and_clone_use_the_whole_document() {
        let ctx = egui::Context::default();
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = RongoApp::new(&cc, rt, true);
        let mut frame = eframe::Frame::_new_kittest();
        let mut run = |app: &mut RongoApp, n: usize| {
            for _ in 0..n {
                let mut out = ctx.run_ui(frame_input(), |ui| eframe::App::ui(app, ui, &mut frame));
                out.textures_delta.clear();
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        run(&mut app, 40);
        let cid = ConnectionProfile::mock().conn_id();
        let ns = Namespace::new("demo-games", "edit_me");
        let full = doc! { "_id": "e1", "name": "shown", "secret": "not in the projection", "n": 5_i64 };
        app.net().send(Origin::None, Some(cid), Command::Insert { ns: ns.clone(), doc: full.clone() });
        run(&mut app, 10);

        let projected = doc! { "_id": "e1", "name": "shown" };
        app.view_action(1, ViewAction::Edit { conn: cid, ns: ns.clone(), doc: projected.clone() });
        run(&mut app, 20);
        let text = app.doc_dialog.as_ref().expect("editor opened").text().to_string();
        assert!(text.contains("not in the projection") && text.contains("NumberLong"), "{text}");

        app.doc_dialog = None;
        app.view_action(1, ViewAction::Clone { conn: cid, ns: ns.clone(), doc: projected });
        run(&mut app, 20);
        let d = app.doc_dialog.as_ref().expect("clone opened");
        assert!(matches!(d.mode, crate::dialogs::DocMode::Insert));
        assert!(d.text().contains("not in the projection") && !d.text().contains("\"e1\""), "{}", d.text());
    }

    /// ⌘F goes to the result view under the pointer (its find bar), and to
    /// the tree's search box when the pointer is over the sidebar.
    #[test]
    fn command_f_finds_where_the_pointer_is() {
        let ctx = egui::Context::default();
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = RongoApp::new(&cc, rt, true);
        let mut frame = eframe::Frame::_new_kittest();
        let mut run = |app: &mut RongoApp, events: Vec<egui::Event>| {
            let input = egui::RawInput { events, ..frame_input() };
            let mut out = ctx.run_ui(input, |ui| eframe::App::ui(app, ui, &mut frame));
            out.textures_delta.clear();
            std::thread::sleep(Duration::from_millis(10));
        };
        for _ in 0..40 {
            run(&mut app, vec![]);
        }
        let cid = ConnectionProfile::mock().conn_id();
        app.tree_action(TreeAction::OpenCollectionTab(cid, Namespace::new("demo-games", "providers")));
        for _ in 0..30 {
            run(&mut app, vec![]);
        }
        let find_open = |app: &RongoApp| match app.tabs.get(app.active_tab).map(|t| &t.kind) {
            Some(TabKind::Collection(c)) => c.view.find_open(),
            _ => panic!("collection tab not active"),
        };
        let cmd_f = egui::Event::Key { key: Key::F, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND };
        let tree_search_focused = |ctx: &egui::Context| ctx.memory(|m| m.has_focus(egui::Id::new("tree-search")));

        run(&mut app, vec![egui::Event::PointerMoved(egui::pos2(900.0, 500.0))]);
        run(&mut app, vec![cmd_f.clone()]);
        run(&mut app, vec![]);
        assert!(find_open(&app), "⌘F over the results opened their find bar");
        assert!(!tree_search_focused(&ctx));

        run(&mut app, vec![egui::Event::PointerMoved(egui::pos2(150.0, 400.0))]);
        run(&mut app, vec![]);
        run(&mut app, vec![cmd_f]);
        run(&mut app, vec![]);
        assert!(tree_search_focused(&ctx), "⌘F over the sidebar went to the tree search");
    }

    /// The index manager loads indexes with sizes, hiding one refreshes it,
    /// and the hover card gets collection statistics.
    #[test]
    fn index_manager_and_hover_stats_on_the_demo_server() {
        let ctx = egui::Context::default();
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = RongoApp::new(&cc, rt, true);
        let mut frame = eframe::Frame::_new_kittest();
        let mut run = |app: &mut RongoApp, n: usize| {
            for _ in 0..n {
                let mut out = ctx.run_ui(frame_input(), |ui| eframe::App::ui(app, ui, &mut frame));
                out.textures_delta.clear();
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        run(&mut app, 40);
        let cid = ConnectionProfile::mock().conn_id();
        let ns = Namespace::new("demo-games", "providers");
        let index = Command::CreateIndex { ns: ns.clone(), keys: doc! { "code": 1 }, options: Some(doc! { "name": "code_1", "unique": true }) };
        app.net().send(Origin::None, Some(cid), index);
        run(&mut app, 20);

        app.tree_action(TreeAction::OpenIndexes(cid, ns.clone()));
        run(&mut app, 30);
        let names = |app: &RongoApp| -> Vec<(String, bool)> {
            let Some(Tab { kind: TabKind::Indexes(it), .. }) = app.tabs.get(app.active_tab) else { panic!("index tab not active") };
            it.items_for_test().iter().map(|i| (i.spec.get_str("name").unwrap().to_string(), i.spec.get_bool("hidden").unwrap_or(false))).collect()
        };
        assert_eq!(names(&app), [("_id_".to_string(), false), ("code_1".to_string(), false)]);

        app.index_action(IndexAction::SetHidden(cid, ns.clone(), "code_1".into(), true));
        run(&mut app, 40);
        assert_eq!(names(&app)[1], ("code_1".to_string(), true), "the tab refreshed after hiding");

        // Hover statistics land on the collection's tree node.
        if let Some(c) = app.conns.get_mut(&cid) {
            if c.dbs.is_none() {
                c.set_databases(vec![rongo_core::DbInfo { name: ns.db.clone(), size_on_disk: 0 }]);
            }
            if let Some(db) = c.db_mut(&ns.db) {
                db.colls = Some(vec![CollNode::new(rongo_core::CollInfo { name: ns.coll.clone(), kind: rongo_core::CollKind::Collection })]);
                db.loading = true;
            }
        }
        app.net().send(Origin::HoverStats(cid, ns.clone()), Some(cid), Command::CollStats { ns: ns.clone() });
        run(&mut app, 30);
        let stats = app.coll_node(cid, &ns).and_then(|n| n.stats.clone()).expect("stats arrived").expect("stats ok");
        assert_eq!((stats.0.count, stats.0.indexes), (8, 2));
        assert!(stats.0.total_index_size > 0);
    }
}
