//! A slice of the document -> positioned HTML.
//!
//! Everything is positioned relative to the window's own top, never in
//! document pixels: the caller places one wrapper at (window top - its scroll
//! offset) and the numbers in here stay small however tall the document is.
//! A file taller than the window is drawn as a box clamped to the window
//! plus `CLAMP`, so no element's height scales with the file either.

use std::fmt::Write;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::doc::{Doc, RowKind, Slice, GAP, HEAD, ROW_H};
use crate::highlight::{escape, line_html, prefetch_around, syntax_for};

const CLAMP: u64 = 4000;

pub struct Window {
    pub html: String,
    pub rows: u64,
}

/// After a render, highlight the blocks just past the window on a background
/// thread, so continued scrolling never waits on a cold block. One prefetch
/// at a time: under a scroll storm the extra ones are dropped, not queued.
pub fn prefetch(doc: &Arc<Doc>, s: &Slice) {
    static BUSY: AtomicBool = AtomicBool::new(false);
    if BUSY.swap(true, Ordering::Acquire) {
        return;
    }
    let doc = doc.clone();
    let secs = s.secs.clone();
    std::thread::spawn(move || {
        for sec in secs.iter().filter(|x| x.b >= x.a) {
            let f = &doc.files[sec.f];
            let Some(syntax) = syntax_for(&f.path) else { continue };
            let rows = &f.rows[sec.a as usize..=sec.b as usize];
            for (side, kinds) in [(&f.new, [RowKind::Ctx, RowKind::Add]), (&f.old, [RowKind::Del, RowKind::Del])] {
                let lines = rows.iter().filter(|r| kinds.contains(&r.kind)).map(|r| r.line);
                if let (Some(lo), Some(hi)) = (lines.clone().min(), lines.max()) {
                    prefetch_around(side, lo, hi, syntax);
                }
            }
        }
        BUSY.store(false, Ordering::Release);
    });
}

/// `file` names the one file a single-file window shows, for the ids.
/// A file's header: its path (both, for a rename), its status, and its counts.
pub fn head_html(f: &crate::doc::FileDiff) -> String {
    let mut h = format!("<div class=\"vhead\"><span class=\"path\">{}</span>", escape(&f.title()));
    if f.status != "modified" {
        let _ = write!(h, "<span class=\"badge {0}\">{0}</span>", f.status);
    }
    let _ = write!(
        h,
        "<span class=\"spacer\"></span><span class=\"adds\">+{}</span><span class=\"dels\">\u{2212}{}</span></div>",
        f.adds, f.dels
    );
    h
}

pub fn render_window(doc: &Doc, s: &Slice, file: Option<usize>) -> Window {
    let mut html = String::new();
    let mut nrows = 0u64;
    for sec in &s.secs {
        let f = &doc.files[sec.f];
        let ft = doc.tops[sec.f];
        let fb = ft + f.height() - GAP;
        let box_top = ft.max(s.top.saturating_sub(CLAMP));
        let box_bot = fb.min(s.bot + CLAMP);
        let (has_head, has_foot) = (box_top == ft, box_bot == fb);
        let _ = write!(
            html,
            "<section id=\"f{0}\" data-file=\"{0}\" class=\"vfile{1}{2}\" style=\"top:{3}px;height:{4}px\">",
            file.unwrap_or(sec.f),
            if has_head { "" } else { " cut-top" },
            if has_foot { "" } else { " cut-bot" },
            box_top as i64 - s.top as i64,
            box_bot - box_top,
        );
        if has_head {
            html.push_str(&head_html(f));
        }
        if sec.b >= sec.a {
            let rows_top = ft + HEAD + sec.a as u64 * ROW_H - box_top;
            let _ = write!(html, "<div class=\"vrows\" style=\"top:{rows_top}px\">");
            let syntax = syntax_for(&f.path);
            for i in sec.a..=sec.b {
                let r = f.rows[i as usize];
                let code = match r.kind {
                    RowKind::Hunk | RowKind::Meta => escape(&f.extra[r.line as usize]),
                    RowKind::Del => line_html(&f.old, r.line, syntax),
                    RowKind::Ctx | RowKind::Add => line_html(&f.new, r.line, syntax),
                };
                let num = |n: u32| if n == 0 { String::new() } else { n.to_string() };
                let _ = write!(
                    html,
                    "<div id=\"r{}-{}\" class=\"row {}\"><div class=\"no\">{}</div><div class=\"no\">{}</div><div class=\"sign\">{}</div><div class=\"code\">{}</div></div>",
                    file.unwrap_or(sec.f), i, r.kind.class(), num(r.old), num(r.new), r.kind.sign(), code
                );
                nrows += 1;
            }
            html.push_str("</div>");
        }
        html.push_str("</section>");
    }
    Window { html, rows: nrows }
}

