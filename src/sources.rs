//! Old and new sources -> files of display rows, diffed here with `similar`.
//!
//! Each side keeps its WHOLE text as lines, so highlighting can carry parser
//! state from the top of the file and stay correct across multi-line
//! comments and strings. A file whose inputs are unchanged since the previous
//! snapshot is reused as is (same `Arc`, same highlight cache).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use similar::{Algorithm, DiffTag, TextDiff};

use crate::doc::{content_key, FileDiff, Row, RowKind, Side};

pub struct FileInput {
    pub path: String,
    pub old_path: Option<String>,
    pub old: Option<String>,
    pub new: Option<String>,
    pub binary: bool,
}

fn side_of(text: &str) -> Side {
    let buf: Arc<str> = Arc::from(text);
    let mut side = Side::new(buf);
    let bytes = text.as_bytes();
    let mut pos = 0;
    while pos < bytes.len() {
        let nl = bytes[pos..].iter().position(|&c| c == b'\n').map(|i| pos + i).unwrap_or(bytes.len());
        let mut end = nl;
        if end > pos && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        side.push(pos, end - pos);
        pos = nl + 1;
    }
    side
}

fn looks_binary(s: &str) -> bool {
    s.as_bytes().iter().take(8192).any(|&b| b == 0)
}

fn range_header(start: usize, len: usize) -> String {
    // unified diff convention: an empty range names the line before it
    match len {
        0 => format!("{},0", start),
        1 => format!("{}", start + 1),
        _ => format!("{},{}", start + 1, len),
    }
}

/// (old path, status, content key): everything derivable from the inputs
/// alone, so reuse can be decided before paying for a diff
fn identity(inp: &FileInput, context: usize) -> (String, &'static str, u64) {
    let old_path = inp.old_path.clone().unwrap_or_else(|| inp.path.clone());
    let status = if inp.old.is_none() {
        "added"
    } else if inp.new.is_none() {
        "deleted"
    } else if old_path != inp.path {
        "renamed"
    } else {
        "modified"
    };
    let key = content_key(&[
        inp.path.as_bytes(),
        old_path.as_bytes(),
        status.as_bytes(),
        inp.old.as_deref().unwrap_or("").as_bytes(),
        inp.new.as_deref().unwrap_or("").as_bytes(),
        &(context as u64).to_le_bytes(),
    ]);
    (old_path, status, key)
}

pub fn diff_file(inp: &FileInput, context: usize) -> Result<FileDiff, String> {
    let old_text = inp.old.as_deref().unwrap_or("");
    let new_text = inp.new.as_deref().unwrap_or("");
    if old_text.len() > u32::MAX as usize || new_text.len() > u32::MAX as usize {
        return Err(format!("{}: a side is larger than 4 GiB", inp.path));
    }
    let (old_path, status, key) = identity(inp, context);
    let binary = inp.binary || looks_binary(old_text) || looks_binary(new_text);

    let mut f = FileDiff {
        path: inp.path.clone(),
        old_path,
        status,
        binary,
        rows: Vec::new(),
        extra: Vec::new(),
        old: side_of(if binary { "" } else { old_text }),
        new: side_of(if binary { "" } else { new_text }),
        adds: 0,
        dels: 0,
        key,
    };
    if binary {
        f.extra.push("Binary file not shown".to_string());
        f.rows.push(Row { kind: RowKind::Meta, old: 0, new: 0, line: 0 });
        return Ok(f);
    }

    // Myers with a deadline: past it `similar` settles for a coarser diff
    // instead of stalling the snapshot on one pathological file
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .timeout(Duration::from_secs(2))
        .diff_lines(old_text, new_text);

    for group in diff.grouped_ops(context) {
        let (first, last) = (group.first().unwrap(), group.last().unwrap());
        let (os, ns) = (first.old_range().start, first.new_range().start);
        let (oe, ne) = (last.old_range().end, last.new_range().end);
        f.extra.push(format!("@@ -{} +{} @@", range_header(os, oe - os), range_header(ns, ne - ns)));
        f.rows.push(Row { kind: RowKind::Hunk, old: 0, new: 0, line: (f.extra.len() - 1) as u32 });
        for op in group {
            let (tag, o, n) = op.as_tag_tuple();
            if tag == DiffTag::Equal {
                for (i, j) in o.zip(n) {
                    f.rows.push(Row { kind: RowKind::Ctx, old: i as u32 + 1, new: j as u32 + 1, line: j as u32 });
                }
                continue;
            }
            for i in o {
                f.rows.push(Row { kind: RowKind::Del, old: i as u32 + 1, new: 0, line: i as u32 });
                f.dels += 1;
            }
            for j in n {
                f.rows.push(Row { kind: RowKind::Add, old: 0, new: j as u32 + 1, line: j as u32 });
                f.adds += 1;
            }
        }
    }
    Ok(f)
}

