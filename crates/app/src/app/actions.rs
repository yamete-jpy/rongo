//! What the tree, the views, the Index Manager and the dialogs ask for, and the questions they confirm.

use rongo_core::history::Kind;
use rongo_core::bson::{Document, doc};
use rongo_core::{Command, ConnId, FindSpec, Namespace};

use crate::indexes::IndexAction;
use crate::dialogs::{Confirm, DocDialog, DocMode, Pending, ViewDocDialog};
use crate::net::Origin;
use crate::result_view::ViewAction;
use crate::sidebar::TreeAction;
use crate::settings::OpenIn;
use crate::tools::ToolDialog;

use super::RongoApp;
use super::paste::database_name_error;

impl RongoApp {
    /// Saves the document editor's document: an edit replaces it by `_id`
    /// (recorded), a new one is inserted. The editor waits for the answer.
    pub(super) fn save_doc(&mut self, doc: Document) {
        let Some(d) = &mut self.doc_dialog else { return };
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
        if let Some(req) = self.send_write(Origin::Save { view }, conn, cmd, true)
            && let Some(d) = &mut self.doc_dialog
        {
            d.saving = Some(req);
        }
    }

    /// Sends what a tool dialog built; the dialog then waits for the answer.
    /// An import refreshes the open view of the collection it goes into.
    pub(super) fn tool_send(&mut self, cmd: Command) {
        let Some(t) = &self.tool else { return };
        let (conn, view, tree, keep) = (t.conn, t.view, t.refresh_tree.then_some(t.conn), t.keep_history());
        let view = match &cmd {
            Command::Import { ns, .. } => view.or_else(|| self.view_for(conn, ns)),
            _ => view,
        };
        if let Some(id) = self.send_write(Origin::Tool { view, tree }, conn, cmd, keep)
            && let Some(t) = &mut self.tool
        {
            t.running = Some(id);
        }
    }

    /// Open Add Index with the collection's field names for the picker,
    /// sampling them first if they are not known yet.
    pub(super) fn add_index_dialog(&mut self, conn: ConnId, ns: Namespace) {
        let known = self.conns.get(&conn).and_then(|c| c.fields.get(&ns).cloned());
        self.sample_fields(conn, &ns);
        self.tool = Some(ToolDialog::index(conn, ns, known.unwrap_or_default()));
    }

    pub(super) fn index_action(&mut self, a: IndexAction) {
        match a {
            IndexAction::Add(conn, ns) => self.add_index_dialog(conn, ns),
            IndexAction::Drop(conn, ns, name) => {
                let msg = format!("Drop the index {name} on {ns}?");
                self.confirm = Some(Confirm::new("Drop Index", msg, "Drop", Pending::DropIndex { conn, ns, name }).danger());
            }
            IndexAction::Edit(conn, ns, spec) => self.tool = Some(ToolDialog::edit_index(conn, ns, &spec)),
            IndexAction::View(doc) => self.view_doc = Some(ViewDocDialog::new(&doc)),
            IndexAction::SetHidden(conn, ns, name, hidden) => {
                let origin = Origin::TreeWrite { conn, db: ns.db.clone() };
                let cmd = Command::ModifyIndex { ns, name, hidden: Some(hidden), expire_after_seconds: None };
                self.net().send(origin, Some(conn), cmd);
            }
        }
    }

