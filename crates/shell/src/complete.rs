//! Autocomplete for the shell editor, from the text before the cursor.
//!
//! It is textual, not a parser: a small scanner tracks strings, comments and
//! open brackets, which is enough to tell where the cursor is:
//! - after `db.`, `db.coll.` or `….find(…).`: collections and methods;
//! - inside a query object of a collection method (`find({ l|`): the
//!   collection's field names and MongoDB operators;
//! - after `:` in a query: BSON constructors and literals;
//! - inside a string key (`{ "profile.c|`): field paths;
//! - elsewhere: globals and keywords.
//!
//! Field names come from the caller (sampled from the collection), so
//! [`complete`] reports which collection it would like fields for.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Field,
    Collection,
    Method,
    Operator,
    Global,
    Keyword,
}

impl Kind {
    fn order(self) -> u8 {
        match self {
            Kind::Field | Kind::Collection => 0,
            Kind::Method => 1,
            Kind::Operator => 2,
            Kind::Global => 3,
            Kind::Keyword => 4,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    pub label: String,
    /// Text that replaces the typed prefix.
    pub insert: String,
    pub kind: Kind,
    /// Byte range of the typed text within `label`, for highlighting.
    pub matched: Option<(usize, usize)>,
}

#[derive(Debug, Default)]
pub struct Completions {
    /// Byte offset where the replacement starts.
    pub start: usize,
    pub items: Vec<Completion>,
    /// The collection whose field names would improve these suggestions,
    /// when the caller has not supplied them yet.
    pub needs_fields: Option<String>,
}

pub const DB_METHODS: &[&str] = &[
    "getCollection", "getCollectionNames", "getCollectionInfos", "getName", "getSiblingDB", "runCommand",
    "adminCommand", "stats", "version", "serverStatus", "hello", "createCollection", "dropDatabase",
];

pub const COLLECTION_METHODS: &[&str] = &[
    "find", "findOne", "aggregate", "countDocuments", "estimatedDocumentCount", "distinct", "insertOne",
    "insertMany", "updateOne", "updateMany", "replaceOne", "deleteOne", "deleteMany", "createIndex",
    "getIndexes", "dropIndex", "drop", "stats", "getName", "getFullName",
];

pub const CURSOR_METHODS: &[&str] = &[
    "sort", "limit", "skip", "projection", "batchSize", "toArray", "forEach", "map", "hasNext", "next",
    "tryNext", "count", "itcount", "pretty", "close",
];

/// Methods whose object arguments are queries, updates or pipelines over the
/// collection's fields.
const QUERY_METHODS: &[&str] = &[
    "find", "findOne", "aggregate", "countDocuments", "count", "distinct", "updateOne", "updateMany",
    "replaceOne", "deleteOne", "deleteMany", "findOneAndUpdate", "findOneAndReplace", "findOneAndDelete",
    "insertOne", "insertMany", "createIndex", "remove", "update",
];

/// Cursor methods that take field documents (`.sort({ … })`).
const CURSOR_FIELD_METHODS: &[&str] = &["sort", "projection", "project", "hint", "min", "max"];

const GLOBALS: &[&str] = &[
    "db", "ObjectId", "ISODate", "NumberLong", "NumberInt", "NumberDecimal", "Double", "UUID", "BinData",
    "Timestamp", "MinKey", "MaxKey", "print", "printjson", "sleep", "console",
];

const KEYWORDS: &[&str] = &[
    "const", "let", "var", "function", "return", "for", "of", "in", "while", "if", "else", "true", "false",
    "null", "undefined", "new", "break", "continue", "try", "catch", "throw",
];

const VALUES: &[&str] = &[
    "ObjectId", "ISODate", "NumberLong", "NumberInt", "NumberDecimal", "Double", "UUID", "Timestamp", "true",
    "false", "null",
];

/// Query, update and aggregation operators, stages and expressions.
pub const OPERATORS: &[&str] = &[
    // query
    "$eq", "$ne", "$gt", "$gte", "$lt", "$lte", "$in", "$nin", "$and", "$or", "$nor", "$not", "$exists",
    "$type", "$regex", "$options", "$expr", "$jsonSchema", "$mod", "$text", "$search", "$language",
    "$caseSensitive", "$diacriticSensitive", "$where", "$all", "$elemMatch", "$size", "$bitsAllClear",
    "$bitsAllSet", "$bitsAnyClear", "$bitsAnySet", "$geoIntersects", "$geoWithin", "$near", "$nearSphere",
    "$box", "$center", "$centerSphere", "$geometry", "$maxDistance", "$minDistance", "$polygon", "$comment",
    "$meta", "$slice",
    // update
    "$set", "$unset", "$inc", "$mul", "$rename", "$min", "$max", "$currentDate", "$setOnInsert", "$push",
    "$pull", "$pullAll", "$addToSet", "$pop", "$each", "$position", "$sort", "$bit",
    // stages
    "$match", "$group", "$project", "$limit", "$skip", "$unwind", "$lookup", "$addFields", "$replaceRoot",
    "$replaceWith", "$count", "$facet", "$bucket", "$bucketAuto", "$sortByCount", "$sample", "$out",
    "$merge", "$graphLookup", "$unionWith", "$densify", "$fill", "$setWindowFields", "$redact",
    "$geoNear", "$indexStats", "$collStats", "$currentOp", "$listLocalSessions", "$listSessions",
    "$listSampledQueries", "$listSearchIndexes", "$planCacheStats", "$documents", "$changeStream",
    // accumulators
    "$sum", "$avg", "$first", "$last", "$firstN", "$lastN", "$maxN", "$minN", "$top", "$topN", "$bottom",
    "$bottomN", "$stdDevPop", "$stdDevSamp", "$mergeObjects", "$accumulator", "$median", "$percentile",
    // expressions
    "$abs", "$add", "$subtract", "$multiply", "$divide", "$ceil", "$floor", "$round", "$trunc", "$sqrt",
    "$pow", "$exp", "$ln", "$log", "$log10", "$cmp", "$cond", "$ifNull", "$switch", "$concat",
    "$concatArrays", "$substr", "$substrBytes", "$substrCP", "$toUpper", "$toLower", "$trim", "$ltrim",
    "$rtrim", "$split", "$strLenBytes", "$strLenCP", "$strcasecmp", "$indexOfBytes", "$indexOfCP",
    "$regexMatch", "$regexFind", "$regexFindAll", "$replaceOne", "$replaceAll", "$arrayElemAt",
    "$arrayToObject", "$objectToArray", "$filter", "$map", "$reduce", "$zip", "$range", "$reverseArray",
    "$isArray", "$indexOfArray", "$let", "$literal", "$getField", "$setField", "$unsetField",
    "$dateAdd", "$dateSubtract", "$dateDiff", "$dateTrunc", "$dateFromParts", "$dateToParts",
    "$dateFromString", "$dateToString", "$year", "$month", "$week", "$dayOfMonth", "$dayOfWeek",
    "$dayOfYear", "$hour", "$minute", "$second", "$millisecond", "$isoWeek", "$isoWeekYear",
    "$isoDayOfWeek", "$toDate", "$toString", "$toInt", "$toLong", "$toDouble", "$toDecimal", "$toBool",
    "$toObjectId", "$convert", "$isNumber", "$linearFill", "$locf", "$rand", "$sampleRate", "$setUnion",
    "$setIntersection", "$setDifference", "$setEquals", "$setIsSubset", "$anyElementTrue",
    "$allElementsTrue", "$binarySize", "$bsonSize", "$function", "$tsIncrement", "$tsSecond",
];

/// Suggestions for the text before the cursor. `fields` returns the field
/// paths of a collection when known.
pub fn complete(before: &str, collections: &[String], fields: &dyn Fn(&str) -> Option<Vec<String>>) -> Completions {
    let scan = scan(before);
    let mut needs_fields = None;
    let mut field_list = |coll: &str| match fields(coll) {
        Some(f) => f,
        None => {
            needs_fields = Some(coll.to_string());
            Vec::new()
        }
    };

    // Inside a string literal.
    if let Some((quote, quote_at)) = scan.string {
        let start = quote_at + 1;
        let prefix = &before[start..];
        let head = &before[..quote_at];
        // Names come from the database: escape them for the string they land in.
        let in_string = |name: &str| escape_in_string(name, quote as char);
        let mut items = Vec::new();
        if head.trim_end().ends_with("getCollection(") {
            items.extend(collections.iter().map(|c| item(c, &in_string(c), Kind::Collection)));
        } else if let Some(coll) = query_collection(before, &scan.stack) {
            let names = field_list(&coll);
            match prev_char(head) {
                Some('{' | ',') if innermost(&scan.stack) == Some(b'{') => {
                    items.extend(names.iter().map(|f| item(f, &in_string(f), Kind::Field)));
                }
                // "$field" references in aggregation expressions.
                _ if prefix.starts_with('$') => {
                    items.extend(names.iter().map(|f| item(&format!("${f}"), &in_string(&format!("${f}")), Kind::Field)));
                }
                _ => {}
            }
        }
        let items = rank(items, prefix);
        return Completions { start, items, needs_fields };
    }

    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_ident(*c))
        .last()
        .map_or(before.len(), |(i, _)| i);
    let prefix = &before[start..];
    let head = &before[..start];
    let mut items = Vec::new();

    // A dotted key in a query (`{ profile.c`) is a field path, not a member
    // access.
    let dotted_start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_ident(*c) || *c == '.')
        .last()
        .map_or(before.len(), |(i, _)| i);
    if dotted_start < start
        && innermost(&scan.stack) == Some(b'{')
        && matches!(prev_char(&before[..dotted_start]), Some('{' | ','))
        && let Some(coll) = query_collection(before, &scan.stack)
    {
        let dotted = &before[dotted_start..];
        let names = field_list(&coll);
        let items: Vec<Completion> = names.iter().map(|f| item(f, &js_string(f), Kind::Field)).collect();
        return Completions { start: dotted_start, items: rank(items, dotted), needs_fields };
    }

