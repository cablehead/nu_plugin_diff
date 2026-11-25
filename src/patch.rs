//! Unified diff text -> files of display rows. The patch buffer is the
//! document: rows hold spans into it, nothing is copied per line.
//!
//! Hunk bodies are delimited by the counts in their `@@` header, never by
//! guessing from a line's first characters, so a deleted line that reads
//! `-- a/x` or an added `++ b/y` cannot be mistaken for a file header.

use std::sync::Arc;

use crate::doc::{content_key, FileDiff, Row, RowKind, Side};

struct Building {
    file: FileDiff,
    head: Vec<(usize, usize)>,
}

fn new_file(buf: &Arc<str>) -> FileDiff {
    FileDiff {
        path: String::new(),
        old_path: String::new(),
        status: "modified",
        binary: false,
        rows: Vec::new(),
        extra: Vec::new(),
        old: Side::new(buf.clone()),
        new: Side::new(buf.clone()),
        adds: 0,
        dels: 0,
        key: 0,
    }
}

/// `@@ -o[,c] +n[,c] @@` -> (o, oc, n, nc)
fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = line.strip_prefix("@@ -")?;
    let end = rest.find(" @@")?;
    let (old, new) = rest[..end].split_once(" +")?;
    let pair = |s: &str| -> Option<(u32, u32)> {
        match s.split_once(',') {
            Some((a, c)) => Some((a.parse().ok()?, c.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let (o, oc) = pair(old)?;
    let (n, nc) = pair(new)?;
    Some((o, oc, n, nc))
}

fn strip_prefix_path(p: &str) -> &str {
    p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(p)
}

fn finish(b: Building, buf: &str, out: &mut Vec<Arc<FileDiff>>) {
    let mut f = b.file;
    for (s, l) in &b.head {
        let line = &buf[*s..*s + *l];
        if line.starts_with("new file mode") {
            f.status = "added";
        } else if line.starts_with("deleted file mode") {
            f.status = "deleted";
        } else if let Some(p) = line.strip_prefix("rename from ") {
            f.status = "renamed";
            f.old_path = p.to_string();
        } else if let Some(p) = line.strip_prefix("rename to ") {
            f.path = p.to_string();
        } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            f.binary = true;
        } else if let Some(p) = line.strip_prefix("--- ") {
            if f.old_path.is_empty() && p != "/dev/null" {
                f.old_path = strip_prefix_path(p.split('\t').next().unwrap_or(p)).to_string();
            }
        } else if let Some(p) = line.strip_prefix("+++ ") {
            if f.path.is_empty() && p != "/dev/null" {
                f.path = strip_prefix_path(p.split('\t').next().unwrap_or(p)).to_string();
            }
        }
    }
    if f.path.is_empty() {
        f.path = f.old_path.clone();
    }
    if f.old_path.is_empty() {
        f.old_path = f.path.clone();
    }
    if f.binary && f.rows.is_empty() {
        f.extra.push("Binary file not shown".to_string());
        f.rows.push(Row { kind: RowKind::Meta, old: 0, new: 0, line: 0 });
    }
    // a patch file never changes under us: the path and size identify it
    f.key = content_key(&[f.path.as_bytes(), &(f.rows.len() as u64).to_le_bytes()]);
    out.push(Arc::new(f));
}

pub fn parse_patch(text: Arc<str>) -> Result<Vec<Arc<FileDiff>>, String> {
    if text.len() > u32::MAX as usize {
        return Err("patch is larger than 4 GiB".into());
    }
    let buf: &str = &text;
    let bytes = buf.as_bytes();
    let mut out = Vec::new();
    let mut cur: Option<Building> = None;
    // lines still owed to the current hunk body, per side
    let (mut old_left, mut new_left) = (0u32, 0u32);
    let (mut o, mut n) = (0u32, 0u32);

    let mut pos = 0usize;
    while pos < bytes.len() {
        let nl = bytes[pos..].iter().position(|&c| c == b'\n').map(|i| pos + i).unwrap_or(bytes.len());
        let mut end = nl;
        if end > pos && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        let (start, len) = (pos, end - pos);
        let line = &buf[start..end];
        pos = nl + 1;

        let in_body = old_left > 0 || new_left > 0;
        if in_body {
            let b = cur.as_mut().expect("hunk body without a file");
            let f = &mut b.file;
            match bytes.get(start) {
                Some(b'+') => {
                    let i = f.new.push(start + 1, len - 1);
                    f.rows.push(Row { kind: RowKind::Add, old: 0, new: n, line: i });
                    f.adds += 1;
                    n += 1;
                    new_left = new_left.saturating_sub(1);
                }
                Some(b'-') => {
                    let i = f.old.push(start + 1, len - 1);
                    f.rows.push(Row { kind: RowKind::Del, old: o, new: 0, line: i });
                    f.dels += 1;
                    o += 1;
                    old_left = old_left.saturating_sub(1);
                }
                Some(b'\\') => {
                    f.extra.push(line.to_string());
                    f.rows.push(Row { kind: RowKind::Meta, old: 0, new: 0, line: (f.extra.len() - 1) as u32 });
                }
                _ => {
                    // context; some tools strip the lone space off blank lines
                    let (s, l) = if len > 0 && bytes[start] == b' ' { (start + 1, len - 1) } else { (start, len) };
                    f.old.push(s, l);
                    let i = f.new.push(s, l);
                    f.rows.push(Row { kind: RowKind::Ctx, old: o, new: n, line: i });
                    o += 1;
                    n += 1;
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                }
            }
            continue;
        }

        if line.starts_with("diff --git ") {
            if let Some(b) = cur.take() {
                finish(b, buf, &mut out);
            }
            let mut f = new_file(&text);
            // "a/<old> b/<new>": split at the LAST " b/" so paths with
            // spaces survive when old == new
            let rest = &line["diff --git ".len()..];
            if let Some(i) = rest.rfind(" b/") {
                f.old_path = strip_prefix_path(&rest[..i]).to_string();
                f.path = rest[i + 3..].to_string();
            }
            cur = Some(Building { file: f, head: Vec::new() });
        } else if let Some((ho, oc, hn, nc)) = parse_hunk_header(line) {
            // a bare `--- a` / `+++ b` / `@@` patch has no "diff --git" line
            let b = cur.get_or_insert_with(|| Building { file: new_file(&text), head: Vec::new() });
            let f = &mut b.file;
            f.extra.push(line.to_string());
            f.rows.push(Row { kind: RowKind::Hunk, old: 0, new: 0, line: (f.extra.len() - 1) as u32 });
            f.old.resets.push(f.old.lines.len() as u32);
            f.new.resets.push(f.new.lines.len() as u32);
            o = ho;
            n = hn;
            old_left = oc;
            new_left = nc;
        } else if line.starts_with('\\') {
            // "\ No newline at end of file" trailing the last hunk line
            if let Some(b) = cur.as_mut() {
                if !b.file.rows.is_empty() {
                    let f = &mut b.file;
                    f.extra.push(line.to_string());
                    f.rows.push(Row { kind: RowKind::Meta, old: 0, new: 0, line: (f.extra.len() - 1) as u32 });
                }
            }
        } else if line.starts_with("--- ") && cur.as_ref().map(|b| !b.file.rows.is_empty()).unwrap_or(true) {
            // next file of a plain unified diff (no "diff --git" separator)
            if let Some(b) = cur.take() {
                finish(b, buf, &mut out);
            }
            cur = Some(Building { file: new_file(&text), head: vec![(start, len)] });
        } else if let Some(b) = cur.as_mut() {
            if b.file.rows.is_empty() {
                b.head.push((start, len));
            }
        }
    }
    if let Some(b) = cur.take() {
        finish(b, buf, &mut out);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "diff --git a/src/a b.rs b/src/a b.rs\nindex 1..2 100644\n--- a/src/a b.rs\n+++ b/src/a b.rs\n@@ -10,4 +10,5 @@ fn x() {\n ctx one\n-- a/looks like a header\n+++ b/looks like a header\n+added\n ctx two\n\n@@ -40,2 +41,2 @@\n-old\n+new\n\\ No newline at end of file\n keep\ndiff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\n--- a/gone.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-one\n-two\ndiff --git a/o.png b/n.png\nsimilarity index 90%\nrename from o.png\nrename to n.png\nBinary files a/o.png and b/n.png differ\n";

    #[test]
    fn parses_files_rows_and_numbers() {
        let files = parse_patch(Arc::from(SAMPLE)).unwrap();
        assert_eq!(files.len(), 3);

        let f = &files[0];
        assert_eq!(f.path, "src/a b.rs");
        assert_eq!(f.status, "modified");
        // header-lookalike body lines are rows, not headers
        let kinds: Vec<&str> = f.rows.iter().map(|r| r.kind.class()).collect();
        assert_eq!(
            kinds,
            ["hunk-header", "ctx", "del", "add", "add", "ctx", "ctx", "hunk-header", "del", "add", "meta", "ctx"]
        );
        assert_eq!(f.old.line(f.rows[2].line), "- a/looks like a header");
        assert_eq!(f.new.line(f.rows[3].line), "++ b/looks like a header");
        assert_eq!((f.rows[1].old, f.rows[1].new), (10, 10));
        assert_eq!((f.rows[2].old, f.rows[2].new), (11, 0));
        assert_eq!((f.rows[4].old, f.rows[4].new), (0, 12));
        assert_eq!((f.rows[5].old, f.rows[5].new), (12, 13));
        // stripped blank context line still counts on both sides
        assert_eq!((f.rows[6].old, f.rows[6].new), (13, 14));
        assert_eq!((f.rows[8].old, f.rows[9].new), (40, 41));
        assert_eq!((f.rows[11].old, f.rows[11].new), (41, 42));
        assert_eq!((f.adds, f.dels), (3, 2));
        assert_eq!(f.new.resets, vec![0, 5]);

        assert_eq!(files[1].status, "deleted");
        assert_eq!(files[1].rows.len(), 3);
        assert_eq!(files[2].status, "renamed");
        assert_eq!(files[2].title(), "o.png -> n.png");
        assert!(files[2].binary);
        assert_eq!(files[2].rows.len(), 1);
    }

    #[test]
    fn plain_unified_diff_without_git_header() {
        let p = "--- old.txt\t2024-01-01\n+++ new.txt\t2024-01-02\n@@ -1 +1 @@\n-a\n+b\n--- x.txt\n+++ x.txt\n@@ -3,1 +3,2 @@\n c\n+d\n";
        let files = parse_patch(Arc::from(p)).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "new.txt");
        assert_eq!(files[1].path, "x.txt");
        assert_eq!(files[1].rows.len(), 3);
    }

    /// the stress fixture, when present: counts cross-checked with awk
    #[test]
    fn bun_diff_counts() {
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../bun-pr-30412.diff");
        let Ok(text) = std::fs::read_to_string(p) else { return };
        let t = std::time::Instant::now();
        let files = parse_patch(Arc::from(text)).unwrap();
        eprintln!("bun parse: {:?}", t.elapsed());
        assert_eq!(files.len(), 2188);
        assert_eq!(files.iter().map(|f| f.rows.len()).sum::<usize>(), 1_019_102 + files.iter().filter(|f| f.binary).count());
        assert_eq!(files.iter().map(|f| f.adds as u64).sum::<u64>(), 1_009_257);
        assert_eq!(files.iter().map(|f| f.dels as u64).sum::<u64>(), 4_024);
    }
}
