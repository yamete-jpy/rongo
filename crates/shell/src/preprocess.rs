//! mongosh accepts a few commands that are not JavaScript (`use x`,
//! `show dbs`). They are rewritten in place, one line for one line, so error
//! line numbers still match the editor.

pub fn preprocess(code: &str) -> String {
    code.split('\n').map(rewrite_line).collect::<Vec<_>>().join("\n")
}

fn rewrite_line(line: &str) -> String {
    let trimmed = line.trim().trim_end_matches(';').trim();
    let mut words = trimmed.split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (Some("use"), Some(name), None) if is_db_name(name) => {
            let quoted = serde_json::to_string(name).unwrap_or_default();
            format!("db = db.getSiblingDB({quoted}); print(\"switched to db \" + {quoted});")
        }
        (Some("show"), Some(what @ ("dbs" | "databases" | "collections" | "tables")), None) => {
            format!("__show(\"{what}\");")
        }
        _ => line.to_string(),
    }
}

fn is_db_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::preprocess;

    #[test]
    fn rewrites_use_and_show_keeping_lines() {
        let src = "use shop\n  show collections;\nconst use_x = 1\nuse a b";
        let out = preprocess(src);
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with("db = db.getSiblingDB(\"shop\")"));
        assert_eq!(lines[1], "__show(\"collections\");");
        assert_eq!(lines[2], "const use_x = 1");
        assert_eq!(lines[3], "use a b");
    }
}
