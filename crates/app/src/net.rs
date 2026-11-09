//! Sending requests to the core and remembering who asked, so responses can be
//! routed back to the right tab, tree node or dialog.

use std::collections::HashMap;

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

pub struct Net<'a> {
    pub core: &'a CoreHandle,
    pub pending: &'a mut HashMap<RequestId, Origin>,
    pub next_view_id: &'a mut u64,
    /// False while a dialog is open, so view shortcuts stay quiet.
    pub keys_enabled: bool,
}

impl Net<'_> {
    pub fn send(&mut self, origin: Origin, conn: Option<ConnId>, cmd: Command) -> RequestId {
        let id = self.core.send(conn, cmd);
        if !matches!(origin, Origin::None) {
            self.pending.insert(id, origin);
        }
        id
    }

    pub fn view_id(&mut self) -> u64 {
        *self.next_view_id += 1;
        *self.next_view_id
    }
}
