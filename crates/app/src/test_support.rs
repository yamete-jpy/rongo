//! Helpers the app's tests share: a temporary folder and vault, a
//! Keychain in memory, a dialog on screen, a click outside it, a long
//! driver error.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rongo_core::profile::SecretBackend;
use rongo_core::vault::{KdfParams, Vault};
use rongo_core::{CoreError, CoreResult};

/// The master password of every test vault.
pub const TEST_PASSWORD: &str = "test-password";

/// A folder of its own in the temporary folder, removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("rongo-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("a temporary folder");
        TempDir(dir)
    }

    pub fn vault(&self) -> PathBuf {
        self.0.join("vault")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A vault of its own, quick to open (`KdfParams::TEST`), set with `TEST_PASSWORD`.
pub fn temp_vault() -> (TempDir, Arc<Vault>) {
    let t = TempDir::new();
    let v = Vault::create(t.vault(), TEST_PASSWORD, KdfParams::TEST, Default::default()).expect("a test vault");
    (t, Arc::new(v))
}

/// The account of the one Keychain item older versions kept every secret in.
pub const KEYCHAIN_VAULT: &str = "vault";

/// A Keychain in memory: the app's tests never touch the real one. With
/// `refuse`, every delete fails, as when macOS asked and the user said no.
#[derive(Default)]
pub struct Mem {
    items: Mutex<HashMap<String, String>>,
    refuse: bool,
}

impl Mem {
    /// One whose every delete fails.
    pub fn refusing() -> Self {
        Self { refuse: true, ..Self::default() }
    }

    pub fn put(&self, account: &str, value: &str) {
        self.items.lock().unwrap().insert(account.into(), value.into());
    }

    pub fn accounts(&self) -> Vec<String> {
        let mut names: Vec<String> = self.items.lock().unwrap().keys().cloned().collect();
        names.sort();
        names
    }
}

impl SecretBackend for Mem {
    fn read(&self, account: &str) -> CoreResult<Option<String>> {
        Ok(self.items.lock().unwrap().get(account).cloned())
    }
    fn delete(&self, account: &str) -> CoreResult<()> {
        if self.refuse {
            return Err(CoreError::Other("keychain: the user said no".into()));
        }
        self.items.lock().unwrap().remove(account);
        Ok(())
    }
}

/// Tests: a click on a dialog's backdrop (the window's top-left corner,
/// outside any centred dialog), as press and release on separate frames.
pub fn click_outside<S>(h: &mut egui_kittest::Harness<'_, S>) {
    let at = egui::pos2(4.0, 4.0);
    let button = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
    h.event(egui::Event::PointerMoved(at));
    h.run_steps(1);
    h.event(button(true));
    h.run_steps(1);
    h.event(button(false));
    h.run_steps(2);
}

/// `state` on screen in a window of `size`, drawn by `draw` with the app's
/// theme, after `frames` frames: a dialog driven as the app drives it.
pub fn harness<S: 'static>(size: egui::Vec2, frames: usize, state: S, mut draw: impl FnMut(&egui::Context, &mut S) + 'static) -> egui_kittest::Harness<'static, S> {
    let mut h = egui_kittest::Harness::builder().with_size(size).build_ui_state(
        move |ui, s: &mut S| {
            crate::theme::install(ui.ctx());
            draw(ui.ctx(), s);
        },
        state,
    );
    h.run_steps(frames);
    h
}

/// Whether the widget labelled `label` is disabled.
pub fn disabled<S>(h: &egui_kittest::Harness<'_, S>, label: &str) -> bool {
    use egui_kittest::kittest::{NodeT, Queryable};
    h.get_by_label(label).accesskit_node().is_disabled()
}

/// What the driver says when no member of a three-node replica set
/// answers: one paragraph, several hundred characters.
pub fn long_driver_error() -> String {
    let server = |n| format!("{{ Address: node-{n}.cluster.example.net:27017, Type: Unknown, Error: Kind: I/O error: failed to lookup address information: nodename nor servname provided, or not known, labels: {{}} }}");
    format!(
        "timed out: Server selection timeout: No available servers. Topology: {{ Type: ReplicaSetNoPrimary, Set Name: rs0, Servers: [ {}, {}, {} ] }}, labels: {{}}",
        server(0),
        server(1),
        server(2)
    )
}
