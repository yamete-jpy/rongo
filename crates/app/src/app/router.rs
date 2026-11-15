//! Replies from the core, routed by the origin of each request.

use rongo_core::bson::{Bson, Document};
use rongo_core::{Command, ConnId, CoreError, Event, Namespace, RequestId, Response, WriteOutcome};

use crate::dialogs::{DocDialog, DocMode, TestState};
use crate::net::{Origin, ViewReq};
use crate::sidebar::{CollNode, Status};
use crate::settings::mb;
use crate::tools::ToolKind;
use crate::tabs::TabKind;

use super::RongoApp;
use super::outcome::{copy_summary, copy_toast, copy_troubled, move_failed, outcome_text, troubled};

impl RongoApp {
    pub(super) fn handle(&mut self, r: Response) {
        if let Event::Progress { done, total, note } = r.event {
            // Not an answer: the request stays pending for its terminal event.
            if let Some(s) = self.requests.pending.get_mut(&r.id) {
                s.progress = Some((done, total, note));
            }
            return;
        }
        let origin = self.requests.pending.remove(&r.id).map(|s| s.origin).unwrap_or(Origin::None);
        // A recorded write refused, before writing anything, because its
        // history would not fit: offer to send it without history.
        if let Some(u) = self.requests.unrecorded.remove(&r.id)
            && let Event::Failed(CoreError::HistoryTooLarge { needed, limit, free }) = &r.event
            && let Some(conn) = r.conn
        {
            let why = format!("Collection History would need about {} for this operation; the size limit is {} and {} is free on the disk.", mb(*needed), mb(*limit), mb(*free));
            // A dialog waiting for this answer stays open, ready again.
            if let Some((waiting, _)) = self.waiting_dialog(&origin)
                && *waiting == Some(r.id)
            {
                *waiting = None;
            }
            self.ask_without_history(origin, conn, u.cmd, u.name, why);
            return;
        }
        // A write-concern error: the primary applied the write, but it was
        // not confirmed as asked. A write without history (a recorded one
        // answers this itself) answers as unconfirmed, never as a refusal
        // that invites a retry; so a drop has dropped.
        let r = match (&origin, r.event) {
            (Origin::Save { .. } | Origin::Notify { .. } | Origin::UpdateOne { .. } | Origin::Tool { .. } | Origin::Dropped { .. }, Event::Failed(e @ CoreError::WriteConcern { .. })) => {
                Response { event: Event::Written(WriteOutcome { unconfirmed: true, warning: Some(e.to_string()), ..Default::default() }), ..r }
            }
            (_, event) => Response { event, ..r },
        };
        match origin {
            Origin::None => {}
            o @ (Origin::Connect(_) | Origin::Test(_)) => self.on_connect(o, r),
            o @ (Origin::Databases(_) | Origin::Collections(..) | Origin::Indexes(..) | Origin::Fields(..) | Origin::HoverStats(..)) => self.on_tree(o, r),
            o @ (Origin::OpenDoc { .. } | Origin::View(..)) => self.on_view(o, r),
            o @ Origin::Save { .. } => self.on_save(o, r),
            o @ Origin::Copy { .. } => self.on_copy(o, r),
            o @ (Origin::Rename { .. } | Origin::Dropped { .. } | Origin::TreeWrite { .. } | Origin::Notify { .. } | Origin::UpdateOne { .. }) => self.on_write(o, r),
            o @ (Origin::Tool { .. } | Origin::Validator) => self.on_tool(o, r),
            o @ (Origin::Info { .. } | Origin::IndexTab(_)) => self.on_info(o, r),
            Origin::History { tab, ns, req } => self.history_answer(tab, ns, req, r.id, r.conn, r.event),
        }
    }

