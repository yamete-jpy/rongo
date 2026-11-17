//! Sending requests to the core and remembering who asked, so responses can be
//! routed back to the right tab, tree node or dialog.

use std::collections::HashMap;
use std::time::Instant;

use rongo_core::bson::Bson;
use rongo_core::{Command, ConnId, CoreHandle, Namespace, RequestId};

#[derive(Clone, Debug, PartialEq)]
pub enum ViewReq {
    Page,
    Count,
    /// Re-read the document with this `_id` (Refresh Document). The reply
    /// finds its row by the `_id`: a header sort may have moved it.
    RefreshDoc(Bson),
}

#[derive(Clone, Debug)]
pub enum Origin {
    /// Nobody waits for the answer (e.g. cancellations).
    None,
    Connect(ConnId),
    /// A connection test, under its own throwaway id.
    Test(ConnId),
    Databases(ConnId),
    Collections(ConnId, String),
    Indexes(ConnId, Namespace),
    View(u64, ViewReq),
    /// The document dialog's save; errors go back into the dialog.
    Save { view: Option<u64> },
    /// A write or admin command: toast the outcome, then refresh the view
    /// and/or the connection's tree.
    Notify { view: Option<u64>, tree: Option<ConnId> },
    /// A write that changes database `db`'s part of the tree (add, drop or
    /// duplicate a collection, drop an index or the database, hide an
    /// index): toast the outcome, then refresh the connection's tree; after
    /// a failure, which may have changed it anyway (a Duplicate's partial
    /// copy), list that database's collections again.
    TreeWrite { conn: ConnId, db: String },
    /// Set to null or Remove Field on the document with this `_id`: toast
    /// the outcome, then the view re-reads just that document.
    UpdateOne { view: u64, id: Bson },
    /// A command whose reply opens in an info tab.
    Info { conn: ConnId, title: String },
    /// The current validator, for the validator dialog.
    Validator,
    /// Sample documents whose field names feed autocomplete.
    Fields(ConnId, Namespace),
    /// Statistics for a collection's hover card.
    HoverStats(ConnId, Namespace),
    /// Index details for the index manager tab with this tab id.
    IndexTab(u64),
    /// A whole document fetched for the editor (`clone`: as a new document).
    OpenDoc { conn: ConnId, ns: Namespace, view: u64, clone: bool },
    /// A paste: the copy's outcome is toasted and the target database
    /// (`db` on `to`) reloaded in the tree.
    Copy { to: ConnId, db: String },
    /// Requests from the export/import/index/validator/bulk dialogs; the reply
    /// goes back to the dialog, which then closes.
    Tool { view: Option<u64>, tree: Option<ConnId> },
    /// A request of the History tab with this tab id, on this collection
    /// (named in the restore's toast even if the tab has closed).
    History { tab: u64, ns: Namespace, req: HistoryReq },
    /// Rename Collection: once the server has renamed `from` to `to`, its
    /// Collection History moves along, its tabs close and the tree reloads.
    Rename { conn: ConnId, from: Namespace, to: Namespace },
    /// Drop Collection: once the server has dropped `ns`, its tabs close
    /// and the tree reloads.
    Dropped { conn: ConnId, ns: Namespace },
}

/// What a History tab asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryReq {
    /// The conflict check that comes before a restore.
    Check,
    Restore,
    /// The current documents, for "Show changes".
    Compare,
}

/// A request waiting for its reply.
#[derive(Clone, Debug)]
pub struct Sent {
    pub origin: Origin,
    pub conn: Option<ConnId>,
    /// What the status bar says while it runs; `None` for background work
    /// (autocomplete samples, hover statistics) and once cancelled.
    pub what: Option<String>,
    pub started: Instant,
    /// The latest `Event::Progress`: done, total (if known) and a note.
    pub progress: Option<(u64, Option<u64>, String)>,
    /// Whether a cancel can stop it (`Command::cancellable`): a write the
    /// server finishes once begun can't be, and shows no ⊗.
    pub cancellable: bool,
    /// A write or a data move the Operations panel lists, and what the panel
    /// needs of its command; None for reads and work that runs unseen.
    pub log: Option<LogSeed>,
    /// The user asked it to stop (a `Command::Cancel` was sent for it): a
    /// stop it answers is theirs, not a problem's.
    pub stop_asked: bool,
}

/// What the Operations panel keeps of a command it lists: a copy's target
/// connection and its items (source and target), one child each.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LogSeed {
    pub to: Option<ConnId>,
    pub items: Vec<(Namespace, Namespace)>,
}

impl LogSeed {
    /// For `cmd`, when the panel lists it: it has words for the status bar
    /// (`what`) and is not a read. Reads (find, count, the tree's listings,
    /// statistics, a restore's conflict check), connecting and testing never
    /// are; neither is what runs unseen (no `what`).
    pub fn of(what: Option<&str>, cmd: &Command) -> Option<LogSeed> {
        if what.is_none() || cmd.only_reads() {
            return None;
        }
        Some(match cmd {
            Command::CopyCollections { to, items, .. } => LogSeed { to: Some(*to), items: items.iter().map(|i| (i.src.clone(), i.dst.clone())).collect() },
            _ => LogSeed::default(),
        })
    }
}

