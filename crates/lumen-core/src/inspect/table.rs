//! One CSV row per file: the capture summary, every EXIF tag and the file and
//! encoding facts, with the union of columns across all files.

use super::{Field, Inspection};
use crate::workbench::csv_row;

/// Groups of `Inspection::other` that go into the table; the rest (XMP, maker
/// note, PNG text and so on) differ too much between files to make columns.
const OTHER_GROUPS: [&str; 3] = ["File", "Encoding", "Color profile (ICC)"];

/// The fields of one file as (column, value), in display order.
fn columns(i: &Inspection) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = i.capture.iter().map(|f: &Field| (f.name.clone(), f.value.clone())).collect();
    for g in i.exif.iter().filter(|g| g.name != "Thumbnail") {
        out.extend(g.fields.iter().map(|f| (format!("{}: {}", g.name, f.name), f.value.clone())));
    }
    for g in i.other.iter().filter(|g| OTHER_GROUPS.contains(&g.name.as_str())) {
        out.extend(g.fields.iter().map(|f| (format!("{}: {}", g.name, f.name), f.value.clone())));
    }
    if !i.warnings.is_empty() {
        out.push(("Warnings".into(), i.warnings.join("; ")));
    }
    out
}

/// CSV of `files` (display name, inspection): a header row, then one row per
/// file. Columns appear in the order they are first seen; a file without a
/// field leaves its cell empty.
pub fn table_csv(files: &[(String, &Inspection)]) -> String {
    let per: Vec<Vec<(String, String)>> = files.iter().map(|(_, i)| columns(i)).collect();
    let mut heads: Vec<String> = Vec::new();
    for c in &per {
        for (h, _) in c {
            if !heads.contains(h) {
                heads.push(h.clone());
            }
        }
    }
    let mut out = String::from("\u{feff}");
    let mut head = vec!["File".to_string()];
    head.extend(heads.iter().cloned());
    out.push_str(&csv_row(&head));
    for ((name, _), cols) in files.iter().zip(&per) {
        let mut row = vec![name.clone()];
        row.extend(heads.iter().map(|h| cols.iter().find(|(k, _)| k == h).map(|(_, v)| v.clone()).unwrap_or_default()));
        out.push_str(&csv_row(&row));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspect::Group;

    fn f(n: &str, v: &str) -> Field {
        Field { name: n.into(), value: v.into(), note: String::new() }
    }

    #[test]
    fn columns_are_the_union_in_first_seen_order() {
        let a = Inspection {
            capture: vec![f("Camera", "Phone A"), f("Exposure", "1/50 s")],
            exif: vec![Group { name: "EXIF".into(), fields: vec![f("FNumber", "f/1.8")] }, Group { name: "Thumbnail".into(), fields: vec![f("Compression", "JPEG")] }],
            other: vec![Group { name: "XMP".into(), fields: vec![f("x", "y")] }, Group { name: "Encoding".into(), fields: vec![f("JPEG quality", "about 92")] }],
            warnings: vec![],
        };
        let b = Inspection { capture: vec![f("Camera", "Phone, B"), f("Night mode / long exposure", "Yes")], exif: vec![], other: vec![], warnings: vec!["Truncated".into()] };
        let csv = table_csv(&[("a.jpg".into(), &a), ("b.jpg".into(), &b)]);
        let lines: Vec<&str> = csv.trim_start_matches('\u{feff}').lines().collect();
        assert_eq!(lines[0], "File,Camera,Exposure,EXIF: FNumber,Encoding: JPEG quality,Night mode / long exposure,Warnings");
        assert_eq!(lines[1], "a.jpg,Phone A,1/50 s,f/1.8,about 92,,");
        assert_eq!(lines[2], "b.jpg,\"Phone, B\",,,,Yes,Truncated");
    }
}