/// The changed files as a tree, as HTML: folders before files, each sorted
/// by name, a folder with one folder inside it shown as one row ("src/a/"),
/// totals on every folder, and the status as a letter in its kind's color.
/// `loaded` false marks the totals as still growing.
pub fn tree_html(doc: &Doc, loaded: bool) -> String {
    #[derive(Default)]
    struct Dir {
        dirs: std::collections::BTreeMap<String, Dir>,
        files: Vec<(String, usize)>,
        adds: u64,
        dels: u64,
    }
    let mut root = Dir::default();
    for (i, f) in doc.files.iter().enumerate() {
        let parts: Vec<&str> = f.path.split('/').collect();
        let mut d = &mut root;
        d.adds += f.adds as u64;
        d.dels += f.dels as u64;
        for p in &parts[..parts.len() - 1] {
            d = d.dirs.entry((*p).to_string()).or_default();
            d.adds += f.adds as u64;
            d.dels += f.dels as u64;
        }
        d.files.push((parts[parts.len() - 1].to_string(), i));
    }
    fn counts(out: &mut String, adds: u64, dels: u64) {
        let _ = write!(out, "<span class=\"n\"><span class=\"adds\">+{adds}</span><span class=\"dels\">\u{2212}{dels}</span></span>");
    }
    fn walk(out: &mut String, doc: &Doc, d: &Dir) {
        out.push_str("<ul>");
        for (name, sub) in &d.dirs {
            // fold a chain of lone folders into one row
            let (mut label, mut sub) = (format!("{name}/"), sub);
            while sub.files.is_empty() && sub.dirs.len() == 1 {
                let (n, s) = sub.dirs.iter().next().unwrap();
                label.push_str(n);
                label.push('/');
                sub = s;
            }
            let _ = write!(out, "<li class=\"dir\"><details open><summary><span class=\"name\">{}</span>", escape(&label));
            counts(out, sub.adds, sub.dels);
            out.push_str("</summary>");
            walk(out, doc, sub);
            out.push_str("</details></li>");
        }
        let mut files: Vec<&(String, usize)> = d.files.iter().collect();
        files.sort();
        for (name, i) in files {
            let f = &doc.files[*i];
            let (letter, kind) = match f.status {
                "added" => ("A", "added"),
                "deleted" => ("D", "deleted"),
                "renamed" => ("R", "renamed"),
                _ => ("M", "modified"),
            };
            let title = if f.status == "renamed" { format!(" title=\"from {}\"", escape(&f.old_path)) } else { String::new() };
            let _ = write!(
                out,
                "<li class=\"file\" data-file=\"{i}\" data-top=\"{}\" data-path=\"{}\"{title}><span class=\"st {kind}\">{letter}</span><span class=\"name\">{}</span>",
                doc.tops[*i],
                escape(&f.path),
                escape(name)
            );
            if f.binary {
                out.push_str("<span class=\"n\">binary</span>");
            } else {
                counts(out, f.adds as u64, f.dels as u64);
            }
            out.push_str("</li>");
        }
        out.push_str("</ul>");
    }
    let mut out = String::new();
    let _ = write!(
        out,
        "<div class=\"dsum{}\"><span>{} files</span>",
        if loaded { "" } else { " loading" },
        doc.files.len()
    );
    counts(&mut out, root.adds, root.dels);
    out.push_str("</div>");
    walk(&mut out, doc, &root);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::parse_patch;
    use std::sync::Arc;

    fn doc(patch: &str) -> Doc {
        Doc::new("t".into(), 1, "patch", parse_patch(Arc::from(patch)).unwrap())
    }

    fn big_patch(files: usize, lines: usize) -> String {
        let mut p = String::new();
        for f in 0..files {
            let _ = write!(p, "diff --git a/f{f}.rs b/f{f}.rs\nnew file mode 100644\n--- /dev/null\n+++ b/f{f}.rs\n@@ -0,0 +1,{lines} @@\n");
            for l in 0..lines {
                let _ = writeln!(p, "+let v{l} = \"<{f}>\";");
            }
        }
        p
    }

    #[test]
    fn window_covers_the_viewport_with_small_coordinates() {
        let d = doc(&big_patch(3, 5000));
        assert_eq!(d.rows, 3 * 5001);
        let y = d.total / 2;
        let s = d.slice(y, 900, 0);
        assert!(s.top <= y && s.bot >= y + 900);
        let w = render_window(&d, &s, None);
        // every visible row of the viewport is in the html
        for sec in d.visible(y, y + 900) {
            for i in sec.a..=sec.b {
                assert!(w.html.contains(&format!("id=\"r{}-{}\"", sec.f, i)), "row {i} missing");
            }
        }
        // positions are window-relative: nothing near the document's scale
        for cap in w.html.split("top:").skip(1) {
            let n: i64 = cap.split("px").next().unwrap().parse().unwrap();
            assert!(n.abs() <= (CLAMP + 4 * 900) as i64, "coordinate {n} is document-scale");
        }
        assert!(w.html.contains("cut-top"));
        assert!(w.html.contains("&lt;1&gt;"));
        assert!(!w.html.contains("<1>"));
    }

    #[test]
    fn fingerprint_is_stable_within_an_anchor_step_and_versioned() {
        let d = doc(&big_patch(2, 3000));
        let a = d.fingerprint(&d.slice(10_000, 900, 0));
        let b = d.fingerprint(&d.slice(10_100, 900, 0));
        let c = d.fingerprint(&d.slice(11_000, 900, 0));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("v1@"));
    }

    #[test]
    fn lead_leans_the_window_in_the_direction_of_travel() {
        let d = doc(&big_patch(2, 5000));
        let y = d.total / 2;
        let rest = d.slice(y, 900, 0);
        let down = d.slice(y, 900, 2);
        let up = d.slice(y, 900, -2);
        assert!(down.bot > rest.bot && down.top > rest.top);
        assert!(up.top < rest.top && up.bot < rest.bot);
        // whatever the lead, the viewport itself stays covered
        for s in [&rest, &down, &up, &d.slice(y, 900, 1), &d.slice(y, 900, -1)] {
            assert!(s.top <= y && s.bot >= y + 900);
        }
        assert_ne!(d.fingerprint(&rest), d.fingerprint(&down));
    }

    #[test]
    fn file_at_a_pixel() {
        let d = doc(&big_patch(3, 10));
        let h = d.files[0].height();
        assert_eq!(d.file_at(0), Some(0));
        assert_eq!(d.file_at(h - 1), Some(0)); // the gap under a file is still that file
        assert_eq!(d.file_at(h), Some(1));
        assert_eq!(d.file_at(d.total + 5000), Some(2));
    }

    #[test]
    fn tree_folds_lone_folders_and_totals_them() {
        let p = "diff --git a/src/a/x.rs b/src/a/x.rs\n--- a/src/a/x.rs\n+++ b/src/a/x.rs\n@@ -1 +1,2 @@\n a\n+b\ndiff --git a/src/a/y.rs b/src/a/y.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/a/y.rs\n@@ -0,0 +1 @@\n+<y>\ndiff --git a/top.md b/top.md\n--- a/top.md\n+++ b/top.md\n@@ -1 +0,0 @@\n-gone\n";
        let d = doc(p);
        let h = tree_html(&d, true);
        assert!(h.contains(">src/a/</span>"), "{h}");
        // folders come before files at the same level
        assert!(h.find("src/a/").unwrap() < h.find("top.md").unwrap());
        assert!(h.contains("<span class=\"st added\">A</span>"));
        assert!(h.contains("data-path=\"top.md\""));
        assert!(!h.contains("loading"));
        assert!(tree_html(&d, false).contains("dsum loading"));
    }

    #[test]
    fn empty_doc_renders_nothing() {
        let d = doc("");
        assert_eq!(d.total, 0);
        assert_eq!(d.file_at(0), None);
        let w = render_window(&d, &d.slice(0, 900, 0), None);
        assert!(w.html.is_empty());
    }
}
