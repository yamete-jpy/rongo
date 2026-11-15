//! Dialogs that build one command: Add Index, Validator, Update/Delete,
//! Export, Import, and Edit Value. The app sends the command; on success the
//! dialog closes, on failure the error shows inside it.

use egui::{Align, Context, Id, Layout, Modal, RichText, Ui, vec2};
use egui_phosphor::regular as icon;
use rongo_core::bson::{Bson, Document, doc};
use rongo_core::format::{self, Style};
use rongo_core::{Command, ConnId, ExportFormat, ExportSpec, FindSpec, ImportFormat, Namespace, RequestId};

use crate::theme;

pub struct ToolDialog {
    pub conn: ConnId,
    /// View to refresh when the command succeeds.
    pub view: Option<u64>,
    /// Refresh this connection's tree on success.
    pub refresh_tree: bool,
    pub running: Option<RequestId>,
    pub error: Option<String>,
    pub kind: ToolKind,
    /// Something was typed into it: a click outside or Esc no longer closes it.
    typed: bool,
    /// The code field's laid-out text (one shows at a time), dropped with
    /// the dialog.
    layout: theme::CodeLayout,
}

/// "Apply predefined query" choices in the Update/Delete dialog.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Selected,
    CurrentQuery,
    All,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BulkTab {
    Query,
    Update,
}

/// Studio 3T's Update Documents / Delete Documents dialog.
pub struct BulkState {
    pub ns: Namespace,
    pub delete: bool,
    tab: BulkTab,
    selected: Vec<Bson>,
    /// "Selected Document(s)" as text, made once: formatting 50,000 _ids
    /// again for each use took a while, and a filter equal to it is known
    /// without parsing it (the _ids go out as they came, in `parsed`).
    selected_text: String,
    query: Document,
    filter: String,
    update: String,
    pipeline: bool,
    upsert: bool,
    multi: bool,
    confirmed: bool,
    validation: Option<Result<String, String>>,
    /// Whether `filter` parses to `{}`, for the text it was checked on:
    /// parsing a query of 50,000 ids every frame took 200 ms.
    empty_query: Option<(String, bool)>,
    /// "Keep the documents in Collection History", ticked by default. Only
    /// shown when `history` is `On`; the app asks before going on without
    /// history when it is wanted but unavailable.
    pub keep_history: bool,
    pub history: HistoryOffer,
}

/// What the Update/Delete dialog says about Collection History.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum HistoryOffer {
    /// History is off for this connection, collection or kind: nothing shown.
    #[default]
    Off,
    /// The "Keep the documents in Collection History" box.
    On,
    /// History is wanted but can't be kept; says why.
    Unavailable(String),
}

const EMPTY_UPDATE: &str = "{\n    $set : {\n        \n    }\n}";
const EMPTY_PIPELINE: &str = "[\n    {\n        $set : {\n            \n        }\n    }\n]";

impl BulkState {
    fn preset_text(&self, p: Preset) -> String {
        match p {
            Preset::Selected => self.selected_text.clone(),
            Preset::CurrentQuery => format::doc_to_shell(&self.query, Style::Editable),
            Preset::All => "{ }".into(),
        }
    }

    /// The query is "Selected Document(s)" as it was put in.
    fn is_selection(&self) -> bool {
        !self.selected.is_empty() && self.filter == self.selected_text
    }