    if let Some(receiver) = head.strip_suffix('.') {
        match receiver_kind(receiver) {
            Receiver::Db => {
                for c in collections {
                    let insert = if is_identifier(c) && !DB_METHODS.contains(&c.as_str()) {
                        c.clone()
                    } else {
                        format!("getCollection({})", js_string(c))
                    };
                    items.push(item(c, &insert, Kind::Collection));
                }
                items.extend(DB_METHODS.iter().map(|m| item(m, m, Kind::Method)));
            }
            Receiver::Collection => items.extend(COLLECTION_METHODS.iter().map(|m| item(m, m, Kind::Method))),
            Receiver::Cursor => items.extend(CURSOR_METHODS.iter().map(|m| item(m, m, Kind::Method))),
            Receiver::Unknown => {}
        }
    } else if let Some(coll) = query_collection(before, &scan.stack).filter(|_| innermost(&scan.stack).is_some()) {
        match (prev_char(head), innermost(&scan.stack)) {
            (Some('{' | ','), Some(b'{')) => {
                let names = field_list(&coll);
                for f in &names {
                    // Dotted paths are not identifiers, so they go in quotes.
                    let insert = if is_identifier(f) { f.clone() } else { js_string(f) };
                    items.push(item(f, &insert, Kind::Field));
                }
                items.extend(OPERATORS.iter().map(|o| item(o, o, Kind::Operator)));
            }
            (Some(':' | '[' | ','), _) => {
                if prefix.starts_with('$') {
                    let names = field_list(&coll);
                    items.extend(names.iter().map(|f| item(&format!("${f}"), &js_string(&format!("${f}")), Kind::Field)));
                    items.extend(OPERATORS.iter().map(|o| item(o, o, Kind::Operator)));
                } else if !prefix.is_empty() {
                    items.extend(VALUES.iter().map(|v| item(v, v, Kind::Global)));
                }
            }
            _ => {}
        }
        if items.is_empty() && !prefix.is_empty() {
            items.extend(GLOBALS.iter().map(|g| item(g, g, Kind::Global)));
        }
    } else if !prefix.is_empty() {
        items.extend(GLOBALS.iter().map(|g| item(g, g, Kind::Global)));
        items.extend(KEYWORDS.iter().map(|k| item(k, k, Kind::Keyword)));
    }
    let items = rank(items, prefix);
    Completions { start, items, needs_fields }
}

