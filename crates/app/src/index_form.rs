//! The Add Index dialog's form, laid out like Studio 3T's: an index name,
//! then Fields / Options / Text options / Geo options / Collation tabs, and
//! Create in background plus a JSON preview at the bottom. Filled from an
//! existing index it is also View details (nothing changes) and Edit index
//! (only the TTL and Hidden change).

use egui::{Response, RichText, Ui, vec2};
use egui_phosphor::regular as icon;
use rongo_core::bson::{Bson, Document, doc};
use rongo_core::format::{self, Style};
use rongo_core::conn::default_index_name;
use rongo_core::{Command, Namespace};

use crate::theme;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
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

/// What filling the form from an index leaves out: what the server sets
/// itself (`v`, `textIndexVersion`, a collation's defaults and version),
/// and what the form has no place for (`wildcardProjection`, a key of a
/// kind it doesn't offer). Both are named rather than dropped unsaid.
#[derive(Default, Debug, PartialEq)]
struct Left {
    by_server: Vec<String>,
    no_place: Vec<String>,
}

/// Add as New Index…: the index the form was filled from.
struct Source {
    name: String,
    left: Left,
    /// What makes it that index for MongoDB, as the form sends it.
    pattern: Option<Pattern>,
}

/// An index's keys in order, its collation and its partial filter: MongoDB
/// keeps one index per pattern (other options, such as unique, don't make
/// another one).
type Pattern = (Vec<(String, Bson)>, Option<Bson>, Option<Bson>);

fn pattern(keys: &Document, options: &Document) -> Pattern {
    (keys.iter().map(|(k, v)| (k.clone(), v.clone())).collect(), options.get("collation").cloned(), options.get("partialFilterExpression").cloned())
}

/// What the form is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Purpose {
    /// Add Index: every field can be changed.
    Add,
    /// View details: an index as the server listed it; nothing changes.
    View,
    /// Edit index: only the TTL's seconds (for a TTL index) and Hidden
    /// (not for `_id_`) change, with `collMod`.
    Edit,
}

/// Edit index: why a field other than the TTL and Hidden is greyed out.
const ONLY_TTL_HIDDEN: &str = "Edit index changes only an index's TTL and Hidden. To change anything else, Add as New Index… makes a new index from this one; then drop this one yourself.";

/// Edit index: why the `_id` index's Hidden is greyed out.
const NO_HIDDEN_ID: &str = "MongoDB can't hide the _id index";

/// Whether a field of the form can be changed, and if not, why.
#[derive(Clone, Copy)]
enum Gate {
    Open,
    /// View details: nothing changes there.
    Shown,
    /// Edit index: it doesn't change there (why is said on hover).
    Fixed(&'static str),
}

impl Gate {
    fn open(self) -> bool {
        matches!(self, Gate::Open)
    }

    /// `r`, saying on hover why it is greyed out.
    fn why(self, r: Response) -> Response {
        match self {
            Gate::Fixed(why) => r.on_disabled_hover_text(why),
            _ => r,
        }
    }
}

/// The fields Edit index treats apart.
#[derive(Clone, Copy)]
enum Field {
    Ttl,
    Hidden,
    Other,
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
    pub purpose: Purpose,
    /// View details and Edit index: the index as the server listed it.
    listed: Option<Document>,
    /// Add as New Index…: the index it was filled from.
    from: Option<Source>,
    /// Add as New Index…: whether the keys, collation and partial filter
    /// are still the old index's, for the form as it was (`inputs`).
    same_pattern: Option<(u64, bool)>,
    /// A listed collation's fields the form has no place for (`alternate`,
    /// `maxVariable`, `normalization`, `backwards`), and the locale they
    /// were listed for: they go along as listed while that locale stays.
    collation_kept: (String, Document),
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
    /// The JSON preview's text, for the form as it was (`inputs`): made
    /// again only when something changes, as it runs the shell's parser.
    json: Option<(u64, String)>,
    json_layout: theme::CodeLayout,
    /// The partial filter's or the weights' editor, laid out (one shows at
    /// a time, on its own tab).
    code_layout: theme::CodeLayout,
}

impl IndexForm {
    pub fn new(ns: Namespace, known_fields: Vec<String>) -> Self {
        Self {
            ns,
            purpose: Purpose::Add,
            listed: None,
            from: None,
            same_pattern: None,
            collation_kept: (String::new(), Document::new()),
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
            json: None,
            json_layout: theme::CodeLayout::default(),
            code_layout: theme::CodeLayout::default(),
        }
    }

    /// The form filled from an index as `listIndexes` lists it (`spec`),
    /// and what of the index it leaves out.
    fn filled(ns: Namespace, known_fields: Vec<String>, spec: &Document) -> (Self, Left) {
        let mut f = Self::new(ns, known_fields);
        let mut left = Left::default();
        f.fields.clear();
        let weights = spec.get_document("weights").ok();
        for (k, v) in spec {
            match (k.as_str(), v) {
                ("name", Bson::String(s)) => f.name = s.clone(),
                ("key", Bson::Document(keys)) => f.fill_keys(keys, weights, &mut left.no_place),
                ("unique", v) => f.unique = truthy(v),
                ("sparse", v) => f.sparse = truthy(v),
                ("hidden", v) => f.hidden = truthy(v),
                ("background", v) => f.background = truthy(v),
                ("expireAfterSeconds", v) => (f.ttl_on, f.ttl) = (true, seconds_text(v)),
                ("partialFilterExpression", Bson::Document(d)) => (f.partial_on, f.partial) = (true, format::doc_to_shell(d, Style::Editable)),
                ("default_language", Bson::String(s)) => f.default_language = s.clone(),
                ("language_override", Bson::String(s)) => f.language_override = s.clone(),
                ("weights", Bson::Document(d)) => f.weights = format::doc_to_shell(d, Style::Editable),
                ("2dsphereIndexVersion", v) if let Some(n) = number(v) => f.sphere_version = Some(n as i32),
                ("bits", v) if let Some(n) = number(v) => f.bits = n.to_string(),
                ("min", v) if let Some(n) = number(v) => f.min = n.to_string(),
                ("max", v) if let Some(n) = number(v) => f.max = n.to_string(),
                ("collation", Bson::Document(c)) => f.fill_collation(c, &mut left),
                ("v" | "ns" | "textIndexVersion", _) => left.by_server.push(k.clone()),
                _ => left.no_place.push(k.clone()),
            }
        }
        if f.fields.is_empty() {
            f.fields.push((String::new(), KeyType::Asc));
        }
        (f, left)
    }

