//! User preferences, saved as settings.json next to the connection profiles,
//! and the Settings dialog, whose Collection History section also lists
//! every history kept on this computer (`HistoryFiles`), and whose Master password
//! section opens Change Master Password… (`ChangePasswordDialog`) and Reset….

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use egui::{Align, Align2, Color32, Context, Id, Key, Layout, Modal, RichText, Sense, TextEdit, Ui, UiBuilder, pos2, vec2};
use egui_phosphor::regular as icon;
use rongo_core::format::{self, DisplayTz};
use rongo_core::history::{HistoryKey, Store};
use rongo_core::vault::{self, Vault};
use rongo_core::{CoreError, CoreResult, Namespace};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::job::Job;
use crate::result_view::thousands;
use crate::theme;
use crate::update::{self, Version};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenIn {
    /// Double-clicking a collection opens IntelliShell with a find() and runs it.
    #[default]
    IntelliShell,
    /// Double-clicking opens a collection tab with a query bar.
    CollectionTab,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub open_collection_in: OpenIn,
    pub timezone: DisplayTz,
    pub page_size: i64,
    pub history: HistorySettings,
    /// Ask GitHub at launch, at most once a day, whether a newer rongo is out.
    pub check_updates: bool,
    /// When rongo last asked (UTC). Kept by the update check, not the dialog.
    #[serde(with = "update::rfc3339")]
    pub last_update_check: Option<rongo_core::bson::DateTime>,
    /// The version Later hid: no banner for it, only for a newer one. Kept
    /// by the update check, not the dialog.
    #[serde(with = "update::version_text")]
    pub skipped_version: Option<Version>,
    /// The newest version the update check found, shown at every launch
    /// until Later hides it or rongo is that version or newer. Kept by the
    /// update check, not the dialog.
    #[serde(with = "update::version_text")]
    pub latest_version: Option<Version>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            open_collection_in: OpenIn::IntelliShell,
            timezone: DisplayTz::Utc,
            page_size: 50,
            history: HistorySettings::default(),
            check_updates: true,
            last_update_check: None,
            skipped_version: None,
            latest_version: None,
        }
    }
}

/// Collection History: what rongo keeps, on this computer, before it deletes or
/// changes documents. A connection can override `enabled` (its profile's
/// `history`); the rest applies to every connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistorySettings {
    pub enabled: bool,
    pub deletes: bool,
    pub updates: bool,
    /// Entries older than this are removed.
    pub keep_days: u32,
    /// The oldest entries are removed to keep the total under this (MiB).
    pub max_mb: u64,
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self { enabled: true, deletes: true, updates: true, keep_days: 30, max_mb: 1024 }
    }
}

impl HistorySettings {
    pub fn max_bytes(&self) -> u64 {
        self.max_mb.saturating_mul(1 << 20)
    }
}

/// Days Settings can keep history for.
const KEEP_DAYS: std::ops::RangeInclusive<u32> = 1..=3650;
/// Size limits Settings offers, in MB (up to 1 TB).
const MAX_MB: std::ops::RangeInclusive<u64> = 1..=1_048_576;

const HISTORY_SWITCH: &str = "Keep the documents rongo deletes or changes, on this computer";

const UPDATE_SWITCH: &str = "Check for new versions at launch";

/// How Settings names a history whose connection profile was deleted.
pub const DELETED_PROFILE: &str = "deleted profile";

/// Why one collection's history can't be deleted now.
pub const HISTORY_IN_USE: &str = "Wait for the delete, update or restore on this collection to finish: its history is still being written";

/// Why Clear All History waits.
pub const HISTORY_ALL_IN_USE: &str = "Wait for the deletes, updates and restores running now to finish: their history is still being written";

/// Why Open waits: opening a history closes Settings.
const UNSAVED: &str = "Save or cancel your changes first: opening a history closes Settings";

/// Why Reset… waits: resetting closes Settings.
const UNSAVED_RESET: &str = "Save or cancel your changes first: resetting closes Settings";

/// A size in MB (MiB, as the history's size limit is set).
pub fn mb(bytes: u64) -> String {
    let v = bytes as f64 / (1u64 << 20) as f64;
    if bytes == 0 {
        "0 MB".into()
    } else if v < 0.1 {
        "under 0.1 MB".into()
    } else if v < 100.0 {
        format!("{v:.1} MB")
    } else {
        format!("{} MB", thousands(v.round() as u64))
    }
}

/// Collection History on disk as Settings shows it, read from file names
/// and sizes only (nothing is decrypted, so no key is needed).
#[derive(Clone, Debug, Default)]
pub struct Scan {
    /// Bytes by profile. Every entry file counts, also one in a folder whose
    /// collection name can't be read back (which `colls` leaves out).
    pub by_profile: BTreeMap<Uuid, u64>,
    /// Every collection with a history: profile, collection, entries, bytes.
    pub colls: Vec<(Uuid, Namespace, u64, u64)>,
}

impl Scan {
    pub fn bytes(&self) -> u64 {
        self.by_profile.values().sum()
    }

    fn read(store: &Store) -> Result<Scan, String> {
        let by_profile = store.usage_by_profile().map_err(|e| e.to_string())?;
        let colls = store.all().map_err(|e| e.to_string())?;
        Ok(Scan { by_profile, colls })
    }
}

/// A change to the history files, made off the UI thread.
type Chore = Box<dyn FnOnce(&Store) -> CoreResult<()> + Send>;

/// A change waiting to start: what Settings says meanwhile, the profile and
/// collection it removes entries of (None: every one), and the work.
struct Change {
    doing: &'static str,
    profile: Option<Uuid>,
    ns: Option<Namespace>,
    chore: Chore,
}

/// What a piece of work on the files answers: the change's error, if it
/// made one and failed, and the measure taken after it.
type Done = (Option<String>, Result<Scan, String>);

/// The Collection History files, for Settings and for deleting a profile:
/// measuring them, deleting one collection's history, a profile's or all of
/// it, and trimming them to new limits. The work runs on a thread of its
/// own, one change at a time, each followed by a fresh measure, so a large
/// history never stalls a frame. A change waits in a queue until the app
/// has checked that nothing is being written where it removes entries
/// (`next`, `start_next`), however long it waited. Nothing here decrypts,
/// so it needs no key (the store's key is a placeholder never used).
pub struct HistoryFiles {
    store: Store,
    /// The work running: what Settings says meanwhile (none for a measure
    /// alone), and the job, answering with the change's error and the measure.
    running: Option<(Option<&'static str>, Job<Done>)>,
    /// Changes waiting for the one running, or for writes where they remove.
    queue: VecDeque<Change>,
    scan: Option<Result<Scan, String>>,
}

impl HistoryFiles {
    pub fn new(root: PathBuf) -> Self {
        Self { store: Store::new(root, HistoryKey::new([0; 32])), running: None, queue: VecDeque::new(), scan: None }
    }