/// A double-quoted JavaScript string literal for `s`.
fn js_string(s: &str) -> String {
    // JSON escaping is valid JavaScript; U+2028/2029 are escaped too, since
    // older engines treat them as line breaks inside strings.
    serde_json::to_string(s).unwrap_or_default().replace('\u{2028}', "\\u2028").replace('\u{2029}', "\\u2029")
}

/// `s` escaped to sit inside an already open string delimited by `quote`.
fn escape_in_string(s: &str, quote: char) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '$' if quote == '`' => out.push_str("\\$"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() || c == '\u{2028}' || c == '\u{2029}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn item(label: &str, insert: &str, kind: Kind) -> Completion {
    Completion { label: label.into(), insert: insert.into(), kind, matched: None }
}

/// Keep items that contain the prefix. Match tiers: exact-case prefix,
/// prefix ignoring case or a leading `$`, then anywhere in the label.
fn rank(items: Vec<Completion>, prefix: &str) -> Vec<Completion> {
    let lower = prefix.to_lowercase();
    let mut scored: Vec<(u8, Completion)> = items
        .into_iter()
        .filter_map(|mut c| {
            if prefix.is_empty() {
                return Some((0, c));
            }
            if c.label == prefix {
                return None;
            }
            let label_lower = c.label.to_lowercase();
            let tier = if c.label.starts_with(prefix) {
                c.matched = Some((0, prefix.len()));
                0
            } else if label_lower.starts_with(&lower) {
                c.matched = Some((0, prefix.len()));
                1
            } else if !prefix.starts_with('$') && label_lower.strip_prefix('$').is_some_and(|l| l.starts_with(&lower)) {
                c.matched = Some((1, 1 + prefix.len()));
                1
            } else if let Some(at) = label_lower.find(&lower) {
                c.matched = Some((at, at + prefix.len()));
                2
            } else {
                return None;
            };
            Some((tier, c))
        })
        .collect();
    // Grouped by kind (fields before operators, as Studio 3T lists them),
    // then by how well they match, then alphabetically.
    scored.sort_by_cached_key(|(tier, c)| (c.kind.order(), *tier, c.label.to_lowercase()));
    scored.into_iter().map(|(_, c)| c).take(200).collect()
}