    /// View details: the form filled from `spec`, nothing open.
    pub fn view(ns: Namespace, spec: &Document) -> Self {
        let (mut f, _) = Self::filled(ns, Vec::new(), spec);
        (f.purpose, f.listed) = (Purpose::View, Some(spec.clone()));
        f
    }

    /// Edit index: the form filled from `spec`, only the TTL's seconds
    /// and Hidden open.
    pub fn edit(ns: Namespace, spec: &Document) -> Self {
        let (mut f, _) = Self::filled(ns, Vec::new(), spec);
        (f.purpose, f.listed) = (Purpose::Edit, Some(spec.clone()));
        f
    }

    /// Add as New Index…: Add Index filled from `spec` with the name left
    /// empty, so MongoDB names the new index unless one is typed.
    pub fn add_as_new(ns: Namespace, known_fields: Vec<String>, spec: &Document) -> Self {
        let (mut f, left) = Self::filled(ns, known_fields, spec);
        let pattern = f.definition().ok().map(|(keys, options)| pattern(&keys, &options));
        f.from = Some(Source { name: std::mem::take(&mut f.name), left, pattern });
        f
    }

    /// Add as New Index…: the old index's name when, with no name typed,
    /// MongoDB would give the new index that same name (its default for
    /// these keys). The server refuses that, or, unchanged, builds nothing.
    fn takes_old_name(&self) -> Option<&str> {
        let old = &self.from.as_ref()?.name;
        let taken = self.name.trim().is_empty() && self.keys().is_ok_and(|k| default_index_name(&k) == *old);
        taken.then_some(old.as_str())
    }

    /// Add as New Index…: whether the keys, collation and partial filter
    /// are still the old index's. Worked out again only when the form
    /// changes (it reads the partial filter with the shell's parser).
    fn same_pattern(&mut self) -> bool {
        if self.from.is_none() {
            return false;
        }
        let inputs = self.inputs();
        if let Some((k, same)) = self.same_pattern
            && k == inputs
        {
            return same;
        }
        let same = match self.from.as_ref().and_then(|s| s.pattern.as_ref()) {
            Some(old) => self.definition().is_ok_and(|(keys, options)| pattern(&keys, &options) == *old),
            None => false,
        };
        self.same_pattern = Some((inputs, same));
        same
    }

    /// View details and Edit index: the index as the server listed it.
    pub fn listed(&self) -> Option<&Document> {
        self.listed.as_ref()
    }

    /// The listed index's name.
    fn listed_name(&self) -> &str {
        self.listed.as_ref().and_then(|l| l.get_str("name").ok()).unwrap_or_default()
    }

    pub fn title(&self) -> String {
        match self.purpose {
            Purpose::Add => format!("{}  Add index — {}", icon::LIGHTNING, self.ns),
            Purpose::View => format!("{}  View Index {} — {}", icon::LIGHTNING, self.listed_name(), self.ns),
            Purpose::Edit => format!("{}  Edit Index {} — {}", icon::LIGHTNING, self.listed_name(), self.ns),
        }
    }

    /// The dialog's main button.
    pub fn primary(&self) -> &'static str {
        match self.purpose {
            Purpose::Add => "Create index",
            Purpose::View | Purpose::Edit => "OK",
        }
    }

