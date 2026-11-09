//! Query evaluation for the mock server: filters, sort order, projection,
//! update operators and a subset of aggregation stages. Unsupported operators
//! are reported as errors instead of silently ignored.

use std::cmp::Ordering;

use bson::{Bson, Document};

use crate::error::{CoreError, CoreResult};

fn unsupported(what: &str) -> CoreError {
    CoreError::InvalidInput(format!("the demo server does not support {what}"))
}

// ---------------------------------------------------------------- values

fn as_f64(v: &Bson) -> Option<f64> {
    match v {
        Bson::Int32(i) => Some(f64::from(*i)),
        Bson::Int64(i) => Some(*i as f64),
        Bson::Double(f) => Some(*f),
        Bson::Decimal128(d) => d.to_string().parse().ok(),
        _ => None,
    }
}

/// Canonical BSON type order used for sorting values of different types.
fn type_rank(v: &Bson) -> u8 {
    match v {
        Bson::MinKey => 0,
        Bson::Null | Bson::Undefined => 1,
        Bson::Int32(_) | Bson::Int64(_) | Bson::Double(_) | Bson::Decimal128(_) => 2,
        Bson::String(_) | Bson::Symbol(_) => 3,
        Bson::Document(_) => 4,
        Bson::Array(_) => 5,
        Bson::Binary(_) => 6,
        Bson::ObjectId(_) => 7,
        Bson::Boolean(_) => 8,
        Bson::DateTime(_) => 9,
        Bson::Timestamp(_) => 10,
        Bson::RegularExpression(_) => 11,
        Bson::MaxKey => 13,
        _ => 12,
    }
}

/// Total order over BSON values, as used by `sort`.
pub fn total_cmp(a: &Bson, b: &Bson) -> Ordering {
    let (ra, rb) = (type_rank(a), type_rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    same_class_cmp(a, b).unwrap_or(Ordering::Equal)
}

fn same_class_cmp(a: &Bson, b: &Bson) -> Option<Ordering> {
    if let (Some(x), Some(y)) = (as_f64(a), as_f64(b)) {
        return x.partial_cmp(&y);
    }
    Some(match (a, b) {
        (Bson::String(x) | Bson::Symbol(x), Bson::String(y) | Bson::Symbol(y)) => x.cmp(y),
        (Bson::ObjectId(x), Bson::ObjectId(y)) => x.bytes().cmp(&y.bytes()),
        (Bson::Boolean(x), Bson::Boolean(y)) => x.cmp(y),
        (Bson::DateTime(x), Bson::DateTime(y)) => x.cmp(y),
        (Bson::Timestamp(x), Bson::Timestamp(y)) => (x.time, x.increment).cmp(&(y.time, y.increment)),
        (Bson::Null | Bson::Undefined, Bson::Null | Bson::Undefined) => Ordering::Equal,
        (Bson::MinKey, Bson::MinKey) | (Bson::MaxKey, Bson::MaxKey) => Ordering::Equal,
        (Bson::Binary(x), Bson::Binary(y)) => x.bytes.cmp(&y.bytes),
        (Bson::Document(x), Bson::Document(y)) => {
            for ((ka, va), (kb, vb)) in x.iter().zip(y.iter()) {
                let o = ka.cmp(kb).then_with(|| total_cmp(va, vb));
                if o != Ordering::Equal {
                    return Some(o);
                }
            }
            x.len().cmp(&y.len())
        }
        (Bson::Array(x), Bson::Array(y)) => {
            for (va, vb) in x.iter().zip(y.iter()) {
                let o = total_cmp(va, vb);
                if o != Ordering::Equal {
                    return Some(o);
                }
            }
            x.len().cmp(&y.len())
        }
        _ => return None,
    })
}

pub fn bson_eq(a: &Bson, b: &Bson) -> bool {
    match (as_f64(a), as_f64(b)) {
        (Some(x), Some(y)) => x == y,
        _ => type_rank(a) == type_rank(b) && same_class_cmp(a, b) == Some(Ordering::Equal),
    }
}

/// Values at a dotted path. Arrays along the way fan out, as in MongoDB.
pub fn lookup<'a>(doc: &'a Document, path: &str) -> Vec<&'a Bson> {
    let mut out = Vec::new();
    let mut parts = path.split('.');
    let Some(first) = parts.next() else { return out };
    let rest: Vec<&str> = parts.collect();
    if let Some(v) = doc.get(first) {
        descend(v, &rest, &mut out);
    }
    out
}