/// What the scanner knows at the end of the text.
struct Scan {
    /// Open brackets: (bracket, byte offset).
    stack: Vec<(u8, usize)>,
    /// An unterminated string: (quote, offset of the quote).
    string: Option<(u8, usize)>,
}

fn scan(src: &str) -> Scan {
    let b = src.as_bytes();
    let mut stack = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            q @ (b'"' | b'\'' | b'`') => {
                let start = i;
                i += 1;
                while i < b.len() && b[i] != q {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                if i >= b.len() {
                    return Scan { stack, string: Some((q, start)) };
                }
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            c @ (b'(' | b'[' | b'{') => stack.push((c, i)),
            c @ (b')' | b']' | b'}') => {
                let open = match c {
                    b')' => b'(',
                    b']' => b'[',
                    _ => b'{',
                };
                if stack.last().is_some_and(|(o, _)| *o == open) {
                    stack.pop();
                }
            }
            _ => {}
        }
        i += 1;
    }
    Scan { stack, string: None }
}

fn innermost(stack: &[(u8, usize)]) -> Option<u8> {
    stack.last().map(|(c, _)| *c)
}

fn prev_char(head: &str) -> Option<char> {
    head.trim_end().chars().last()
}

/// The collection whose query the cursor is inside, if any: the nearest open
/// `(` of a query method, or of a field-taking cursor method on such a call.
fn query_collection(text: &str, stack: &[(u8, usize)]) -> Option<String> {
    let (_, paren) = stack.iter().rev().find(|(c, _)| *c == b'(')?;
    let callee_expr = text[..*paren].trim_end();
    let (receiver, method) = split_last_member(callee_expr)?;
    if QUERY_METHODS.contains(&method) {
        return collection_name(receiver);
    }
    if CURSOR_FIELD_METHODS.contains(&method) {
        return chain_collection(receiver);
    }
    None
}

