//! The Operations panel's glue: taking the writes and data moves sent into
//! its log, their answers, ⊗, and showing an export's file.

use std::path::PathBuf;
use std::time::Instant;

use rongo_core::bson::DateTime;
use rongo_core::{ConnId, CoreError, Event, RequestId};

use crate::job::Job;
use crate::operations::{Answer, ConnLabel, Entry, OpsAction, Status};
use crate::theme;

use super::RongoApp;
use super::outcome::{copy_summary, copy_troubled, outcome_text, troubled};
use super::status::OpKey;

impl RongoApp {
    /// Takes the requests sent since into the Operations log (those whose
    /// `Sent::log` is set), naming their connections as they are now. Called
    /// before an answer is handled, each frame, and before a connection
    /// closes, so the names are captured while the connections are open.
    pub(super) fn log_sent(&mut self) {
        // A stop asked for elsewhere (a History tab's Cancel sends its own).
        for (id, s) in &self.requests.pending {
            if s.stop_asked {
                self.ops.stopping(*id);
            }
        }
        let mut new: Vec<RequestId> = self.requests.pending.iter().filter(|(id, s)| s.log.is_some() && !self.ops.has(**id)).map(|(id, _)| *id).collect();
        if new.is_empty() {
            return;
        }
        new.sort_unstable();
        let (now, wall) = (Instant::now(), DateTime::now().timestamp_millis());
        let label = |cid: Option<ConnId>| match cid.and_then(|c| self.conns.get(&c)) {
            Some(c) => ConnLabel { name: c.profile.name.clone(), color: theme::conn_color(c.profile.color.as_deref()) },
            None => ConnLabel { name: "(closed)".into(), color: None },
        };
        let mut entries = Vec::new();
        for id in new {
            let s = &self.requests.pending[&id];
            let Some(seed) = &s.log else { continue };
            // A copy names both ends: "SIT → UAT".
            let mut conns = Vec::new();
            if s.conn.is_some() || seed.to.is_some() {
                conns.push(label(s.conn));
            }
            if let Some(to) = seed.to {
                conns.push(label(Some(to)));
            }
            let ago = now.saturating_duration_since(s.started).as_millis() as i64;
            let title = s.what.clone().unwrap_or_else(|| "Operation".into());
            entries.push(Entry::new(id, title, conns, DateTime::from_millis(wall - ago), s.started, s.cancellable, &seed.items));
        }
        for e in entries {
            self.ops.add(e);
        }
    }

    /// Whether the panel shows: the sidebar open and the panel unfolded.
    /// Then the corner leaves out what the panel lists.
    pub(super) fn ops_panel_shows(&self) -> bool {
        self.sidebar.open && self.ops.open
    }

    pub(super) fn ops_action(&mut self, a: OpsAction) {
        match a {
            OpsAction::Cancel(id) => self.cancel_op(OpKey::Request(id)),
            OpsAction::Reveal(path) => self.reveal(path),
        }
    }

    /// Shows an export's file in the file manager, off the UI thread
    /// (`open` waits for Finder); a file that is gone, or a failure, is
    /// said in an error toast (`poll_reveal`).
    pub(super) fn reveal(&mut self, path: PathBuf) {
        let reveal = self.update.hooks.reveal.clone();
        self.revealing = Some(Job::start(&self.ctx, "reveal-file", move |_| reveal(&path)));
    }

    pub(super) fn poll_reveal(&mut self) {
        if let Some(done) = self.revealing.as_ref().and_then(|j| j.poll()) {
            self.revealing = None;
            match done {
                Ok(Ok(())) => {}
                Ok(Err(e)) | Err(e) => self.toast(format!("Couldn't show the file: {e}"), true),
            }
        }
    }
}

/// What a logged request's terminal answer says for its entry: stopped
/// (cancelled: by the user, or by a problem, which `Operations::finish`
/// tells apart), failed, done with something to know, or done; whether
/// nothing was written; what to say when handling it toasts nothing; an
/// export's file; a copy's report.
pub(super) fn answer_of(ev: &Event) -> Answer {
    let status = match ev {
        Event::Failed(CoreError::Cancelled) => Status::Stopped,
        Event::Failed(_) => Status::Failed,
        Event::Written(w) if w.cancelled => Status::Stopped,
        Event::Copied(r) if r.cancelled => Status::Stopped,
        Event::Restored { cancelled: true, .. } => Status::Stopped,
        Event::Copied(r) if copy_troubled(r) => Status::Warning,
        Event::Restored { warning, unconfirmed, .. } if warning.is_some() || *unconfirmed => Status::Warning,
        ev if troubled(ev) => Status::Warning,
        _ => Status::Done,
    };
    // Nothing written, nor maybe written (an unconfirmed write may have been).
    let nothing_done = match ev {
        Event::Failed(_) => true,
        Event::Written(w) => !w.unconfirmed && w.inserted == 0 && w.modified == 0 && w.deleted == 0 && w.inserted_id.is_none(),
        Event::Restored { restored, overwritten, inserted_new, unconfirmed, .. } => !unconfirmed && restored + overwritten + inserted_new == 0,
        Event::Copied(r) => r.items.iter().all(|i| i.inserted == 0 && i.overwritten == 0 && !i.unconfirmed),
        _ => false,
    };
    let fallback = match ev {
        Event::Failed(e) => Some(e.to_string()),
        Event::Copied(r) => Some(copy_summary(r)),
        Event::Done => None,
        ev => Some(outcome_text(ev)).filter(|t| t != "Done"),
    };
    let path = match ev {
        Event::Exported { path, .. } => Some(path.clone()),
        _ => None,
    };
    let copied = match ev {
        Event::Copied(r) => Some(r.clone()),
        _ => None,
    };
    Answer { status, nothing_done, fallback, path, copied }
}