    /// Measures again. A change running or waiting is followed by a measure anyway.
    pub fn rescan(&mut self, ctx: &Context) {
        if self.doing().is_some() {
            return;
        }
        let store = self.store.clone();
        self.running = Some((None, Job::start(ctx, "history-files", move |_| (None, Scan::read(&store)))));
    }

    /// Queues a change to the files, which removes entries of `profile` on
    /// `ns` (None: of every one); it then measures again. `doing` is what
    /// Settings says meanwhile. It starts only through `start_next`.
    pub fn queue(&mut self, doing: &'static str, profile: Option<Uuid>, ns: Option<Namespace>, chore: impl FnOnce(&Store) -> CoreResult<()> + Send + 'static) {
        self.queue.push_back(Change { doing, profile, ns, chore: Box::new(chore) });
    }

    /// The change that would start next, once no change runs: the profile
    /// and collection it removes entries of (None: every one). The app
    /// starts it (`start_next`) only while nothing is written there.
    pub fn next(&self) -> Option<(Option<Uuid>, Option<&Namespace>)> {
        if self.running.as_ref().is_some_and(|(d, _)| d.is_some()) {
            return None;
        }
        self.queue.front().map(|c| (c.profile, c.ns.as_ref()))
    }

    /// Starts the next change, unless one runs. A measure alone running
    /// would be out of date: it gives way.
    pub fn start_next(&mut self, ctx: &Context) {
        if self.next().is_none() {
            return;
        }
        let Some(Change { doing, chore, .. }) = self.queue.pop_front() else { return };
        let store = self.store.clone();
        // Said after "Collection History: ", so not twice.
        let why = |e: CoreError| match e {
            CoreError::History(m) => m,
            e => e.to_string(),
        };
        let job = Job::start(ctx, "history-files", move |_| (chore(&store).err().map(why), Scan::read(&store)));
        self.running = Some((Some(doing), job));
    }

    /// Takes in finished work. Some when a change finished: Ok, or what
    /// went wrong. The next change waits for `start_next`.
    pub fn poll(&mut self) -> Option<Result<(), String>> {
        let done = self.running.as_ref()?.1.poll()?;
        let (doing, _) = self.running.take().expect("polled");
        let error = match done {
            Ok((error, scan)) => {
                self.scan = Some(scan);
                error
            }
            Err(_) => Some("the work on its files stopped unexpectedly".into()),
        };
        doing.map(|_| error.map_or(Ok(()), Err))
    }

    /// Nothing running or waiting.
    pub fn idle(&self) -> bool {
        self.running.is_none() && self.queue.is_empty()
    }

    /// The change running or next, for Settings to say.
    pub fn doing(&self) -> Option<&'static str> {
        self.running.as_ref().and_then(|(d, _)| *d).or_else(|| self.queue.front().map(|c| c.doing))
    }

    /// The latest measure, or why it failed; None before the first.
    pub fn scan(&self) -> Option<&Result<Scan, String>> {
        self.scan.as_ref()
    }

    #[cfg(test)]
    pub fn set_scan(&mut self, scan: Scan) {
        self.scan = Some(Ok(scan));
    }
}

/// What the Settings dialog shows of Collection History besides the settings.
pub struct HistoryView<'a> {
    pub files: &'a HistoryFiles,
    /// Connection names by profile; a profile not here was deleted.
    pub names: &'a HashMap<Uuid, String>,
    /// Collections whose history is being written (or read for a restore)
    /// now, by profile: none of it may be deleted meanwhile. Borrowed from
    /// the app's requests, not copied every frame.
    pub in_use: &'a [(Uuid, &'a Namespace)],
}

pub const MAX_PAGE_SIZE: i64 = 50_000;

/// Settings writes in this process, one at a time: `write_atomically`'s
/// temporary file is named for the process, so a Save and the update
/// check's thread writing at once would rename each other's half-written
/// file (or fail).
static WRITING: Mutex<()> = Mutex::new(());