    fn parsed(&self) -> Result<(Document, Option<rongo_core::UpdateBody>), String> {
        // "Selected Document(s)" as it was put in: the _ids exactly as they
        // came from the server, not read back from their text.
        let filter = if self.is_selection() {
            doc! { "_id": { "$in": self.selected.clone() } }
        } else {
            rongo_shell::parse_document(&self.filter).map_err(|e| format!("Query: {}", e.message))?
        };
        if self.delete {
            return Ok((filter, None));
        }
        let body = if self.pipeline {
            let stages = rongo_shell::parse_pipeline(&self.update).map_err(|e| format!("Update: {}", e.message))?;
            if stages.is_empty() {
                return Err("Update: the pipeline has no stages".into());
            }
            rongo_core::UpdateBody::Pipeline(stages)
        } else {
            let update = rongo_shell::parse_document(&self.update).map_err(|e| format!("Update: {}", e.message))?;
            if update.is_empty() || !update.keys().all(|k| k.starts_with('$')) {
                return Err("Update: use update operators, e.g. { $set: { status: \"done\" } }".into());
            }
            rongo_core::UpdateBody::Doc(update)
        };
        Ok((filter, Some(body)))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ExportScope {
    Query,
    Collection,
}

pub enum ToolKind {
    Index(Box<crate::index_form::IndexForm>),
    /// `loading`: the request reading the current validator, until it answers.
    Validator { ns: Namespace, loading: Option<RequestId>, text: String, level: String, action: String },
    Bulk(Box<BulkState>),
    /// `query_text`: the current query as the dialog shows it, made once
    /// and cut short (a query of 50,000 ids is a megabyte of text);
    /// `query_hover`: all of it on hover (empty when nothing was cut).
    Export { query: FindSpec, query_text: String, query_hover: String, scope: ExportScope, format: ExportFormat, path: String, fields: String },
    Import { db: String, coll: String, path: String, format: ImportFormat },
    Value { ns: Namespace, id: Bson, path: String, text: String },
    /// TTL and hidden are all MongoDB lets you change on an existing index.
    EditIndex { ns: Namespace, name: String, ttl: Option<String>, original_ttl: Option<String>, hidden: bool, original_hidden: bool },
}

pub enum ToolAction {
    Send(Command),
    Close,
}

impl ToolDialog {
    pub fn new(conn: ConnId, view: Option<u64>, kind: ToolKind) -> Self {
        let refresh_tree = matches!(kind, ToolKind::Index(_) | ToolKind::Import { .. } | ToolKind::EditIndex { .. });
        Self { conn, view, refresh_tree, running: None, error: None, kind, typed: false, layout: theme::CodeLayout::default() }
    }

    /// `fields` are the collection's field names for the field picker.
    pub fn index(conn: ConnId, ns: Namespace, fields: Vec<String>) -> Self {
        Self::new(conn, None, ToolKind::Index(Box::new(crate::index_form::IndexForm::new(ns, fields))))
    }

    /// Field names for the Add Index picker arrived.
    pub fn set_known_fields(&mut self, ns: &Namespace, fields: &[String]) {
        if let ToolKind::Index(f) = &mut self.kind
            && f.ns == *ns
        {
            f.set_known_fields(fields.to_vec());
        }
    }

    /// Whether the primary button can be pressed yet.
    fn ready(&self) -> bool {
        match &self.kind {
            ToolKind::Index(f) => f.ready(),
            // An empty query over every document waits for its box to be
            // ticked (known from the form, which parses the query only when
            // it changes).
            ToolKind::Bulk(b) => {
                let empty = b.empty_query.as_ref().is_some_and(|(text, empty)| *text == b.filter && *empty);
                !(empty && b.multi && !b.confirmed)
            }
            ToolKind::Export { path, .. } | ToolKind::Import { path, .. } => !path.trim().is_empty(),
            _ => true,
        }
    }

    /// `loading` is the request already sent for the current validator.
    pub fn validator(conn: ConnId, ns: Namespace, loading: RequestId) -> Self {
        Self::new(conn, None, ToolKind::Validator { ns, loading: Some(loading), text: "{ }".into(), level: "strict".into(), action: "error".into() })
    }

    /// `field` is the cell the dialog was opened from: its path and value
    /// fill in the `$set`, as Studio 3T does.
    pub fn bulk(conn: ConnId, ns: Namespace, view: Option<u64>, delete: bool, selected: Vec<Bson>, query: Document, field: Option<(String, Bson)>) -> Self {
        let update = match &field {
            Some((path, value)) => format!(
                "{{\n    $set : {{\n        {} : {}\n    }}\n}}",
                serde_json::to_string(path).unwrap_or_default(),
                format::to_shell(value, Style::Editable)
            ),
            None => EMPTY_UPDATE.to_string(),
        };
        let selected_text = if selected.is_empty() { String::new() } else { format::doc_to_shell(&doc! { "_id": { "$in": selected.clone() } }, Style::Editable) };
        let mut b = BulkState {
            ns,
            delete,
            tab: BulkTab::Query,
            selected,
            selected_text,
            query,
            filter: String::new(),
            update,
            pipeline: false,
            upsert: false,
            multi: true,
            confirmed: false,
            validation: None,
            empty_query: None,
            keep_history: true,
            history: HistoryOffer::Off,
        };
        let preset = if !b.selected.is_empty() { Preset::Selected } else if !b.query.is_empty() { Preset::CurrentQuery } else { Preset::All };
        b.filter = b.preset_text(preset);
        Self::new(conn, view, ToolKind::Bulk(Box::new(b)))
    }

    /// What the Update/Delete dialog offers about Collection History.
    pub fn offer_history(&mut self, offer: HistoryOffer) {
        if let ToolKind::Bulk(b) = &mut self.kind {
            b.history = offer;
        }
    }

    /// Whether what this dialog changes should be kept in Collection History
    /// (when history is on for it): the Update/Delete dialog's box; always
    /// for the others.
    pub fn keep_history(&self) -> bool {
        match &self.kind {
            ToolKind::Bulk(b) => b.keep_history,
            _ => true,
        }
    }

    pub fn export(conn: ConnId, query: FindSpec, from_view: bool) -> Self {
        let scope = if from_view && !query.filter.is_empty() { ExportScope::Query } else { ExportScope::Collection };
        let full = format::doc_to_shell(&query.filter, Style::Display).replace('\n', " ");
        let (query_text, query_hover) = match full.char_indices().nth(QUERY_SHOWN) {
            None => (full, String::new()),
            Some((cut, _)) => {
                let shown = format!("{} …", &full[..cut]);
                let hover = match full.char_indices().nth(QUERY_HOVER) {
                    None => full,
                    Some((cut, _)) => format!("{} … ({} more characters)", &full[..cut], crate::result_view::thousands(full[cut..].chars().count() as u64)),
                };
                (shown, hover)
            }
        };
        Self::new(conn, None, ToolKind::Export { query, query_text, query_hover, scope, format: ExportFormat::JsonLines, path: String::new(), fields: String::new() })
    }

    pub fn import(conn: ConnId, db: String, coll: String) -> Self {
        Self::new(conn, None, ToolKind::Import { db, coll, path: String::new(), format: ImportFormat::Auto })
    }

    pub fn value(conn: ConnId, view: Option<u64>, ns: Namespace, id: Bson, path: String, value: &Bson) -> Self {
        let text = format::value_to_shell_pretty(value, Style::Editable);
        Self::new(conn, view, ToolKind::Value { ns, id, path, text })
    }

    pub fn edit_index(conn: ConnId, ns: Namespace, spec: &Document) -> Self {
        let name = spec.get_str("name").unwrap_or_default().to_string();
        let ttl = spec.get("expireAfterSeconds").map(|v| match v {
            Bson::Int32(n) => n.to_string(),
            Bson::Int64(n) => n.to_string(),
            Bson::Double(f) => (*f as i64).to_string(),
            other => format::cell_text(other),
        });
        let hidden = spec.get_bool("hidden").unwrap_or(false);
        Self::new(conn, None, ToolKind::EditIndex { ns, name, original_ttl: ttl.clone(), ttl, hidden, original_hidden: hidden })
    }

    /// The current validator arrived in answer to `req`: taken only by the
    /// validator dialog still waiting for that answer (one cancelled before
    /// it loaded may have been followed by this one, on another collection).
    pub fn set_validator(&mut self, req: RequestId, reply: &Document) {
        if let ToolKind::Validator { loading, text, level, action, .. } = &mut self.kind
            && *loading == Some(req)
        {
            *loading = None;
            *text = format::doc_to_shell(&reply.get_document("validator").cloned().unwrap_or_default(), Style::Editable);
            *level = reply.get_str("validationLevel").unwrap_or("strict").into();
            *action = reply.get_str("validationAction").unwrap_or("error").into();
        }
    }

    /// Reading the current validator (`req`) failed: the dialog waiting for
    /// that answer starts from no validator and says why.
    pub fn validator_failed(&mut self, req: RequestId, why: &str) {
        if matches!(&self.kind, ToolKind::Validator { loading, .. } if *loading == Some(req)) {
            self.set_validator(req, &doc! {});
            self.error = Some(format!("Could not read the current validator: {why}"));
        }
    }

    pub fn success_message(&self) -> &'static str {
        match &self.kind {
            ToolKind::Index(_) => "Index created",
            ToolKind::Validator { .. } => "Validator saved",
            ToolKind::Bulk(b) if b.delete => "Delete finished",
            ToolKind::Bulk(_) => "Update finished",
            ToolKind::Export { .. } => "Export finished",
            ToolKind::Import { .. } => "Import finished",
            ToolKind::Value { .. } => "Value saved",
            ToolKind::EditIndex { .. } => "Index updated",
        }
    }

    pub fn show(&mut self, ctx: &Context) -> Option<ToolAction> {
        let mut action = None;
        let busy = self.running.is_some();
        let title = match &self.kind {
            ToolKind::Index(f) => format!("{}  Add index — {}", icon::LIGHTNING, f.ns),
            ToolKind::Validator { ns, .. } => format!("{}  Validator — {ns}", icon::SHIELD_CHECK),
            ToolKind::Bulk(b) if b.delete => format!("{}  Delete Documents — {}", icon::BROOM, b.ns),
            ToolKind::Bulk(b) => format!("{}  Update Documents — {}", icon::PENCIL_LINE, b.ns),
            ToolKind::Export { query, .. } => format!("{}  Export — {}", icon::EXPORT, query.ns),
            ToolKind::Import { db, .. } => format!("{}  Import into {db}", icon::DOWNLOAD_SIMPLE),
            ToolKind::Value { path, .. } => format!("{}  Edit Value — {path}", icon::PENCIL_SIMPLE),
            ToolKind::EditIndex { ns, name, .. } => format!("{}  Edit Index {name} — {ns}", icon::LIGHTNING),
        };
        let edits = theme::text_edits(ctx);
        let modal = Modal::new(Id::new("tool-dialog")).show(ctx, |ui| {
            ui.set_width(if matches!(self.kind, ToolKind::Index(_)) { 780.0 } else { 640.0 });
            ui.heading(title);
            ui.add_space(6.0);
            let primary = match &mut self.kind {
                ToolKind::Index(f) => {
                    f.ui(ui);
                    "Create index"
                }
                ToolKind::Validator { loading, text, level, action, .. } => validator_form(ui, loading.is_some(), text, level, action, &mut self.layout),
                ToolKind::Bulk(b) => bulk_form(ui, b, &mut self.layout),
                ToolKind::Export { scope, format, path, fields, query, query_text, query_hover, .. } => export_form(ui, query, (query_text, query_hover), scope, format, path, fields),
                ToolKind::Import { db, coll, path, format } => import_form(ui, db, coll, path, format),
                ToolKind::EditIndex { name, ttl, hidden, .. } => {
                    egui::Grid::new("edit-index").num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
                        if let Some(t) = ttl {
                            ui.label("Expire after (seconds)");
                            ui.add(egui::TextEdit::singleline(t).desired_width(160.0));
                            ui.end_row();
                        }
                        ui.label("Hidden");
                        ui.add_enabled(name != "_id_", egui::Checkbox::new(hidden, "keep it updated but unused by queries"));
                        ui.end_row();
                    });
                    ui.weak("To change keys, uniqueness or other options, drop the index and add it again.");
                    "Save"
                }
                ToolKind::Value { text, .. } => {
                    ui.weak("Shell syntax: \"text\", 42, Double(1), NumberLong(\"…\"), ISODate(\"…\"), ObjectId(\"…\"), { … }, [ … ]");
                    code_field(ui, text, 8, &mut self.layout);
                    "Save"
                }
            };
            if let Some(e) = &self.error {
                theme::error_note(ui, e);
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if busy {
                    ui.spinner();
                    ui.label("Working…");
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let danger = matches!(&self.kind, ToolKind::Bulk(b) if b.delete);
                    if ui.add_enabled(!busy && self.ready(), theme::primary_button(primary, danger)).clicked() {
                        match self.build() {
                            Ok(cmd) => {
                                self.error = None;
                                action = Some(ToolAction::Send(cmd));
                            }
                            Err(e) => self.error = Some(e),
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        action = Some(ToolAction::Close);
                    }
                });
            });
        });
        self.typed |= theme::text_edits(ctx) > edits;
        if theme::may_close(&modal, self.typed) && action.is_none() && !busy {
            action = Some(ToolAction::Close);
        }
        action
    }

    pub(crate) fn build(&mut self) -> Result<Command, String> {
        let doc_of = |label: &str, text: &str| rongo_shell::parse_document(text).map_err(|e| format!("{label}: {}", e.message));
        Ok(match &mut self.kind {
            ToolKind::Index(f) => f.build()?,
            ToolKind::Validator { ns, loading, text, level, action } => {
                if loading.is_some() {
                    return Err("Still loading the current validator".into());
                }
                Command::SetValidator { ns: ns.clone(), validator: doc_of("Validator", text)?, level: level.clone(), action: action.clone() }
            }
            ToolKind::Bulk(b) => {
                let (filter, body) = b.parsed()?;
                if filter.is_empty() && b.multi && !b.confirmed {
                    b.tab = BulkTab::Query;
                    return Err(format!("The query is empty, so this applies to every document in {}. Tick the box to confirm.", b.ns));
                }
                match body {
                    None => Command::DeleteWhere { ns: b.ns.clone(), filter, many: b.multi },
                    Some(update) => Command::Update { ns: b.ns.clone(), filter, update, many: b.multi, upsert: b.upsert },
                }
            }
            ToolKind::Export { query, scope, format, path, fields, .. } => {
                if path.trim().is_empty() {
                    return Err("Choose a file to export to".into());
                }
                let (filter, sort, projection) = match scope {
                    ExportScope::Query => (query.filter.clone(), query.sort.clone(), query.projection.clone()),
                    ExportScope::Collection => (Document::new(), None, None),
                };
                let fields: Vec<String> = fields.split(',').map(|f| f.trim().to_string()).filter(|f| !f.is_empty()).collect();
                Command::Export(Box::new(ExportSpec {
                    ns: query.ns.clone(),
                    filter,
                    sort,
                    projection,
                    format: *format,
                    path: path.trim().into(),
                    fields: (*format == ExportFormat::Csv && !fields.is_empty()).then_some(fields),
                }))
            }
            ToolKind::Import { db, coll, path, format } => {
                if path.trim().is_empty() {
                    return Err("Choose a file to import".into());
                }
                if coll.trim().is_empty() {
                    return Err("Enter the collection to import into".into());
                }
                Command::Import { ns: Namespace::new(db.clone(), coll.trim()), path: path.trim().into(), format: *format }
            }
            ToolKind::EditIndex { ns, name, ttl, original_ttl, hidden, original_hidden } => {
                let new_ttl = match ttl {
                    Some(t) if Some(t.trim()) != original_ttl.as_deref().map(str::trim) => {
                        Some(t.trim().parse::<i64>().ok().filter(|n| *n >= 0).ok_or("Expire after must be a whole number of seconds")?)
                    }
                    _ => None,
                };
                let new_hidden = (*hidden != *original_hidden).then_some(*hidden);
                if new_ttl.is_none() && new_hidden.is_none() {
                    return Err("Nothing changed".into());
                }
                Command::ModifyIndex { ns: ns.clone(), name: name.clone(), hidden: new_hidden, expire_after_seconds: new_ttl }
            }
            ToolKind::Value { ns, id, path, text } => {
                let value = rongo_shell::parse_value(text).map_err(|e| e.to_string())?;
                Command::UpdateMany { ns: ns.clone(), filter: doc! { "_id": id.clone() }, update: doc! { "$set": { path.clone(): value } } }
            }
        })
    }
}