/// `a.b.method` → (`a.b`, `method`).
fn split_last_member(expr: &str) -> Option<(&str, &str)> {
    let name_start = ident_start(expr);
    let method = &expr[name_start..];
    let receiver = expr[..name_start].strip_suffix('.')?;
    (!method.is_empty()).then_some((receiver.trim_end(), method))
}

/// The collection a cursor chain starts from: `db.x.find({}).sort({})` → `x`.
fn chain_collection(expr: &str) -> Option<String> {
    let expr = expr.trim_end();
    if !expr.ends_with(')') {
        return None;
    }
    let open = matching_open(expr)?;
    let (receiver, method) = split_last_member(expr[..open].trim_end())?;
    if QUERY_METHODS.contains(&method) {
        collection_name(receiver)
    } else {
        chain_collection(receiver)
    }
}

/// `db.coll` or `db.getCollection("coll")` → `coll`.
fn collection_name(expr: &str) -> Option<String> {
    let expr = expr.trim_end();
    if expr.ends_with(')') {
        let open = matching_open(expr)?;
        let (receiver, method) = split_last_member(expr[..open].trim_end())?;
        if method == "getCollection" && matches!(receiver_kind(receiver), Receiver::Db) {
            let arg = expr[open + 1..expr.len() - 1].trim();
            let quoted = arg.len() >= 2 && (arg.starts_with('"') && arg.ends_with('"') || arg.starts_with('\'') && arg.ends_with('\''));
            return quoted.then(|| arg[1..arg.len() - 1].to_string());
        }
        return None;
    }
    let (receiver, name) = split_last_member(expr)?;
    (matches!(receiver_kind(receiver), Receiver::Db) && !DB_METHODS.contains(&name)).then(|| name.to_string())
}

enum Receiver {
    Db,
    Collection,
    Cursor,
    Unknown,
}

/// What the expression ending just before a `.` evaluates to.
fn receiver_kind(expr: &str) -> Receiver {
    let expr = expr.trim_end();
    if expr.ends_with(')') {
        let Some(open) = matching_open(expr) else { return Receiver::Unknown };
        let callee = &expr[..open];
        let name_start = ident_start(callee);
        let name = &callee[name_start..];
        let before_name = &callee[..name_start];
        return match name {
            "getCollection" if receiver_is_db(before_name) => Receiver::Collection,
            "getSiblingDB" => Receiver::Db,
            "find" | "aggregate" => Receiver::Cursor,
            n if CURSOR_METHODS.contains(&n) && !matches!(n, "toArray" | "next" | "tryNext" | "count" | "itcount" | "hasNext" | "map" | "forEach" | "close") => {
                Receiver::Cursor
            }
            _ => Receiver::Unknown,
        };
    }
    let last_start = ident_start(expr);
    let last = &expr[last_start..];
    let before = &expr[..last_start];
    if last == "db" && !before.ends_with('.') {
        return Receiver::Db;
    }
    if !last.is_empty() && receiver_is_db(before) && !DB_METHODS.contains(&last) {
        return Receiver::Collection;
    }
    Receiver::Unknown
}

fn receiver_is_db(before: &str) -> bool {
    before.strip_suffix('.').is_some_and(|b| matches!(receiver_kind(b), Receiver::Db))
}