impl Settings {
    /// The real settings file, in the app's configuration folder. Test
    /// builds have no such function: each test app keeps its settings in a
    /// temporary folder of its own (`RongoApp::new`), so no test can read or
    /// write the user's settings.
    #[cfg(not(test))]
    pub fn default_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("dev", "rongo", "rongo").map(|d| d.config_dir().join("settings.json"))
    }

    /// Settings saved at `path`, or the defaults when there are none; values
    /// out of the ranges Settings offers are brought back into them. A file
    /// that can't be read is not lost (nor overwritten by the next Save): it
    /// is moved aside to `settings.json.unreadable`, replacing an older one,
    /// the defaults are used, and the notice (for a toast) says so.
    pub fn load(path: &Path) -> (Self, Option<String>) {
        let why = match Self::read(path) {
            Ok(s) => return (s.unwrap_or_default(), None),
            Err(why) => why,
        };
        let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut aside = path.as_os_str().to_owned();
        aside.push(".unreadable");
        let aside = PathBuf::from(aside);
        let notice = match std::fs::rename(path, &aside) {
            Ok(()) => format!("{} couldn't be read ({why}), so rongo uses the default settings. The file is kept as {}.", name(path), name(&aside)),
            Err(e) => format!("{} couldn't be read ({why}), so rongo uses the default settings. It couldn't be kept aside ({e}): saving Settings replaces it.", name(path)),
        };
        (Self::default(), Some(notice))
    }

    /// Settings saved at `path` (brought into range), None when there are
    /// none, or why the file can't be read. Nothing is moved or written.
    pub fn read(path: &Path) -> Result<Option<Self>, String> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<Settings>(&bytes).map(|s| Some(s.clamped())).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Within the ranges the dialog offers: an edited file can't make the
    /// start-up purge keep nothing (0 days or 0 MB), and the dialog doesn't
    /// open with changes of its own.
    fn clamped(mut self) -> Self {
        self.page_size = self.page_size.clamp(1, MAX_PAGE_SIZE);
        self.history.keep_days = self.history.keep_days.clamp(*KEEP_DAYS.start(), *KEEP_DAYS.end());
        self.history.max_mb = self.history.max_mb.clamp(*MAX_MB.start(), *MAX_MB.end());
        self
    }

    /// Written whole (a temporary file, synced, renamed over it): a crash or
    /// a full disk leaves the old settings, never half a file.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let _one = WRITING.lock().unwrap_or_else(|e| e.into_inner());
        self.write(path)
    }

    /// The settings saved at `path` as they are on disk now, changed by
    /// `change` and saved, with no other settings write of this process in
    /// between; with no file yet, `fallback` changed. A file that can't be
    /// read is left alone (the error says why). The update check saves its
    /// own fields this way, so it never undoes Settings another window saved.
    pub fn change_on_disk(path: &Path, fallback: &Settings, change: impl FnOnce(&mut Settings)) -> Result<(), String> {
        let _one = WRITING.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = Self::read(path)?.unwrap_or_else(|| fallback.clone());
        change(&mut s);
        s.write(path)
    }

    /// `save`, with `WRITING` held.
    fn write(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            rongo_core::fsx::create_private_dir(dir).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        rongo_core::vault::write_atomically(path, &json).map_err(|e| format!("{}: {e}", path.display()))?;
        // What a crash left in another process: settings have no lock, so only files no write could still be busy with.
        rongo_core::vault::remove_stale_temps(path, std::time::Duration::from_secs(60));
        Ok(())
    }

    /// Push the settings into the parts of the app that read them globally.
    pub fn apply(&self) {
        format::set_display_tz(self.timezone);
        crate::result_view::set_default_page_size(self.page_size);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TzChoice {
    Utc,
    Local,
    Offset,
}

pub struct SettingsDialog {
    /// The settings as they were saved, to tell unsaved changes.
    current: Settings,
    draft: Settings,
    tz: TzChoice,
    offset_hours: i32,
    offset_minutes: i32,
}

pub enum SettingsAction {
    Save(Settings),
    Close,
    /// Open this collection's Collection History tab (Settings closes).
    OpenHistory(Uuid, Namespace),
    /// Delete this collection's Collection History, once confirmed.
    DeleteHistory(Uuid, Namespace),
    /// Delete all Collection History, once confirmed.
    ClearHistory,
    /// Change Master Password… (Settings stays open behind it).
    ChangeMasterPassword,
    /// Reset…: asks first; then Settings closes and Set follows.
    ResetVault,
}

/// Rows the history list shows before it scrolls.
const LIST_ROWS: usize = 6;

impl SettingsDialog {
    pub fn new(current: &Settings) -> Self {
        let (tz, minutes) = match current.timezone {
            DisplayTz::Utc => (TzChoice::Utc, 420),
            DisplayTz::Local => (TzChoice::Local, 420),
            DisplayTz::Offset(m) => (TzChoice::Offset, m),
        };
        Self { current: current.clone(), draft: current.clone(), tz, offset_hours: minutes / 60, offset_minutes: (minutes % 60).abs() }
    }

    /// The settings as the dialog has them now.
    fn edited(&self) -> Settings {
        let mut s = self.draft.clone();
        s.timezone = self.timezone();
        s
    }

    /// Something was changed and not saved yet.
    pub fn unsaved(&self) -> bool {
        self.edited() != self.current
    }

    fn timezone(&self) -> DisplayTz {
        match self.tz {
            TzChoice::Utc => DisplayTz::Utc,
            TzChoice::Local => DisplayTz::Local,
            TzChoice::Offset => {
                let sign = if self.offset_hours < 0 { -1 } else { 1 };
                DisplayTz::Offset(self.offset_hours * 60 + sign * self.offset_minutes)
            }
        }
    }

    pub fn show(&mut self, ctx: &Context, history: &HistoryView) -> Option<SettingsAction> {
        let mut action = None;
        // Sized from the window, never from the dialog's own last size: on
        // a short window the body scrolls and Save stays on screen. A scroll
        // area is otherwise only as tall as the room left in the dialog as
        // it was last frame, and the dialog grows a little each frame.
        let screen = ctx.content_rect().size();
        let modal = Modal::new(Id::new("settings")).show(ctx, |ui| {
            ui.set_width(560.0f32.min(screen.x - 40.0).max(240.0));
            ui.heading(format!("{}  Settings", icon::GEAR));
            ui.add_space(8.0);
            let body = (screen.y - 150.0).max(120.0);
            let scroll = egui::ScrollArea::vertical().id_salt("settings-body").max_height(body).min_scrolled_height(body).auto_shrink([false, true]);
            scroll.show(ui, |ui| {
                self.body(ui, history, &mut action);
            });
            ui.add_space(12.0);
            theme::dialog_buttons(ui, |ui| {
                if ui.add(crate::theme::primary_button("Save", false)).clicked() {
                    action = Some(SettingsAction::Save(self.edited()));
                }
                if ui.button("Cancel").clicked() {
                    action = Some(SettingsAction::Close);
                }
            });
        });
        if theme::may_close(&modal, self.unsaved()) && action.is_none() {
            action = Some(SettingsAction::Close);
        }
        action
    }

    fn body(&mut self, ui: &mut Ui, history: &HistoryView, action: &mut Option<SettingsAction>) {
        ui.label(RichText::new("Double-clicking a collection opens").strong());
        ui.radio_value(&mut self.draft.open_collection_in, OpenIn::IntelliShell, "IntelliShell, with db.getCollection(…).find({}) already run");
        ui.radio_value(&mut self.draft.open_collection_in, OpenIn::CollectionTab, "A collection tab with a query bar");
        ui.add_space(10.0);

        ui.label(RichText::new("Show dates in").strong());
        ui.radio_value(&mut self.tz, TzChoice::Utc, "UTC (…Z), as MongoDB stores them");
        ui.radio_value(&mut self.tz, TzChoice::Local, format!("This computer's time zone (now UTC{})", format::local_offset_label()));
        ui.horizontal(|ui| {
            ui.radio_value(&mut self.tz, TzChoice::Offset, "A fixed offset:  UTC");
            ui.add_enabled_ui(self.tz == TzChoice::Offset, |ui| {
                ui.add(egui::DragValue::new(&mut self.offset_hours).range(-12..=14).custom_formatter(|v, _| format!("{v:+03}")));
                ui.label(":");
                egui::ComboBox::from_id_salt("tz-min").width(48.0).selected_text(format!("{:02}", self.offset_minutes)).show_ui(ui, |ui| {
                    for m in [0, 30, 45] {
                        ui.selectable_value(&mut self.offset_minutes, m, format!("{m:02}"));
                    }
                });
            });
        });
        let sample = rongo_core::bson::DateTime::from_millis(1_767_323_045_000);
        ui.weak(format!("Example: {}  (stored as {})", format::iso_date_in(sample, self.timezone()), format::iso_date_utc(sample)));
        ui.weak("Tables, Tree and JSON views, the document editor and shell output use this zone. Exported files stay in UTC.");
        ui.add_space(10.0);

        ui.horizontal(|ui| {
            ui.label(RichText::new("Default page size").strong());
            ui.add(egui::DragValue::new(&mut self.draft.page_size).range(1..=MAX_PAGE_SIZE).speed(5.0));
            ui.weak("documents (you can also type a size in each result view)");
        });
        ui.add_space(10.0);
        self.history_ui(ui, history, action);
        ui.add_space(10.0);
        ui.label(RichText::new("Updates").strong());
        ui.checkbox(&mut self.draft.check_updates, UPDATE_SWITCH);
        ui.weak("At most once a day, rongo asks GitHub for its latest release, sending only its own version, and says when a newer one is out. It never downloads or installs anything itself.");
        // Last: seldom used, and Reset… is the most drastic thing here.
        ui.add_space(10.0);
        self.master_password_ui(ui, action);
    }

    /// Master password: Change Master Password… and Reset…, which closes
    /// Settings, so it waits for unsaved changes (as Open does).
    fn master_password_ui(&self, ui: &mut Ui, action: &mut Option<SettingsAction>) {
        ui.label(RichText::new("Master password").strong());
        ui.horizontal_wrapped(|ui| {
            let change = ui.button("Change Master Password…").on_hover_text("Seal the saved passwords and the Collection History key under a new master password");
            if change.clicked() {
                *action = Some(SettingsAction::ChangeMasterPassword);
            }
            let unsaved = self.unsaved();
            let reset = ui.add_enabled(!unsaved, egui::Button::new(RichText::new("Reset…").color(theme::ERROR)));
            let reset = if unsaved { reset.on_disabled_hover_text(UNSAVED_RESET) } else { reset.on_hover_text("For a forgotten master password: deletes every saved password and makes Collection History unreadable; asks first") };
            if reset.clicked() {
                *action = Some(SettingsAction::ResetVault);
            }
        });
        ui.weak("rongo asks for it each time it starts. A forgotten one can't be recovered: Reset… deletes every saved password (the connections stay) and Collection History can no longer be read.");
    }

    /// Collection History: the switches and limits, the space it takes with
    /// Clear All History…, and every collection with a history.
    fn history_ui(&mut self, ui: &mut Ui, view: &HistoryView, action: &mut Option<SettingsAction>) {
        ui.label(RichText::new("Collection History").strong());
        let h = &mut self.draft.history;
        ui.checkbox(&mut h.enabled, HISTORY_SWITCH);
        ui.horizontal_wrapped(|ui| {
            ui.label("Record");
            ui.checkbox(&mut h.deletes, "Deletes");
            ui.checkbox(&mut h.updates, "Updates");
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("Keep for");
            ui.add(egui::DragValue::new(&mut h.keep_days).range(KEEP_DAYS));
            ui.label("days and at most");
            ui.add(egui::DragValue::new(&mut h.max_mb).range(MAX_MB).speed(8.0));
            ui.label("MB; the oldest entries go first");
        });
        ui.weak("Encrypted, with its key in the vault. A connection can turn history on or off for itself in its Edit connection form. New limits apply when you save.");
        ui.add_space(6.0);

        let doing = view.files.doing();
        let scan = view.files.scan();
        ui.horizontal_wrapped(|ui| {
            match scan {
                None => _ = ui.weak("Measuring…"),
                Some(Err(e)) => _ = ui.colored_label(theme::ERROR, format!("Could not measure it: {e}")),
                Some(Ok(s)) => _ = ui.label(format!("Using {}", mb(s.bytes()))),
            }
            if let Some(d) = doing {
                ui.spinner();
                ui.weak(d);
            }
            let has = matches!(scan, Some(Ok(s)) if s.bytes() > 0);
            let blocked = match doing {
                Some(d) => Some(format!("Wait until this is done: {d}")),
                None if !view.in_use.is_empty() => Some(HISTORY_ALL_IN_USE.to_string()),
                None if !has => Some("Nothing is kept".to_string()),
                None => None,
            };
            let clear = ui.add_enabled(blocked.is_none(), egui::Button::new(RichText::new("Clear All History…").color(theme::ERROR)));
            let clear = match blocked {
                Some(why) => clear.on_disabled_hover_text(why),
                None => clear.on_hover_text("Delete every collection's history, of every connection; asks first"),
            };
            if clear.clicked() {
                *action = Some(SettingsAction::ClearHistory);
            }
        });
        if let Some(Ok(s)) = scan {
            ui.add_space(4.0);
            if s.colls.is_empty() {
                ui.weak("Nothing is kept yet.");
            } else {
                self.history_list(ui, view, &s.colls, action);
            }
        }
    }

    /// Every collection with a history: connection (or "deleted profile"),
    /// database, collection, entries and size, with Open and Delete…. Only
    /// the rows in view are laid out, in a fixed height that scrolls; long
    /// names are cut short (in full on hover).
    fn history_list(&self, ui: &mut Ui, view: &HistoryView, rows: &[(Uuid, Namespace, u64, u64)], action: &mut Option<SettingsAction>) {
        let width = ui.available_width();
        let row_h = 24.0;
        let buttons = 112.0;
        let cols = [0.24, 0.20, 0.24, 0.13, 0.19].map(|f| f * (width - buttons).max(120.0));
        let (head, _) = ui.allocate_exact_size(vec2(width, row_h), Sense::hover());
        ui.painter().rect_filled(head, 0.0, theme::RAISED);
        let mut x = head.left();
        for (title, w) in ["Connection", "Database", "Collection", "Entries", "Size"].iter().zip(cols) {
            let clip = egui::Rect::from_x_y_ranges(x..=x + w - 4.0, head.y_range());
            ui.painter_at(clip).text(pos2(x + 6.0, head.center().y), Align2::LEFT_CENTER, *title, egui::FontId::proportional(13.0), theme::TEXT);
            x += w;
        }
        let unsaved = self.unsaved();
        let in_use = |p: &Uuid, ns: &Namespace| view.in_use.iter().any(|&(q, n)| q == *p && n == ns);
        let gap = ui.spacing().item_spacing.y;
        let height = rows.len().min(LIST_ROWS) as f32 * (row_h + gap);
        // Its own fixed height, whatever room is left: the body scrolls to it.
        let scroll = egui::ScrollArea::vertical().id_salt("settings-history").max_height(height).min_scrolled_height(height).auto_shrink([false, true]);
        scroll.show_rows(ui, row_h, rows.len(), |ui, range| {
            for (i, (profile, ns, entries, bytes)) in rows.iter().enumerate().skip(range.start).take(range.len()) {
                let (rect, _) = ui.allocate_exact_size(vec2(width, row_h), Sense::hover());
                if i % 2 == 1 {
                    ui.painter().rect_filled(rect, 0.0, Color32::from_white_alpha(6));
                }
                let (name, colour) = match view.names.get(profile) {
                    Some(n) => (n.as_str(), theme::TEXT),
                    None => (DELETED_PROFILE, theme::MUTED),
                };
                let (entries, bytes) = (thousands(*entries), mb(*bytes));
                let cells = [(name, colour), (ns.db.as_str(), theme::TEXT), (ns.coll.as_str(), theme::TEXT), (entries.as_str(), theme::TEXT), (bytes.as_str(), theme::MUTED)];
                let mut x = rect.left();
                for (col, ((text, colour), w)) in cells.iter().zip(cols).enumerate() {
                    let cell = egui::Rect::from_x_y_ranges(x + 6.0..=x + w - 4.0, rect.y_range());
                    let mut cell_ui = ui.new_child(UiBuilder::new().max_rect(cell).layout(Layout::left_to_right(Align::Center)).id_salt(("history-cell", profile, &ns.db, &ns.coll, col)));
                    cell_ui.set_clip_rect(cell.intersect(ui.clip_rect()));
                    theme::clipped_text(&mut cell_ui, text, *colour, 1, cell.width());
                    x += w;
                }
                let right = egui::Rect::from_x_y_ranges(x..=rect.right(), rect.y_range());
                let mut b = ui.new_child(UiBuilder::new().max_rect(right).layout(Layout::right_to_left(Align::Center)).id_salt(("history-row", profile, &ns.db, &ns.coll)));
                b.set_clip_rect(right.intersect(ui.clip_rect()));
                let blocked = match view.files.doing() {
                    Some(d) => Some(format!("Wait until this is done: {d}")),
                    None => in_use(profile, ns).then(|| HISTORY_IN_USE.to_string()),
                };
                let delete = b.add_enabled(blocked.is_none(), egui::Button::new("Delete…").small());
                let delete = match blocked {
                    Some(why) => delete.on_disabled_hover_text(why),
                    None => delete.on_hover_text("Delete this collection's history; asks first"),
                };
                if delete.clicked() {
                    *action = Some(SettingsAction::DeleteHistory(*profile, ns.clone()));
                }
                let open = b.add_enabled(!unsaved, egui::Button::new("Open").small());
                let open = if unsaved { open.on_disabled_hover_text(UNSAVED) } else { open.on_hover_text("Open its Collection History tab (closes Settings), even if the collection was dropped") };
                if open.clicked() {
                    *action = Some(SettingsAction::OpenHistory(*profile, ns.clone()));
                }
            }
        });
    }
}

/// How the Change Master Password dialog ended.
pub enum ChangeOutcome {
    Changed,
    Cancel,
    /// Another rongo window changed the master password or reset the vault
    /// meanwhile: nothing was written, and the vault must be unlocked again.
    VaultChanged,
}

const WRONG_CURRENT: &str = "Wrong current master password. Try again.";

/// Settings → Change Master Password…: the current password, and the new one
/// twice. The same secrets and Collection History key are sealed again under
/// the new one (`Vault::change_password`). That derives two keys, about a
/// second, on a thread of its own while the dialog shows a spinner and takes
/// no input.
pub struct ChangePasswordDialog {
    vault: Arc<Vault>,
    current: String,
    new: String,
    again: String,
    /// What went wrong last; trying again may help.
    error: Option<String>,
    working: Option<Job<CoreResult<()>>>,
    /// Which field to focus on the next frame.
    focus: Option<usize>,
}

impl ChangePasswordDialog {
    pub fn new(vault: Arc<Vault>) -> Self {
        Self { vault, current: String::new(), new: String::new(), again: String::new(), error: None, working: None, focus: Some(0) }
    }

    /// The change is running.
    pub fn busy(&self) -> bool {
        self.working.is_some()
    }

    /// Tests: what went wrong last, as the dialog shows it.
    #[cfg(test)]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Draws the dialog; how it ended, once it has.
    pub fn show(&mut self, ctx: &Context) -> Option<ChangeOutcome> {
        if let Some(done) = self.poll() {
            return Some(done);
        }
        let busy = self.busy();
        let (mut submit, mut cancel) = (false, false);
        let screen = ctx.content_rect().size();
        let modal = Modal::new(Id::new("change-master-password")).show(ctx, |ui| {
            ui.set_width(440.0f32.min(screen.x - 40.0).max(240.0));
            ui.heading(format!("{}  Change Master Password", icon::LOCK_KEY));
            ui.add_space(4.0);
            ui.label("Saved passwords, SSH secrets and the Collection History key stay as they are, sealed under the new master password. Other rongo windows then ask for the new one.");
            ui.add_space(8.0);
            let field = |ui: &mut Ui, label: &str, text: &mut String| {
                ui.label(label);
                ui.add_enabled(!busy, TextEdit::singleline(text).password(true).desired_width(f32::INFINITY))
            };
            let fields = [field(ui, "Current master password", &mut self.current), field(ui, "New master password", &mut self.new), field(ui, "Type the new one again", &mut self.again)];
            if let Some(i) = self.focus.take() {
                fields[i.min(2)].request_focus();
            }
            if let Some(e) = &self.error {
                ui.add_space(4.0);
                ui.label(RichText::new(e).color(theme::ERROR));
            }
            if busy {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Changing the master password…");
                });
            }
            ui.add_space(8.0);
            theme::dialog_buttons(ui, |ui| {
                submit |= ui.add_enabled(!busy, theme::primary_button("Change", false)).clicked();
                cancel |= ui.add_enabled(!busy, egui::Button::new("Cancel")).clicked();
            });
            // Enter moves on to the next field while that is empty, else changes.
            if !busy && ui.input(|i| i.key_pressed(Key::Enter)) {
                match [&self.new, &self.again].iter().zip(&fields).position(|(next, f)| f.lost_focus() && next.is_empty()) {
                    Some(i) => self.focus = Some(i + 1),
                    None => submit = true,
                }
            }
        });
        if busy {
            return None;
        }
        let typed = !(self.current.is_empty() && self.new.is_empty() && self.again.is_empty());
        if cancel || theme::may_close(&modal, typed) {
            return Some(ChangeOutcome::Cancel);
        }
        if submit {
            self.submit(ctx);
        }
        None
    }

    /// The finished change, if it finished: how the dialog ends, or what
    /// went wrong, shown on it.
    fn poll(&mut self) -> Option<ChangeOutcome> {
        let done = self.working.as_ref()?.poll()?;
        self.working = None;
        match done {
            Ok(Ok(())) => {
                self.current.clear();
                self.new.clear();
                self.again.clear();
                return Some(ChangeOutcome::Changed);
            }
            Ok(Err(CoreError::WrongPassword)) => {
                self.error = Some(WRONG_CURRENT.into());
                self.current.clear();
                self.focus = Some(0);
            }
            Ok(Err(CoreError::VaultChanged)) => return Some(ChangeOutcome::VaultChanged),
            Ok(Err(e)) => self.error = Some(format!("Couldn't change the master password: {e}")),
            Err(_) => self.error = Some("Couldn't change the master password: the key derivation stopped".into()),
        }
        None
    }

    /// Starts the change for what was typed, unless it is refused first.
    fn submit(&mut self, ctx: &Context) {
        if self.busy() {
            return;
        }
        let refused = if self.current.is_empty() {
            Some(("Type your current master password.".to_string(), 0))
        } else if let Err(e) = vault::check_new_password(&self.new) {
            Some((crate::vault_ui::sentence(&e), 1))
        } else if self.new != self.again {
            Some(("The two new passwords don't match.".to_string(), 2))
        } else {
            None
        };
        if let Some((why, field)) = refused {
            self.error = Some(why);
            self.focus = Some(field);
            return;
        }
        self.error = None;
        let (vault, old, new) = (self.vault.clone(), self.current.clone(), self.new.clone());
        self.working = Some(Job::start(ctx, "vault-password", move |_| vault.change_password(&old, &new)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_tolerate_missing_fields() {
        let history = HistorySettings { enabled: false, deletes: true, updates: false, keep_days: 7, max_mb: 50 };
        let s = Settings {
            open_collection_in: OpenIn::CollectionTab,
            timezone: DisplayTz::Offset(420),
            page_size: 200,
            history,
            check_updates: false,
            last_update_check: Some(rongo_core::bson::DateTime::from_millis(1_800_000_000_000)),
            skipped_version: crate::update::Version::parse("0.1.6"),
            latest_version: crate::update::Version::parse("0.1.7"),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), s);
        let old: Settings = serde_json::from_str(r#"{"page_size": 75}"#).unwrap();
        assert_eq!((old.page_size, old.open_collection_in, old.timezone), (75, OpenIn::IntelliShell, DisplayTz::Utc));
        assert_eq!(old.history, HistorySettings::default(), "a file from before Collection History");
        assert_eq!((old.check_updates, old.last_update_check, old.skipped_version, old.latest_version), (true, None, None, None), "a file from before the update check");
        let partial: Settings = serde_json::from_str(r#"{"history": {"keep_days": 3}}"#).unwrap();
        assert_eq!(partial.history, HistorySettings { keep_days: 3, ..HistorySettings::default() });
    }

    /// Collection History is on by default: deletes and updates, 30 days, 1 GB.
    #[test]
    fn history_is_on_by_default() {
        let h = Settings::default().history;
        assert_eq!((h.enabled, h.deletes, h.updates, h.keep_days, h.max_mb), (true, true, true, 30, 1024));
    }

    /// The update check is on by default; it has never asked, and Later hid nothing.
    #[test]
    fn the_update_check_is_on_by_default() {
        let s = Settings::default();
        assert_eq!((s.check_updates, s.last_update_check, s.skipped_version, s.latest_version), (true, None, None, None));
    }

    /// Settings written from two threads at once (a Save, and the update
    /// check's thread) never collide: the temporary file `write_atomically`
    /// uses is named for the process, not the thread. Every save works,
    /// the file reads back as one of them, and no temporary file is left.
    #[test]
    fn settings_saves_from_two_threads_never_collide() {
        let dir = std::env::temp_dir().join(format!("rongo-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let threads: Vec<_> = [75, 200]
            .into_iter()
            .map(|page_size| {
                let path = path.clone();
                std::thread::spawn(move || (0..100).map(|_| Settings { page_size, ..Settings::default() }.save(&path)).filter(Result::is_err).count())
            })
            .collect();
        let failed: usize = threads.into_iter().map(|t| t.join().unwrap()).sum();
        assert_eq!(failed, 0, "every save works");
        let (saved, notice) = Settings::load(&path);
        assert!(notice.is_none() && [75, 200].contains(&saved.page_size), "{notice:?} {}", saved.page_size);
        let names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["settings.json"], "no temporary file left");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A settings write that makes rongo's folder (the update check can, before
    /// the vault exists) makes it private, as the vault would; a folder that
    /// exists is left as it is.
    #[cfg(unix)]
    #[test]
    fn a_settings_write_makes_a_new_folder_private() {
        use std::os::unix::fs::PermissionsExt;
        let top = std::env::temp_dir().join(format!("rongo-test-{}", Uuid::new_v4()));
        let path = top.join("config").join("settings.json");
        Settings::default().save(&path).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let made = (mode(&top), mode(&top.join("config")));
        std::fs::set_permissions(top.join("config"), std::fs::Permissions::from_mode(0o755)).unwrap();
        Settings::default().save(&path).unwrap();
        let kept = mode(&top.join("config"));
        let _ = std::fs::remove_dir_all(&top);
        assert_eq!(made, (0o700, 0o700));
        assert_eq!(kept, 0o755, "an existing folder is left as it is");
    }

    /// A drawn text: what it says, where it is, and the part of the screen
    /// it may show in.
    struct Drawn {
        text: String,
        rect: egui::Rect,
        clip: egui::Rect,
    }

    /// The texts of the last of `frames` frames of the dialog at `size`.
    fn draw(ctx: &Context, d: &mut SettingsDialog, view: &HistoryView, size: egui::Vec2, frames: usize) -> Vec<Drawn> {
        let mut texts = Vec::new();
        for _ in 0..frames {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| _ = d.show(ui.ctx(), view));
            out.textures_delta.clear();
            texts = out.shapes.iter().filter_map(|s| match &s.shape {
                egui::Shape::Text(t) => Some(Drawn { text: t.galley.text().to_string(), rect: t.galley.rect.translate(t.pos.to_vec2()), clip: s.clip_rect }),
                _ => None,
            }).collect::<Vec<_>>();
        }
        texts
    }

    fn themed() -> Context {
        let ctx = Context::default();
        crate::theme::install(&ctx);
        ctx
    }

    fn no_history() -> HistoryFiles {
        HistoryFiles::new(std::env::temp_dir().join(format!("rongo-test-{}", Uuid::new_v4())).join("history"))
    }

    /// The Settings dialog has a Collection History section: the switch,
    /// deletes and updates, the days and the size limit.
    #[test]
    fn settings_dialog_has_a_history_switch() {
        let mut d = SettingsDialog::new(&Settings::default());
        let files = no_history();
        let view = HistoryView { files: &files, names: &HashMap::new(), in_use: &[] };
        let texts = draw(&themed(), &mut d, &view, egui::vec2(1000.0, 800.0), 3);
        for want in ["Collection History", HISTORY_SWITCH, "Deletes", "Updates", "Keep for", "days and at most", "MB; the oldest entries go first"] {
            assert!(texts.iter().any(|d| d.text == want), "{want}: {:?}", texts.iter().map(|d| &d.text).collect::<Vec<_>>());
        }
    }

    /// Settings has the update check's switch, on by default; turning it off
    /// is a change to save.
    #[test]
    fn settings_dialog_has_the_update_switch() {
        let mut d = SettingsDialog::new(&Settings::default());
        let files = no_history();
        let view = HistoryView { files: &files, names: &HashMap::new(), in_use: &[] };
        let texts = draw(&themed(), &mut d, &view, egui::vec2(1000.0, 800.0), 3);
        assert!(texts.iter().any(|d| d.text == UPDATE_SWITCH), "{:?}", texts.iter().map(|d| &d.text).collect::<Vec<_>>());
        assert!(d.draft.check_updates && !d.unsaved());
        d.draft.check_updates = false;
        assert!(d.unsaved());
    }

    /// At 760×720 with 200 histories of long names, Settings fits the
    /// window and keeps its place from frame to frame; the list scrolls in
    /// a fixed height (only the rows in view are drawn), and long names are
    /// cut short rather than widening the dialog.
    #[test]
    fn a_long_history_list_fits_and_scrolls() {
        let long = "ประวัติ_history_".repeat(12);
        let mut files = no_history();
        let colls: Vec<_> = (0..200u128).map(|i| (Uuid::from_u128(i % 3), Namespace::new(format!("db_{long}"), format!("coll_{i}_{long}")), 3, 4096 * i as u64)).collect();
        let by_profile = (0..3u128).map(|p| (Uuid::from_u128(p), 1 << 20)).collect();
        files.set_scan(Scan { by_profile, colls });
        let names = HashMap::from([(Uuid::from_u128(0), format!("conn_{long}"))]);
        let view = HistoryView { files: &files, names: &names, in_use: &[] };
        let mut d = SettingsDialog::new(&Settings::default());
        let size = egui::vec2(760.0, 720.0);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        let ctx = themed();
        let first = draw(&ctx, &mut d, &view, size, 3);
        let texts = draw(&ctx, &mut d, &view, size, 3);
        let at = |texts: &[Drawn], want: &str| texts.iter().find(|d| d.text == want).map(|d| d.rect).unwrap_or_else(|| panic!("{want} is drawn"));
        for want in ["Save", "Cancel", "Collection History", "Clear All History…"] {
            assert_eq!(at(&first, want), at(&texts, want), "{want} stays put");
        }
        assert!(screen.contains_rect(at(&texts, "Save")), "Save is on screen");
        for d in texts.iter().filter(|d| d.clip.is_positive()) {
            let shown = d.rect.intersect(d.clip);
            assert!(!shown.is_positive() || (shown.left() >= 0.0 && shown.right() <= size.x), "{} is inside the window: {shown:?}", d.text);
        }
        let rows: Vec<&Drawn> = texts.iter().filter(|d| d.text.starts_with("coll_")).collect();
        assert!((1..=LIST_ROWS + 2).contains(&rows.len()), "only the rows in view: {}", rows.len());
        let list = rows[0].clip.intersect(rows[rows.len() - 1].clip);
        assert!(list.height() <= (LIST_ROWS as f32 + 1.0) * 28.0, "a fixed height: {list:?}");
        assert!(rows.iter().all(|r| r.clip.width() < 200.0), "long names are cut to their column");
        assert!(texts.iter().any(|d| d.text == DELETED_PROFILE), "a profile with no name");
    }

    /// A settings file with values out of the dialog's ranges loads within
    /// them: 0 days or 0 MB would make the start-up purge remove all
    /// history, and the dialog would open with changes of its own (its
    /// fields clamp what they show), greying out Open.
    #[test]
    fn loading_brings_values_into_range() {
        let dir = std::env::temp_dir().join(format!("rongo-test-{}", Uuid::new_v4()));
        let path = dir.join("settings.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, r#"{"page_size": 0, "history": {"keep_days": 0, "max_mb": 0}}"#).unwrap();
        let low = Settings::load(&path).0;
        assert_eq!((low.page_size, low.history.keep_days, low.history.max_mb), (1, 1, 1));
        std::fs::write(&path, r#"{"page_size": 99999999, "history": {"keep_days": 99999, "max_mb": 99999999}}"#).unwrap();
        let high = Settings::load(&path).0;
        assert_eq!((high.page_size, high.history.keep_days, high.history.max_mb), (MAX_PAGE_SIZE, 3650, 1_048_576));
        assert_eq!(Settings::load(&dir.join("missing.json")), (Settings::default(), None), "none saved yet: nothing to say");

        let files = no_history();
        let view = HistoryView { files: &files, names: &HashMap::new(), in_use: &[] };
        for s in [low, high] {
            let mut d = SettingsDialog::new(&s);
            draw(&themed(), &mut d, &view, egui::vec2(1000.0, 800.0), 3);
            assert!(!d.unsaved(), "nothing changed by drawing it");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A settings file that can't be read (a typo made by hand, say) is not
    /// lost: it is kept as settings.json.unreadable (replacing an older one),
    /// the defaults are used, and the notice says so. Saving then writes a
    /// new file whole, through a temporary one, and leaves the kept file be.
    #[test]
    fn an_unreadable_settings_file_is_kept_aside() {
        let dir = std::env::temp_dir().join(format!("rongo-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let aside = dir.join("settings.json.unreadable");
        std::fs::write(&path, r#"{"page_size": 75,}"#).unwrap();
        std::fs::write(&aside, "an older one").unwrap();
        let (s, notice) = Settings::load(&path);
        assert_eq!(s, Settings::default());
        let notice = notice.expect("it says so");
        assert!(notice.contains("default settings") && notice.contains("settings.json.unreadable"), "{notice}");
        assert!(!path.exists(), "moved aside, so a save can't overwrite it");
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), r#"{"page_size": 75,}"#);

        let saved = Settings { page_size: 200, ..Settings::default() };
        saved.save(&path).unwrap();
        assert_eq!(Settings::load(&path), (saved, None));
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), r#"{"page_size": 75,}"#, "the kept file stays");
        let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["settings.json", "settings.json.unreadable"], "no temporary file left");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Saving sweeps temporary files other processes left by a crash
    /// (`settings.json.<pid>.tmp` a minute old or more); a fresh one may be
    /// another window's write in progress, and stays.
    #[test]
    fn saving_settings_sweeps_stale_temporary_files() {
        let dir = std::env::temp_dir().join(format!("rongo-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let stale = dir.join("settings.json.4000000000.tmp");
        let fresh = dir.join("settings.json.4000000001.tmp");
        std::fs::write(&stale, "left by a crash").unwrap();
        std::fs::write(&fresh, "being written").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        std::fs::File::options().write(true).open(&stale).unwrap().set_modified(old).unwrap();
        Settings::default().save(&path).unwrap();
        assert!(!stale.exists(), "swept");
        assert!(fresh.exists(), "a write maybe in progress: left");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Open waits while the dialog has unsaved changes (opening closes it).
    #[test]
    fn open_waits_for_unsaved_changes() {
        let mut d = SettingsDialog::new(&Settings::default());
        assert!(!d.unsaved());
        d.draft.history.keep_days = 7;
        assert!(d.unsaved());
        d.draft.history.keep_days = 30;
        d.tz = TzChoice::Local;
        assert!(d.unsaved(), "the time zone counts too");
    }

    /// Settings → Master password: Change Master Password… and Reset…. Reset
    /// closes Settings, so like Open it waits for unsaved changes.
    #[test]
    fn settings_offer_to_change_or_reset_the_master_password() {
        use egui_kittest::Harness;
        use egui_kittest::kittest::{NodeT, Queryable};
        struct Probe {
            dialog: SettingsDialog,
            files: HistoryFiles,
            actions: Vec<SettingsAction>,
        }
        let probe = Probe { dialog: SettingsDialog::new(&Settings::default()), files: no_history(), actions: Vec::new() };
        let mut h = Harness::builder().with_size(egui::vec2(1000.0, 900.0)).build_ui_state(
            |ui, p: &mut Probe| {
                crate::theme::install(ui.ctx());
                let names = HashMap::new();
                let view = HistoryView { files: &p.files, names: &names, in_use: &[] };
                if let Some(a) = p.dialog.show(ui.ctx(), &view) {
                    p.actions.push(a);
                }
            },
            probe,
        );
        h.run_steps(3);
        assert!(h.query_by_label("Master password").is_some());
        h.get_by_label("Change Master Password…").click();
        h.run_steps(1);
        assert!(matches!(h.state_mut().actions.pop(), Some(SettingsAction::ChangeMasterPassword)));
        h.get_by_label("Reset…").click();
        h.run_steps(1);
        assert!(matches!(h.state_mut().actions.pop(), Some(SettingsAction::ResetVault)));

        h.state_mut().dialog.draft.page_size = 75;
        h.run_steps(2);
        assert!(h.get_by_label("Reset…").accesskit_node().is_disabled(), "waits for unsaved changes");
        h.get_by_label("Reset…").click();
        h.run_steps(1);
        assert!(h.state().actions.is_empty());
    }

    /// The change dialog refuses before deriving anything: no current
    /// password, a new one too short, or two that don't match. The vault
    /// still opens with the password it had.
    #[test]
    fn change_password_refuses_before_deriving() {
        use crate::test_support::{TEST_PASSWORD, temp_vault};
        let (_t, vault) = temp_vault();
        let ctx = themed();
        let mut d = ChangePasswordDialog::new(vault.clone());
        let submit = |d: &mut ChangePasswordDialog, current: &str, new: &str, again: &str| {
            (d.current, d.new, d.again) = (current.into(), new.into(), again.into());
            d.submit(&ctx);
            assert!(d.working.is_none(), "nothing derived");
            d.error.clone().unwrap_or_default()
        };
        assert!(submit(&mut d, "", "a good password", "a good password").contains("current master password"));
        assert!(submit(&mut d, TEST_PASSWORD, "1234567", "1234567").contains("at least 8"));
        assert!(submit(&mut d, TEST_PASSWORD, "a good password", "a good passw0rd").contains("don't match"));
        assert!(Vault::unlock(vault.path().to_path_buf(), TEST_PASSWORD).is_ok());
    }

    /// A click outside Settings, or Esc, closes it only while nothing is
    /// changed; with unsaved changes only Cancel (or Save) closes it.
    #[test]
    fn settings_keep_unsaved_changes_on_an_outside_click_or_esc() {
        use egui_kittest::Harness;
        use egui_kittest::kittest::Queryable;
        struct Probe {
            dialog: SettingsDialog,
            files: HistoryFiles,
            closed: bool,
        }
        let open = || {
            let probe = Probe { dialog: SettingsDialog::new(&Settings::default()), files: no_history(), closed: false };
            let mut h = Harness::builder().with_size(egui::vec2(1000.0, 900.0)).build_ui_state(
                |ui, p: &mut Probe| {
                    crate::theme::install(ui.ctx());
                    let names = HashMap::new();
                    let view = HistoryView { files: &p.files, names: &names, in_use: &[] };
                    if let Some(SettingsAction::Close) = p.dialog.show(ui.ctx(), &view) {
                        p.closed = true;
                    }
                },
                probe,
            );
            h.run_steps(3);
            h
        };
        let mut h = open();
        crate::test_support::click_outside(&mut h);
        assert!(h.state().closed, "nothing changed: a click outside closes it");
        let mut h = open();
        h.key_press(Key::Escape);
        h.run_steps(2);
        assert!(h.state().closed, "nothing changed: Esc closes it");

        let mut h = open();
        h.state_mut().dialog.draft.page_size = 75;
        h.run_steps(2);
        crate::test_support::click_outside(&mut h);
        h.key_press(Key::Escape);
        h.run_steps(2);
        assert!(!h.state().closed, "unsaved changes kept");
        h.get_by_label("Cancel").click();
        h.run_steps(2);
        assert!(h.state().closed, "Cancel closes it");
    }

    /// Change Master Password closes on a click outside or Esc only while
    /// nothing is typed in it.
    #[test]
    fn change_password_keeps_typed_passwords_on_an_outside_click_or_esc() {
        use crate::test_support::temp_vault;
        use egui_kittest::Harness;
        use egui_kittest::kittest::Queryable;
        struct Probe {
            dialog: ChangePasswordDialog,
            cancelled: bool,
        }
        let (_t, vault) = temp_vault();
        let open = |typed: &str| {
            let mut dialog = ChangePasswordDialog::new(vault.clone());
            dialog.new = typed.into();
            let mut h = Harness::builder().with_size(egui::vec2(1000.0, 800.0)).build_ui_state(
                |ui, p: &mut Probe| {
                    crate::theme::install(ui.ctx());
                    if let Some(ChangeOutcome::Cancel) = p.dialog.show(ui.ctx()) {
                        p.cancelled = true;
                    }
                },
                Probe { dialog, cancelled: false },
            );
            h.run_steps(3);
            h
        };
        let mut h = open("");
        crate::test_support::click_outside(&mut h);
        assert!(h.state().cancelled, "nothing typed: a click outside closes it");
        let mut h = open("");
        h.key_press(Key::Escape);
        h.run_steps(2);
        assert!(h.state().cancelled, "nothing typed: Esc closes it");

        let mut h = open("half a new pass");
        crate::test_support::click_outside(&mut h);
        h.key_press(Key::Escape);
        h.run_steps(2);
        assert!(!h.state().cancelled, "typed passwords kept");
        h.get_by_label("Cancel").click();
        h.run_steps(2);
        assert!(h.state().cancelled, "Cancel closes it");
    }

    #[test]
    fn negative_offsets_keep_their_minutes() {
        let mut d = SettingsDialog::new(&Settings { timezone: DisplayTz::Offset(-150), ..Settings::default() });
        assert_eq!((d.offset_hours, d.offset_minutes), (-2, 30));
        assert_eq!(d.timezone(), DisplayTz::Offset(-150));
        d.offset_hours = 5;
        d.offset_minutes = 45;
        assert_eq!(d.timezone(), DisplayTz::Offset(345));
    }
}
