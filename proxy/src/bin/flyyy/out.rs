//! Output: tables and the JSON flag.

use serde_json::Value;

use crate::Json;

/// An image ref shortened for a table: its digest's first 12 hex digits.
pub(crate) fn short(image: &str) -> String {
    match image.split_once("@sha256:") {
        Some((repo, d)) => format!("{repo}@{}", &d[..d.len().min(12)]),
        None => image.to_owned(),
    }
}

/// A JSON value as a table cell: "-" for empty or null.
pub(crate) fn s(v: &Value) -> String {
    match v {
        Value::String(s) if s.is_empty() => "-".into(),
        Value::String(s) => s.clone(),
        Value::Null => "-".into(),
        v => v.to_string(),
    }
}

/// Rows (the first one the header) in columns.
pub(crate) fn table(rows: Vec<Vec<String>>) {
    let widths: Vec<usize> = (0..rows[0].len()).map(|i| rows.iter().map(|r| r[i].len()).max().unwrap_or(0)).collect();
    for r in rows {
        let line: Vec<String> = r.iter().zip(&widths).map(|(c, w)| format!("{c:<w$}")).collect();
        println!("{}", line.join("  ").trim_end());
    }
}

/// The API's JSON as it is with --json, else `table` of it.
pub(crate) fn show(json: &Json, v: &Value, table: &dyn Fn(&Value)) {
    match json.json {
        true => println!("{v}"),
        false => table(v),
    }
}
