//! The corner's running work: what shows there, and ⊗ to discard it.

use std::time::{Duration, Instant};

use egui::{Align2, RichText};
use rongo_core::{ConnId, RequestId};

use crate::net::{Origin, Sent};
use crate::result_view::thousands;
use crate::tabs::TabKind;
use crate::theme;

use super::RongoApp;

impl RongoApp {
    /// Whether the corner shows this request (once it has taken a moment):
    /// not background work (no `what`), not a result view's own request (its
    /// status line shows it), nor the on-screen History tab's (the tab
    /// shows it), nor, while the Operations panel shows, one it lists. The
    /// repaints the corner asks for follow the same rule.
    fn shown_in_corner(&self, s: &Sent) -> bool {
        let on_screen = self.tabs.get(self.active_tab).map(|t| t.id);
        let in_panel = s.log.is_some() && self.ops_panel_shows();
        s.what.is_some() && !in_panel && !matches!(s.origin, Origin::View(..)) && !matches!(s.origin, Origin::History { tab, .. } if Some(tab) == on_screen)
    }

    /// Running work the status bar shows, oldest first: requests that have
    /// taken longer than a moment (quicker ones would only flicker) and
    /// running scripts.
    pub(super) fn operations(&self, now: Instant) -> Vec<Operation> {
        let name = |cid: Option<ConnId>| cid.and_then(|c| self.conns.get(&c)).map(|c| c.profile.name.clone());
        let label = |what: &str, cid: Option<ConnId>| match name(cid) {
            Some(n) => format!("{what} · {n}"),
            None => what.to_string(),
        };
        // A copy names both ends: "SIT → UAT".
        let ends = |s: &Sent| match (&s.origin, name(s.conn)) {
            (Origin::Copy { to, .. }, Some(from)) => Some(format!("{from} → {}", name(Some(*to)).unwrap_or_else(|| "(closed)".into()))),
            (_, from) => from,
        };
        // "{what}… {note} {done} / {total}"; the ellipsis then belongs to the text.
        let with_progress = |s: &Sent| {
            let what = s.what.as_deref()?;
            let what = match ends(s) {
                Some(n) => format!("{what} · {n}"),
                None => what.to_string(),
            };
            Some(match &s.progress {
                None => (what, false),
                Some((done, total, note)) => {
                    let count = match total {
                        Some(t) => format!("{} / {}", thousands(*done), thousands(*t)),
                        None => thousands(*done),
                    };
                    (if note.is_empty() { format!("{what}… {count}") } else { format!("{what}… {note} {count}") }, true)
                }
            })
        };
        let mut ops: Vec<Operation> = self
            .requests
            .pending
            .iter()
            .filter(|(_, s)| self.shown_in_corner(s))
            .filter(|(_, s)| now.saturating_duration_since(s.started) >= theme::SHOW_BUSY_AFTER)
            .filter_map(|(id, s)| {
                let (text, ellipsis) = with_progress(s)?;
                Some(Operation { key: OpKey::Request(*id), text, ellipsis, started: s.started, stop: s.cancellable })
            })
            .collect();
        for (i, t) in self.tabs.iter().enumerate() {
            // The shell on screen shows "Running…" and Stop itself.
            if let TabKind::Shell(sh) = &t.kind
                && i != self.active_tab
                && let Some(started) = sh.running_since()
            {
                ops.push(Operation { key: OpKey::Script(t.id), text: label(&format!("Running a script on {}", sh.db), Some(sh.conn)), ellipsis: false, started, stop: true });
            }
        }
        ops.sort_by_key(|o| o.started);
        ops
    }

