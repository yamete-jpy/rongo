//! The requests sent to the core, and those still waiting for their answers.

use std::collections::HashMap;

use rongo_core::{Command, CoreHandle, Namespace, RequestId};

use crate::net::{HistoryReq, Net, Origin, Sent};

use super::history::Unrecorded;

/// The core and the requests waiting for its answers, in one field of the
/// app: a `Net` borrows it while the tabs, the tree and the dialogs are
/// borrowed too.
pub(super) struct Requests {
    pub(super) core: CoreHandle,
    /// Every request sent and not answered yet, and who wants its answer.
    pub(super) pending: HashMap<RequestId, Sent>,
    pub(super) next_view_id: u64,
    /// Recorded writes waiting for their answer, as they would be sent
    /// without history: a `HistoryTooLarge` refusal offers to send that.
    pub(super) unrecorded: HashMap<RequestId, Unrecorded>,
}

impl Requests {
    pub(super) fn new(core: CoreHandle) -> Self {
        Requests { core, pending: HashMap::new(), next_view_id: 0, unrecorded: HashMap::new() }
    }

    /// What views and the tree send requests with; `keys_enabled` while no
    /// dialog is open.
    pub(super) fn net(&mut self, keys_enabled: bool) -> Net<'_> {
        Net { core: &self.core, pending: &mut self.pending, next_view_id: &mut self.next_view_id, keys_enabled }
    }

    /// Cancels request `id`, and the corner no longer shows it; its reply
    /// (cancelled, or what it did before it stopped) still comes back. Its
    /// origin, while it waits.
    pub(super) fn cancel(&mut self, id: RequestId) -> Option<&Origin> {
        self.core.send(None, Command::Cancel(id));
        let s = self.pending.get_mut(&id)?;
        s.what = None;
        s.stop_asked = true;
        Some(&s.origin)
    }

    /// Cancels and forgets every request whose origin `waited` takes: what
    /// a closed tab waited for, which nothing else shows or stops.
    pub(super) fn cancel_for(&mut self, waited: impl Fn(&Origin) -> bool) {
        let ids: Vec<RequestId> = self.pending.iter().filter(|(_, s)| waited(&s.origin)).map(|(id, _)| *id).collect();
        for id in ids {
            self.core.send(None, Command::Cancel(id));
            self.pending.remove(&id);
        }
    }

    /// Every collection whose Collection History is being written now, by
    /// profile: recorded writes and pastes overwriting with history still
    /// waiting for their answer, and conflict checks and restores (whose
    /// tab may have closed since). Nothing of these may be removed or moved
    /// meanwhile. Borrowed, in no particular order: asked every frame while
    /// a History tab or Settings shows, it collects and copies nothing.
    pub(super) fn in_use(&self) -> impl Iterator<Item = (uuid::Uuid, &Namespace)> {
        let copies = self.unrecorded.values().flat_map(|u| {
            let (to, items) = match &u.cmd {
                Command::CopyCollections { to, items, .. } => (to.0, items.as_slice()),
                _ => (uuid::Uuid::nil(), &[][..]),
            };
            items.iter().map(move |i| (to, &i.dst))
        });
        let writes = self.unrecorded.iter().filter_map(|(id, u)| match &u.cmd {
            Command::CopyCollections { .. } => None,
            cmd => Some((self.pending.get(id)?.conn?.0, cmd.recorded_target()?.0)),
        });
        let checks = self.pending.values().filter_map(|s| match (&s.origin, s.conn) {
            (Origin::History { ns, req: HistoryReq::Check | HistoryReq::Restore, .. }, Some(conn)) => Some((conn.0, ns)),
            _ => None,
        });
        copies.chain(writes).chain(checks)
    }
}