/// Text past this size isn't put in an editor unless asked: egui lays out
/// all of it every frame (20 ms for a query of 50,000 ids).
const MAX_SHOWN: usize = 200_000;

/// How much of the current query Export shows, in characters.
const QUERY_SHOWN: usize = 120;

/// How much of it its hover shows: past this a tooltip would be a
/// megabyte of text laid out while the pointer rests there.
const QUERY_HOVER: usize = 10_000;

/// `layout` keeps the laid-out text between frames (while the dialog waits
/// on its command the spinner repaints every frame): coloured up to
/// theme::MAX_HIGHLIGHT, plain beyond.
fn code_field(ui: &mut Ui, text: &mut String, rows: usize, layout: &mut theme::CodeLayout) {
    let show_id = ui.id().with("show-large-text");
    if text.len() > MAX_SHOWN && !ui.data(|d| d.get_temp::<bool>(show_id).unwrap_or(false)) {
        egui::Frame::new().fill(theme::BG).inner_margin(6.0).corner_radius(4.0).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            let preview: String = text.chars().take(120).collect::<String>().replace('\n', " ");
            ui.label(RichText::new(format!("{preview} …")).font(theme::mono(13.0)).color(theme::MUTED));
            ui.horizontal(|ui| {
                ui.weak(format!("{} of text, used as it is.", format::human_bytes(text.len() as u64)));
                if ui.button("Show and edit").on_hover_text("Large text makes the dialog slow while it is shown").clicked() {
                    ui.data_mut(|d| d.insert_temp(show_id, true));
                }
            });
        });
        return;
    }
    let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| layout.galley(ui, buf.as_str(), theme::mono(13.0), wrap);
    egui::Frame::new().fill(theme::BG).inner_margin(6.0).corner_radius(4.0).show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt(ui.next_auto_id()).max_height(rows as f32 * 20.0).show(ui, |ui| {
            ui.add(egui::TextEdit::multiline(text).code_editor().frame(egui::Frame::NONE).desired_width(f32::INFINITY).desired_rows(rows).layouter(&mut layouter));
        });
    });
}

