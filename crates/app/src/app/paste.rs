//! Copying collections in the tree and pasting them: the clip, the Paste dialog and its write.

pub(crate) use rongo_core::names::database_name_error;
use rongo_core::{Command, ConnId};

use crate::net::Origin;
use crate::paste::{CollClip, PasteAction, PasteDialog};

use super::RongoApp;

impl RongoApp {
    /// Remember collections for Paste, and put their names on the system
    /// clipboard: ⌘V only reaches the app when the clipboard holds text.
    pub(super) fn copy_collections(&mut self, conn: ConnId, db: String, colls: Vec<String>) {
        if colls.is_empty() {
            return;
        }
        let n = colls.len();
        self.ctx.copy_text(colls.join("\n"));
        self.coll_clip = Some(CollClip { conn, db, colls });
        self.toast(format!("Copied {n} collection{}", if n == 1 { "" } else { "s" }), false);
    }

    /// Why nothing can be pasted into `conn` (toasted), if so.
    pub(super) fn paste_blocked(&mut self, conn: ConnId) -> bool {
        let why = if self.conns.get(&conn).is_some_and(|c| c.profile.read_only) {
            crate::result_view::LOCKED
        } else if self.coll_clip.is_none() {
            "Copy collections first"
        } else if self.coll_clip.as_ref().is_some_and(|c| !self.conns.contains_key(&c.conn)) {
            "The connection the collections were copied from is closed"
        } else {
            return false;
        };
        self.toast(why, true);
        true
    }

    /// Open the Paste dialog for `db` on `to`, and ask what that database
    /// already holds so rows that land on a collection can offer Replace.
    pub(super) fn open_paste(&mut self, to: ConnId, db: String) {
        if self.paste_blocked(to) {
            return;
        }
        let Some(clip) = &self.coll_clip else { return };
        let mut dialog = PasteDialog::new(clip, to, db.clone());
        dialog.set_source_name(self.conns.get(&clip.conn).map(|c| c.profile.name.clone()).unwrap_or_default());
        self.paste_dialog = Some(dialog);
        self.net().send(Origin::Collections(to, db.clone()), Some(to), Command::ListCollections { db });
    }

    pub(super) fn paste_action(&mut self, a: PasteAction) {
        match a {
            PasteAction::Close => self.paste_dialog = None,
            PasteAction::Send(cmd) => {
                // The history key can't be checked yet: the dialog stays, to paste again.
                let Some(from) = self.paste_dialog.as_ref().map(|d| d.from()) else { return };
                if !self.history_ready(from, &cmd) {
                    return;
                }
                let Some(d) = self.paste_dialog.take() else { return };
                let origin = Origin::Copy { to: d.to(), db: d.to_db().to_string() };
                // Append + Overwrite keeps what it replaces in the target's history.
                self.send_write(origin, d.from(), cmd, true);
            }
        }
    }
}