    /// Discard an operation: cancel the request (its reply then says
    /// cancelled, or what it did before it stopped between batches) or
    /// interrupt the script. A write the server finishes once begun (clear,
    /// drop, create index, rename, …) is never cancelled: it would go on on
    /// the server while the app said cancelled, and only a rename's answer
    /// may move the collection's history. A Duplicate stops between batches
    /// and drops its partial copy, so it can be.
    pub(super) fn cancel_op(&mut self, key: OpKey) {
        match key {
            OpKey::Request(id) if self.requests.pending.get(&id).is_some_and(|s| !s.cancellable) => {}
            OpKey::Request(id) => {
                self.ops.stopping(id);
                let tab = match self.requests.cancel(id) {
                    Some(Origin::History { tab, .. }) => Some(*tab),
                    _ => None,
                };
                // A History tab waiting on it must not go on when an answer
                // already on its way arrives (a check would ask to restore).
                if let Some(run) = tab.and_then(|t| self.history_tab_mut(t)).and_then(|h| h.running.as_mut())
                    && run.req == id
                {
                    run.cancelling = true;
                }
            }
            OpKey::Script(tab) => {
                if let Some(TabKind::Shell(sh)) = self.tabs.iter().find(|t| t.id == tab).map(|t| &t.kind) {
                    sh.stop();
                }
            }
        }
    }

    /// How far above the window's bottom the corner floats: 10 points, or,
    /// while the sidebar shows the Operations panel, just above the panel
    /// (its header, folded), which it would otherwise cover: unfolding it is
    /// what a long paste calls for.
    pub(super) fn corner_lift(&self, ctx: &egui::Context) -> f32 {
        match self.ops_top.filter(|_| self.sidebar.open) {
            Some(top) => (ctx.content_rect().bottom() - top + 6.0).max(10.0),
            None => 10.0,
        }
    }

    /// Work running outside the result view on screen (connecting, the
    /// tree, writes, scripts in other tabs), floating in the bottom-left
    /// corner only while there is some: the latest, how long it has taken,
    /// ⊗ to discard it, and the others under "+n more".
    pub(super) fn activity(&mut self, ctx: &egui::Context) {
        self.activity_at(ctx, Instant::now());
    }

    /// `activity` at a given moment (tests fix it, so the times they read
    /// don't depend on how slowly the machine runs).
    pub(super) fn activity_at(&mut self, ctx: &egui::Context, now: Instant) {
        let ops = self.operations(now);
        let mut cancel = None;
        if let Some(op) = ops.last() {
            egui::Area::new(egui::Id::new("activity"))
                .order(egui::Order::Foreground)
                .anchor(Align2::LEFT_BOTTOM, egui::vec2(10.0, -self.corner_lift(ctx)))
                .interactable(true)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).inner_margin(egui::Margin::symmetric(8, 4)).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if op.stop && theme::discard_button(ui) {
                                cancel = Some(op.key);
                            }
                            theme::busy_bar(ui, 80.0);
                            ui.label(if op.ellipsis { op.text.clone() } else { format!("{}…", op.text) });
                            ui.label(RichText::new(theme::elapsed(now.saturating_duration_since(op.started))).color(theme::MUTED));
                            if ops.len() > 1 {
                                ui.menu_button(format!("+{} more", ops.len() - 1), |ui| {
                                    for o in ops.iter().rev() {
                                        ui.horizontal(|ui| {
                                            if o.stop && theme::discard_button(ui) {
                                                cancel = Some(o.key);
                                                ui.close();
                                            }
                                            ui.label(&o.text);
                                            ui.label(RichText::new(theme::elapsed(now.saturating_duration_since(o.started))).color(theme::MUTED));
                                        });
                                    }
                                });
                            }
                        });
                    });
                });
        }
        if let Some(k) = cancel {
            self.cancel_op(k);
        }
        // While something shows, its elapsed time and progress (sent at most
        // every 100 ms) need no more; the bar slides in steps. Else a
        // repaint when the next request is due to show, if one will be.
        if !ops.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if let Some(next) = self.requests.pending.values().filter(|s| self.shown_in_corner(s)).map(|s| s.started + theme::SHOW_BUSY_AFTER).filter(|t| *t > now).min() {
            ctx.request_repaint_after(next.saturating_duration_since(now));
        }
    }
}

/// Something the status bar can discard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OpKey {
    Request(RequestId),
    /// The script running in the shell tab with this tab id.
    Script(u64),
}

pub(super) struct Operation {
    pub(super) key: OpKey,
    pub(super) text: String,
    /// Whether `text` already ends the work's name with "…" (it carries progress).
    ellipsis: bool,
    started: Instant,
    /// Whether ⊗ can stop it (not a write the server finishes: see `cancel_op`).
    pub(super) stop: bool,
}