fn validator_form(ui: &mut Ui, loading: bool, text: &mut String, level: &mut String, action: &mut String, layout: &mut theme::CodeLayout) -> &'static str {
    if loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Loading the current validator…");
        });
        return "Save";
    }
    ui.label("Validator query, e.g. { $jsonSchema: { required: [\"name\"] } } — leave { } to remove validation");
    code_field(ui, text, 12, layout);
    ui.horizontal(|ui| {
        ui.label("Validation level");
        egui::ComboBox::from_id_salt("vlevel").selected_text(level.as_str()).show_ui(ui, |ui| {
            for l in ["off", "strict", "moderate"] {
                ui.selectable_value(level, l.to_string(), l);
            }
        });
        ui.add_space(16.0);
        ui.label("Validation action");
        egui::ComboBox::from_id_salt("vaction").selected_text(action.as_str()).show_ui(ui, |ui| {
            for a in ["error", "warn"] {
                ui.selectable_value(action, a.to_string(), a);
            }
        });
    });
    "Save"
}

fn bulk_form(ui: &mut Ui, b: &mut BulkState, layout: &mut theme::CodeLayout) -> &'static str {
    let verb = if b.delete { "Delete" } else { "Update" };
    if !b.delete {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut b.tab, BulkTab::Query, "Query");
            ui.selectable_value(&mut b.tab, BulkTab::Update, "Update");
        });
        ui.separator();
    }
    match b.tab {
        BulkTab::Query => {
            ui.label(format!("{verb} document(s) matching the query below:"));
            code_field(ui, &mut b.filter, 9, layout);
            ui.horizontal(|ui| {
                ui.label("Apply predefined query:");
                let n = b.selected.len();
                let mut pick = None;
                egui::ComboBox::from_id_salt("preset").width(220.0).selected_text("Choose…").show_ui(ui, |ui| {
                    if ui.add_enabled(n > 0, egui::Button::selectable(false, format!("Selected Document(s) ({n})"))).clicked() {
                        pick = Some(Preset::Selected);
                    }
                    if ui.selectable_label(false, "Current Query").clicked() {
                        pick = Some(Preset::CurrentQuery);
                    }
                    if ui.selectable_label(false, "All Documents").clicked() {
                        pick = Some(Preset::All);
                    }
                });
                if let Some(p) = pick {
                    b.filter = b.preset_text(p);
                    b.validation = None;
                }
            });
        }
        BulkTab::Update => {
            ui.label("Update command to be run on found document(s):");
            code_field(ui, &mut b.update, 9, layout);
            ui.label("Update mode:");
            let before = b.pipeline;
            ui.radio_value(&mut b.pipeline, false, "Simple Update with Update Document");
            ui.radio_value(&mut b.pipeline, true, "Update with Aggregation Pipeline");
            // Swap the empty template when the mode changes.
            if before != b.pipeline {
                if b.pipeline && b.update.trim() == EMPTY_UPDATE.trim() {
                    b.update = EMPTY_PIPELINE.into();
                } else if !b.pipeline && b.update.trim() == EMPTY_PIPELINE.trim() {
                    b.update = EMPTY_UPDATE.into();
                }
            }
        }
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if !b.delete {
            ui.checkbox(&mut b.upsert, "Upsert").on_hover_text("Insert a document when nothing matches the query");
            ui.add_space(12.0);
        }
        ui.checkbox(&mut b.multi, "Multi").on_hover_text(format!("{verb} every matching document, not just the first"));
        ui.add_space(12.0);
        if ui.button("Validate JSON").clicked() {
            b.validation = Some(b.parsed().map(|(f, _)| {
                if f.is_empty() { "Valid. The query is empty: it matches every document.".to_string() } else { "Valid.".to_string() }
            }));
        }
    });
    match &b.validation {
        Some(Ok(m)) => {
            ui.colored_label(theme::OK, m);
        }
        Some(Err(e)) => {
            ui.colored_label(theme::ERROR, e);
        }
        None => {}
    }
    let empty_query = match &b.empty_query {
        Some((text, empty)) if *text == b.filter => *empty,
        _ => {
            // The selected _ids aren't parsed (200 ms for 50,000): an `$in`
            // of a selection is never empty.
            let empty = !b.is_selection() && rongo_shell::parse_document(&b.filter).is_ok_and(|f| f.is_empty());
            b.empty_query = Some((b.filter.clone(), empty));
            empty
        }
    };
    if empty_query && b.multi {
        ui.checkbox(&mut b.confirmed, RichText::new(format!("The query is empty: {} every document in {}", verb.to_lowercase(), b.ns)).color(theme::WARN));
    }
    match &b.history {
        HistoryOffer::Off => {}
        HistoryOffer::On => {
            ui.checkbox(&mut b.keep_history, "Keep the documents in Collection History")
                .on_hover_text(format!("Save the documents as they are now, encrypted on this computer, before they are {}d", verb.to_lowercase()));
        }
        // Wrapped to the dialog's width (a reason can be long).
        HistoryOffer::Unavailable(why) => {
            ui.colored_label(theme::WARN, format!("Collection History is unavailable: {why}"));
        }
    }
    verb
}

