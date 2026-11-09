//! The Add Index dialog's form, laid out like Studio 3T's: an index name,
//! then Fields / Options / Text options / Geo options / Collation tabs, and
//! Create in background plus a JSON preview at the bottom.

use egui::{RichText, Ui, vec2};
use egui_phosphor::regular as icon;
use rongo_core::bson::{Bson, Document, doc};
use rongo_core::format::{self, Style};
use rongo_core::{Command, Namespace};

use crate::{highlight, theme};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyType {
    Asc,
    Desc,
    Text,
    Sphere,
    Flat,
    Hashed,
}

impl KeyType {
    const ALL: [KeyType; 6] = [KeyType::Asc, KeyType::Desc, KeyType::Text, KeyType::Sphere, KeyType::Flat, KeyType::Hashed];

    fn label(self) -> &'static str {
        match self {
            KeyType::Asc => "1 (ascending)",
            KeyType::Desc => "-1 (descending)",
            KeyType::Text => "text",
            KeyType::Sphere => "2dsphere",
            KeyType::Flat => "2d",
            KeyType::Hashed => "hashed",
        }
    }

    fn value(self) -> Bson {
        match self {
            KeyType::Asc => Bson::Int32(1),
            KeyType::Desc => Bson::Int32(-1),
            KeyType::Text => "text".into(),
            KeyType::Sphere => "2dsphere".into(),
            KeyType::Flat => "2d".into(),
            KeyType::Hashed => "hashed".into(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Fields,
    Options,
    Text,
    Geo,
    Collation,
}

pub struct IndexForm {
    pub ns: Namespace,
    name: String,
    tab: Tab,
    pub fields: Vec<(String, KeyType)>,
    /// Field names sampled from the collection, offered when picking fields.
    known_fields: Vec<String>,
    pub unique: bool,
    pub sparse: bool,
    pub hidden: bool,
    pub ttl_on: bool,
    pub ttl: String,
    pub partial_on: bool,
    pub partial: String,
    pub default_language: String,
    pub language_override: String,
    pub weights: String,
    pub sphere_version: Option<i32>,
    pub bits: String,
    pub min: String,
    pub max: String,
    pub collation_on: bool,
    pub locale: String,
    pub strength: i32,
    pub case_level: bool,
    pub case_first: String,
    pub numeric_ordering: bool,
    pub background: bool,
    show_json: bool,
}

impl IndexForm {
    pub fn new(ns: Namespace, known_fields: Vec<String>) -> Self {
        Self {
            ns,
            name: String::new(),
            tab: Tab::Fields,
            fields: vec![(String::new(), KeyType::Asc)],
            known_fields,
            unique: false,
            sparse: false,
            hidden: false,
            ttl_on: false,
            ttl: String::new(),
            partial_on: false,
            partial: "{\n}".into(),
            default_language: String::new(),
            language_override: String::new(),
            weights: String::new(),
            sphere_version: None,
            bits: String::new(),
            min: String::new(),
            max: String::new(),
            collation_on: false,
            locale: "simple".into(),
            strength: 3,
            case_level: false,
            case_first: "off".into(),
            numeric_ordering: false,
            background: false,
            show_json: false,
        }
    }

    pub fn set_known_fields(&mut self, fields: Vec<String>) {
        self.known_fields = fields;
    }

    /// Create index stays disabled until at least one field is named.
    pub fn ready(&self) -> bool {
        self.fields.iter().any(|(f, _)| !f.trim().is_empty())
    }

    fn has(&self, t: KeyType) -> bool {
        self.fields.iter().any(|(f, k)| *k == t && !f.trim().is_empty())
    }

    /// The index's keys and options, as they will be sent.
    pub fn definition(&self) -> Result<(Document, Document), String> {
        let mut keys = Document::new();
        for (f, t) in &self.fields {
            let f = f.trim();
            if f.is_empty() {
                continue;
            }
            if keys.contains_key(f) {
                return Err(format!("Fields: {f} is listed twice"));
            }
            keys.insert(f, t.value());
        }
        if keys.is_empty() {
            return Err("Fields: add at least one field".into());
        }
        let parse = |label: &str, text: &str| rongo_shell::parse_document(text).map_err(|e| format!("{label}: {}", e.message));
        let number = |label: &str, text: &str| text.trim().parse::<f64>().map_err(|_| format!("{label} must be a number"));
        let mut o = Document::new();
        if !self.name.trim().is_empty() {
            o.insert("name", self.name.trim());
        }
        if self.unique {
            o.insert("unique", true);
        }
        if self.sparse {
            o.insert("sparse", true);
        }
        if self.hidden {
            o.insert("hidden", true);
        }
        if self.ttl_on {
            if keys.len() != 1 {
                return Err("TTL: a TTL index must have exactly one field".into());
            }
            let secs: i64 = self.ttl.trim().parse().ok().filter(|n| *n >= 0).ok_or("TTL: expire after must be a whole number of seconds")?;
            o.insert("expireAfterSeconds", secs);
        }
        if self.partial_on {
            if self.sparse {
                return Err("Sparse and Partial can't be combined (MongoDB rejects it): a partial filter like { field: { $exists: true } } does what sparse does".into());
            }
            let filter = parse("Partial", &self.partial)?;
            if filter.is_empty() {
                return Err("Partial: enter a filter expression".into());
            }
            o.insert("partialFilterExpression", filter);
        }
        if self.has(KeyType::Text) {
            if !self.default_language.trim().is_empty() {
                o.insert("default_language", self.default_language.trim());
            }
            if !self.language_override.trim().is_empty() {
                o.insert("language_override", self.language_override.trim());
            }
            if !self.weights.trim().is_empty() {
                o.insert("weights", parse("Text weights", &self.weights)?);
            }
        }
        if self.has(KeyType::Sphere)
            && let Some(v) = self.sphere_version
        {
            o.insert("2dsphereIndexVersion", v);
        }
        if self.has(KeyType::Flat) {
            if !self.bits.trim().is_empty() {
                let bits: i32 = self.bits.trim().parse().ok().filter(|b| (1..=32).contains(b)).ok_or("Geo: bits must be 1 to 32")?;
                o.insert("bits", bits);
            }
            if !self.min.trim().is_empty() {
                o.insert("min", number("Geo: min", &self.min)?);
            }
            if !self.max.trim().is_empty() {
                o.insert("max", number("Geo: max", &self.max)?);
            }
        }
        if self.collation_on {
            let locale = self.locale.trim();
            if locale.is_empty() {
                return Err("Collation: enter a locale, e.g. en or th".into());
            }
            let mut c = doc! { "locale": locale };
            if locale != "simple" {
                c.insert("strength", self.strength);
                if self.case_level {
                    c.insert("caseLevel", true);
                }
                if self.case_first != "off" {
                    c.insert("caseFirst", self.case_first.clone());
                }
                if self.numeric_ordering {
                    c.insert("numericOrdering", true);
                }
            }
            o.insert("collation", c);
        }
        if self.background {
            o.insert("background", true);
        }
        Ok((keys, o))
    }

    pub fn build(&self) -> Result<Command, String> {
        let (keys, options) = self.definition()?;
        Ok(Command::CreateIndex { ns: self.ns.clone(), keys, options: (!options.is_empty()).then_some(options) })
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        ui.label(RichText::new("Index name").strong());
        ui.add(egui::TextEdit::singleline(&mut self.name).hint_text("leave empty for the default, e.g. chatID_1_sentAt_1").desired_width(f32::INFINITY));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            for (t, label) in [(Tab::Fields, "Fields"), (Tab::Options, "Options"), (Tab::Text, "Text options"), (Tab::Geo, "Geo options"), (Tab::Collation, "Collation")] {
                ui.selectable_value(&mut self.tab, t, label);
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().max_height(420.0).auto_shrink([false, true]).show(ui, |ui| match self.tab {
            Tab::Fields => self.fields_tab(ui),
            Tab::Options => self.options_tab(ui),
            Tab::Text => self.text_tab(ui),
            Tab::Geo => self.geo_tab(ui),
            Tab::Collation => self.collation_tab(ui),
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.background, "Create in background")
                .on_hover_text("Ignored by MongoDB 4.2 and newer, which always build indexes without blocking the collection for long");
            ui.add_space(16.0);
            ui.toggle_value(&mut self.show_json, format!("{}  JSON", icon::BRACKETS_CURLY)).on_hover_text("Show the index definition that will be sent");
        });
        if self.show_json {
            let text = match self.definition() {
                Ok((keys, options)) => {
                    let mut d = doc! { "key": keys };
                    d.extend(options);
                    format::doc_to_shell(&d, Style::Display)
                }
                Err(e) => format!("// {e}"),
            };
            let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
                let job = highlight::layout_job(buf.as_str(), theme::mono(12.5), wrap);
                ui.fonts_mut(|f| f.layout_job(job))
            };
            egui::Frame::new().fill(theme::BG).inner_margin(6.0).corner_radius(4.0).show(ui, |ui| {
                let mut view = text.as_str();
                ui.add(egui::TextEdit::multiline(&mut view).code_editor().frame(egui::Frame::NONE).desired_width(f32::INFINITY).layouter(&mut layouter));
            });
        }
    }

    fn fields_tab(&mut self, ui: &mut Ui) {
        ui.label(RichText::new("Indexed fields").strong());
        ui.weak("Order matters for compound indexes: queries can use any prefix of these fields.");
        ui.add_space(4.0);
        let n = self.fields.len();
        let mut swap = None;
        let mut remove = None;
        egui::Grid::new("index-fields").num_columns(5).spacing(vec2(8.0, 6.0)).show(ui, |ui| {
            for (i, (name, kind)) in self.fields.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(name).hint_text("field (dotted paths allowed)").desired_width(300.0));
                    // Pick from the collection's fields, filtered by what is typed.
                    ui.menu_button(icon::CARET_DOWN, |ui| {
                        let typed = name.to_lowercase();
                        let picks: Vec<&String> = self.known_fields.iter().filter(|f| f.to_lowercase().contains(&typed)).take(60).collect();
                        if picks.is_empty() {
                            ui.weak(if self.known_fields.is_empty() { "Loading field names…" } else { "No matching fields" });
                        }
                        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                            for f in picks {
                                if ui.button(f).clicked() {
                                    *name = f.clone();
                                    ui.close();
                                }
                            }
                        });
                    });
                });
                egui::ComboBox::from_id_salt(("key-type", i)).width(140.0).selected_text(kind.label()).show_ui(ui, |ui| {
                    for t in KeyType::ALL {
                        ui.selectable_value(kind, t, t.label());
                    }
                });
                if ui.add_enabled(i > 0, egui::Button::new(icon::ARROW_UP).frame(false)).on_hover_text("Move up").clicked() {
                    swap = Some((i, i - 1));
                }
                if ui.add_enabled(i + 1 < n, egui::Button::new(icon::ARROW_DOWN).frame(false)).on_hover_text("Move down").clicked() {
                    swap = Some((i, i + 1));
                }
                if ui.add_enabled(n > 1, egui::Button::new(RichText::new(icon::X).color(theme::MUTED)).frame(false)).on_hover_text("Remove").clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some((a, b)) = swap {
            self.fields.swap(a, b);
        }
        if let Some(i) = remove {
            self.fields.remove(i);
        }
        ui.add_space(4.0);
        if ui.button(format!("{}  Add field", icon::PLUS)).clicked() {
            self.fields.push((String::new(), KeyType::Asc));
        }
    }

    fn options_tab(&mut self, ui: &mut Ui) {
        ui.label(RichText::new("Index options").strong());
        ui.add_space(4.0);
        let option = |ui: &mut Ui, on: &mut bool, title: &str, about: &str| {
            ui.checkbox(on, RichText::new(title).size(14.0));
            ui.label(RichText::new(about).color(theme::MUTED));
            ui.add_space(8.0);
        };
        option(ui, &mut self.unique, "Unique", "Prevents the insertion of documents where the index key(s) match(es) an already existing value in the collection. Often combined with the sparse option.");
        option(ui, &mut self.sparse, "Sparse", "Omit references to documents that don't include the indexed field. For fields that are only present in some documents. They may provide significant space savings.");
        option(ui, &mut self.hidden, "Hidden", "This index becomes invisible to the query planner and can't be used to support a query. Available only in MongoDB 4.4 servers and newer.");
        option(ui, &mut self.ttl_on, "TTL", "A special type of index that MongoDB can use to automatically remove documents from a collection after a certain amount of time. The indexed field must be a date type.");
        ui.horizontal(|ui| {
            ui.add_enabled_ui(self.ttl_on, |ui| {
                ui.label("Expire after");
                ui.add(egui::TextEdit::singleline(&mut self.ttl).desired_width(80.0));
                ui.label("seconds");
            });
        });
        ui.add_space(8.0);
        ui.checkbox(&mut self.partial_on, RichText::new("Partial").size(14.0));
        ui.label(RichText::new("Only index the documents in a collection that meet the filter expression entered below:").color(theme::MUTED));
        ui.add_enabled_ui(self.partial_on, |ui| code(ui, &mut self.partial, 5));
    }

    fn text_tab(&mut self, ui: &mut Ui) {
        if !self.has(KeyType::Text) {
            ui.weak("These apply when a field's type is \"text\" on the Fields tab.");
            ui.add_space(4.0);
        }
        ui.add_enabled_ui(self.has(KeyType::Text), |ui| {
            egui::Grid::new("text-opts").num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
                ui.label("Default language");
                ui.add(egui::TextEdit::singleline(&mut self.default_language).hint_text("english").desired_width(200.0));
                ui.end_row();
                ui.label("Language override field");
                ui.add(egui::TextEdit::singleline(&mut self.language_override).hint_text("language").desired_width(200.0));
                ui.end_row();
            });
            ui.label("Weights (how much each field counts in the text score)");
            code(ui, &mut self.weights, 4);
            ui.weak("e.g. { title: 10, body: 1 }. Fields not listed weigh 1.");
        });
    }

    fn geo_tab(&mut self, ui: &mut Ui) {
        if !self.has(KeyType::Sphere) && !self.has(KeyType::Flat) {
            ui.weak("These apply when a field's type is \"2dsphere\" or \"2d\" on the Fields tab.");
            ui.add_space(4.0);
        }
        ui.add_enabled_ui(self.has(KeyType::Sphere), |ui| {
            ui.horizontal(|ui| {
                ui.label("2dsphere index version");
                let text = self.sphere_version.map_or("default".to_string(), |v| v.to_string());
                egui::ComboBox::from_id_salt("sphere-version").selected_text(text).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.sphere_version, None, "default");
                    for v in [1, 2, 3] {
                        ui.selectable_value(&mut self.sphere_version, Some(v), v.to_string());
                    }
                });
            });
        });
        ui.add_space(8.0);
        ui.add_enabled_ui(self.has(KeyType::Flat), |ui| {
            egui::Grid::new("geo-2d").num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
                ui.label("2d bits (precision)");
                ui.add(egui::TextEdit::singleline(&mut self.bits).hint_text("26").desired_width(80.0));
                ui.end_row();
                ui.label("2d min");
                ui.add(egui::TextEdit::singleline(&mut self.min).hint_text("-180").desired_width(80.0));
                ui.end_row();
                ui.label("2d max");
                ui.add(egui::TextEdit::singleline(&mut self.max).hint_text("180").desired_width(80.0));
                ui.end_row();
            });
        });
    }

    fn collation_tab(&mut self, ui: &mut Ui) {
        ui.checkbox(&mut self.collation_on, RichText::new("Use a collation").size(14.0));
        ui.label(RichText::new("Language-aware string comparison, e.g. case-insensitive matching. Queries use this index only when they ask for the same collation.").color(theme::MUTED));
        ui.add_space(6.0);
        ui.add_enabled_ui(self.collation_on, |ui| {
            egui::Grid::new("collation").num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
                ui.label("Locale");
                ui.add(egui::TextEdit::singleline(&mut self.locale).hint_text("en, th, simple…").desired_width(160.0));
                ui.end_row();
                ui.label("Strength");
                egui::ComboBox::from_id_salt("strength").selected_text(strength_label(self.strength)).show_ui(ui, |ui| {
                    for s in 1..=5 {
                        ui.selectable_value(&mut self.strength, s, strength_label(s));
                    }
                });
                ui.end_row();
                ui.label("Case first");
                egui::ComboBox::from_id_salt("case-first").selected_text(self.case_first.as_str()).show_ui(ui, |ui| {
                    for c in ["off", "upper", "lower"] {
                        ui.selectable_value(&mut self.case_first, c.to_string(), c);
                    }
                });
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut self.case_level, "Case level");
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut self.numeric_ordering, "Numeric ordering (\"10\" after \"9\")");
                ui.end_row();
            });
        });
    }
}

