//! Sending requests to the core and remembering who asked, so responses can be
//! routed back to the right tab, tree node or dialog.

use std::collections::HashMap;
use std::time::Instant;

use rongo_core::{Command, ConnId, CoreHandle, Namespace, RequestId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewReq {
    Page,
    Count,
    /// Re-read the document at this row index (Refresh Document).
    RefreshDoc(usize),
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
    /// Requests from the export/import/index/validator/bulk dialogs; the reply
    /// goes back to the dialog, which then closes.
    Tool { view: Option<u64>, tree: Option<ConnId> },
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
}

#[cfg(test)]
impl Sent {
    pub fn new(origin: Origin) -> Self {
        Self { origin, conn: None, what: None, started: Instant::now() }
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
        Command::Disconnect | Command::Cancel(_) => return None,
        Command::ListDatabases => "Loading databases".into(),
        Command::ListCollections { db } => format!("Loading collections of {db}"),
        Command::ListIndexes { ns } | Command::IndexDetails { ns } => format!("Loading indexes of {ns}"),
        Command::Find(spec) if matches!(origin, Origin::OpenDoc { .. }) => format!("Reading a document from {}", spec.ns),
        Command::Find(spec) => format!("Running find on {}", spec.ns),
        Command::Count { ns, .. } => format!("Counting documents in {ns}"),
        Command::Insert { ns, .. } => format!("Inserting a document into {ns}"),
        Command::Replace { ns, .. } => format!("Saving a document in {ns}"),
        Command::Delete { ns, ids } => format!("Deleting {} document{} from {ns}", ids.len(), plural(ids.len())),
        Command::InsertMany { ns, docs } => format!("Inserting {} document{} into {ns}", docs.len(), plural(docs.len())),
        Command::UpdateMany { ns, .. } | Command::Update { ns, .. } => format!("Updating documents in {ns}"),
        Command::DeleteMany { ns, .. } | Command::DeleteWhere { ns, .. } => format!("Deleting documents from {ns}"),
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
        let id = self.core.send(conn, cmd);
        if !matches!(origin, Origin::None) {
            // A connect goes out before the connection exists; name it by its id.
            let conn = conn.or(match origin {
                Origin::Connect(c) => Some(c),
                _ => None,
            });
            self.pending.insert(id, Sent { origin, conn, what, started: Instant::now() });
        }
        id
    }

    pub fn view_id(&mut self) -> u64 {
        *self.next_view_id += 1;
        *self.next_view_id
    }
}