    /// A Connect's answer, or a connection test's.
    fn on_connect(&mut self, origin: Origin, r: Response) {
        match (origin, r.event) {
            // Only the latest Connect speaks for its connection. An earlier
            // one was given up on by Disconnect, which cancelled it, but it
            // may have connected just before: unless the connection is open
            // again (its newer Connect, sent after the cancel, replaces this
            // one in the core), the core has just registered it (and maybe
            // an SSH tunnel), so close it. Its failure says nothing.
            (Origin::Connect(cid), Event::Connected(_)) if self.connecting.get(&cid) != Some(&r.id) && !self.conns.contains_key(&cid) => {
                self.requests.core.send(Some(cid), Command::Disconnect);
            }
            (Origin::Connect(cid), _) if self.connecting.get(&cid) != Some(&r.id) => {}
            (Origin::Connect(cid), Event::Connected(info)) => {
                self.connecting.remove(&cid);
                if let Some(c) = self.conns.get_mut(&cid) {
                    c.status = Status::Connected(info);
                    c.dbs = None;
                    c.loading_dbs = false;
                    // The lock may have been switched while this connected.
                    let on = c.profile.read_only;
                    self.net().send(Origin::None, Some(cid), Command::SetReadOnly(on));
                }
            }
            (Origin::Connect(cid), Event::Failed(e)) => {
                self.connecting.remove(&cid);
                // The tree shows it Failed: the core must not keep it (or a tunnel) either.
                self.requests.core.send(Some(cid), Command::Disconnect);
                if let Some(c) = self.conns.get_mut(&cid) {
                    c.status = Status::Failed(e.to_string());
                    let name = c.profile.name.clone();
                    self.toast(format!("{name}: {e}"), true);
                }
            }
            // Connect answers Connected or Failed.
            (Origin::Test(cid), Event::Connected(info)) => {
                // Close the test connection even if the dialog has gone.
                self.requests.core.send(Some(cid), Command::Disconnect);
                if let Some(test) = self.test_waiting(r.id) {
                    let rs = info.replica_set.map(|s| format!(", replica set {s}")).unwrap_or_default();
                    *test = TestState::Ok(format!("Connected: MongoDB {} on {} ({}{rs})", info.version, info.host, info.role));
                }
            }
            (Origin::Test(_), Event::Failed(e)) => {
                if let Some(test) = self.test_waiting(r.id) {
                    *test = TestState::Failed(e.to_string());
                }
            }
            _ => {}
        }
    }