    pub(super) fn tree_action(&mut self, a: TreeAction) {
        match a {
            TreeAction::OpenCollection(cid, ns) => match self.settings.open_collection_in {
                OpenIn::IntelliShell => self.open_collection_shell(cid, ns),
                OpenIn::CollectionTab => self.open_collection(cid, ns),
            },
            TreeAction::OpenCollectionShell(cid, ns) => self.open_collection_shell(cid, ns),
            TreeAction::OpenCollectionTab(cid, ns) => self.open_collection(cid, ns),
            TreeAction::OpenShell(cid, db) => self.open_shell(cid, db, None),
            a @ (TreeAction::Refresh(_)
            | TreeAction::Disconnect(_)
            | TreeAction::Reconnect(_)
            | TreeAction::RefreshAll
            | TreeAction::DisconnectOthers(_)
            | TreeAction::DisconnectAll
            | TreeAction::MoveConnection { .. }
            | TreeAction::SetColor(..)
            | TreeAction::ExportConnection(_)
            | TreeAction::SetReadOnly(..)) => self.connection_action(a),
            TreeAction::CreateCollection(conn, db) => {
                let msg = format!("Name of the new collection in {db}:");
                self.confirm = Some(Confirm::new("Add Collection", msg, "Create", Pending::CreateCollection { conn, db }).input(String::new()));
            }
            TreeAction::DropCollection(conn, ns) => {
                let msg = format!("Drop {ns} and all of its documents and indexes? This cannot be undone.");
                self.confirm = Some(Confirm::new("Drop Collection", msg, "Drop", Pending::DropCollection { conn, ns }).danger());
            }
            TreeAction::DropDatabase(conn, db) => {
                let msg = format!("Type {db} to drop it and everything in it. This cannot be undone.");
                self.confirm = Some(Confirm::new("Drop Database", msg, "Drop", Pending::DropDatabase { conn, db: db.clone() }).danger().input(String::new()).must_type(db));
            }
            TreeAction::Export(conn, ns) => self.tool = Some(ToolDialog::export(conn, FindSpec::all(ns, 50), false)),
            TreeAction::Import(conn, db, coll) => self.tool = Some(ToolDialog::import(conn, db, coll.unwrap_or_default())),
            // Its history moves with it, so not while some is being written.
            TreeAction::Rename(conn, ns) if let Some(why) = self.rename_blocked(conn, &ns) => self.toast(why, true),
            TreeAction::Rename(conn, ns) => {
                let msg = format!("New name for {ns}:");
                let current = ns.coll.clone();
                self.confirm = Some(Confirm::new("Rename Collection", msg, "Rename", Pending::RenameCollection { conn, ns }).input(current));
            }
            TreeAction::Duplicate(conn, ns) => {
                let msg = format!("Copy {ns} (documents and indexes) into a new collection named:");
                let suggested = format!("{}_copy", ns.coll);
                self.confirm = Some(Confirm::new("Duplicate Collection", msg, "Duplicate", Pending::DuplicateCollection { conn, ns }).input(suggested));
            }
            TreeAction::Clear(conn, ns) => {
                let msg = format!("Delete every document in {ns}? The collection and its indexes are kept. This cannot be undone.");
                self.confirm = Some(Confirm::new("Clear Collection", msg, "Clear", Pending::ClearCollection { conn, ns }).danger());
            }
            TreeAction::AddIndex(conn, ns) => self.add_index_dialog(conn, ns),
            TreeAction::OpenIndexes(conn, ns) => self.open_indexes(conn, ns),
            TreeAction::OpenHistory(conn, ns) => self.open_history(conn, ns),
            TreeAction::DropIndex(conn, ns, name) => {
                let msg = format!("Drop the index {name} on {ns}?");
                self.confirm = Some(Confirm::new("Drop Index", msg, "Drop", Pending::DropIndex { conn, ns, name }).danger());
            }
            TreeAction::Validator(conn, ns) => {
                let req = self.net().send(Origin::Validator, Some(conn), Command::GetValidator { ns: ns.clone() });
                self.tool = Some(ToolDialog::validator(conn, ns, req));
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
            TreeAction::CopyCollections { conn, db, colls } => self.copy_collections(conn, db, colls),
            TreeAction::PasteCollections { conn, db } => self.open_paste(conn, db),
            TreeAction::PasteAsDatabase(conn) => {
                if self.paste_blocked(conn) {
                    return;
                }
                let msg = "Name of the new database:".to_string();
                self.confirm = Some(Confirm::new("Paste as New Database", msg, "Continue", Pending::PasteAsDatabase { conn }).input(String::new()));
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

    pub(super) fn view_action(&mut self, view: u64, a: ViewAction) {
        match a {
            ViewAction::Edit { conn, ns, doc } => self.open_doc(conn, ns, &doc, view, false),
            ViewAction::Clone { conn, ns, doc } => self.open_doc(conn, ns, &doc, view, true),
            ViewAction::Insert { conn, ns, template } => {
                self.doc_dialog = Some(DocDialog::insert(conn, ns, template.as_ref(), Some(view)));
            }
            ViewAction::Delete { conn, ns, ids } => {
                let n = ids.len();
                let msg = format!("Delete {n} document{} from {ns}?", if n == 1 { "" } else { "s" });
                self.confirm = Some(Confirm::new("Delete Documents", msg, "Delete", Pending::Delete { conn, ns, ids, view: Some(view) }).danger());
            }
            ViewAction::View { doc } => self.view_doc = Some(ViewDocDialog::new(&doc)),
            ViewAction::EditValue { conn, ns, id, path, value } => {
                self.tool = Some(ToolDialog::value(conn, Some(view), ns, id, path, &value));
            }
            ViewAction::UpdateOne { conn, ns, id, update } => {
                let cmd = Command::UpdateMany { ns, filter: doc! { "_id": id.clone() }, update };
                self.send_write(Origin::UpdateOne { view, id }, conn, cmd, true);
            }
            ViewAction::Paste { conn, ns, text } => match rongo_shell::parse_documents_text(&text) {
                Ok(docs) => {
                    let n = docs.len();
                    let has_ids = docs.iter().any(|d| d.contains_key("_id"));
                    let msg = format!("Import {n} document{} from the clipboard into {ns}?", if n == 1 { "" } else { "s" });
                    let checkbox = has_ids.then(|| ("Remove _id fields (insert as new documents)".to_string(), false));
                    self.confirm = Some(Confirm { checkbox, ..Confirm::new("Paste Documents", msg, "OK", Pending::Paste { conn, ns, docs, view: Some(view) }) });
                }
                Err(e) => self.toast(format!("Nothing to paste: {}", e.message), true),
            },
            ViewAction::Bulk { conn, ns, delete, selected, query, field } => {
                let offer = self.history_offer(conn, &ns, if delete { Kind::Delete } else { Kind::Update });
                let mut dialog = ToolDialog::bulk(conn, ns, Some(view), delete, selected, query, field);
                dialog.offer_history(offer);
                self.tool = Some(dialog);
            }
            ViewAction::Export { conn, spec } => self.tool = Some(ToolDialog::export(conn, spec, true)),
            ViewAction::SortChanged(_) => {}
        }
    }

    pub(super) fn confirmed(&mut self, c: Confirm) {
        // A recorded delete whose history key can't be checked yet keeps its question.
        if let Pending::Delete { conn, ns, ids, .. } = &c.pending
            && !self.history_ready(*conn, &Command::Delete { ns: ns.clone(), ids: ids.clone() })
        {
            self.confirm = Some(c);
            return;
        }
        let input = c.input.unwrap_or_default().trim().to_string();
        let ticked = c.checkbox.is_some_and(|(_, t)| t);
        let tree = |conn, db: &str| Origin::TreeWrite { conn, db: db.to_string() };
        match c.pending {
            Pending::Delete { conn, ns, ids, view } => {
                self.send_write(Origin::Notify { view, tree: None }, conn, Command::Delete { ns, ids }, true);
            }
            // Its tabs close once the server has dropped it (a refusal keeps them).
            Pending::DropCollection { conn, ns } => {
                self.net().send(Origin::Dropped { conn, ns: ns.clone() }, Some(conn), Command::DropCollection { ns });
            }
            Pending::DropDatabase { conn, db } => {
                if input != db {
                    self.toast("The name did not match; nothing was dropped", true);
                    return;
                }
                self.net().send(tree(conn, &db), Some(conn), Command::DropDatabase { db });
            }
            Pending::CreateCollection { conn, db } => {
                let ns = Namespace::new(db, input);
                self.net().send(tree(conn, &ns.db), Some(conn), Command::CreateCollection { ns });
            }
            Pending::RenameCollection { conn, ns } => {
                if input == ns.coll {
                    return;
                }
                // A write may have begun since the question.
                if let Some(why) = self.rename_blocked(conn, &ns) {
                    return self.toast(why, true);
                }
                // Its tabs close once the server has renamed it (`renamed`).
                let origin = Origin::Rename { conn, from: ns.clone(), to: Namespace::new(ns.db.clone(), input.clone()) };
                self.net().send(origin, Some(conn), Command::RenameCollection { ns, to: input });
            }
            Pending::DuplicateCollection { conn, ns } => {
                self.net().send(tree(conn, &ns.db), Some(conn), Command::DuplicateCollection { ns, to: input });
            }
            Pending::ClearCollection { conn, ns } => {
                let view = self.view_for(conn, &ns);
                self.net().send(Origin::Notify { view, tree: None }, Some(conn), Command::ClearCollection { ns });
            }
            Pending::DropIndex { conn, ns, name } => {
                self.net().send(tree(conn, &ns.db), Some(conn), Command::DropIndex { ns, name });
            }
            Pending::AllowWrites { conn } => self.set_read_only(conn, false),
            Pending::PasteAsDatabase { conn } => match database_name_error(&input) {
                Some(why) => self.toast(format!("{input:?} is not a database name: {why}"), true),
                None => self.open_paste(conn, input),
            },
            Pending::Paste { conn, ns, mut docs, view } => {
                if ticked {
                    for d in &mut docs {
                        d.remove("_id");
                    }
                }
                self.net().send(Origin::Notify { view, tree: None }, Some(conn), Command::InsertMany { ns, docs });
            }
            Pending::RemoveHistory { tab, entry } => self.remove_history_entry(tab, entry),
            Pending::EmptyHistory { profile, ns, before } => self.empty_history(profile, ns, before),
            Pending::DeleteHistory { profile, ns, before } => self.delete_history(profile, ns, before),
            Pending::ClearHistory => self.clear_history(),
            Pending::SaveSettings(s) => self.save_settings(*s),
            Pending::DeleteProfile { id } => self.delete_profile(id, ticked),
            Pending::ResetVault => self.reset_vault(ticked),
            Pending::WithoutHistory { origin, conn, cmd, name } => self.send_without_history(origin, conn, *cmd, &name, &input),
        }
    }
}