fn strength_label(s: i32) -> String {
    match s {
        1 => "1: base letters only".into(),
        2 => "2: + accents (case-insensitive)".into(),
        3 => "3: + case (default)".into(),
        4 => "4: + punctuation".into(),
        _ => "5: identical".into(),
    }
}

fn code(ui: &mut Ui, text: &mut String, rows: usize) {
    let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
        let job = highlight::layout_job(buf.as_str(), theme::mono(13.0), wrap);
        ui.fonts_mut(|f| f.layout_job(job))
    };
    egui::Frame::new().fill(theme::BG).inner_margin(6.0).corner_radius(4.0).show(ui, |ui| {
        ui.add(egui::TextEdit::multiline(text).code_editor().frame(egui::Frame::NONE).desired_width(f32::INFINITY).desired_rows(rows).layouter(&mut layouter));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> IndexForm {
        IndexForm::new(Namespace::new("db", "c"), vec![])
    }

    /// Every tab draws (headless) with fields of each kind present.
    #[test]
    fn every_tab_renders() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut f = IndexForm::new(Namespace::new("db", "c"), vec!["a".into(), "b.c".into()]);
        f.fields = vec![("a".into(), KeyType::Text), ("b".into(), KeyType::Sphere), ("g".into(), KeyType::Flat)];
        f.show_json = true;
        for tab in [Tab::Fields, Tab::Options, Tab::Text, Tab::Geo, Tab::Collation] {
            f.tab = tab;
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| f.ui(ui));
            out.textures_delta.clear();
        }
    }

    #[test]
    fn needs_a_field_and_keeps_field_order_and_types() {
        let mut f = form();
        assert!(!f.ready());
        f.fields = vec![("chatID".into(), KeyType::Asc), ("sentAt".into(), KeyType::Desc)];
        assert!(f.ready());
        let (keys, opts) = f.definition().unwrap();
        assert_eq!(keys, doc! { "chatID": 1, "sentAt": -1 });
        assert!(opts.is_empty());
        f.fields.push(("chatID".into(), KeyType::Asc));
        assert!(f.definition().unwrap_err().contains("twice"));
    }

    #[test]
    fn options_tab_like_studio_3t() {
        let mut f = form();
        f.name = "ingest_ttl".into();
        f.fields = vec![("ingestedAt".into(), KeyType::Asc)];
        f.unique = true;
        f.hidden = true;
        f.ttl_on = true;
        f.ttl = "86400".into();
        f.partial_on = true;
        f.partial = "{ kind: \"temp\" }".into();
        f.background = true;
        let (_, o) = f.definition().unwrap();
        assert_eq!(
            o,
            doc! { "name": "ingest_ttl", "unique": true, "hidden": true, "expireAfterSeconds": 86400_i64, "partialFilterExpression": { "kind": "temp" }, "background": true }
        );
        f.sparse = true;
        assert!(f.definition().unwrap_err().contains("Sparse and Partial"));
        f.partial_on = false;
        assert_eq!(f.definition().unwrap().1.get_bool("sparse").ok(), Some(true));
        f.fields.push(("other".into(), KeyType::Asc));
        assert!(f.definition().unwrap_err().contains("exactly one field"));
    }

    #[test]
    fn text_geo_and_collation_options_apply_only_when_relevant() {
        let mut f = form();
        f.fields = vec![("title".into(), KeyType::Text), ("loc".into(), KeyType::Sphere)];
        f.default_language = "english".into();
        f.weights = "{ title: 10 }".into();
        f.sphere_version = Some(3);
        f.bits = "20".into(); // ignored: no 2d field
        f.collation_on = true;
        f.locale = "th".into();
        f.strength = 2;
        f.numeric_ordering = true;
        let (keys, o) = f.definition().unwrap();
        assert_eq!(keys, doc! { "title": "text", "loc": "2dsphere" });
        assert_eq!(
            o,
            doc! { "default_language": "english", "weights": { "title": 10 }, "2dsphereIndexVersion": 3, "collation": { "locale": "th", "strength": 2, "numericOrdering": true } }
        );
    }
}