/// Build a snapshot's files, reusing any file of `prev` whose inputs match.
pub fn diff_files(
    inputs: &[FileInput],
    context: usize,
    prev: Option<&[Arc<FileDiff>]>,
) -> Result<(Vec<Arc<FileDiff>>, usize), String> {
    let by_key: HashMap<u64, &Arc<FileDiff>> =
        prev.map(|p| p.iter().map(|f| (f.key, f)).collect()).unwrap_or_default();
    let mut reused = 0;
    let mut out = Vec::with_capacity(inputs.len());
    for inp in inputs {
        let (_, _, key) = identity(inp, context);
        if let Some(f) = by_key.get(&key) {
            reused += 1;
            out.push((*f).clone());
        } else {
            out.push(Arc::new(diff_file(inp, context)?));
        }
    }
    Ok((out, reused))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(path: &str, old: Option<&str>, new: Option<&str>) -> FileInput {
        FileInput { path: path.into(), old_path: None, old: old.map(Into::into), new: new.map(Into::into), binary: false }
    }

    #[test]
    fn diffs_with_context_and_line_numbers() {
        let old: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 15\n", "line fifteen\nextra\n");
        let f = diff_file(&input("a.txt", Some(&old), Some(&new)), 3).unwrap();
        assert_eq!(f.extra[0], "@@ -12,7 +12,8 @@");
        let kinds: Vec<&str> = f.rows.iter().map(|r| r.kind.class()).collect();
        assert_eq!(kinds, ["hunk-header", "ctx", "ctx", "ctx", "del", "add", "add", "ctx", "ctx", "ctx"]);
        assert_eq!((f.rows[4].old, f.rows[4].new), (15, 0));
        assert_eq!(f.old.line(f.rows[4].line), "line 15");
        assert_eq!((f.rows[5].old, f.rows[5].new), (0, 15));
        assert_eq!(f.new.line(f.rows[6].line), "extra");
        assert_eq!((f.rows[9].old, f.rows[9].new), (18, 19));
        assert_eq!((f.adds, f.dels), (2, 1));
        // the whole file is on each side, not just the hunk
        assert_eq!((f.old.lines.len(), f.new.lines.len()), (30, 31));
    }

    #[test]
    fn added_deleted_renamed_binary() {
        assert_eq!(diff_file(&input("n.txt", None, Some("a\nb\n")), 3).unwrap().status, "added");
        let d = diff_file(&input("d.txt", Some("a\n"), None), 3).unwrap();
        assert_eq!((d.status, d.dels), ("deleted", 1));
        let mut r = input("new.rs", Some("x\n"), Some("x\ny\n"));
        r.old_path = Some("old.rs".into());
        assert_eq!(diff_file(&r, 3).unwrap().title(), "old.rs -> new.rs");
        let b = diff_file(&input("b.bin", Some("a\0b"), Some("c")), 3).unwrap();
        assert!(b.binary);
        assert_eq!(b.rows.len(), 1);
    }

    #[test]
    fn unchanged_files_are_shared_between_snapshots() {
        let a = input("a.txt", Some("1\n"), Some("2\n"));
        let b = input("b.txt", Some("1\n"), Some("2\n"));
        let (v1, r1) = diff_files(&[a, b], 3, None).unwrap();
        assert_eq!(r1, 0);
        let a2 = input("a.txt", Some("1\n"), Some("2\n"));
        let b2 = input("b.txt", Some("1\n"), Some("3\n"));
        let (v2, r2) = diff_files(&[a2, b2], 3, Some(&v1)).unwrap();
        assert_eq!(r2, 1);
        assert!(Arc::ptr_eq(&v1[0], &v2[0]));
        assert!(!Arc::ptr_eq(&v1[1], &v2[1]));
    }
}
