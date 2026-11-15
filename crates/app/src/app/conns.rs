//! Connections: connecting and disconnecting, the tree's reloads, the Connection Manager's actions, and saved profiles.

use rongo_core::profile::{ConnectionProfile, ProfileStore, SecretKind};
use rongo_core::{Command, ConnId, ConnectRequest, CoreError};

use crate::dialogs::{Confirm, ConnectAction, ConnectDialog, Pending, TestState};
use crate::net::Origin;
use crate::shortcut;
use crate::sidebar::{self, OpenConn, Status, TreeAction};
use crate::tabs::{Tab, TabKind};
use crate::vault_ui;

use super::RongoApp;
use super::history::DELETE_ITS_HISTORY;

impl RongoApp {
    pub(super) fn open_demo(&mut self) {
        let profile = ConnectionProfile::mock();
        let cid = profile.conn_id();
        if !self.conns.contains_key(&cid) {
            self.connect(profile);
        }
        self.open_shell(cid, Some("demo-games".into()), Some(demo_script()));
        if let Some(TabKind::Shell(s)) = self.tabs.last_mut().map(|t| &mut t.kind) {
            s.run_when_ready = true;
        }
    }

    pub(super) fn connect(&mut self, profile: ConnectionProfile) {
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
            // Changed in another window: asked to unlock again, then Connect again.
            let Some(vault) = self.fresh_vault() else { return };
            let pw = if profile.uses_password() { vault.get(profile.id, SecretKind::Password) } else { None };
            let ssh = if profile.needs_ssh_secret() { vault.get(profile.id, SecretKind::Ssh) } else { None };
            (pw, ssh)
        };
        self.conns.insert(cid, OpenConn::new(profile.clone()));
        self.sidebar.open = true;
        self.sidebar.selection = sidebar::Selection { conn: Some(cid), db: None, coll: None };
        let req = ConnectRequest { profile, password, ssh_secret };
        let id = self.net().send(Origin::Connect(cid), None, Command::Connect(Box::new(req)));
        self.connecting.insert(cid, id);
    }

    /// Closes these connections and their tabs. A script running in one of
    /// their shell tabs is stopped first, as its Stop button does (a dropped
    /// tab must not leave it running unseen), and one toast says how many.
    fn disconnect(&mut self, ids: &[ConnId]) {
        let stopped: usize = ids.iter().map(|&cid| self.drop_conn(cid)).sum();
        match stopped {
            0 => {}
            1 => self.toast("Stopped 1 running script", false),
            n => self.toast(format!("Stopped {n} running scripts"), false),
        }
    }

    /// Closes one connection and its tabs; the number of scripts stopped.
    /// A Connect still running for it is cancelled first, so the connection
    /// it would make is never registered (its answer is then stale).
    fn drop_conn(&mut self, cid: ConnId) -> usize {
        if let Some(id) = self.connecting.remove(&cid) {
            self.requests.cancel(id);
        }
        self.net().send(Origin::None, Some(cid), Command::Disconnect);
        self.conns.remove(&cid);
        let (gone, kept): (Vec<Tab>, Vec<Tab>) = std::mem::take(&mut self.tabs).into_iter().partition(|t| t.conn() == Some(cid));
        self.tabs = kept;
        let stopped = gone.into_iter().map(|t| self.tab_closed(t)).filter(|&s| s).count();
        if self.tabs.is_empty() {
            let id = self.next_tab();
            self.tabs.push(Tab { id, kind: TabKind::Welcome });
        }
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        if self.sidebar.selection.conn == Some(cid) {
            self.sidebar.selection = sidebar::Selection::default();
        }
        stopped
    }

    /// ⇧⌘R over the tree: reload what is under the selected item, as its
    /// Refresh Selected Item menu entry does.
    pub(super) fn refresh_selected(&mut self) {
        let crate::sidebar::Selection { conn: Some(cid), db, coll } = &self.sidebar.selection else { return };
        let (cid, db, coll) = (*cid, db.clone(), coll.clone());
        let Some(conn) = self.conns.get_mut(&cid) else { return };
        match (db, coll) {
            (Some(db), Some(coll)) => {
                if let Some(c) = conn.db_mut(&db).and_then(|d| d.colls.as_mut()).and_then(|cs| cs.iter_mut().find(|c| c.info.name == coll)) {
                    c.indexes = None;
                    c.loading = false;
                }
            }
            (Some(db), None) => {
                if let Some(d) = conn.db_mut(&db) {
                    d.colls = None;
                    d.loading = false;
                }
            }
            _ => self.refresh_tree(cid),
        }
    }

    pub(super) fn refresh_tree(&mut self, cid: ConnId) {
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

    /// Reload one database's collections in the tree after a paste; a
    /// database the tree doesn't know yet means reloading the list.
    pub(super) fn reload_database(&mut self, cid: ConnId, db: &str) {
        let Some(conn) = self.conns.get_mut(&cid) else { return };
        match conn.db_mut(db) {
            Some(node) => {
                node.colls = None;
                node.loading = false;
            }
            None => self.refresh_tree(cid),
        }
    }

    fn move_profile(&mut self, id: uuid::Uuid, before: Option<uuid::Uuid>) {
        if let Some(store) = &mut self.store
            && let Err(e) = store.move_before(id, before)
        {
            self.toast(e.to_string(), true);
        }
    }

    /// Export Connection… for a saved profile (or, failing that, an open
    /// connection's); the demo server has nothing to export.
    fn open_export(&mut self, id: uuid::Uuid) {
        let saved = self.store.as_ref().and_then(|s| s.get(id)).cloned();
        if let Some(p) = saved.or_else(|| self.conns.get(&ConnId(id)).map(|c| c.profile.clone()))
            && !p.is_mock()
            && let Some(vault) = self.vault.clone()
        {
            self.export_conn = Some(crate::export_conn::ExportConnDialog::new(p, vault));
        }
    }

    fn set_color(&mut self, id: uuid::Uuid, color: Option<String>) {
        if let Some(c) = self.conns.get_mut(&ConnId(id)) {
            c.profile.color = color.clone();
        }
        // The demo connection isn't saved; its colour lasts until it closes.
        self.update_profile(id, |p| p.color = color);
    }

    /// Changes a saved connection as it is on disk now (`ProfileStore::update`),
    /// so another window's edits to it stay. One that isn't saved here (the
    /// demo) is left alone; one deleted in another window since is not
    /// brought back, and a toast says so.
    fn update_profile(&mut self, id: uuid::Uuid, edit: impl FnOnce(&mut ConnectionProfile)) {
        let Some(store) = &mut self.store else { return };
        let Some(name) = store.get(id).map(|p| p.name.clone()) else { return };
        if let Err(e) = store.update(id, edit) {
            self.toast(format!("Couldn't change {name}: {e}"), true);
        }
    }

    /// The confirmation every way of lifting a lock goes through.
    fn ask_allow_writes(&mut self, cid: ConnId, name: &str) {
        let msg = format!("Allow writes to {name}? Its lock is saved and stays on until you allow writes.");
        self.confirm = Some(Confirm::new("Allow Writes", msg, "Allow Writes", Pending::AllowWrites { conn: cid }).danger());
    }

    /// Lock or unlock a connection: the saved profile, the open connection
    /// and the core all follow.
    pub(super) fn set_read_only(&mut self, cid: ConnId, on: bool) {
        self.apply_read_only(cid, on);
        // The demo connection isn't saved; its lock lasts until it closes.
        self.update_profile(cid.0, |p| p.read_only = on);
    }

    /// Make an open connection (and the core's side of it) match `on`.
    fn apply_read_only(&mut self, cid: ConnId, on: bool) {
        let Some(c) = self.conns.get_mut(&cid) else { return };
        // Nothing changed: send nothing (the core runs requests concurrently,
        // so a redundant SetReadOnly could land after a later one).
        if c.profile.read_only == on {
            return;
        }
        c.profile.read_only = on;
        // Still connecting: the core doesn't know it yet. Connecting
        // sends the flag then.
        if matches!(c.status, Status::Connected(_)) {
            self.net().send(Origin::None, Some(cid), Command::SetReadOnly(on));
        }
    }

    /// Deletes a saved connection, and with `history` its Collection
    /// History (off the UI thread, once nothing of it is being recorded);
    /// unticked, that stays, listed in Settings under "deleted profile".
    pub(super) fn delete_profile(&mut self, id: uuid::Uuid, history: bool) {
        let (Some(store), Some(vault)) = (&mut self.store, &self.vault) else { return self.toast("Saved connections are unavailable", true) };
        let name = store.get(id).map(|p| p.name.clone()).unwrap_or_default();
        if let Err(e) = store.remove(id, vault) {
            // Still listed: nothing was removed.
            if store.get(id).is_some() {
                if !self.vault_write_failed(&e, "delete the connection again") {
                    self.toast(e.to_string(), true);
                }
                return;
            }
            // Gone from the file, but the vault refused its secrets afterwards.
            self.toast(format!("The connection {name} was deleted, but its saved passwords stay in the vault: {e}"), true);
            if e == CoreError::VaultChanged {
                self.lock_again();
            }
        }
        // Starts once nothing of that profile is being recorded (`poll_history_files`).
        if history {
            self.history_files.queue("Deleting a deleted connection's Collection History…", Some(id), None, move |s| s.delete_profile(id));
        }
    }

    /// Opens the Connection Manager (`new`: on a New Connection form) with
    /// the saved connections read again: another rongo window may have saved
    /// some since. A file that can't be read is said; the list stays. One
    /// that couldn't be read at launch is tried again (fixed meanwhile).
    pub(super) fn open_connection_manager(&mut self, new: bool) {
        match &mut self.store {
            Some(store) => {
                if let Err(e) = store.reload() {
                    self.toast(format!("Could not read the saved connections again: {e}"), true);
                }
            }
            None => match self.vault_path.as_ref().map(|v| ProfileStore::load(v.with_file_name("profiles.json"))) {
                Some(Ok(store)) => self.store = Some(store),
                Some(Err(e)) => self.toast(format!("Could not load saved connections: {e}"), true),
                None => {}
            },
        }
        self.connect_dialog = Some(if new { ConnectDialog::new_connection() } else { ConnectDialog::new() });
    }

    /// What the tree asks of a connection (`tree_action` hands these over):
    /// reload, disconnect or reconnect it, move, colour or export its saved
    /// profile, lock or unlock it.
    pub(super) fn connection_action(&mut self, a: TreeAction) {
        match a {
            TreeAction::Refresh(cid) => self.refresh_tree(cid),
            TreeAction::Disconnect(cid) => self.disconnect(&[cid]),
            // `connect` replaces the failed connection, or leaves it in the
            // tree when it can't start (the master password asked first).
            TreeAction::Reconnect(cid) => {
                if let Some(p) = self.conns.get(&cid).map(|c| c.profile.clone()) {
                    self.connect(p);
                }
            }
            // Connections still connecting, or failed, have nothing to list yet.
            TreeAction::RefreshAll => {
                let ids: Vec<ConnId> = self.conns.iter().filter(|(_, c)| matches!(c.status, Status::Connected(_))).map(|(id, _)| *id).collect();
                for cid in ids {
                    self.refresh_tree(cid);
                }
            }
            TreeAction::DisconnectOthers(keep) => {
                let ids: Vec<ConnId> = self.conns.keys().copied().filter(|c| *c != keep).collect();
                self.disconnect(&ids);
            }
            TreeAction::DisconnectAll => {
                let ids: Vec<ConnId> = self.conns.keys().copied().collect();
                self.disconnect(&ids);
            }
            TreeAction::MoveConnection { id, before } => self.move_profile(id.0, before.map(|b| b.0)),
            TreeAction::SetColor(cid, color) => self.set_color(cid.0, color),
            TreeAction::ExportConnection(cid) => self.open_export(cid.0),
            TreeAction::SetReadOnly(cid, true) => self.set_read_only(cid, true),
            TreeAction::SetReadOnly(cid, false) => {
                let Some(name) = self.conns.get(&cid).filter(|c| c.profile.read_only).map(|c| c.profile.name.clone()) else { return };
                self.ask_allow_writes(cid, &name);
            }
            // Not a connection's: `tree_action` keeps those. Listed, not `_`,
            // so a new action must be placed in both matches.
            TreeAction::OpenCollection(..)
            | TreeAction::OpenCollectionShell(..)
            | TreeAction::OpenCollectionTab(..)
            | TreeAction::OpenShell(..)
            | TreeAction::CreateCollection(..)
            | TreeAction::DropCollection(..)
            | TreeAction::DropDatabase(..)
            | TreeAction::Export(..)
            | TreeAction::Import(..)
            | TreeAction::Rename(..)
            | TreeAction::Duplicate(..)
            | TreeAction::Clear(..)
            | TreeAction::AddIndex(..)
            | TreeAction::OpenIndexes(..)
            | TreeAction::OpenHistory(..)
            | TreeAction::DropIndex(..)
            | TreeAction::Validator(..)
            | TreeAction::CollStats(..)
            | TreeAction::DbStats(..)
            | TreeAction::ServerInfo(..)
            | TreeAction::CopyCollections { .. }
            | TreeAction::PasteCollections { .. }
            | TreeAction::PasteAsDatabase(_) => {}
        }
    }

    pub(super) fn connect_action(&mut self, a: ConnectAction) {
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
            ConnectAction::Export(id) => self.open_export(id),
            ConnectAction::VaultChanged => self.lock_again(),
            ConnectAction::Notice(text) => self.toast(text, true),
            // Asks first, offering to delete the profile's history too
            // (ticked: it holds copies of that server's data); the offer
            // says how much once that is measured, off the UI thread.
            ConnectAction::Delete(id) => {
                let name = self.store.as_ref().and_then(|s| s.get(id)).map(|p| p.name.clone()).unwrap_or_default();
                let msg = format!("Delete the connection {name}? Its saved passwords are deleted too.");
                self.confirm = Some(Confirm::new("Delete Connection", msg, "Delete", Pending::DeleteProfile { id }).danger().checkbox(DELETE_ITS_HISTORY, true));
                self.history_files.rescan(&self.ctx);
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
                let id = profile.id;
                let Some(result) = self.save_profile(profile, password, ssh_secret) else { return };
                match result {
                    // Not saved: the form comes back with what was typed and why.
                    Err(e) => {
                        // Changed elsewhere: locked again (nothing was written under the old key).
                        let why = if self.vault_write_failed(&e, "Save") { vault_ui::changed_message("Save") } else { format!("Couldn't save: {e}") };
                        if let Some(d) = &mut self.connect_dialog {
                            d.restore_form(why);
                        }
                    }
                    Ok(()) if then_connect => {
                        if let Some(p) = self.store.as_ref().and_then(|s| s.get(id)).cloned() {
                            if self.conns.contains_key(&p.conn_id()) {
                                self.disconnect(&[p.conn_id()]);
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

    /// Saves the connection form's profile and secrets: what was saved, or
    /// None when there is nowhere to save it (said in a toast, or on the
    /// form). An open connection on it follows at once; clearing a lock
    /// saved on disk asks Allow Writes first.
    fn save_profile(&mut self, profile: ConnectionProfile, password: Option<String>, ssh_secret: Option<String>) -> Option<rongo_core::CoreResult<()>> {
        let Some(vault) = self.vault.clone() else {
            self.toast("Saved connections are unavailable", true);
            return None;
        };
        // profiles.json couldn't be read: the form stays, with why (the Connection Manager reads it again when opened).
        let Some(store) = &mut self.store else {
            let path = self.vault_path.as_ref().map(|v| v.with_file_name("profiles.json").display().to_string()).unwrap_or_else(|| "profiles.json".into());
            if let Some(d) = &mut self.connect_dialog {
                d.restore_form(format!("Couldn't save: the saved connections ({path}) couldn't be read. Fix or move that file, then open the Connection Manager again."));
            }
            return None;
        };
        let id = profile.id;
        let name = profile.name.clone();
        // Both secrets in one vault write: all or nothing. Clearing the
        // lock of a profile locked on disk (read there, under the lock)
        // saves the rest of the form but keeps the lock until "Allow
        // Writes" is confirmed: `unlocking`.
        let (result, unlocking) = match store.upsert(profile, password, ssh_secret, &vault) {
            Ok(kept_lock) => (Ok(()), kept_lock),
            Err(e) => (Err(e), false),
        };
        // An open connection shows the new name and colour, and keeps
        // history as now chosen, straight away; the rest applies when
        // it next connects.
        if result.is_ok()
            && let (Some(p), Some(c)) = (self.store.as_ref().and_then(|s| s.get(id)), self.conns.get_mut(&ConnId(id)))
        {
            c.profile.name = p.name.clone();
            c.profile.color = p.color.clone();
            c.profile.history = p.history;
            let on = p.read_only;
            self.apply_read_only(ConnId(id), on);
        }
        if unlocking {
            self.ask_allow_writes(ConnId(id), &name);
        }
        Some(result)
    }
}

/// The demo shell's script, its shortcuts written for this OS.
fn demo_script() -> String {
    shortcut::label(r#"// IntelliShell runs mongosh-style JavaScript. ⌘↩ runs everything (or the
// selection), ⇧⌘↩ runs the statement under the cursor. A find() you don't
// read opens as a result tab you can page through.
db.getCollection("advertisers").find({})

db.bet_logs.find({ status: "pending", amount: { $gt: 1000 } }).sort({ amount: -1 })

db.bet_logs.aggregate([
    { $group: { _id: "$provider", bets: { $sum: 1 }, total: { $sum: "$amount" } } },
    { $sort: { total: -1 } }
])
"#).into_owned()
}