    pub fn success_message(&self) -> &'static str {
        match self.purpose {
            Purpose::Add => "Index created",
            Purpose::View | Purpose::Edit => "Index updated",
        }
    }

    /// Whether a field can be changed, and if not, why.
    fn gate(&self, field: Field) -> Gate {
        match (self.purpose, field) {
            (Purpose::Add, _) => Gate::Open,
            (Purpose::View, _) => Gate::Shown,
            (Purpose::Edit, Field::Ttl) if self.listed.as_ref().is_some_and(|l| l.contains_key("expireAfterSeconds")) => Gate::Open,
            (Purpose::Edit, Field::Hidden) if self.listed_name() == "_id_" => Gate::Fixed(NO_HIDDEN_ID),
            (Purpose::Edit, Field::Hidden) => Gate::Open,
            (Purpose::Edit, _) => Gate::Fixed(ONLY_TTL_HIDDEN),
        }
    }

    /// Edit index: what changed, as (the TTL's seconds as typed, Hidden).
    fn changes(&self) -> (Option<&str>, Option<bool>) {
        let listed = self.listed.as_ref();
        let ttl = listed.and_then(|l| l.get("expireAfterSeconds")).map(seconds_text);
        let new_ttl = ttl.filter(|t| t.trim() != self.ttl.trim()).map(|_| self.ttl.trim());
        let hidden = listed.and_then(|l| l.get("hidden")).is_some_and(truthy);
        (new_ttl, (self.hidden != hidden).then_some(self.hidden))
    }

    /// The keys in order. A text index lists `_fts: "text", _ftsx: 1` for
    /// its text fields: they stand where `_fts` does, named by its weights.
    fn fill_keys(&mut self, keys: &Document, weights: Option<&Document>, no_place: &mut Vec<String>) {
        for (k, v) in keys {
            match (k.as_str(), v) {
                ("_fts", Bson::String(s)) if s == "text" => self.fields.extend(weights.into_iter().flat_map(Document::keys).map(|f| (f.clone(), KeyType::Text))),
                ("_ftsx", _) => {}
                _ => match key_type(v) {
                    Some(t) => self.fields.push((k.clone(), t)),
                    None => no_place.push(format!("key {k}: {}", format::to_shell(v, Style::Display))),
                },
            }
        }
    }

    /// A collation as a server lists it: every field, ICU's defaults and
    /// version included.
    fn fill_collation(&mut self, c: &Document, left: &mut Left) {
        self.collation_on = true;
        for (k, v) in c {
            match (k.as_str(), v) {
                ("locale", Bson::String(s)) => self.locale = s.clone(),
                ("strength", v) if let Some(n) = number(v) => self.strength = n as i32,
                ("caseLevel", Bson::Boolean(b)) => self.case_level = *b,
                ("caseFirst", Bson::String(s)) => self.case_first = s.clone(),
                ("numericOrdering", Bson::Boolean(b)) => self.numeric_ordering = *b,
                ("alternate" | "maxVariable" | "normalization" | "backwards", v) => _ = self.collation_kept.1.insert(k, v.clone()),
                ("version", _) => left.by_server.push(format!("collation.{k}")),
                (k, _) => left.no_place.push(format!("collation.{k}")),
            }
        }
        self.collation_kept.0 = self.locale.clone();
    }

    /// A hash of everything typed or picked in the form.
    fn inputs(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::hash::DefaultHasher::new();
        (&self.name, &self.fields, self.unique, self.sparse, self.hidden, self.ttl_on, &self.ttl, self.partial_on, &self.partial).hash(&mut h);
        (&self.default_language, &self.language_override, &self.weights, self.sphere_version, &self.bits, &self.min, &self.max).hash(&mut h);
        (self.collation_on, &self.locale, self.strength, self.case_level, &self.case_first, self.numeric_ordering, self.background).hash(&mut h);
        h.finish()
    }

    pub fn set_known_fields(&mut self, fields: Vec<String>) {
        self.known_fields = fields;
    }

    /// Create index stays disabled until at least one field is named;
    /// Edit index's OK until something changed; View details' OK always.
    /// Add as New Index…'s Create index also waits while, unnamed, the new
    /// index would take the old one's name.
    pub fn ready(&self) -> bool {
        match self.purpose {
            Purpose::Add => self.fields.iter().any(|(f, _)| !f.trim().is_empty()) && self.takes_old_name().is_none(),
            Purpose::View => false,
            Purpose::Edit => self.changes() != (None, None),
        }
    }

    fn has(&self, t: KeyType) -> bool {
        self.fields.iter().any(|(f, k)| *k == t && !f.trim().is_empty())
    }

    /// The index's keys in order, as they will be sent.
    fn keys(&self) -> Result<Document, String> {
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
        Ok(keys)
    }

    /// The index's keys and options, as they will be sent.
    pub fn definition(&self) -> Result<(Document, Document), String> {
        let keys = self.keys()?;
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
                // What the form has no place for goes as listed, while the
                // locale is still the one it was listed for.
                if locale == self.collation_kept.0 {
                    c.extend(self.collation_kept.1.clone());
                }
            }
            o.insert("collation", c);
        }
        if self.background {
            o.insert("background", true);
        }
        Ok((keys, o))
    }

    /// Add Index: `createIndexes` with the definition. Edit index:
    /// `collMod` with only what changed. View details: nothing.
    pub fn build(&self) -> Result<Command, String> {
        match self.purpose {
            Purpose::Add => {
                let (keys, options) = self.definition()?;
                Ok(Command::CreateIndex { ns: self.ns.clone(), keys, options: (!options.is_empty()).then_some(options) })
            }
            Purpose::View => Err("View details changes nothing".into()),
            Purpose::Edit => {
                let (ttl, hidden) = self.changes();
                let ttl = match ttl {
                    Some(t) => Some(t.parse::<i64>().ok().filter(|n| *n >= 0).ok_or("Expire after must be a whole number of seconds")?),
                    None => None,
                };
                if ttl.is_none() && hidden.is_none() {
                    return Err("Nothing changed".into());
                }
                Ok(Command::ModifyIndex { ns: self.ns.clone(), name: self.listed_name().to_string(), hidden, expire_after_seconds: ttl })
            }
        }
    }

    /// What the JSON preview shows: Add Index the definition it sends,
    /// View details the index as listed, Edit index the `collMod` OK sends.
    fn json_text(&self) -> String {
        let shell = |d: &Document| format::doc_to_shell(d, Style::Display);
        match self.purpose {
            Purpose::Add => match self.definition() {
                Ok((keys, options)) => {
                    let mut d = doc! { "key": keys };
                    d.extend(options);
                    shell(&d)
                }
                Err(e) => format!("// {e}"),
            },
            Purpose::View => self.listed.as_ref().map(shell).unwrap_or_default(),
            Purpose::Edit => match self.build() {
                Ok(Command::ModifyIndex { ns, name, hidden, expire_after_seconds }) => shell(&rongo_core::conn::modify_index_command(&ns, &name, hidden, expire_after_seconds)),
                Ok(_) => String::new(),
                Err(e) => format!("// {e}"),
            },
        }
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        let other = self.gate(Field::Other);
        ui.label(RichText::new("Index name").strong());
        other.why(ui.add_enabled(other.open(), egui::TextEdit::singleline(&mut self.name).hint_text("leave empty for the default, e.g. chatID_1_sentAt_1").desired_width(f32::INFINITY)));
        let same_pattern = self.same_pattern();
        if let Some(old) = self.takes_old_name() {
            ui.colored_label(theme::WARN, format!("With no name, MongoDB would name it {old}, as the index it was filled from: type a name, or change the keys."));
        }
        if let Some(Source { name: old, left, .. }) = &self.from {
            ui.weak(format!("Filled from {old}, which stays as it is: drop it yourself once the new index is built."));
            if !left.no_place.is_empty() {
                ui.colored_label(theme::WARN, format!("Not carried over (the form has no place for them): {}", left.no_place.join(", ")));
            }
            if !left.by_server.is_empty() {
                ui.weak(format!("Not carried over (the server sets them): {}", left.by_server.join(", ")));
            }
            if same_pattern {
                ui.weak(format!(
                    "Same keys, collation and partial filter as {old}: MongoDB keeps one index per key pattern unless the collation or partial filter differ, so changing only options such as Unique or Sparse needs {old} dropped first. rongo doesn't drop it for you."
                ));
            }
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            for (t, label) in [(Tab::Fields, "Fields"), (Tab::Options, "Options"), (Tab::Text, "Text options"), (Tab::Geo, "Geo options"), (Tab::Collation, "Collation")] {
                ui.selectable_value(&mut self.tab, t, label);
            }
        });
        ui.separator();
        // In a Modal a ScrollArea takes the room left as of last frame:
        // without a min_scrolled_height it grew a little each frame after a
        // switch to a tall tab, and the dialog moved under the pointer.
        egui::ScrollArea::vertical().max_height(420.0).min_scrolled_height(420.0).auto_shrink([false, true]).show(ui, |ui| match self.tab {
            Tab::Fields => self.fields_tab(ui),
            Tab::Options => self.options_tab(ui),
            Tab::Text => self.text_tab(ui),
            Tab::Geo => self.geo_tab(ui),
            Tab::Collation => self.collation_tab(ui),
        });
        ui.separator();
        ui.horizontal(|ui| {
            other
                .why(ui.add_enabled(other.open(), egui::Checkbox::new(&mut self.background, "Create in background")))
                .on_hover_text("Ignored by MongoDB 4.2 and newer, which always build indexes without blocking the collection for long");
            ui.add_space(16.0);
            let about = match self.purpose {
                Purpose::Add => "Show the index definition that will be sent",
                Purpose::View => "Show the index as the server lists it",
                Purpose::Edit => "Show the command OK will send",
            };
            ui.toggle_value(&mut self.show_json, format!("{}  JSON", icon::BRACKETS_CURLY)).on_hover_text(about);
        });
        if self.purpose == Purpose::Edit {
            ui.weak("Edit index changes only the TTL and Hidden. For anything else, Add as New Index… makes a new index from this one; drop this one yourself once it is built.");
        }
        if self.show_json {
            let inputs = self.inputs();
            if self.json.as_ref().is_none_or(|(k, _)| *k != inputs) {
                self.json = Some((inputs, self.json_text()));
            }
            let text = self.json.as_ref().map_or("", |(_, t)| t.as_str());
            let layout = &mut self.json_layout;
            let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| layout.galley(ui, buf.as_str(), theme::mono(12.5), wrap);
            egui::Frame::new().fill(theme::BG).inner_margin(6.0).corner_radius(4.0).show(ui, |ui| {
                let mut view = text;
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
        let gate = self.gate(Field::Other);
        let add = ui.add_enabled_ui(gate.open(), |ui| {
            egui::Grid::new("index-fields").num_columns(5).spacing(vec2(8.0, 6.0)).show(ui, |ui| {
                for (i, (name, kind)) in self.fields.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        gate.why(ui.add(egui::TextEdit::singleline(name).hint_text("field (dotted paths allowed)").desired_width(300.0)));
                        // Pick from the collection's fields, filtered by what is typed.
                        let picker = ui.menu_button(icon::CARET_DOWN, |ui| {
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
                        gate.why(picker.response);
                    });
                    let kinds = egui::ComboBox::from_id_salt(("key-type", i)).width(140.0).selected_text(kind.label()).show_ui(ui, |ui| {
                        for t in KeyType::ALL {
                            ui.selectable_value(kind, t, t.label());
                        }
                    });
                    gate.why(kinds.response);
                    if gate.why(ui.add_enabled(i > 0, egui::Button::new(icon::ARROW_UP).frame(false)).on_hover_text("Move up")).clicked() {
                        swap = Some((i, i - 1));
                    }
                    if gate.why(ui.add_enabled(i + 1 < n, egui::Button::new(icon::ARROW_DOWN).frame(false)).on_hover_text("Move down")).clicked() {
                        swap = Some((i, i + 1));
                    }
                    if gate.why(ui.add_enabled(n > 1, egui::Button::new(RichText::new(icon::X).color(theme::MUTED)).frame(false)).on_hover_text("Remove")).clicked() {
                        remove = Some(i);
                    }
                    ui.end_row();
                }
            });
            ui.add_space(4.0);
            gate.why(ui.button(format!("{}  Add field", icon::PLUS))).clicked()
        });
        if let Some((a, b)) = swap {
            self.fields.swap(a, b);
        }
        if let Some(i) = remove {
            self.fields.remove(i);
        }
        if add.inner {
            self.fields.push((String::new(), KeyType::Asc));
        }
    }

    fn options_tab(&mut self, ui: &mut Ui) {
        ui.label(RichText::new("Index options").strong());
        ui.add_space(4.0);
        let (other, hidden, ttl) = (self.gate(Field::Other), self.gate(Field::Hidden), self.gate(Field::Ttl));
        let option = |ui: &mut Ui, gate: Gate, on: &mut bool, title: &str, about: &str| {
            gate.why(ui.add_enabled(gate.open(), egui::Checkbox::new(on, RichText::new(title).size(14.0))));
            ui.label(RichText::new(about).color(theme::MUTED));
            ui.add_space(8.0);
        };
        option(ui, other, &mut self.unique, "Unique", "Prevents the insertion of documents where the index key(s) match(es) an already existing value in the collection. Often combined with the sparse option.");
        option(ui, other, &mut self.sparse, "Sparse", "Omit references to documents that don't include the indexed field. For fields that are only present in some documents. They may provide significant space savings.");
        option(ui, hidden, &mut self.hidden, "Hidden", "This index becomes invisible to the query planner and can't be used to support a query. Available only in MongoDB 4.4 servers and newer.");
        option(ui, other, &mut self.ttl_on, "TTL", "A special type of index that MongoDB can use to automatically remove documents from a collection after a certain amount of time. The indexed field must be a date type.");
        ui.horizontal(|ui| {
            ui.add_enabled_ui(self.ttl_on && ttl.open(), |ui| {
                ui.label("Expire after");
                ttl.why(ui.add(egui::TextEdit::singleline(&mut self.ttl).desired_width(80.0)));
                ui.label("seconds");
            });
        });
        ui.add_space(8.0);
        other.why(ui.add_enabled(other.open(), egui::Checkbox::new(&mut self.partial_on, RichText::new("Partial").size(14.0))));
        ui.label(RichText::new("Only index the documents in a collection that meet the filter expression entered below:").color(theme::MUTED));
        ui.add_enabled_ui(self.partial_on && other.open(), |ui| other.why(code(ui, &mut self.partial, 5, &mut self.code_layout)));
    }

    fn text_tab(&mut self, ui: &mut Ui) {
        if !self.has(KeyType::Text) {
            ui.weak("These apply when a field's type is \"text\" on the Fields tab.");
            ui.add_space(4.0);
        }
        let gate = self.gate(Field::Other);
        ui.add_enabled_ui(self.has(KeyType::Text) && gate.open(), |ui| {
            egui::Grid::new("text-opts").num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
                ui.label("Default language");
                gate.why(ui.add(egui::TextEdit::singleline(&mut self.default_language).hint_text("english").desired_width(200.0)));
                ui.end_row();
                ui.label("Language override field");
                gate.why(ui.add(egui::TextEdit::singleline(&mut self.language_override).hint_text("language").desired_width(200.0)));
                ui.end_row();
            });
            ui.label("Weights (how much each field counts in the text score)");
            gate.why(code(ui, &mut self.weights, 4, &mut self.code_layout));
            ui.weak("e.g. { title: 10, body: 1 }. Fields not listed weigh 1.");
        });
    }

    fn geo_tab(&mut self, ui: &mut Ui) {
        if !self.has(KeyType::Sphere) && !self.has(KeyType::Flat) {
            ui.weak("These apply when a field's type is \"2dsphere\" or \"2d\" on the Fields tab.");
            ui.add_space(4.0);
        }
        let gate = self.gate(Field::Other);
        ui.add_enabled_ui(self.has(KeyType::Sphere) && gate.open(), |ui| {
            ui.horizontal(|ui| {
                ui.label("2dsphere index version");
                let text = self.sphere_version.map_or("default".to_string(), |v| v.to_string());
                let version = egui::ComboBox::from_id_salt("sphere-version").selected_text(text).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.sphere_version, None, "default");
                    for v in [1, 2, 3] {
                        ui.selectable_value(&mut self.sphere_version, Some(v), v.to_string());
                    }
                });
                gate.why(version.response);
            });
        });
        ui.add_space(8.0);
        ui.add_enabled_ui(self.has(KeyType::Flat) && gate.open(), |ui| {
            egui::Grid::new("geo-2d").num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
                ui.label("2d bits (precision)");
                gate.why(ui.add(egui::TextEdit::singleline(&mut self.bits).hint_text("26").desired_width(80.0)));
                ui.end_row();
                ui.label("2d min");
                gate.why(ui.add(egui::TextEdit::singleline(&mut self.min).hint_text("-180").desired_width(80.0)));
                ui.end_row();
                ui.label("2d max");
                gate.why(ui.add(egui::TextEdit::singleline(&mut self.max).hint_text("180").desired_width(80.0)));
                ui.end_row();
            });
        });
    }

    fn collation_tab(&mut self, ui: &mut Ui) {
        let gate = self.gate(Field::Other);
        gate.why(ui.add_enabled(gate.open(), egui::Checkbox::new(&mut self.collation_on, RichText::new("Use a collation").size(14.0))));
        ui.label(RichText::new("Language-aware string comparison, e.g. case-insensitive matching. Queries use this index only when they ask for the same collation.").color(theme::MUTED));
        ui.add_space(6.0);
        ui.add_enabled_ui(self.collation_on && gate.open(), |ui| {
            egui::Grid::new("collation").num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
                ui.label("Locale");
                gate.why(ui.add(egui::TextEdit::singleline(&mut self.locale).hint_text("en, th, simple…").desired_width(160.0)));
                ui.end_row();
                ui.label("Strength");
                let strength = egui::ComboBox::from_id_salt("strength").selected_text(strength_label(self.strength)).show_ui(ui, |ui| {
                    for s in 1..=5 {
                        ui.selectable_value(&mut self.strength, s, strength_label(s));
                    }
                });
                gate.why(strength.response);
                ui.end_row();
                ui.label("Case first");
                let case_first = egui::ComboBox::from_id_salt("case-first").selected_text(self.case_first.as_str()).show_ui(ui, |ui| {
                    for c in ["off", "upper", "lower"] {
                        ui.selectable_value(&mut self.case_first, c.to_string(), c);
                    }
                });
                gate.why(case_first.response);
                ui.end_row();
                ui.label("");
                gate.why(ui.checkbox(&mut self.case_level, "Case level"));
                ui.end_row();
                ui.label("");
                gate.why(ui.checkbox(&mut self.numeric_ordering, "Numeric ordering (\"10\" after \"9\")"));
                ui.end_row();
            });
        });
        // A listed collation's fields with no place here: kept while the
        // locale is still theirs, left out (and said) once it isn't.
        let (theirs, kept) = &self.collation_kept;
        if self.collation_on && !kept.is_empty() && self.locale.trim() != "simple" {
            ui.add_space(6.0);
            if self.locale.trim() == theirs {
                let fields: Vec<String> = kept.iter().map(|(k, v)| format!("{k}: {}", format::to_shell(v, Style::Display))).collect();
                ui.weak(format!("Kept as listed for {theirs}, with no place of their own here: {}", fields.join(", ")));
            } else {
                let names: Vec<&str> = kept.keys().map(String::as_str).collect();
                ui.colored_label(theme::WARN, format!("Left out, as they were set for the locale {theirs}: {}", names.join(", ")));
            }
        }
    }
}