    /// What fills the tree: databases, collections and indexes, and the
    /// field samples and statistics kept on its nodes.
    fn on_tree(&mut self, origin: Origin, r: Response) {
        match (origin, r.event) {
            (Origin::Databases(cid), Event::Databases(dbs)) => {
                if let Some(c) = self.conns.get_mut(&cid) {
                    c.set_databases(dbs);
                }
            }
            (Origin::Collections(cid, db), Event::Collections { items, .. }) => {
                if let Some(d) = &mut self.paste_dialog
                    && d.to() == cid
                    && d.to_db() == db
                {
                    d.set_existing(items.iter().map(|c| c.name.clone()).collect());
                }
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
            // Named, as several connections may be listing at once; one
            // closed meanwhile has nothing to say.
            (Origin::Databases(cid), Event::Failed(e)) => {
                if let Some(c) = self.conns.get_mut(&cid) {
                    c.loading_dbs = false;
                    if c.dbs.is_none() {
                        c.dbs = Some(Vec::new());
                    }
                    let name = c.profile.name.clone();
                    self.toast(format!("{name}: {e}"), true);
                }
            }
            (Origin::Collections(cid, db), Event::Failed(e)) => {
                // The Paste dialog waiting on this listing stays off and says why.
                if let Some(d) = &mut self.paste_dialog
                    && d.to() == cid
                    && d.to_db() == db
                {
                    d.set_listing_failed(e.to_string());
                }
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
            (Origin::Fields(cid, ns), Event::Page { docs, .. }) => {
                let docs: Vec<rongo_core::bson::Document> = docs.into_iter().filter_map(|d| d.try_into().ok()).collect();
                let fields = rongo_core::format::field_paths(&docs, 3);
                if let Some(t) = &mut self.tool {
                    t.set_known_fields(&ns, &fields);
                }
                if let Some(c) = self.conns.get_mut(&cid) {
                    c.fields.insert(ns, fields);
                }
            }
            // A failed sample isn't kept: the next shell or Add Index asks again.
            (Origin::Fields(..), _) => {}
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
            _ => {}
        }
    }

    /// A result view's page, count or re-read document, and the document
    /// the editor opens with.
    fn on_view(&mut self, origin: Origin, r: Response) {
        match (origin, r.event) {
            // Not over a dialog already open: an editor with edits typed in
            // it (a second double-click's reply), or a question.
            (Origin::OpenDoc { .. }, Event::Page { .. }) if self.modal_open() => {}
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
            (Origin::View(vid, ViewReq::RefreshDoc(id)), Event::Page { docs, .. }) => {
                if let Some(v) = self.view_mut(vid) {
                    v.on_doc(r.id, id, docs);
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
                if let Some(v) = self.view_mut(vid) {
                    if e == CoreError::Cancelled { v.on_cancelled(r.id) } else { v.on_error(r.id, e.to_string()) }
                }
            }
            _ => {}
        }
    }

    /// The document editor's save.
    fn on_save(&mut self, origin: Origin, r: Response) {
        match (&origin, r.event) {
            // A recorded save stopped (⊗) before it wrote anything.
            (Origin::Save { .. }, Event::Written(w)) if w.cancelled && w.matched == 0 && w.inserted == 0 => {
                let why = "Cancelled: the document was not saved";
                match self.waiting_dialog(&origin) {
                    Some((waiting, error)) if *waiting == Some(r.id) => {
                        *waiting = None;
                        *error = Some(why.into());
                    }
                    _ => self.toast(why, true),
                }
            }
            // A save the server did not confirm may or may not have been
            // applied: the editor stays open with the edits and why (a toast
            // if it was closed since), and the view re-reads the document so
            // what the server holds now can be checked.
            (&Origin::Save { view }, Event::Written(w)) if w.unconfirmed => {
                let why = outcome_text(&Event::Written(w));
                let edited = match &self.doc_dialog {
                    Some(d) if d.saving == Some(r.id) => match &d.mode {
                        DocMode::Edit { id } => Some(id.clone()),
                        DocMode::Insert => None,
                    },
                    _ => None,
                };
                match self.waiting_dialog(&origin) {
                    Some((waiting, error)) if *waiting == Some(r.id) => {
                        *waiting = None;
                        *error = Some(why);
                    }
                    _ => self.toast(why, true),
                }
                match edited {
                    Some(id) => self.after_write_to(view, id),
                    None => self.after_write(view, None),
                }
            }
            // Replies go to the dialog that sent them; a dialog cancelled
            // while saving may have been replaced by another one since.
            (&Origin::Save { view }, Event::Written(w)) => {
                let mine = self.doc_dialog.as_ref().filter(|d| d.saving == Some(r.id));
                let inserted = mine.is_some_and(|d| matches!(d.mode, DocMode::Insert));
                // An edit replaced one document: the view re-reads just it.
                // An insert (or a dialog gone since) fetches the page.
                let edited = mine.and_then(|d| match &d.mode {
                    DocMode::Edit { id } => Some(id.clone()),
                    DocMode::Insert => None,
                });
                if mine.is_some() {
                    self.doc_dialog = None;
                }
                match edited {
                    Some(id) => self.after_write_to(view, id),
                    None => self.after_write(view, None),
                }
                let done = if inserted { "Document added" } else { "Document saved" };
                match &w.warning {
                    Some(why) => self.toast(format!("{done}; {why}"), true),
                    None => self.toast(done, false),
                }
            }
            (Origin::Save { .. }, Event::Failed(e)) => match self.waiting_dialog(&origin) {
                Some((waiting, error)) if *waiting == Some(r.id) => {
                    *waiting = None;
                    *error = Some(e.to_string());
                }
                _ => self.toast(format!("Saving the document failed: {e}"), true),
            },
            _ => {}
        }
    }

    /// A paste's outcome.
    fn on_copy(&mut self, origin: Origin, r: Response) {
        match (origin, r.event) {
            (Origin::Copy { to, db }, Event::Copied(report)) => {
                let name = self.conns.get(&to).map(|c| c.profile.name.clone());
                // A target closed meanwhile has no name to give.
                let text = match name {
                    Some(n) => copy_toast(&report, Some(&n)),
                    None => copy_summary(&report),
                };
                self.toast(text, copy_troubled(&report));
                self.reload_database(to, &db);
            }
            (Origin::Copy { to, db }, Event::Failed(e)) => {
                self.toast(format!("Paste failed: {e}"), true);
                // Part of it may have been written.
                self.reload_database(to, &db);
            }
            _ => {}
        }
    }

    /// A write's outcome: a rename, a drop, a change to the tree, or a
    /// write a view or the tree shows.
    fn on_write(&mut self, origin: Origin, r: Response) {
        match (origin, r.event) {
            // A refused rename or drop moves and closes nothing.
            (Origin::Rename { .. }, Event::Failed(e)) => self.toast(e.to_string(), true),
            (Origin::Rename { conn, from, to }, _) => self.renamed(conn, from, to),
            (Origin::Dropped { conn, ns }, ev) if !matches!(ev, Event::Failed(_)) => {
                self.close_tabs_for(conn, &ns);
                self.toast(outcome_text(&ev), troubled(&ev));
                self.after_write(None, Some(conn));
            }
            // A failed write that changes the tree may have changed it anyway
            // (a Duplicate whose partial copy remains): its database's
            // collections list again.
            (Origin::TreeWrite { conn, db }, Event::Failed(e)) => {
                self.toast(e.to_string(), true);
                self.reload_database(conn, &db);
            }
            (Origin::TreeWrite { conn, .. }, ev) => {
                self.toast(outcome_text(&ev), troubled(&ev));
                self.after_write(None, Some(conn));
            }
            (Origin::Notify { .. } | Origin::UpdateOne { .. } | Origin::Dropped { .. }, Event::Failed(e)) => self.toast(e.to_string(), true),
            (Origin::Notify { view, tree }, ev) => {
                self.toast(outcome_text(&ev), troubled(&ev));
                self.after_write(view, tree);
            }
            (Origin::UpdateOne { view, id }, ev) => {
                self.toast(outcome_text(&ev), troubled(&ev));
                self.after_write_to(Some(view), id);
            }
            _ => {}
        }
    }

    /// A tool dialog's answer: its write, or the validator it shows.
    fn on_tool(&mut self, origin: Origin, r: Response) {
        match (&origin, r.event) {
            // Only the validator dialog that asked, while it still waits.
            (Origin::Validator, Event::Document(d)) => {
                if let Some(t) = &mut self.tool {
                    t.set_validator(r.id, &d);
                }
            }
            (Origin::Validator, Event::Failed(e)) => {
                if let Some(t) = &mut self.tool {
                    t.validator_failed(r.id, &e.to_string());
                }
            }
            // A failure may come after part of it was written (an import
            // inserts as it reads): the view and the tree show what is there.
            (&Origin::Tool { view, tree }, Event::Failed(e)) => {
                match self.waiting_dialog(&origin) {
                    Some((waiting, error)) if *waiting == Some(r.id) => {
                        *waiting = None;
                        *error = Some(e.to_string());
                    }
                    _ => self.toast(e.to_string(), true),
                }
                self.after_write(view, tree);
            }
            (&Origin::Tool { view, tree }, ev) => {
                let mine = self.tool.as_ref().is_some_and(|t| t.running == Some(r.id));
                // Edit Value wrote to one document: the view re-reads just it.
                let one = self.tool.as_ref().filter(|_| mine).and_then(|t| match &t.kind {
                    ToolKind::Value { id, .. } => Some(id.clone()),
                    _ => None,
                });
                let what = if mine { self.tool.take().map_or("Done", |t| t.success_message()) } else { "Done" };
                let detail = outcome_text(&ev);
                // A write that stopped (⊗) didn't finish, and one the server
                // did not confirm may not have: no "… finished" / "… saved".
                let unfinished = matches!(&ev, Event::Written(w) if w.cancelled || w.unconfirmed);
                let text = if unfinished {
                    detail
                } else if detail == "Done" {
                    what.to_string()
                } else {
                    format!("{what}: {detail}")
                };
                self.toast(text, troubled(&ev));
                match one {
                    Some(id) => {
                        self.after_write_to(view, id);
                        self.after_write(None, tree);
                    }
                    None => self.after_write(view, tree),
                }
            }
            _ => {}
        }
    }

    /// Statistics or server information (opened as a tab), and an Index
    /// Manager's details.
    fn on_info(&mut self, origin: Origin, r: Response) {
        match (origin, r.event) {
            (Origin::Info { .. }, Event::Failed(e)) => self.toast(e.to_string(), true),
            (Origin::Info { conn, title }, Event::Document(d)) => self.open_info(conn, title, d),
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
            _ => {}
        }
    }

    /// The dialog a Tool or Save request's answer goes to, if it is open:
    /// the request it waits for (None when it isn't busy) and its error line.
    /// An answer is its own only when the requests match: a dialog cancelled
    /// while busy may have been replaced since.
    pub(super) fn waiting_dialog(&mut self, origin: &Origin) -> Option<(&mut Option<RequestId>, &mut Option<String>)> {
        match origin {
            Origin::Tool { .. } => self.tool.as_mut().map(|t| (&mut t.running, &mut t.error)),
            Origin::Save { .. } => self.doc_dialog.as_mut().map(|d| (&mut d.saving, &mut d.error)),
            _ => None,
        }
    }

    /// The Connection Manager's test state, while it waits for request `id`.
    fn test_waiting(&mut self, id: RequestId) -> Option<&mut TestState> {
        self.connect_dialog.as_mut().map(|d| &mut d.test).filter(|t| matches!(t, TestState::Running(req) if *req == id))
    }

    /// After a write known to touch only the document with this `_id`: the
    /// view re-reads just that document, keeping the rest of its page, the
    /// selection and the scroll position (it fetched the whole page before).
    fn after_write_to(&mut self, view: Option<u64>, id: Bson) {
        if let Some(v) = view.and_then(|v| self.view_mut(v)) {
            v.refresh_id(id);
        }
    }

    /// After a write that can touch many documents, an insert or a delete:
    /// the view fetches its page again. `tree`: that connection's tree (and
    /// its Index Managers) reload.
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

    /// The server renamed `from` to `to`: only now does its Collection
    /// History move along, a History tab open on it follows, and its other
    /// tabs close. The move runs off the UI thread (no key needed: a rename
    /// of its folder, or entry by entry when `to` has a history already),
    /// once nothing is being written to `to`; the History tabs list again
    /// when it is done (`poll_history_files`), and a failure is said then.
    fn renamed(&mut self, conn: ConnId, from: Namespace, to: Namespace) {
        self.close_tabs_for(conn, &from);
        let (profile, f, t) = (conn.0, from.clone(), to.clone());
        self.history_files.queue("Moving a renamed collection's history…", Some(profile), Some(to.clone()), move |s| {
            s.move_collection(profile, &f, &t).map_err(|e| CoreError::History(move_failed(&f, &t, e)))
        });
        for t in &mut self.tabs {
            if let TabKind::History(h) = &mut t.kind
                && h.conn == conn
            {
                if h.ns == from {
                    h.renamed(to.clone());
                } else if h.ns == to {
                    h.reload();
                }
            }
        }
        self.toast(format!("Renamed {from} to {}", to.coll), false);
        self.after_write(None, Some(conn));
    }

    pub(super) fn coll_node(&mut self, cid: ConnId, ns: &Namespace) -> Option<&mut CollNode> {
        self.conns.get_mut(&cid)?.db_mut(&ns.db)?.colls.as_mut()?.iter_mut().find(|c| c.info.name == ns.coll)
    }
}