/// `query_text`: the current query as shown, and in full for its hover.
fn export_form(ui: &mut Ui, query: &FindSpec, query_text: (&str, &str), scope: &mut ExportScope, format: &mut ExportFormat, path: &mut String, fields: &mut String) -> &'static str {
    ui.label("Export");
    let (shown, hover) = query_text;
    ui.add_enabled_ui(!query.filter.is_empty() || query.sort.is_some(), |ui| {
        let r = ui.radio_value(scope, ExportScope::Query, format!("documents matching the current query: {shown}"));
        if !hover.is_empty() {
            r.on_hover_text(hover);
        }
    });
    ui.radio_value(scope, ExportScope::Collection, "the whole collection");
    ui.add_space(6.0);
    egui::Grid::new("export").num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
        ui.label("Format");
        ui.horizontal(|ui| {
            ui.radio_value(format, ExportFormat::JsonLines, "JSON (one document per line)");
            ui.radio_value(format, ExportFormat::JsonArray, "JSON array");
            ui.radio_value(format, ExportFormat::Csv, "CSV");
        });
        ui.end_row();
        if *format == ExportFormat::Csv {
            ui.label("Fields");
            ui.add(egui::TextEdit::singleline(fields).hint_text("comma separated, dotted paths allowed — empty: detect").desired_width(420.0));
            ui.end_row();
        }
        ui.label("File");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(path).desired_width(380.0));
            if ui.button(format!("{}  Browse…", icon::FOLDER_OPEN)).clicked() {
                let ext = match format {
                    ExportFormat::Csv => "csv",
                    _ => "json",
                };
                if let Some(p) = rfd::FileDialog::new().set_file_name(format!("{}.{ext}", query.ns.coll)).save_file() {
                    *path = p.display().to_string();
                }
            }
        });
        ui.end_row();
    });
    ui.weak("JSON uses relaxed Extended JSON, so ObjectId, Date and Long come back with their types on import.");
    "Export"
}