/// What a key's value makes it: any positive number ascending, any
/// negative one descending, or one of the kinds the form offers.
fn key_type(v: &Bson) -> Option<KeyType> {
    match v {
        Bson::String(s) => KeyType::ALL.into_iter().find(|t| t.value().as_str() == Some(s)),
        v => number(v).filter(|n| *n != 0.0).map(|n| if n > 0.0 { KeyType::Asc } else { KeyType::Desc }),
    }
}

fn number(v: &Bson) -> Option<f64> {
    match v {
        Bson::Int32(n) => Some(f64::from(*n)),
        Bson::Int64(n) => Some(*n as f64),
        Bson::Double(f) => Some(*f),
        _ => None,
    }
}

/// An option's flag as the server reads it (old servers kept `unique: 1`).
fn truthy(v: &Bson) -> bool {
    match v {
        Bson::Boolean(b) => *b,
        v => number(v).is_some_and(|n| n != 0.0),
    }
}

/// `expireAfterSeconds` as the TTL field shows it.
fn seconds_text(v: &Bson) -> String {
    match v {
        Bson::Int32(n) => n.to_string(),
        Bson::Int64(n) => n.to_string(),
        Bson::Double(f) if f.fract() == 0.0 => (*f as i64).to_string(),
        other => format::cell_text(other),
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

/// A code editor, laid out again only when its text changes (`layout`).
fn code(ui: &mut Ui, text: &mut String, rows: usize, layout: &mut theme::CodeLayout) -> Response {
    let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| layout.galley(ui, buf.as_str(), theme::mono(13.0), wrap);
    egui::Frame::new()
        .fill(theme::BG)
        .inner_margin(6.0)
        .corner_radius(4.0)
        .show(ui, |ui| ui.add(egui::TextEdit::multiline(text).code_editor().frame(egui::Frame::NONE).desired_width(f32::INFINITY).desired_rows(rows).layouter(&mut layouter)))
        .inner
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> IndexForm {
        IndexForm::new(Namespace::new("db", "c"), vec![])
    }

    /// Milliseconds per frame of the form with a partial filter and text
    /// weights (each read by the shell's parser), its JSON preview on or off.
    fn form_frame_ms(json: bool) -> f64 {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut f = form();
        f.fields = vec![("title".into(), KeyType::Text)];
        (f.partial_on, f.partial) = (true, "{ kind: \"temp\", n: { $gt: 5 } }".into());
        f.weights = "{ title: 10 }".into();
        f.show_json = json;
        let mut frame = || {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| f.ui(ui));
            out.textures_delta.clear();
        };
        frame();
        frame();
        (0..5)
            .map(|_| {
                let t = std::time::Instant::now();
                for _ in 0..5 {
                    frame();
                }
                t.elapsed().as_secs_f64() * 1000.0 / 5.0
            })
            .fold(f64::INFINITY, f64::min)
    }

    /// The JSON preview is made again only when the form changes: with it
    /// on, a frame costs about what it does with it off (it used to run the
    /// shell's parser on the partial filter and the weights every frame).
    #[test]
    fn json_preview_is_made_only_when_the_form_changes() {
        let off = form_frame_ms(false);
        let on = form_frame_ms(true);
        assert!(on < off * 1.5 + 0.5, "{on:.2} ms per frame with the preview vs {off:.2} ms without");
    }

    /// The preview follows what is typed.
    #[test]
    fn json_preview_follows_the_form() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut f = form();
        f.show_json = true;
        let preview = |f: &mut IndexForm| {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| f.ui(ui));
            out.textures_delta.clear();
            out.shapes.iter().filter_map(|s| match &s.shape {
                egui::Shape::Text(t) if t.galley.text().contains("key") || t.galley.text().starts_with("//") => Some(t.galley.text().to_string()),
                _ => None,
            }).collect::<Vec<_>>().join("\n")
        };
        assert!(preview(&mut f).contains("add at least one field"));
        f.fields[0].0 = "chatID".into();
        let text = preview(&mut f);
        assert!(text.contains("chatID") && !text.contains("unique"), "{text}");
        f.unique = true;
        assert!(preview(&mut f).contains("unique"));
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

    /// Numbers as doubles: `1`, `1.0` and `NumberLong(1)` are the same key
    /// or option to the server.
    fn norm(v: &Bson) -> Bson {
        match v {
            Bson::Int32(n) => Bson::Double(f64::from(*n)),
            Bson::Int64(n) => Bson::Double(*n as f64),
            Bson::Document(d) => Bson::Document(d.iter().map(|(k, v)| (k.clone(), norm(v))).collect()),
            Bson::Array(a) => Bson::Array(a.iter().map(norm).collect()),
            v => v.clone(),
        }
    }

    /// A document's fields in order.
    fn in_order(d: &Document) -> Vec<(String, Bson)> {
        d.iter().map(|(k, v)| (k.clone(), norm(v))).collect()
    }

    /// An index as listed, less what the server adds to it: `key` (compared
    /// apart), `v`, `ns`, `textIndexVersion`, and in a collation its ICU
    /// `version` and the defaults the form leaves unsaid (`caseLevel:
    /// false`, `caseFirst: "off"`, `numericOrdering: false`).
    fn as_sent(listed: &Document) -> Document {
        let mut d = listed.clone();
        for added in ["key", "v", "ns", "textIndexVersion"] {
            d.remove(added);
        }
        if let Ok(c) = d.get_document_mut("collation") {
            c.remove("version");
            for (k, default) in [("caseLevel", Bson::Boolean(false)), ("caseFirst", "off".into()), ("numericOrdering", Bson::Boolean(false))] {
                if c.get(k) == Some(&default) {
                    c.remove(k);
                }
            }
        }
        d
    }

    /// A collation as a server lists it for `locale`, ICU's defaults filled
    /// in (Thai's are `alternate: "shifted"` and `normalization: true`).
    fn listed_collation(locale: &str, set: Document) -> Document {
        let thai = locale == "th";
        let mut c = doc! {
            "locale": locale, "caseLevel": false, "caseFirst": "off", "strength": 3, "numericOrdering": false,
            "alternate": if thai { "shifted" } else { "non-ignorable" }, "maxVariable": "punct", "normalization": thai, "backwards": false, "version": "57.1"
        };
        c.extend(set);
        c
    }

    /// Filled from an index as `listIndexes` lists it, the form gives back
    /// the index's definition: its keys in order and its options, all but
    /// what the server adds (`as_sent`). A text index lists `_fts`/`_ftsx`
    /// for its text fields: they come back as the fields its weights name,
    /// where they stood among the other keys. A collation's fields the form
    /// has no place for (Thai's `alternate`, `normalization`) go along as
    /// listed.
    #[test]
    fn filling_from_an_index_gives_back_its_definition() {
        let text = |key: Document, name: &str, weights: Document| {
            doc! { "v": 2, "key": key, "name": name, "weights": weights, "default_language": "english", "language_override": "language", "textIndexVersion": 3 }
        };
        // Each index as listed, and the keys it was created with.
        let cases: Vec<(Document, Document)> = vec![
            (doc! { "v": 2, "key": { "_id": 1 }, "name": "_id_" }, doc! { "_id": 1 }),
            (doc! { "v": 2, "key": { "chatID": 1, "sentAt": -1 }, "name": "chatID_1_sentAt_-1", "unique": true, "ns": "db.c" }, doc! { "chatID": 1, "sentAt": -1 }),
            (doc! { "v": 2, "key": { "sentAt": -1, "chatID": 1.0 }, "name": "by_time" }, doc! { "sentAt": -1, "chatID": 1 }),
            (doc! { "v": 2, "key": { "ingestedAt": 1 }, "name": "ttl", "expireAfterSeconds": 3600, "hidden": true }, doc! { "ingestedAt": 1 }),
            (doc! { "v": 2, "key": { "email": 1_i64 }, "name": "email", "sparse": true, "background": true }, doc! { "email": 1 }),
            (
                doc! { "v": 2, "key": { "status": 1, "at": -1 }, "name": "open", "partialFilterExpression": { "status": { "$in": ["open", "held"] }, "n": { "$gt": 5 } } },
                doc! { "status": 1, "at": -1 },
            ),
            (text(doc! { "_fts": "text", "_ftsx": 1 }, "posts", doc! { "body": 1, "title": 10 }), doc! { "body": "text", "title": "text" }),
            (text(doc! { "tenant": 1, "_fts": "text", "_ftsx": 1, "at": -1 }, "tenant_text", doc! { "body": 1 }), doc! { "tenant": 1, "body": "text", "at": -1 }),
            (text(doc! { "_fts": "text", "_ftsx": 1 }, "$**_text", doc! { "$**": 1 }), doc! { "$**": "text" }),
            (doc! { "v": 2, "key": { "loc": "2dsphere" }, "name": "loc", "2dsphereIndexVersion": 3 }, doc! { "loc": "2dsphere" }),
            (doc! { "v": 2, "key": { "pos": "2d" }, "name": "pos", "bits": 20, "min": -90.0, "max": 90.0 }, doc! { "pos": "2d" }),
            (doc! { "v": 2, "key": { "userId": "hashed" }, "name": "userId_hashed" }, doc! { "userId": "hashed" }),
            (doc! { "v": 2, "key": { "$**": 1 }, "name": "$**_1" }, doc! { "$**": 1 }),
            (doc! { "v": 2, "key": { "attrs.$**": 1 }, "name": "attrs" }, doc! { "attrs.$**": 1 }),
            (doc! { "v": 2, "key": { "name": 1 }, "name": "name_th", "collation": listed_collation("th", doc! {}) }, doc! { "name": 1 }),
            (
                doc! { "v": 2, "key": { "name": 1 }, "name": "name_th_2", "collation": listed_collation("th", doc! { "strength": 2, "caseLevel": true, "caseFirst": "upper", "numericOrdering": true }) },
                doc! { "name": 1 },
            ),
            (doc! { "v": 2, "key": { "title": 1 }, "name": "title_fr", "collation": listed_collation("fr_CA", doc! { "backwards": true }) }, doc! { "title": 1 }),
            (doc! { "v": 2, "key": { "code": 1 }, "name": "code", "collation": { "locale": "simple" } }, doc! { "code": 1 }),
        ];
        for (listed, keys) in cases {
            let (f, left) = IndexForm::filled(Namespace::new("db", "c"), vec![], &listed);
            let (k, o) = f.definition().unwrap_or_else(|e| panic!("{listed}: {e}"));
            assert_eq!(in_order(&k), in_order(&keys), "the keys of {listed}");
            assert_eq!(norm(&Bson::Document(o)), norm(&Bson::Document(as_sent(&listed))), "the options of {listed}");
            assert!(left.no_place.is_empty(), "{listed}: nothing without a place: {left:?}");
        }
    }

    /// Old servers kept flags as numbers (`unique: 1`): read as set.
    #[test]
    fn numeric_flags_are_read_as_set() {
        let listed = doc! { "v": 1, "key": { "a": 1 }, "name": "a_1", "unique": 1, "sparse": 1.0, "hidden": 0 };
        let (f, _) = IndexForm::filled(Namespace::new("db", "c"), vec![], &listed);
        assert!(f.unique && f.sparse && !f.hidden);
        assert_eq!(f.definition().unwrap().1, doc! { "name": "a_1", "unique": true, "sparse": true });
    }

    /// The texts a few frames of the form draw.
    fn drawn(f: &mut IndexForm) -> Vec<String> {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut texts = Vec::new();
        for _ in 0..2 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| f.ui(ui));
            out.textures_delta.clear();
            texts = out.shapes.iter().filter_map(|s| match &s.shape {
                egui::Shape::Text(t) => Some(t.galley.text().to_string()),
                _ => None,
            }).collect();
        }
        texts
    }

    /// A collation's fields the form has no place for (Thai's `alternate`
    /// and `normalization`) go along as listed while the locale stays; the
    /// Collation tab says so. Another locale leaves them out (they were
    /// that locale's), and says that too.
    #[test]
    fn collation_fields_without_a_place_follow_their_locale() {
        let listed = doc! { "v": 2, "key": { "name": 1 }, "name": "name_th", "collation": listed_collation("th", doc! {}) };
        let mut f = IndexForm::add_as_new(Namespace::new("db", "c"), vec![], &listed);
        f.tab = Tab::Collation;
        let collation = |f: &IndexForm| f.definition().unwrap().1.get_document("collation").unwrap().clone();
        assert_eq!(collation(&f), doc! { "locale": "th", "strength": 3, "alternate": "shifted", "maxVariable": "punct", "normalization": true, "backwards": false });
        let texts = drawn(&mut f);
        assert!(texts.iter().any(|t| t.contains("as listed") && t.contains("alternate")), "{texts:?}");
        assert!(!texts.iter().any(|t| t.starts_with("Not carried over (the form has no place")), "nothing false to warn of: {texts:?}");

        f.locale = "en".into();
        assert_eq!(collation(&f), doc! { "locale": "en", "strength": 3 }, "Thai's settings stay with Thai");
        let texts = drawn(&mut f);
        assert!(texts.iter().any(|t| t.contains("Left out") && t.contains("th") && t.contains("alternate")), "{texts:?}");
        f.locale = "th".into();
        assert!(collation(&f).contains_key("alternate"), "back to Thai, back with them");
    }

    /// Add as New Index… of an index named by MongoDB's default: with no
    /// name, the new index would take that same name, which the server
    /// refuses (or, unchanged, builds nothing). Create index waits for a
    /// name or other keys, and says why.
    #[test]
    fn a_new_index_may_not_take_the_old_ones_default_name() {
        let ns = || Namespace::new("db", "c");
        let mut f = IndexForm::add_as_new(ns(), vec![], &doc! { "v": 2, "key": { "email": 1 }, "name": "email_1" });
        assert!(!f.ready(), "its default name would be email_1");
        assert!(drawn(&mut f).iter().any(|t| t.contains("email_1") && t.contains("type a name")), "says why");
        f.unique = true;
        assert!(!f.ready(), "options don't change the name");
        f.name = "email_unique".into();
        assert!(f.ready(), "named");
        assert!(!drawn(&mut f).iter().any(|t| t.contains("type a name")));
        f.name.clear();
        f.fields.push(("at".into(), KeyType::Desc));
        assert!(f.ready(), "other keys, another default name");
        let f = IndexForm::add_as_new(ns(), vec![], &doc! { "v": 2, "key": { "email": 1 }, "name": "by_email" });
        assert!(f.ready(), "a name of its own leaves the default free");
    }

    /// With the same keys, collation and partial filter as the old index,
    /// Add as New Index… notes that MongoDB keeps one index per key pattern
    /// unless the collation or partial filter differ, so changing only
    /// options such as Unique needs the old index dropped first. Other
    /// keys, collation or partial filter, and the note goes.
    #[test]
    fn a_new_index_with_the_same_key_pattern_is_noted() {
        let listed = doc! { "v": 2, "key": { "email": 1 }, "name": "by_email", "partialFilterExpression": { "live": true } };
        let mut f = IndexForm::add_as_new(Namespace::new("db", "c"), vec![], &listed);
        let noted = |f: &mut IndexForm| drawn(f).iter().any(|t| t.contains("one index per key pattern") && t.contains("by_email"));
        assert!(noted(&mut f));
        f.unique = true;
        assert!(noted(&mut f), "Unique alone doesn't make another index");
        f.partial = "{ live: false }".into();
        assert!(!noted(&mut f), "another partial filter");
        f.partial = "{ live: true }".into();
        f.collation_on = true;
        assert!(!noted(&mut f), "another collation");
        f.collation_on = false;
        f.fields[0].1 = KeyType::Desc;
        assert!(!noted(&mut f), "other keys");
        let mut plain = IndexForm::new(Namespace::new("db", "c"), vec![]);
        assert!(!noted(&mut plain), "Add Index has nothing to compare with");
    }

    /// What the server sets itself, and what the form has no place for,
    /// is named rather than left out without a word.
    #[test]
    fn filling_names_what_it_leaves_out() {
        let ns = || Namespace::new("db", "c");
        // A collation as a server lists it: every field, defaults included.
        let collation = doc! {
            "locale": "en", "caseLevel": false, "caseFirst": "off", "strength": 3, "numericOrdering": false,
            "alternate": "shifted", "maxVariable": "punct", "normalization": false, "backwards": false, "version": "57.1"
        };
        let listed = doc! { "v": 2, "key": { "attrs.$**": 1 }, "name": "attrs", "wildcardProjection": { "attrs.secret": 0 }, "collation": collation };
        let (f, left) = IndexForm::filled(ns(), vec![], &listed);
        assert_eq!(left.no_place, ["wildcardProjection"]);
        assert_eq!(left.by_server, ["v", "collation.version"]);
        assert!(f.collation_on && f.locale == "en" && f.strength == 3 && f.case_first == "off" && !f.case_level && !f.numeric_ordering);
        let sent = f.definition().unwrap().1;
        assert_eq!(sent.get_document("collation").unwrap().get_str("alternate").ok(), Some("shifted"), "carried as listed: {sent}");

        // A key of a kind the form doesn't offer is named, not guessed.
        let (f, left) = IndexForm::filled(ns(), vec![], &doc! { "v": 1, "key": { "pos": "geoHaystack", "type": 1 }, "name": "old", "bucketSize": 1, "textIndexVersion": 3 });
        assert_eq!(f.fields, [("type".to_string(), KeyType::Asc)]);
        assert_eq!(left.no_place, ["key pos: \"geoHaystack\"", "bucketSize"]);
        assert_eq!(left.by_server, ["v", "textIndexVersion"]);
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