#[cfg(test)]
impl Sent {
    pub fn new(origin: Origin) -> Self {
        Self { origin, conn: None, what: None, started: Instant::now(), progress: None, cancellable: true, log: None, stop_asked: false }
    }
}

/// The status bar's words for a request, or `None` when it runs unseen.
pub fn describe(origin: &Origin, cmd: &Command) -> Option<String> {
    if matches!(origin, Origin::None | Origin::Fields(..) | Origin::HoverStats(..)) {
        return None;
    }
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    Some(match cmd {
        Command::Connect(_) if matches!(origin, Origin::Test(_)) => "Testing the connection".into(),
        Command::Connect(_) => "Connecting".into(),
        Command::Disconnect | Command::Cancel(_) | Command::SetReadOnly(_) => return None,
        Command::ListDatabases => "Loading databases".into(),
        Command::ListCollections { db } => format!("Loading collections of {db}"),
        Command::ListIndexes { ns } | Command::IndexDetails { ns } => format!("Loading indexes of {ns}"),
        Command::Find(spec) if matches!(origin, Origin::OpenDoc { .. }) => format!("Reading a document from {}", spec.ns),
        Command::Find(spec) if matches!(origin, Origin::History { .. }) => format!("Reading the current documents of {}", spec.ns),
        Command::Find(spec) => format!("Running find on {}", spec.ns),
        Command::Count { ns, .. } => format!("Counting documents in {ns}"),
        Command::Insert { ns, .. } => format!("Inserting a document into {ns}"),
        Command::Replace { ns, .. } => format!("Saving a document in {ns}"),
        Command::Delete { ns, ids } => format!("Deleting {} document{} from {ns}", ids.len(), plural(ids.len())),
        Command::InsertMany { ns, docs } => format!("Inserting {} document{} into {ns}", docs.len(), plural(docs.len())),
        Command::UpdateMany { ns, .. } | Command::Update { ns, .. } => format!("Updating documents in {ns}"),
        Command::DeleteWhere { ns, .. } => format!("Deleting documents from {ns}"),
        Command::CreateCollection { ns } => format!("Creating {ns}"),
        Command::DropCollection { ns } => format!("Dropping {ns}"),
        Command::ClearCollection { ns } => format!("Clearing {ns}"),
        Command::RenameCollection { ns, .. } => format!("Renaming {ns}"),
        Command::DuplicateCollection { ns, .. } => format!("Duplicating {ns}"),
        Command::DropDatabase { db } => format!("Dropping database {db}"),
        Command::CreateIndex { ns, .. } => format!("Creating an index on {ns}"),
        Command::DropIndex { ns, name } => format!("Dropping index {name} on {ns}"),
        Command::ModifyIndex { ns, name, .. } => format!("Changing index {name} on {ns}"),
        Command::GetValidator { ns } => format!("Reading the validator of {ns}"),
        Command::SetValidator { ns, .. } => format!("Saving the validator of {ns}"),
        Command::CollStats { ns } => format!("Reading statistics of {ns}"),
        Command::RunCommand { db, cmd } => match cmd.keys().next() {
            Some(name) => format!("Running {name} on {db}"),
            None => format!("Running a command on {db}"),
        },
        Command::Export(spec) => format!("Exporting {}", spec.ns),
        Command::Import { ns, .. } => format!("Importing into {ns}"),
        Command::CopyCollections { items, .. } => format!("Copying {} collection{}", items.len(), plural(items.len())),
        Command::Recorded { cmd, .. } => return describe(origin, cmd),
        Command::HistoryConflicts { entry, .. } => format!("Checking {} for conflicts", entry.ns),
        Command::HistoryRestore { entry, .. } => format!("Restoring documents into {}", entry.ns),
    })
}

pub struct Net<'a> {
    pub core: &'a CoreHandle,
    pub pending: &'a mut HashMap<RequestId, Sent>,
    pub next_view_id: &'a mut u64,
    /// False while a dialog is open, so view shortcuts stay quiet.
    pub keys_enabled: bool,
}

impl Net<'_> {
    pub fn send(&mut self, origin: Origin, conn: Option<ConnId>, cmd: Command) -> RequestId {
        let what = describe(&origin, &cmd);
        let cancellable = cmd.cancellable();
        let log = LogSeed::of(what.as_deref(), &cmd);
        if let Command::Cancel(target) = &cmd
            && let Some(s) = self.pending.get_mut(target)
        {
            s.stop_asked = true;
        }
        let id = self.core.send(conn, cmd);
        if !matches!(origin, Origin::None) {
            // A connect goes out before the connection exists; name it by its id.
            let conn = conn.or(match origin {
                Origin::Connect(c) => Some(c),
                _ => None,
            });
            self.pending.insert(id, Sent { origin, conn, what, started: Instant::now(), progress: None, cancellable, log, stop_asked: false });
        }
        id
    }

    pub fn view_id(&mut self) -> u64 {
        *self.next_view_id += 1;
        *self.next_view_id
    }
}
