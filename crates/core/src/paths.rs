//! Dotted paths into a document (`a.b.0.c`): reading as MongoDB matches
//! (arrays fan out), setting and removing. Used by the demo server and by
//! import/export.

use bson::{Bson, Document};

use crate::error::{CoreError, CoreResult};

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