fn descend<'a>(v: &'a Bson, rest: &[&str], out: &mut Vec<&'a Bson>) {
    let Some((head, tail)) = rest.split_first() else {
        out.push(v);
        return;
    };
    match v {
        Bson::Document(d) => {
            if let Some(next) = d.get(*head) {
                descend(next, tail, out);
            }
        }
        Bson::Array(items) => {
            if let Ok(i) = head.parse::<usize>() {
                if let Some(next) = items.get(i) {
                    descend(next, tail, out);
                }
            } else {
                for item in items {
                    if let Bson::Document(d) = item
                        && let Some(next) = d.get(*head)
                    {
                        descend(next, tail, out);
                    }
                }
            }
        }
        _ => {}
    }
}

/// First value at a dotted path, without array fan-out beyond indices.
pub fn get_path<'a>(doc: &'a Document, path: &str) -> Option<&'a Bson> {
    lookup(doc, path).into_iter().next()
}

// ---------------------------------------------------------------- filters

pub fn matches(doc: &Document, filter: &Document) -> CoreResult<bool> {
    for (key, cond) in filter {
        let ok = match key.as_str() {
            "$and" => logical(doc, cond)?.iter().all(|b| *b),
            "$or" => logical(doc, cond)?.iter().any(|b| *b),
            "$nor" => !logical(doc, cond)?.iter().any(|b| *b),
            "$comment" => true,
            k if k.starts_with('$') => return Err(unsupported(k)),
            path => field_matches(&lookup(doc, path), cond)?,
        };
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

fn logical(doc: &Document, cond: &Bson) -> CoreResult<Vec<bool>> {
    let Bson::Array(items) = cond else {
        return Err(CoreError::InvalidInput("$and/$or/$nor need an array".into()));
    };
    items
        .iter()
        .map(|i| match i {
            Bson::Document(f) => matches(doc, f),
            _ => Err(CoreError::InvalidInput("$and/$or/$nor entries must be objects".into())),
        })
        .collect()
}

fn is_operator_doc(cond: &Bson) -> Option<&Document> {
    match cond {
        Bson::Document(d) if d.keys().next().is_some_and(|k| k.starts_with('$')) => Some(d),
        _ => None,
    }
}

/// Whether the values found at a path satisfy `cond`.
fn field_matches(values: &[&Bson], cond: &Bson) -> CoreResult<bool> {
    if let Some(ops) = is_operator_doc(cond) {
        for (op, arg) in ops {
            if !operator_matches(values, op, arg, ops)? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    if let Bson::RegularExpression(re) = cond {
        let re = compile_regex(re.pattern.as_str(), re.options.as_str())?;
        return Ok(any_scalar(values, |v| matches!(v, Bson::String(s) if re.is_match(s))));
    }
    if values.is_empty() {
        return Ok(matches!(cond, Bson::Null));
    }
    Ok(any_scalar(values, |v| bson_eq(v, cond)) || values.iter().any(|v| bson_eq(v, cond)))
}

/// Apply `pred` to each value, and to each element of array values.
fn any_scalar(values: &[&Bson], pred: impl Fn(&Bson) -> bool) -> bool {
    values.iter().any(|v| match v {
        Bson::Array(items) => pred(v) || items.iter().any(&pred),
        _ => pred(v),
    })
}

fn compile_regex(pattern: &str, options: &str) -> CoreResult<regex::Regex> {
    let mut b = regex::RegexBuilder::new(pattern);
    for c in options.chars() {
        match c {
            'i' => b.case_insensitive(true),
            'm' => b.multi_line(true),
            's' => b.dot_matches_new_line(true),
            'x' => b.ignore_whitespace(true),
            _ => &mut b,
        };
    }
    b.build().map_err(|e| CoreError::InvalidInput(format!("bad regex: {e}")))
}

fn operator_matches(values: &[&Bson], op: &str, arg: &Bson, all: &Document) -> CoreResult<bool> {
    let cmp = |want: fn(Ordering) -> bool| {
        any_scalar(values, |v| type_rank(v) == type_rank(arg) && same_class_cmp(v, arg).is_some_and(want))
    };
    Ok(match op {
        "$eq" => field_matches(values, arg)?,
        "$ne" => !field_matches(values, arg)?,
        "$gt" => cmp(|o| o == Ordering::Greater),
        "$gte" => cmp(|o| o != Ordering::Less),
        "$lt" => cmp(|o| o == Ordering::Less),
        "$lte" => cmp(|o| o != Ordering::Greater),
        "$in" | "$nin" => {
            let Bson::Array(options) = arg else {
                return Err(CoreError::InvalidInput(format!("{op} needs an array")));
            };
            let mut hit = false;
            for o in options {
                if field_matches(values, o)? {
                    hit = true;
                    break;
                }
            }
            if op == "$in" { hit } else { !hit }
        }
        "$exists" => {
            let want = !matches!(arg, Bson::Boolean(false) | Bson::Int32(0) | Bson::Int64(0));
            values.is_empty() != want
        }
        "$regex" => {
            let (pattern, mut options) = match arg {
                Bson::String(s) => (s.clone(), String::new()),
                Bson::RegularExpression(r) => (r.pattern.as_str().to_string(), r.options.as_str().to_string()),
                _ => return Err(CoreError::InvalidInput("$regex needs a string".into())),
            };
            if let Ok(o) = all.get_str("$options") {
                options.push_str(o);
            }
            let re = compile_regex(&pattern, &options)?;
            any_scalar(values, |v| matches!(v, Bson::String(s) if re.is_match(s)))
        }
        "$options" => true,
        "$size" => {
            let n = as_f64(arg).ok_or_else(|| CoreError::InvalidInput("$size needs a number".into()))?;
            values.iter().any(|v| matches!(v, Bson::Array(a) if a.len() as f64 == n))
        }
        "$all" => {
            let Bson::Array(wanted) = arg else {
                return Err(CoreError::InvalidInput("$all needs an array".into()));
            };
            let mut ok = true;
            for w in wanted {
                ok &= field_matches(values, w)?;
            }
            ok
        }
        "$elemMatch" => {
            let Bson::Document(sub) = arg else {
                return Err(CoreError::InvalidInput("$elemMatch needs an object".into()));
            };
            let mut hit = false;
            for v in values {
                if let Bson::Array(items) = v {
                    for item in items {
                        hit |= match item {
                            Bson::Document(d) if is_operator_doc(arg).is_none() => matches(d, sub)?,
                            other => field_matches(&[other], arg)?,
                        };
                    }
                }
            }
            hit
        }
        "$not" => !field_matches(values, arg)?,
        "$type" => {
            let want = match arg {
                Bson::String(s) => s.clone(),
                other => as_f64(other).map(|n| (n as i64).to_string()).unwrap_or_default(),
            };
            values.iter().any(|v| type_alias(v).iter().any(|a| *a == want))
        }
        other => return Err(unsupported(other)),
    })
}

fn type_alias(v: &Bson) -> &'static [&'static str] {
    match v {
        Bson::Double(_) => &["double", "1", "number"],
        Bson::String(_) => &["string", "2"],
        Bson::Document(_) => &["object", "3"],
        Bson::Array(_) => &["array", "4"],
        Bson::Binary(_) => &["binData", "5"],
        Bson::ObjectId(_) => &["objectId", "7"],
        Bson::Boolean(_) => &["bool", "8"],
        Bson::DateTime(_) => &["date", "9"],
        Bson::Null => &["null", "10"],
        Bson::RegularExpression(_) => &["regex", "11"],
        Bson::Int32(_) => &["int", "16", "number"],
        Bson::Timestamp(_) => &["timestamp", "17"],
        Bson::Int64(_) => &["long", "18", "number"],
        Bson::Decimal128(_) => &["decimal", "19", "number"],
        _ => &[],
    }
}

// ---------------------------------------------------------------- sort & projection

pub fn sort_docs(docs: &mut [Document], sort: &Document) {
    let keys: Vec<(String, bool)> = sort
        .iter()
        .map(|(k, v)| (k.clone(), as_f64(v).is_none_or(|n| n >= 0.0)))
        .collect();
    docs.sort_by(|a, b| {
        for (k, asc) in &keys {
            let va = get_path(a, k).cloned().unwrap_or(Bson::Null);
            let vb = get_path(b, k).cloned().unwrap_or(Bson::Null);
            let o = total_cmp(&va, &vb);
            if o != Ordering::Equal {
                return if *asc { o } else { o.reverse() };
            }
        }
        Ordering::Equal
    });
}

fn truthy(v: &Bson) -> bool {
    !matches!(v, Bson::Boolean(false) | Bson::Null | Bson::Undefined) && as_f64(v) != Some(0.0)
}

pub fn project(doc: &Document, projection: &Document) -> CoreResult<Document> {
    if projection.is_empty() {
        return Ok(doc.clone());
    }
    let include_id = projection.get("_id").is_none_or(truthy);
    let fields: Vec<(&String, &Bson)> = projection.iter().filter(|(k, _)| *k != "_id").collect();
    let inclusive = fields.iter().any(|(_, v)| truthy(v) || matches!(v, Bson::String(_) | Bson::Document(_)));
    if fields.iter().any(|(_, v)| !truthy(v)) && inclusive {
        return Err(CoreError::InvalidInput("cannot mix inclusion and exclusion in a projection".into()));
    }
    let mut out = Document::new();
    if inclusive {
        if include_id && let Some(id) = doc.get("_id") {
            out.insert("_id", id.clone());
        }
        for (k, v) in fields {
            match v {
                Bson::String(_) | Bson::Document(_) => {
                    out.insert(k.clone(), eval_expr(doc, v)?);
                }
                _ => {
                    if let Some(found) = get_path(doc, k) {
                        set_path(&mut out, k, found.clone())?;
                    }
                }
            }
        }
    } else {
        out = doc.clone();
        if !include_id {
            out.remove("_id");
        }
        for (k, _) in fields {
            unset_path(&mut out, k);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- paths

pub fn set_path(doc: &mut Document, path: &str, value: Bson) -> CoreResult<()> {
    match path.split_once('.') {
        None => {
            doc.insert(path, value);
            Ok(())
        }
        Some((head, tail)) => {
            let entry = doc.entry(head.to_string()).or_insert_with(|| Bson::Document(Document::new()));
            match entry {
                Bson::Document(sub) => set_path(sub, tail, value),
                Bson::Array(items) => {
                    let (idx, rest) = tail.split_once('.').map_or((tail, None), |(a, b)| (a, Some(b)));
                    let i: usize = idx
                        .parse()
                        .map_err(|_| CoreError::InvalidInput(format!("cannot create field '{idx}' in an array")))?;
                    while items.len() <= i {
                        items.push(Bson::Null);
                    }
                    match rest {
                        None => {
                            items[i] = value;
                            Ok(())
                        }
                        Some(rest) => {
                            if !matches!(items[i], Bson::Document(_)) {
                                items[i] = Bson::Document(Document::new());
                            }
                            let Bson::Document(sub) = &mut items[i] else { unreachable!() };
                            set_path(sub, rest, value)
                        }
                    }
                }
                _ => Err(CoreError::InvalidInput(format!("cannot create field '{tail}' in a non-object"))),
            }
        }
    }
}

pub fn unset_path(doc: &mut Document, path: &str) {
    match path.split_once('.') {
        None => {
            doc.remove(path);
        }
        Some((head, tail)) => {
            if let Some(Bson::Document(sub)) = doc.get_mut(head) {
                unset_path(sub, tail);
            }
        }
    }
}

// ---------------------------------------------------------------- updates

/// Apply an update document (`$set`, `$inc`, …) or treat it as a replacement.
pub fn apply_update(doc: &mut Document, update: &Document) -> CoreResult<()> {
    if !update.keys().all(|k| k.starts_with('$')) {
        return Err(CoreError::InvalidInput("update document must contain only update operators".into()));
    }
    for (op, arg) in update {
        let Bson::Document(fields) = arg else {
            return Err(CoreError::InvalidInput(format!("{op} needs an object")));
        };
        for (path, value) in fields {
            if path == "_id" && op != "$setOnInsert" {
                return Err(CoreError::InvalidInput("the _id field cannot be changed".into()));
            }
            match op.as_str() {
                "$set" => set_path(doc, path, value.clone())?,
                "$setOnInsert" => {}
                "$unset" => unset_path(doc, path),
                "$inc" | "$mul" => {
                    let cur = get_path(doc, path).cloned().unwrap_or(Bson::Int32(if op == "$inc" { 0 } else { 1 }));
                    let next = arith(&cur, value, op == "$inc")?;
                    set_path(doc, path, next)?;
                }
                "$min" | "$max" => {
                    let replace = match get_path(doc, path) {
                        None => true,
                        Some(cur) => {
                            let o = total_cmp(value, cur);
                            if op == "$min" { o == Ordering::Less } else { o == Ordering::Greater }
                        }
                    };
                    if replace {
                        set_path(doc, path, value.clone())?;
                    }
                }
                "$rename" => {
                    let Bson::String(to) = value else {
                        return Err(CoreError::InvalidInput("$rename needs a string".into()));
                    };
                    if let Some(v) = get_path(doc, path).cloned() {
                        unset_path(doc, path);
                        set_path(doc, to, v)?;
                    }
                }
                "$currentDate" => set_path(doc, path, Bson::DateTime(bson::DateTime::now()))?,
                "$push" | "$addToSet" => {
                    let items: Vec<Bson> = match value {
                        Bson::Document(d) if d.contains_key("$each") => match d.get("$each") {
                            Some(Bson::Array(a)) => a.clone(),
                            _ => return Err(CoreError::InvalidInput("$each needs an array".into())),
                        },
                        v => vec![v.clone()],
                    };
                    let mut arr = match get_path(doc, path) {
                        None => Vec::new(),
                        Some(Bson::Array(a)) => a.clone(),
                        Some(_) => return Err(CoreError::InvalidInput(format!("{path} is not an array"))),
                    };
                    for item in items {
                        if op == "$push" || !arr.iter().any(|x| bson_eq(x, &item)) {
                            arr.push(item);
                        }
                    }
                    set_path(doc, path, Bson::Array(arr))?;
                }
                "$pull" => {
                    if let Some(Bson::Array(a)) = get_path(doc, path) {
                        let mut kept = Vec::new();
                        for x in a {
                            let hit = match (x, value) {
                                (Bson::Document(xd), Bson::Document(cond)) if is_operator_doc(value).is_none() => {
                                    matches(xd, cond)?
                                }
                                _ => field_matches(&[x], value)?,
                            };
                            if !hit {
                                kept.push(x.clone());
                            }
                        }
                        set_path(doc, path, Bson::Array(kept))?;
                    }
                }
                "$pop" => {
                    if let Some(Bson::Array(a)) = get_path(doc, path) {
                        let mut a = a.clone();
                        if as_f64(value).is_some_and(|n| n < 0.0) {
                            if !a.is_empty() {
                                a.remove(0);
                            }
                        } else {
                            a.pop();
                        }
                        set_path(doc, path, Bson::Array(a))?;
                    }
                }
                other => return Err(unsupported(other)),
            }
        }
    }
    Ok(())
}

fn arith(cur: &Bson, by: &Bson, add: bool) -> CoreResult<Bson> {
    let bad = || CoreError::InvalidInput("cannot apply arithmetic to a non-numeric value".into());
    Ok(match (cur, by) {
        (Bson::Int32(a), Bson::Int32(b)) => {
            let r = if add { i64::from(*a) + i64::from(*b) } else { i64::from(*a) * i64::from(*b) };
            i32::try_from(r).map(Bson::Int32).unwrap_or(Bson::Int64(r))
        }
        (Bson::Int32(_) | Bson::Int64(_), Bson::Int32(_) | Bson::Int64(_)) => {
            let (a, b) = (as_f64(cur).ok_or_else(bad)? as i64, as_f64(by).ok_or_else(bad)? as i64);
            Bson::Int64(if add { a.wrapping_add(b) } else { a.wrapping_mul(b) })
        }
        _ => {
            let (a, b) = (as_f64(cur).ok_or_else(bad)?, as_f64(by).ok_or_else(bad)?);
            Bson::Double(if add { a + b } else { a * b })
        }
    })
}

// ---------------------------------------------------------------- aggregation

pub fn eval_expr(doc: &Document, e: &Bson) -> CoreResult<Bson> {
    match e {
        Bson::String(s) if s.starts_with("$$") => match s.as_str() {
            "$$ROOT" | "$$CURRENT" => Ok(Bson::Document(doc.clone())),
            _ => Err(unsupported(s)),
        },
        Bson::String(s) if s.starts_with('$') => {
            let found = lookup(doc, &s[1..]);
            Ok(match found.len() {
                0 => Bson::Null,
                1 if !s[1..].contains('.') => found[0].clone(),
                _ if s[1..].contains('.') && found.len() > 1 => Bson::Array(found.into_iter().cloned().collect()),
                _ => found[0].clone(),
            })
        }
        Bson::Document(d) if d.len() == 1 && d.keys().next().is_some_and(|k| k.starts_with('$')) => {
            let (op, arg) = d.iter().next().expect("one key");
            let args: Vec<Bson> = match arg {
                Bson::Array(a) => a.iter().map(|x| eval_expr(doc, x)).collect::<CoreResult<_>>()?,
                other => vec![eval_expr(doc, other)?],
            };
            eval_operator(op, &args)
        }
        Bson::Document(d) => {
            let mut out = Document::new();
            for (k, v) in d {
                out.insert(k.clone(), eval_expr(doc, v)?);
            }
            Ok(Bson::Document(out))
        }
        Bson::Array(a) => Ok(Bson::Array(a.iter().map(|x| eval_expr(doc, x)).collect::<CoreResult<_>>()?)),
        other => Ok(other.clone()),
    }
}

fn eval_operator(op: &str, args: &[Bson]) -> CoreResult<Bson> {
    let num = |i: usize| as_f64(args.get(i).unwrap_or(&Bson::Null));
    Ok(match op {
        "$literal" => args.first().cloned().unwrap_or(Bson::Null),
        "$add" | "$multiply" => {
            let mut acc = if op == "$add" { Bson::Int32(0) } else { Bson::Int32(1) };
            for a in args {
                if matches!(a, Bson::Null) {
                    return Ok(Bson::Null);
                }
                acc = arith(&acc, a, op == "$add")?;
            }
            acc
        }
        "$subtract" => match (num(0), num(1)) {
            (Some(a), Some(b)) => Bson::Double(a - b),
            _ => Bson::Null,
        },
        "$divide" => match (num(0), num(1)) {
            (Some(_), Some(0.0)) => return Err(CoreError::InvalidInput("can't $divide by zero".into())),
            (Some(a), Some(b)) => Bson::Double(a / b),
            _ => Bson::Null,
        },
        "$concat" => {
            let mut s = String::new();
            for a in args {
                match a {
                    Bson::String(x) => s.push_str(x),
                    Bson::Null => return Ok(Bson::Null),
                    _ => return Err(CoreError::InvalidInput("$concat only supports strings".into())),
                }
            }
            Bson::String(s)
        }
        "$toUpper" | "$toLower" => match args.first() {
            Some(Bson::String(s)) => Bson::String(if op == "$toUpper" { s.to_uppercase() } else { s.to_lowercase() }),
            _ => Bson::String(String::new()),
        },
        "$size" => match args.first() {
            Some(Bson::Array(a)) => Bson::Int32(a.len() as i32),
            _ => return Err(CoreError::InvalidInput("$size needs an array".into())),
        },
        "$ifNull" => args.iter().find(|a| !matches!(a, Bson::Null)).cloned().unwrap_or(Bson::Null),
        "$eq" => Bson::Boolean(args.len() == 2 && bson_eq(&args[0], &args[1])),
        "$ne" => Bson::Boolean(!(args.len() == 2 && bson_eq(&args[0], &args[1]))),
        "$gt" | "$gte" | "$lt" | "$lte" if args.len() == 2 => {
            let o = total_cmp(&args[0], &args[1]);
            Bson::Boolean(match op {
                "$gt" => o == Ordering::Greater,
                "$gte" => o != Ordering::Less,
                "$lt" => o == Ordering::Less,
                _ => o != Ordering::Greater,
            })
        }
        "$cond" if args.len() == 3 => {
            if truthy(&args[0]) { args[1].clone() } else { args[2].clone() }
        }
        other => return Err(unsupported(other)),
    })
}

pub fn aggregate(mut docs: Vec<Document>, pipeline: &[Document]) -> CoreResult<Vec<Document>> {
    for stage in pipeline {
        let Some((name, arg)) = stage.iter().next().filter(|_| stage.len() == 1) else {
            return Err(CoreError::InvalidInput("each pipeline stage must have exactly one field".into()));
        };
        docs = match (name.as_str(), arg) {
            ("$match", Bson::Document(f)) => {
                let mut kept = Vec::new();
                for d in docs {
                    if matches(&d, f)? {
                        kept.push(d);
                    }
                }
                kept
            }
            ("$sort", Bson::Document(s)) => {
                sort_docs(&mut docs, s);
                docs
            }
            ("$skip", n) => {
                let n = as_f64(n).unwrap_or(0.0) as usize;
                docs.into_iter().skip(n).collect()
            }
            ("$limit", n) => {
                let n = as_f64(n).unwrap_or(0.0) as usize;
                docs.into_iter().take(n).collect()
            }
            ("$project", Bson::Document(p)) => docs.iter().map(|d| project(d, p)).collect::<CoreResult<_>>()?,
            ("$addFields" | "$set", Bson::Document(fields)) => {
                let mut out = Vec::with_capacity(docs.len());
                for mut d in docs {
                    for (k, e) in fields {
                        let v = eval_expr(&d, e)?;
                        set_path(&mut d, k, v)?;
                    }
                    out.push(d);
                }
                out
            }
            ("$unset", spec) => {
                let fields: Vec<String> = match spec {
                    Bson::String(s) => vec![s.clone()],
                    Bson::Array(a) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
                    _ => return Err(CoreError::InvalidInput("$unset needs a string or array".into())),
                };
                for d in &mut docs {
                    for f in &fields {
                        unset_path(d, f);
                    }
                }
                docs
            }
            ("$count", Bson::String(field)) => {
                let mut d = Document::new();
                d.insert(field.clone(), Bson::Int32(docs.len() as i32));
                if docs.is_empty() { vec![] } else { vec![d] }
            }
            ("$unwind", spec) => {
                let path = match spec {
                    Bson::String(s) => s.clone(),
                    Bson::Document(d) => d.get_str("path").map_err(|_| unsupported("$unwind without path"))?.to_string(),
                    _ => return Err(CoreError::InvalidInput("$unwind needs a path".into())),
                };
                let field = path.trim_start_matches('$').to_string();
                let mut out = Vec::new();
                for d in docs {
                    if let Some(Bson::Array(items)) = get_path(&d, &field).cloned() {
                        for item in items {
                            let mut copy = d.clone();
                            set_path(&mut copy, &field, item)?;
                            out.push(copy);
                        }
                    }
                }
                out
            }
            ("$group", Bson::Document(spec)) => group(docs, spec)?,
            ("$sortByCount", e) => {
                let mut grouped = group(
                    docs,
                    &bson::doc! { "_id": e.clone(), "count": { "$sum": 1 } },
                )?;
                sort_docs(&mut grouped, &bson::doc! { "count": -1 });
                grouped
            }
            (other, _) => return Err(unsupported(&format!("the {other} stage"))),
        };
    }
    Ok(docs)
}

fn group(docs: Vec<Document>, spec: &Document) -> CoreResult<Vec<Document>> {
    let id_expr = spec.get("_id").cloned().unwrap_or(Bson::Null);
    let mut groups: Vec<(Bson, Vec<Document>)> = Vec::new();
    for d in docs {
        let key = eval_expr(&d, &id_expr)?;
        match groups.iter_mut().find(|(k, _)| bson_eq(k, &key)) {
            Some((_, members)) => members.push(d),
            None => groups.push((key, vec![d])),
        }
    }
    let mut out = Vec::with_capacity(groups.len());
    for (key, members) in groups {
        let mut row = Document::new();
        row.insert("_id", key);
        for (field, acc) in spec.iter().filter(|(k, _)| *k != "_id") {
            let Bson::Document(acc) = acc else {
                return Err(CoreError::InvalidInput(format!("group field '{field}' must be an accumulator object")));
            };
            let Some((op, e)) = acc.iter().next() else {
                return Err(CoreError::InvalidInput(format!("group field '{field}' is empty")));
            };
            let values: Vec<Bson> = members.iter().map(|m| eval_expr(m, e)).collect::<CoreResult<_>>()?;
            let v = match op.as_str() {
                "$sum" => {
                    let mut acc = Bson::Int32(0);
                    for v in &values {
                        if as_f64(v).is_some() {
                            acc = arith(&acc, v, true)?;
                        }
                    }
                    acc
                }
                "$avg" => {
                    let nums: Vec<f64> = values.iter().filter_map(as_f64).collect();
                    if nums.is_empty() { Bson::Null } else { Bson::Double(nums.iter().sum::<f64>() / nums.len() as f64) }
                }
                "$min" => values.iter().filter(|v| !matches!(v, Bson::Null)).min_by(|a, b| total_cmp(a, b)).cloned().unwrap_or(Bson::Null),
                "$max" => values.iter().max_by(|a, b| total_cmp(a, b)).cloned().unwrap_or(Bson::Null),
                "$first" => values.first().cloned().unwrap_or(Bson::Null),
                "$last" => values.last().cloned().unwrap_or(Bson::Null),
                "$push" => Bson::Array(values),
                "$addToSet" => {
                    let mut set: Vec<Bson> = Vec::new();
                    for v in values {
                        if !set.iter().any(|x| bson_eq(x, &v)) {
                            set.push(v);
                        }
                    }
                    Bson::Array(set)
                }
                "$count" => Bson::Int32(members.len() as i32),
                other => return Err(unsupported(other)),
            };
            row.insert(field.clone(), v);
        }
        out.push(row);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    fn m(d: Document, f: Document) -> bool {
        matches(&d, &f).unwrap()
    }

    #[test]
    fn equality_and_comparison() {
        let d = doc! { "a": 5, "s": "Hello", "tags": ["x", "y"], "sub": { "n": 2.5 } };
        assert!(m(d.clone(), doc! { "a": 5.0 }));
        assert!(m(d.clone(), doc! { "a": { "$gt": 4, "$lte": 5 } }));
        assert!(!m(d.clone(), doc! { "a": { "$gt": "4" } }));
        assert!(m(d.clone(), doc! { "tags": "y" }));
        assert!(m(d.clone(), doc! { "sub.n": { "$lt": 3 } }));
        assert!(m(d.clone(), doc! { "s": { "$regex": "^hel", "$options": "i" } }));
        assert!(m(d.clone(), doc! { "missing": null }));
        assert!(m(d.clone(), doc! { "missing": { "$exists": false } }));
        assert!(m(d.clone(), doc! { "$or": [ { "a": 1 }, { "tags": { "$in": ["z", "x"] } } ] }));
        assert!(m(d.clone(), doc! { "tags": { "$size": 2 } }));
        assert!(matches(&d, &doc! { "$where": "x" }).is_err());
    }

    #[test]
    fn sort_mixes_types_in_bson_order() {
        let mut docs = vec![doc! { "v": "b" }, doc! { "v": 2 }, doc! {}, doc! { "v": 1.5 }, doc! { "v": "a" }];
        sort_docs(&mut docs, &doc! { "v": 1 });
        let vs: Vec<_> = docs.iter().map(|d| d.get("v").cloned()).collect();
        assert_eq!(vs, [None, Some(Bson::Double(1.5)), Some(Bson::Int32(2)), Some("a".into()), Some("b".into())]);
    }

    #[test]
    fn updates() {
        let mut d = doc! { "_id": 1, "n": 1, "tags": ["a"] };
        apply_update(&mut d, &doc! { "$inc": { "n": 2 }, "$set": { "x.y": true }, "$addToSet": { "tags": "a" }, "$push": { "tags": "b" } }).unwrap();
        assert_eq!(d, doc! { "_id": 1, "n": 3, "tags": ["a", "b"], "x": { "y": true } });
        assert!(apply_update(&mut d, &doc! { "$set": { "_id": 2 } }).is_err());
        assert!(apply_update(&mut d, &doc! { "plain": 1 }).is_err());
    }

    #[test]
    fn projection() {
        let d = doc! { "_id": 1, "a": 1, "b": { "c": 2, "d": 3 } };
        assert_eq!(project(&d, &doc! { "b.c": 1 }).unwrap(), doc! { "_id": 1, "b": { "c": 2 } });
        assert_eq!(project(&d, &doc! { "_id": 0, "b": 0 }).unwrap(), doc! { "a": 1 });
    }

    #[test]
    fn group_and_count() {
        let docs = vec![
            doc! { "k": "a", "v": 1 },
            doc! { "k": "b", "v": 2 },
            doc! { "k": "a", "v": 3 },
        ];
        let out = aggregate(
            docs,
            &[doc! { "$group": { "_id": "$k", "total": { "$sum": "$v" }, "n": { "$sum": 1 } } }, doc! { "$sort": { "_id": 1 } }],
        )
        .unwrap();
        assert_eq!(out, vec![doc! { "_id": "a", "total": 4, "n": 2 }, doc! { "_id": "b", "total": 2, "n": 1 }]);
        assert!(aggregate(vec![], &[doc! { "$lookup": {} }]).is_err());
    }
}
