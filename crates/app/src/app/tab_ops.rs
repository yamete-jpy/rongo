//! Tabs: opening, finding and closing them, and the views they hold.

use rongo_core::bson::Document;
use rongo_core::{Command, ConnId, FindSpec, Namespace};

use crate::dialogs::{Confirm, Pending};
use crate::indexes::IndexTab;
use crate::net::{HistoryReq, Origin};
use crate::result_view::{Mode, ResultView, Source};
use crate::tabs::{CollectionTab, ShellTab, Tab, TabKind};

use super::RongoApp;

/// What a tab's right-click menu closes, from the tab right-clicked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CloseWhich {
    This,
    Others,
    ToTheRight,
    /// Every tab of its connection.
    OfConnection,
    All,
}

impl RongoApp {
    /// The tab menu's closes, from tab `i`. Close is the tab's ✕. The
    /// others close several: a tab running a script stays (said once), and
    /// when shell tabs holding a script would close, it asks first, since
    /// a script goes with its tab.
    pub(super) fn close_from_menu(&mut self, i: usize, which: CloseWhich) {
        let Some(tab) = self.tabs.get(i) else { return };
        if which == CloseWhich::This {
            return self.close_tab(i);
        }
        let (id, conn) = (tab.id, tab.conn());
        let ids: Vec<u64> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(k, t)| match which {
                CloseWhich::This => *k == i,
                CloseWhich::Others => *k != i,
                CloseWhich::ToTheRight => *k > i,
                CloseWhich::OfConnection => conn.is_some() && t.conn() == conn,
                CloseWhich::All => true,
            })
            .map(|(_, t)| t.id)
            .collect();
        // The tab right-clicked is shown if the active one closes.
        let focus = Some(id);
        let scripts = self.tabs.iter().filter(|t| ids.contains(&t.id) && matches!(&t.kind, TabKind::Shell(s) if s.has_script() && !s.is_running())).count();
        if ids.len() > 1 && scripts > 0 {
            let lost = if scripts == 1 { "The script in 1 IntelliShell tab closes with it.".to_string() } else { format!("The scripts in {scripts} IntelliShell tabs close with them.") };
            let msg = format!("Close {} tabs? {lost}", ids.len());
            self.confirm = Some(Confirm::new("Close Tabs", msg, "Close", Pending::CloseTabs { ids, focus }));
            return;
        }
        self.close_tab_ids(&ids, focus);
    }

    /// Closes the tabs with these ids that are still open, except one
    /// running a script: those stay, said once. The active tab stays active
    /// if it is left; if not, `focus` is shown when it is left.
    pub(super) fn close_tab_ids(&mut self, ids: &[u64], focus: Option<u64>) {
        let active = self.tabs.get(self.active_tab).map(|t| t.id);
        let mut running = 0;
        for id in ids {
            let Some(i) = self.tabs.iter().position(|t| t.id == *id) else { continue };
            if matches!(&self.tabs[i].kind, TabKind::Shell(s) if s.is_running()) {
                running += 1;
                continue;
            }
            self.close_tab(i);
        }
        let position = |id: Option<u64>| id.and_then(|id| self.tabs.iter().position(|t| t.id == id));
        if let Some(i) = position(active).or_else(|| position(focus)) {
            self.active_tab = i;
        }
        match running {
            0 => {}
            1 => self.toast("1 tab stays open: its script is running", true),
            n => self.toast(format!("{n} tabs stay open: their scripts are running"), true),
        }
    }

    /// The collection tab's view for `ns`, if one is open.
    pub(super) fn view_for(&self, conn: ConnId, ns: &Namespace) -> Option<u64> {
        self.tabs.iter().find_map(|t| match &t.kind {
            TabKind::Collection(c) if c.conn == conn && c.ns == *ns => Some(c.view.id),
            _ => None,
        })
    }

    pub(super) fn open_info(&mut self, conn: ConnId, title: String, doc: Document) {
        let id = self.net().view_id();
        let mut view = ResultView::new(id, title, Source::Static { docs: vec![doc], truncated: false });
        view.mode = Mode::Json;
        self.push_tab(|_| TabKind::Info { conn, view: Box::new(view) });
    }

    /// Collection the toolbar's Export/Import act on: the active collection
    /// tab, else the tree selection.
    pub(super) fn target_collection(&self) -> Option<(ConnId, String, Option<String>, Option<FindSpec>)> {
        if let Some(Tab { kind: TabKind::Collection(c), .. }) = self.tabs.get(self.active_tab) {
            let spec = match &c.view.source {
                Source::Find { spec, .. } => Some(spec.clone()),
                Source::Static { .. } | Source::Raw { .. } => None,
            };
            return Some((c.conn, c.ns.db.clone(), Some(c.ns.coll.clone()), spec));
        }
        let sel = &self.sidebar.selection;
        Some((sel.conn?, sel.db.clone()?, sel.coll.clone(), None))
    }

    /// The connection of `target_collection`, read each frame (Import's
    /// lock) without copying the tab's query.
    pub(super) fn target_conn(&self) -> Option<ConnId> {
        if let Some(Tab { kind: TabKind::Collection(c), .. }) = self.tabs.get(self.active_tab) {
            return Some(c.conn);
        }
        let sel = &self.sidebar.selection;
        sel.db.as_ref().and(sel.conn)
    }

    pub(super) fn open_collection(&mut self, cid: ConnId, ns: Namespace) {
        if self.focus_tab(|k| matches!(k, TabKind::Collection(c) if c.conn == cid && c.ns == ns)) {
            return;
        }
        let view_id = self.net().view_id();
        self.push_tab(|_| TabKind::Collection(Box::new(CollectionTab::new(cid, ns, view_id))));
    }

    /// IntelliShell on the collection's database with a find() already run.
    pub(super) fn open_collection_shell(&mut self, cid: ConnId, ns: Namespace) {
        let query = format!("db.getCollection({}).find({{}})\n", serde_json::to_string(&ns.coll).unwrap_or_default());
        // Sample the fields now so suggestions are ready when typing starts.
        self.sample_fields(cid, &ns);
        self.open_shell(cid, Some(ns.db), Some(query));
        if let Some(TabKind::Shell(s)) = self.tabs.last_mut().map(|t| &mut t.kind) {
            s.run_when_ready = true;
        }
    }

    /// Samples a collection's field names (autocomplete, the Add Index
    /// picker) unless they are known or a sample is already on its way.
    pub(super) fn sample_fields(&mut self, cid: ConnId, ns: &Namespace) {
        let known = self.conns.get(&cid).is_none_or(|c| c.fields.contains_key(ns));
        let asked = self.requests.pending.values().any(|s| matches!(&s.origin, Origin::Fields(c, n) if *c == cid && n == ns));
        if !known && !asked {
            self.net().send(Origin::Fields(cid, ns.clone()), Some(cid), Command::Find(FindSpec::all(ns.clone(), 100)));
        }
    }

    pub(super) fn open_shell(&mut self, cid: ConnId, db: Option<String>, text: Option<String>) {
        let db = db
            .or_else(|| self.conns.get(&cid).and_then(|c| c.profile.default_db.clone()))
            .unwrap_or_else(|| "test".into());
        let text = text.unwrap_or_default();
        self.push_tab(|tab_id| TabKind::Shell(Box::new(ShellTab::new(cid, db, tab_id, text))));
    }

    pub(super) fn open_shell_for_selection(&mut self) {
        let sel = self.sidebar.selection.clone();
        let cid = sel.conn.or_else(|| self.conns.iter().find(|(_, c)| c.info().is_some()).map(|(id, _)| *id));
        let Some(cid) = cid else {
            self.toast("Connect to a server first", true);
            return;
        };
        let text = sel.coll.as_ref().map(|c| format!("db.getCollection({}).find({{}})\n", serde_json::to_string(c).unwrap_or_default()));
        self.open_shell(cid, sel.db.clone(), text);
    }

    pub(super) fn close_tab(&mut self, i: usize) {
        if let TabKind::Shell(s) = &self.tabs[i].kind
            && s.is_running()
        {
            self.toast("Stop the running script before closing its tab", true);
            return;
        }
        let tab = self.tabs.remove(i);
        self.tab_closed(tab);
        if self.tabs.is_empty() {
            let id = self.next_tab();
            self.tabs.push(Tab { id, kind: TabKind::Welcome });
        }
        if self.active_tab >= self.tabs.len() || self.active_tab > i {
            self.active_tab = self.active_tab.saturating_sub(1);
        }
    }

    /// A tab was taken out of the tab bar. The requests it waits for are
    /// cancelled and forgotten, since nothing else shows or stops them: its
    /// views' finds and counts, an Index Manager's details, a History tab's
    /// conflict check or comparison. A restore goes on (it writes, and its
    /// toast names the collection). A script running in it is stopped, as
    /// its Stop button does (true when one was).
    pub(super) fn tab_closed(&mut self, mut tab: Tab) -> bool {
        let views: Vec<u64> = tab.views_mut().iter().map(|v| v.id).collect();
        self.requests.cancel_for(|o| match o {
            Origin::View(v, _) => views.contains(v),
            Origin::IndexTab(t) => *t == tab.id,
            Origin::History { tab: t, req: HistoryReq::Check | HistoryReq::Compare, .. } => *t == tab.id,
            _ => false,
        });
        match &tab.kind {
            TabKind::Shell(s) if s.is_running() => {
                s.stop();
                true
            }
            _ => false,
        }
    }

    /// The result view with this id, in whichever tab holds it (on every
    /// view reply: no list of views is built per tab).
    pub(super) fn view_mut(&mut self, id: u64) -> Option<&mut ResultView> {
        self.tabs.iter_mut().find_map(|t| match &mut t.kind {
            TabKind::Collection(c) => Some(&mut c.view).filter(|v| v.id == id),
            TabKind::Shell(s) => s.results.iter_mut().map(|r| &mut r.view).find(|v| v.id == id),
            TabKind::Info { view, .. } => Some(view.as_mut()).filter(|v| v.id == id),
            TabKind::History(h) => h.views_mut().into_iter().find(|v| v.id == id),
            TabKind::Welcome | TabKind::Indexes(_) => None,
        })
    }

    pub(super) fn open_indexes(&mut self, cid: ConnId, ns: Namespace) {
        if self.focus_tab(|k| matches!(k, TabKind::Indexes(it) if it.conn == cid && it.ns == ns)) {
            return;
        }
        self.push_tab(|tab_id| TabKind::Indexes(Box::new(IndexTab::new(tab_id, cid, ns))));
    }

    /// Shows the first tab `is_it` takes, if there is one (true).
    pub(super) fn focus_tab(&mut self, is_it: impl Fn(&TabKind) -> bool) -> bool {
        let Some(i) = self.tabs.iter().position(|t| is_it(&t.kind)) else { return false };
        self.active_tab = i;
        true
    }

    /// Adds a tab, made from its new id, and shows it.
    pub(super) fn push_tab(&mut self, make: impl FnOnce(u64) -> TabKind) {
        let id = self.next_tab();
        self.tabs.push(Tab { id, kind: make(id) });
        self.active_tab = self.tabs.len() - 1;
    }

    /// Closes the collection tabs and Index Manager tabs of `ns` (renamed or
    /// dropped). Its History tab stays: the history outlives a drop, and
    /// follows a rename.
    pub(super) fn close_tabs_for(&mut self, conn: ConnId, ns: &Namespace) {
        self.close_tabs_where(|k| match k {
            TabKind::Collection(c) => c.conn == conn && c.ns == *ns,
            TabKind::Indexes(t) => t.conn == conn && t.ns == *ns,
            _ => false,
        });
    }

    /// Closes every tab `hit` takes, each tried once, so it always ends: a
    /// shell tab running a script stays (`close_tab` refuses it), and so
    /// does the Welcome tab that closing the last tab opens.
    pub(super) fn close_tabs_where(&mut self, hit: impl Fn(&TabKind) -> bool) {
        let ids: Vec<u64> = self.tabs.iter().filter(|t| hit(&t.kind)).map(|t| t.id).collect();
        for id in ids {
            if let Some(i) = self.tabs.iter().position(|t| t.id == id) {
                self.close_tab(i);
            }
        }
    }
}