/// Index of the `(` matching the final `)`, skipping string literals.
fn matching_open(expr: &str) -> Option<usize> {
    let bytes = expr.as_bytes();
    let mut depth = 0i32;
    let mut i = bytes.len();
    let mut in_str: Option<u8> = None;
    while i > 0 {
        i -= 1;
        let b = bytes[i];
        if let Some(q) = in_str {
            if b == q && (i == 0 || bytes[i - 1] != b'\\') {
                in_str = None;
            }
            continue;
        }
        match b {
            b'"' | b'\'' | b'`' => in_str = Some(b),
            b')' | b']' | b'}' => depth += 1,
            b'(' | b'[' | b'{' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Where the identifier that `s` ends with starts (a byte offset on a
/// character boundary; the character before it may be several bytes).
fn ident_start(s: &str) -> usize {
    s.char_indices().rev().find(|(_, c)| !is_ident(*c)).map_or(0, |(i, c)| i + c.len_utf8())
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$') && chars.all(is_ident)
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLLS: [&str; 3] = ["users", "bet-logs", "banners"];

    fn fields(coll: &str) -> Option<Vec<String>> {
        match coll {
            "launch_domain_daily" | "users" => Some(["_id", "launches", "embeddable", "expireAt", "profile", "profile.country"].map(String::from).to_vec()),
            _ => None,
        }
    }

    fn run(text: &str) -> Completions {
        let colls: Vec<String> = COLLS.map(String::from).to_vec();
        complete(text, &colls, &fields)
    }

    fn labels(text: &str) -> Vec<String> {
        run(text).items.into_iter().map(|c| c.label).collect()
    }

    /// Thai tone marks, ์ and ’ are not identifier characters and take
    /// more than one byte: typing `.` after them used to panic.
    #[test]
    fn multibyte_characters_before_a_dot_do_not_panic() {
        for src in ["// ไฟล์.", "// don’t.", "const ผู้ใช้ = 1; ผู้ใช้.", "ไฟล์(", "x = ไฟล์().", "db.ผู้ใช้.fi"] {
            let _ = labels(src);
        }
    }

    #[test]
    fn db_dot_lists_collections_then_methods() {
        let l = labels("db.");
        assert_eq!(&l[..3], ["banners", "bet-logs", "users"]);
        assert!(l.contains(&"getCollection".to_string()));
        // Prefix matches first; names merely containing the text follow.
        assert!(labels("db.b").starts_with(&["banners".to_string(), "bet-logs".to_string()]));
        let c = run("db.bet");
        assert_eq!(c.items[0].insert, "getCollection(\"bet-logs\")");
    }

    #[test]
    fn collection_and_cursor_methods() {
        assert!(labels("db.users.fi").starts_with(&["find".to_string(), "findOne".to_string()]));
        assert!(labels("db.getCollection(\"a.b\").ins").contains(&"insertOne".to_string()));
        assert!(labels("db.users.find({ a: \"(\" }).so").contains(&"sort".to_string()));
        assert!(labels("db.users.find().sort({x:1}).").contains(&"limit".to_string()));
        assert!(labels("db.users.find().toArray().").is_empty());
    }

    #[test]
    fn query_keys_suggest_fields_then_operators_like_studio_3t() {
        // The screenshot: find({l → launches, embeddable, $language, $last, …
        let c = run("db.getCollection(\"launch_domain_daily\").find({l");
        let l: Vec<&str> = c.items.iter().map(|i| i.label.as_str()).collect();
        // Fields first (prefix matches, then ones containing "l"), then operators.
        assert_eq!(&l[..4], ["launches", "embeddable", "profile", "profile.country"]);
        assert!(c.items[4..].iter().all(|i| i.kind == Kind::Operator));
        assert_eq!(l[4..8], ["$language", "$last", "$lastN", "$let"]);
        assert!(l.contains(&"$limit") && l.contains(&"$lt"));
        assert_eq!(c.items[1].matched, Some((8, 9)), "the l in embeddable is highlighted");
        assert_eq!(c.needs_fields, None);
        // After a comma, nested objects and cursor methods still know the collection.
        assert_eq!(labels("db.users.find({ a: 1, exp")[0], "expireAt");
        let ops = run("db.users.find({ profile: { $");
        assert!(ops.items.iter().all(|i| i.kind == Kind::Operator) && ops.items.iter().any(|i| i.label == "$eq"));
        assert_eq!(labels("db.users.find({}).sort({ lau")[0], "launches");
        // Dotted paths are inserted quoted; inside quotes they go in raw.
        let c = run("db.users.find({ profile.c");
        assert!(c.items.iter().any(|i| i.label == "profile.country" && i.insert == "\"profile.country\""));
        let c = run("db.users.find({ \"profile.co");
        assert_eq!((c.items[0].label.as_str(), c.items[0].insert.as_str()), ("profile.country", "profile.country"));
    }

    #[test]
    fn values_and_field_references() {
        assert_eq!(labels("db.users.find({ _id: Obj"), ["ObjectId"]);
        assert!(labels("db.users.find({ at: { $gt: IS")[0] == "ISODate");
        let c = run("db.users.aggregate([{ $group: { _id: \"$pro");
        assert_eq!(c.items[0].label, "$profile");
        let l = labels("db.users.aggregate([{ $group: { total: { $sum: $lau");
        assert_eq!(l[0], "$launches");
    }

    #[test]
    fn asks_for_fields_it_does_not_have() {
        let c = run("db.getCollection(\"bet-logs\").find({ st");
        assert_eq!(c.needs_fields.as_deref(), Some("bet-logs"));
        assert!(c.items.iter().all(|i| i.kind == Kind::Operator));
    }

    /// Field and collection names come from the database, so whatever is
    /// inserted must stay inside its string literal.
    #[test]
    fn hostile_names_are_escaped_wherever_they_land() {
        let evil = "x\"}); db.dropDatabase(); //".to_string();
        let evil_fields = |_: &str| Some(vec![evil.clone(), "a'b".to_string(), "c`${d}".to_string()]);
        let colls = vec!["q\"); db.dropDatabase(); (\"".to_string()];
        let pick = |text: &str, label: &str| -> String {
            let c = complete(text, &colls, &evil_fields);
            c.items.into_iter().find(|i| i.label == label).unwrap_or_else(|| panic!("{label} not offered for {text}")).insert
        };
        // Each insert, completed and run through the real parser, must yield
        // the original name as a string, not code.
        let check = |code: String, expected: &str| {
            let value = crate::parse_value(&code).unwrap_or_else(|e| panic!("{code}: {e}"));
            assert_eq!(value, rongo_core::bson::Bson::String(expected.to_string()), "{code}");
        };
        let dollar = format!("${evil}");
        check(pick("db.u.aggregate([{ $group: { _id: $", &dollar), &dollar);
        check(format!("\"{}\"", pick("db.u.find({ \"x", &evil)), &evil);
        check(format!("'{}'", pick("db.u.find({ 'a", "a'b")), "a'b");
        check(format!("`{}`", pick("db.u.find({ `c", "c`${d}")), "c`${d}");
        check(format!("\"{}\"", pick("db.u.aggregate([{ $group: { _id: \"$", &dollar)), &dollar);
        check(format!("\"{}\"", pick("db.getCollection(\"q", &colls[0])), &colls[0]);
        // Outside a string, a non-identifier name becomes one quoted literal.
        check(pick("db.u.find({ x", &evil), &evil);
    }

    #[test]
    fn getcollection_string_lists_collections() {
        assert_eq!(labels("db.getCollection(\"ba"), ["banners"]);
    }

    #[test]
    fn globals_need_a_prefix() {
        assert!(labels("").is_empty());
        assert_eq!(labels("Obj"), ["ObjectId"]);
        assert_eq!(run("const x = ISO").start, 10);
        assert!(labels("print(\"db.users.find({ l").is_empty(), "inside an unrelated string");
    }
}
