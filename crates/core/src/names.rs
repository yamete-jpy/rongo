//! The names MongoDB accepts for collections and databases, checked before
//! a request is sent so the answer is a plain sentence.

/// Whether MongoDB refuses `name` as a collection name: empty, with a `$`
/// or a NUL, or in the reserved `system.` namespace.
pub fn bad_collection_name(name: &str) -> bool {
    name.is_empty() || name.contains('$') || name.contains('\0') || name.starts_with("system.")
}

/// Why `name` can't be a MongoDB database name, if it can't: non-empty,
/// under 64 bytes, none of `/\\. "$` (or a NUL).
pub fn database_name_error(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("it is empty".into());
    }
    if name.len() >= 64 {
        return Some("it is 64 bytes or longer".into());
    }
    name.chars().find(|c| matches!(c, '/' | '\\' | '.' | ' ' | '"' | '$' | '\0')).map(|c| match c {
        ' ' => "it contains a space".into(),
        '\0' => "it contains a NUL".into(),
        c => format!("it contains {c}"),
    })
}
