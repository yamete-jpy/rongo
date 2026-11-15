//! `Job`: work on a thread of its own, for the UI to poll each frame.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};

use egui::Context;

/// Work on a thread of its own, polled each frame. Dropping it (another
/// entry selected, the tab closed) raises the stop flag the work is given,
/// so a read nobody waits for any more ends early. Settings measures and
/// changes the history files with it too.
pub struct Job<T> {
    rx: Receiver<T>,
    stop: Arc<AtomicBool>,
}

impl<T: Send + 'static> Job<T> {
    /// Runs `work` on a thread called `name`. If the thread can't start,
    /// `poll` says so.
    pub fn start(ctx: &Context, name: &str, work: impl FnOnce(&AtomicBool) -> T + Send + 'static) -> Self {
        Self::spawn(ctx, name, work).0
    }

    /// `start`, answering at once why the thread could not start.
    pub fn try_start(ctx: &Context, name: &str, work: impl FnOnce(&AtomicBool) -> T + Send + 'static) -> std::io::Result<Self> {
        let (job, spawned) = Self::spawn(ctx, name, work);
        spawned.map(|()| job)
    }

    fn spawn(ctx: &Context, name: &str, work: impl FnOnce(&AtomicBool) -> T + Send + 'static) -> (Self, std::io::Result<()>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        // If the thread can't start, `poll` says so (the sender is gone).
        let spawned = std::thread::Builder::new().name(name.into()).spawn(move || {
            if tx.send(work(&flag)).is_ok() {
                ctx.request_repaint();
            }
        });
        (Job { rx, stop }, spawned.map(drop))
    }

    /// The work's answer once it has one. Its thread ending without one (a
    /// panic, or a thread that never started) is an error in neutral words,
    /// which a caller may replace with its own.
    pub fn poll(&self) -> Option<Result<T, String>> {
        match self.rx.try_recv() {
            Ok(v) => Some(Ok(v)),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("it stopped unexpectedly".into())),
        }
    }

    /// Tests: the flag dropping the job raises.
    #[cfg(test)]
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }
}

impl<T> Drop for Job<T> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
