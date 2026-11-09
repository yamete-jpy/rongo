//! User preferences, saved as settings.json next to the connection profiles,
//! and the Settings dialog.

use std::path::PathBuf;

use egui::{Align, Context, Id, Layout, Modal, RichText};
use egui_phosphor::regular as icon;
use rongo_core::format::{self, DisplayTz};
use serde::{Deserialize, Serialize};

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
}

impl Default for Settings {
    fn default() -> Self {
        Self { open_collection_in: OpenIn::IntelliShell, timezone: DisplayTz::Utc, page_size: 50 }
    }
}

pub const MAX_PAGE_SIZE: i64 = 50_000;

impl Settings {
    fn path() -> Option<PathBuf> {
        directories::ProjectDirs::from("dev", "rongo", "rongo").map(|d| d.config_dir().join("settings.json"))
    }

    /// Saved settings, or the defaults when there are none (or they are unreadable).
    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path().ok_or("no configuration directory")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, json).map_err(|e| format!("{}: {e}", path.display()))
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
    draft: Settings,
    tz: TzChoice,
    offset_hours: i32,
    offset_minutes: i32,
}

pub enum SettingsAction {
    Save(Settings),
    Close,
}

impl SettingsDialog {
    pub fn new(current: &Settings) -> Self {
        let (tz, minutes) = match current.timezone {
            DisplayTz::Utc => (TzChoice::Utc, 420),
            DisplayTz::Local => (TzChoice::Local, 420),
            DisplayTz::Offset(m) => (TzChoice::Offset, m),
        };
        Self { draft: current.clone(), tz, offset_hours: minutes / 60, offset_minutes: (minutes % 60).abs() }
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

    pub fn show(&mut self, ctx: &Context) -> Option<SettingsAction> {
        let mut action = None;
        let modal = Modal::new(Id::new("settings")).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.heading(format!("{}  Settings", icon::GEAR));
            ui.add_space(8.0);

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
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button(RichText::new("Save").strong()).clicked() {
                        let mut s = self.draft.clone();
                        s.timezone = self.timezone();
                        action = Some(SettingsAction::Save(s));
                    }
                    if ui.button("Cancel").clicked() {
                        action = Some(SettingsAction::Close);
                    }
                });
            });
        });
        if modal.should_close() && action.is_none() {
            action = Some(SettingsAction::Close);
        }
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_tolerate_missing_fields() {
        let s = Settings { open_collection_in: OpenIn::CollectionTab, timezone: DisplayTz::Offset(420), page_size: 200 };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), s);
        let old: Settings = serde_json::from_str(r#"{"page_size": 75}"#).unwrap();
        assert_eq!((old.page_size, old.open_collection_in, old.timezone), (75, OpenIn::IntelliShell, DisplayTz::Utc));
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