fn import_form(ui: &mut Ui, db: &str, coll: &mut String, path: &mut String, format: &mut ImportFormat) -> &'static str {
    egui::Grid::new("import").num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
        ui.label("File");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(path).desired_width(380.0));
            if ui.button(format!("{}  Browse…", icon::FOLDER_OPEN)).clicked()
                && let Some(p) = rfd::FileDialog::new().add_filter("JSON / CSV", &["json", "jsonl", "ndjson", "csv"]).pick_file()
            {
                *path = p.display().to_string();
                if coll.is_empty()
                    && let Some(stem) = p.file_stem()
                {
                    *coll = stem.to_string_lossy().to_string();
                }
            }
        });
        ui.end_row();
        ui.label("Format");
        egui::ComboBox::from_id_salt("import-format")
            .selected_text(match format {
                ImportFormat::Auto => "Detect from file",
                ImportFormat::JsonArray => "JSON array",
                ImportFormat::JsonLines => "JSON (one document per line)",
                ImportFormat::Csv => "CSV (first row is field names)",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(format, ImportFormat::Auto, "Detect from file");
                ui.selectable_value(format, ImportFormat::JsonArray, "JSON array");
                ui.selectable_value(format, ImportFormat::JsonLines, "JSON (one document per line)");
                ui.selectable_value(format, ImportFormat::Csv, "CSV (first row is field names)");
            });
        ui.end_row();
        ui.label("Into collection");
        ui.horizontal(|ui| {
            ui.label(format!("{db}."));
            ui.add(egui::TextEdit::singleline(coll).desired_width(300.0));
        });
        ui.end_row();
    });
    ui.weak("A missing collection is created. CSV numbers and true/false become typed values; dotted headers (profile.country) become nested fields.");
    ui.weak("Every document that can go in does: one whose _id is already in the collection is skipped, and the result says how many were inserted, skipped and refused.");
    "Import"
}

#[cfg(test)]
mod tests {
    use super::*;
    use rongo_core::bson::oid::ObjectId;
    use rongo_core::UpdateBody;

    fn ids(n: usize) -> Vec<Bson> {
        (0..n).map(|_| Bson::ObjectId(ObjectId::new())).collect()
    }

    fn bulk(dialog: &mut ToolDialog) -> &mut BulkState {
        match &mut dialog.kind {
            ToolKind::Bulk(b) => b,
            _ => unreachable!(),
        }
    }

    /// Like Studio 3T: the query lists the selected _ids and the update sets
    /// the clicked field to its current value, type included.
    #[test]
    fn update_dialog_prefills_selection_and_clicked_field() {
        let sel = ids(3);
        let field = Some(("chatId".to_string(), Bson::Int64(-1_002_322_629_941)));
        let mut d = ToolDialog::bulk(ConnId(uuid::Uuid::nil()), Namespace::new("db", "c"), Some(1), false, sel.clone(), Document::new(), field);
        let Command::Update { filter, update, many, upsert, .. } = d.build().unwrap() else { panic!() };
        assert_eq!(filter, doc! { "_id": { "$in": sel } });
        assert_eq!(update, UpdateBody::Doc(doc! { "$set": { "chatId": -1_002_322_629_941_i64 } }));
        assert!(many && !upsert, "Multi on and Upsert off by default");

        let b = bulk(&mut d);
        b.multi = false;
        b.upsert = true;
        let Command::Update { many, upsert, .. } = d.build().unwrap() else { panic!() };
        assert!(!many && upsert);
    }

    #[test]
    fn pipeline_mode_and_validation() {
        let mut d = ToolDialog::bulk(ConnId(uuid::Uuid::nil()), Namespace::new("db", "c"), None, false, ids(1), Document::new(), None);
        let b = bulk(&mut d);
        b.pipeline = true;
        b.update = "[{ $set: { total: { $add: [\"$a\", 1] } } }]".into();
        let Command::Update { update: UpdateBody::Pipeline(stages), .. } = d.build().unwrap() else { panic!() };
        assert_eq!(stages.len(), 1);
        let b = bulk(&mut d);
        b.pipeline = false;
        b.update = "{ name: 1 }".into();
        assert!(d.build().unwrap_err().contains("update operators"));
    }

    /// The main button waits until it can work: Delete over every document
    /// until its box is ticked, Export until a file is chosen.
    #[test]
    fn main_button_waits_until_ready() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let show = |d: &mut ToolDialog| {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| _ = d.show(ui.ctx()));
            out.textures_delta.clear();
        };
        let mut d = ToolDialog::bulk(ConnId(uuid::Uuid::nil()), Namespace::new("db", "c"), None, true, Vec::new(), Document::new(), None);
        show(&mut d);
        assert!(!d.ready(), "an empty query over every document needs the box first");
        bulk(&mut d).confirmed = true;
        assert!(d.ready());
        bulk(&mut d).confirmed = false;
        bulk(&mut d).filter = "{ code: 1 }".into();
        show(&mut d);
        assert!(d.ready(), "a real query needs no box");

        let mut e = ToolDialog::export(ConnId(uuid::Uuid::nil()), FindSpec::all(Namespace::new("db", "c"), 50), false);
        assert!(!e.ready(), "no file yet");
        if let ToolKind::Export { path, .. } = &mut e.kind {
            *path = "/tmp/x.json".into();
        }
        assert!(e.ready());
    }

    #[test]
    fn empty_query_needs_confirmation_and_delete_respects_multi() {
        let mut d = ToolDialog::bulk(ConnId(uuid::Uuid::nil()), Namespace::new("db", "c"), None, true, Vec::new(), Document::new(), None);
        assert!(d.build().unwrap_err().contains("every document"));
        bulk(&mut d).confirmed = true;
        assert!(matches!(d.build().unwrap(), Command::DeleteWhere { many: true, .. }));
        // Deleting a single match of an empty query needs no confirmation.
        let b = bulk(&mut d);
        b.confirmed = false;
        b.multi = false;
        assert!(matches!(d.build().unwrap(), Command::DeleteWhere { many: false, .. }));
    }

    /// The Delete dialog for 50,000 selected documents costs about what it
    /// does for 10 per frame: the query isn't parsed or tokenized again
    /// every frame.
    #[test]
    fn a_huge_selection_does_not_slow_every_frame() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let frame_ms = |n: usize| {
            let mut d = ToolDialog::bulk(ConnId(uuid::Uuid::nil()), Namespace::new("db", "c"), None, true, ids(n), Document::new(), None);
            let mut run = || {
                let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                    d.show(ui.ctx());
                });
                out.textures_delta.clear();
            };
            run();
            run();
            // The fastest of several rounds: on a loaded machine another
            // process can slow any one round, but not every one.
            (0..7)
                .map(|_| {
                    let t = std::time::Instant::now();
                    for _ in 0..5 {
                        run();
                    }
                    t.elapsed().as_secs_f64() * 1000.0 / 5.0
                })
                .fold(f64::INFINITY, f64::min)
        };
        let small = frame_ms(10);
        let big = frame_ms(50_000);
        assert!(big < small * 3.0 + 15.0, "{big:.1} ms per frame with 50,000 ids vs {small:.1} ms with 10");
    }

    /// Milliseconds to open (draw the first frame of) the Delete dialog for
    /// `n` selected documents.
    fn bulk_open_ms(ctx: &egui::Context, n: usize) -> f64 {
        (0..3)
            .map(|_| {
                let mut d = ToolDialog::bulk(ConnId(uuid::Uuid::nil()), Namespace::new("db", "c"), None, true, ids(n), Document::new(), None);
                let t = std::time::Instant::now();
                let mut out = ctx.run_ui(egui::RawInput::default(), |ui| _ = d.show(ui.ctx()));
                out.textures_delta.clear();
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                assert!(d.ready(), "a selection needs no box to tick");
                ms
            })
            .fold(f64::INFINITY, f64::min)
    }

    /// Milliseconds per frame of Export opened from a view filtered on `n` _ids.
    fn export_frame_ms(ctx: &egui::Context, n: usize) -> f64 {
        let mut spec = FindSpec::all(Namespace::new("db", "c"), 50);
        spec.filter = doc! { "_id": { "$in": ids(n) } };
        let mut d = ToolDialog::export(ConnId(uuid::Uuid::nil()), spec, true);
        let mut run = || {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| _ = d.show(ui.ctx()));
            out.textures_delta.clear();
        };
        run();
        run();
        (0..5)
            .map(|_| {
                let t = std::time::Instant::now();
                for _ in 0..3 {
                    run();
                }
                t.elapsed().as_secs_f64() * 1000.0 / 3.0
            })
            .fold(f64::INFINITY, f64::min)
    }

    /// Opening the Delete dialog for 50,000 selected documents takes about
    /// as long as for 10: the prefilled query of their _ids isn't parsed
    /// (it is known not to be empty, and the _ids go out as they came).
    #[test]
    fn opening_a_huge_selection_is_quick() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let small = bulk_open_ms(&ctx, 10);
        let big = bulk_open_ms(&ctx, 50_000);
        assert!(big < small * 3.0 + 15.0, "{big:.1} ms to open with 50,000 ids vs {small:.1} ms with 10");
    }

    /// Export opened from a view filtered on 50,000 _ids costs about what it
    /// does for a small query per frame: the query's text is made once,
    /// and shown cut short.
    #[test]
    fn export_with_a_huge_query_stays_cheap() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let small = export_frame_ms(&ctx, 2);
        let big = export_frame_ms(&ctx, 50_000);
        assert!(big < small * 3.0 + 5.0, "{big:.1} ms per frame with 50,000 ids vs {small:.1} ms with 2");
    }

    /// Export shows the current query cut short, and the whole of it on
    /// hover (up to `QUERY_HOVER` characters: a query of 50,000 ids would
    /// be a megabyte of tooltip).
    #[test]
    fn export_shows_the_whole_query_on_hover() {
        use egui_kittest::kittest::Queryable;
        let mut spec = FindSpec::all(Namespace::new("db", "c"), 50);
        spec.filter = doc! { "_id": { "$in": (0..60).collect::<Vec<i32>>() }, "status": "the last field" };
        let mut h = harness(ToolDialog::export(ConnId(uuid::Uuid::nil()), spec, true), egui::vec2(1000.0, 800.0));
        let texts = |h: &egui_kittest::Harness<'_, Probe>| h.output().shapes.iter().filter_map(|s| match &s.shape {
            egui::Shape::Text(t) => Some(t.galley.text().to_string()),
            _ => None,
        }).collect::<Vec<_>>();
        let label = texts(&h).into_iter().find(|t| t.starts_with("documents matching the current query")).expect("the query's choice");
        assert!(label.ends_with('…') && !label.contains("the last field"), "cut short: {label}");
        h.get_by_label_contains("documents matching the current query").hover();
        h.run_steps(6);
        assert!(texts(&h).iter().any(|t| t.contains("59") && t.contains("the last field")), "the whole query on hover: {:?}", texts(&h));

        let mut huge = FindSpec::all(Namespace::new("db", "c"), 50);
        huge.filter = doc! { "_id": { "$in": (0..50_000).collect::<Vec<i32>>() } };
        let ToolKind::Export { query_hover, .. } = ToolDialog::export(ConnId(uuid::Uuid::nil()), huge, true).kind else { unreachable!() };
        let shown = query_hover.chars().count();
        assert!(shown <= QUERY_HOVER + 40 && query_hover.ends_with("more characters)"), "{shown} characters: …{}", &query_hover[query_hover.len() - 60..]);
    }

    /// Prints the tool dialogs' timings (run with --release --ignored --nocapture).
    #[test]
    #[ignore = "prints timings"]
    fn report_tool_dialog_times() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        for n in [10, 50_000] {
            println!("Delete dialog, {n} ids: {:.2} ms to open", bulk_open_ms(&ctx, n));
        }
        for n in [2, 50_000] {
            println!("Export, a query of {n} ids: {:.2} ms per frame", export_frame_ms(&ctx, n));
        }
    }

    #[test]
    fn predefined_queries() {
        let sel = ids(2);
        let mut d = ToolDialog::bulk(ConnId(uuid::Uuid::nil()), Namespace::new("db", "c"), None, false, sel.clone(), doc! { "status": "x" }, None);
        let b = bulk(&mut d);
        assert_eq!(rongo_shell::parse_document(&b.preset_text(Preset::Selected)).unwrap(), doc! { "_id": { "$in": sel } });
        assert_eq!(rongo_shell::parse_document(&b.preset_text(Preset::CurrentQuery)).unwrap(), doc! { "status": "x" });
        assert_eq!(rongo_shell::parse_document(&b.preset_text(Preset::All)).unwrap(), doc! {});
    }

    /// "Selected Document(s)", left as it was put in, deletes or updates
    /// exactly the selected _ids: they go to the server as they came, not
    /// through the text (where an _id holding e.g. `{ $date: … }` would
    /// change type).
    #[test]
    fn selected_documents_use_their_ids_as_they_are() {
        let odd = Bson::Document(doc! { "$date": "not a date" });
        let selected = vec![odd.clone(), Bson::Int32(7)];
        let mut d = ToolDialog::bulk(ConnId(uuid::Uuid::nil()), Namespace::new("db", "c"), None, true, selected.clone(), Document::new(), None);
        match d.build() {
            Ok(Command::DeleteWhere { filter, .. }) => assert_eq!(filter, doc! { "_id": { "$in": selected } }),
            other => panic!("{:?}", other.map(|_| "another command")),
        }
        // Edited, the text is what counts.
        bulk(&mut d).filter = "{ _id: 7 }".into();
        match d.build() {
            Ok(Command::DeleteWhere { filter, .. }) => assert_eq!(filter, doc! { "_id": 7 }),
            other => panic!("{:?}", other.map(|_| "another command")),
        }
    }

    /// A tool dialog in a harness, noting when it asks to close.
    struct Probe {
        dialog: ToolDialog,
        closed: bool,
    }

    fn harness(dialog: ToolDialog, size: egui::Vec2) -> egui_kittest::Harness<'static, Probe> {
        let mut h = egui_kittest::Harness::builder().with_size(size).build_ui_state(
            |ui, p: &mut Probe| {
                crate::theme::install(ui.ctx());
                if let Some(ToolAction::Close) = p.dialog.show(ui.ctx()) {
                    p.closed = true;
                }
            },
            Probe { dialog, closed: false },
        );
        h.run_steps(3);
        h
    }

    /// A click outside a tool dialog, or Esc, closes it only while nothing
    /// was typed into it (Edit Value's text, Add Index's name); after
    /// typing only Cancel closes it.
    #[test]
    fn tool_dialogs_keep_typed_work_on_an_outside_click_or_esc() {
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;
        let conn = ConnId(uuid::Uuid::nil());
        let ns = Namespace::new("db", "c");
        let value = || ToolDialog::value(conn, None, ns.clone(), Bson::Int32(1), "name".into(), &Bson::String("a".into()));
        let index = || ToolDialog::index(conn, ns.clone(), Vec::new());
        let size = egui::vec2(1000.0, 900.0);
        for (what, make, role) in [("Edit Value", &value as &dyn Fn() -> ToolDialog, Role::MultilineTextInput), ("Add Index", &index, Role::TextInput)] {
            let mut h = harness(make(), size);
            crate::test_support::click_outside(&mut h);
            assert!(h.state().closed, "{what}, nothing typed: a click outside closes it");
            let mut h = harness(make(), size);
            h.key_press(egui::Key::Escape);
            h.run_steps(2);
            assert!(h.state().closed, "{what}, nothing typed: Esc closes it");

            let mut h = harness(make(), size);
            h.query_all_by_role(role).next().expect("a text field").focus();
            h.run_steps(2);
            h.query_all_by_role(role).next().unwrap().type_text("x");
            h.run_steps(2);
            crate::test_support::click_outside(&mut h);
            h.key_press(egui::Key::Escape);
            h.run_steps(2);
            assert!(!h.state().closed, "{what}: typed work kept");
            h.get_by_label("Cancel").click();
            h.run_steps(2);
            assert!(h.state().closed, "{what}: Cancel closes it");
        }
    }

    /// On the smallest window the app allows, a long error (a validator
    /// listing every rule a document broke) stays a few lines, whole on
    /// hover, and the dialog's buttons stay on screen.
    #[test]
    fn a_long_error_keeps_the_buttons_on_screen() {
        use egui_kittest::kittest::Queryable;
        let size = egui::vec2(960.0, 600.0);
        let mut d = ToolDialog::bulk(ConnId(uuid::Uuid::nil()), Namespace::new("db", "c"), None, true, ids(3), Document::new(), None);
        d.error = Some(format!("server error 121: Document failed validation {}", crate::test_support::long_driver_error().repeat(3)));
        let mut h = harness(d, size);
        h.run_steps(3);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        for label in ["Delete", "Cancel"] {
            let r = h.get_by_label(label).rect();
            assert!(screen.contains_rect(r), "{label} at {r:?}");
        }
    }
}
