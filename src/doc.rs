//! The in-memory diff: files of display rows over shared text buffers, a
//! fixed-geometry layout, and the scroll-window slice.
//!
//! A `Doc` is an immutable snapshot. A new version of a live diff is a new
//! `Doc` that shares every unchanged `FileDiff` with the previous one.

use std::sync::{Arc, Mutex};

use crate::highlight::HlCache;

/// Fixed geometry: every display row is `ROW_H` px, every file is
/// `HEAD` px of header + rows + `GAP` px of space. Pixel-to-row is arithmetic.
pub const ROW_H: u64 = 20;
pub const HEAD: u64 = 40;
pub const GAP: u64 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    Hunk,
    Ctx,
    Add,
    Del,
    Meta,
}

impl RowKind {
    pub fn class(self) -> &'static str {
        match self {
            RowKind::Hunk => "hunk-header",
            RowKind::Ctx => "ctx",
            RowKind::Add => "add",
            RowKind::Del => "del",
            RowKind::Meta => "meta",
        }
    }
    pub fn sign(self) -> &'static str {
        match self {
            RowKind::Add => "+",
            RowKind::Del => "-",
            _ => "",
        }
    }
}

/// One display row. `line` indexes the new side for Ctx/Add, the old side
/// for Del, and `FileDiff::extra` for Hunk/Meta. 0 means "no number".
#[derive(Clone, Copy, Debug)]
pub struct Row {
    pub kind: RowKind,
    pub old: u32,
    pub new: u32,
    pub line: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Span {
    pub start: u32,
    pub len: u32,
}

/// One side's text as a sequence of lines over a shared buffer. From
/// sources this is the whole file, so highlighting carries parser state from
/// line 0. From a patch it is only the hunk lines, and `resets` marks where
/// a hunk starts and the parser state has to start over.
pub struct Side {
    pub buf: Arc<str>,
    pub lines: Vec<Span>,
    pub resets: Vec<u32>,
    pub hl: Mutex<HlCache>,
}

impl Side {
    pub fn new(buf: Arc<str>) -> Self {
        Side { buf, lines: Vec::new(), resets: Vec::new(), hl: Mutex::new(HlCache::default()) }
    }
    pub fn line(&self, i: u32) -> &str {
        let s = self.lines[i as usize];
        &self.buf[s.start as usize..(s.start + s.len) as usize]
    }
    pub fn push(&mut self, start: usize, len: usize) -> u32 {
        self.lines.push(Span { start: start as u32, len: len as u32 });
        (self.lines.len() - 1) as u32
    }
}

pub struct FileDiff {
    pub path: String,
    pub old_path: String,
    pub status: &'static str,
    pub binary: bool,
    pub rows: Vec<Row>,
    pub extra: Vec<String>,
    pub old: Side,
    pub new: Side,
    pub adds: u32,
    pub dels: u32,
    /// content key: a file with the same key in the previous snapshot is reused
    pub key: u64,
}

impl FileDiff {
    pub fn title(&self) -> String {
        if self.status == "renamed" && self.old_path != self.path {
            format!("{} -> {}", self.old_path, self.path)
        } else {
            self.path.clone()
        }
    }
    pub fn height(&self) -> u64 {
        HEAD + self.rows.len() as u64 * ROW_H + GAP
    }
}

pub struct Doc {
    pub name: String,
    pub version: u64,
    pub mode: &'static str,
    pub files: Vec<Arc<FileDiff>>,
    pub tops: Vec<u64>,
    pub total: u64,
    pub rows: u64,
    pub adds: u64,
    pub dels: u64,
    /// files carried over unchanged from the previous snapshot
    pub reused: usize,
}

/// A file's contribution to a pixel range: display rows a..=b. b < a means
/// only chrome (header or gap) intersects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Section {
    pub f: usize,
    pub a: i64,
    pub b: i64,
}

pub struct Slice {
    pub top: u64,
    pub bot: u64,
    pub secs: Vec<Section>,
}

impl Doc {
    pub fn new(name: String, version: u64, mode: &'static str, files: Vec<Arc<FileDiff>>) -> Doc {
        let mut tops = Vec::with_capacity(files.len());
        let (mut y, mut rows, mut adds, mut dels) = (0u64, 0u64, 0u64, 0u64);
        for f in &files {
            tops.push(y);
            y += f.height();
            rows += f.rows.len() as u64;
            adds += f.adds as u64;
            dels += f.dels as u64;
        }
        Doc { name, version, mode, files, tops, total: y, rows, adds, dels, reused: 0 }
    }

    /// The file at document pixel `px` (a file owns the gap under it), or
    /// None for an empty document. Past the end it is the last file.
    pub fn file_at(&self, px: u64) -> Option<usize> {
        if self.files.is_empty() {
            None
        } else {
            Some(self.first_file_past(px).min(self.files.len() - 1))
        }
    }

    /// index of the first file whose bottom edge is past `px`
    fn first_file_past(&self, px: u64) -> usize {
        let (mut lo, mut hi) = (0, self.files.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.tops[mid] + self.files[mid].height() > px {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        lo
    }

    /// files whose pixel extent intersects [top, bot), with their row ranges
    pub fn visible(&self, top: u64, bot: u64) -> Vec<Section> {
        let mut out = Vec::new();
        let mut i = self.first_file_past(top);
        while i < self.files.len() {
            let ft = self.tops[i];
            if ft >= bot {
                break;
            }
            let n = self.files[i].rows.len() as i64;
            let rel_top = top as i64 - ft as i64 - HEAD as i64;
            let rel_bot = bot as i64 - ft as i64 - HEAD as i64;
            let a = rel_top.div_euclid(ROW_H as i64).max(0);
            let b = (rel_bot.div_euclid(ROW_H as i64) + 1).min(n - 1);
            out.push(Section { f: i, a, b });
            i += 1;
        }
        out
    }

    /// The rendered window, in half-viewports above and below the anchor.
    /// At rest it is 1.5 viewports above and 2.5 below. `lead` leans it in
    /// the direction of travel, further the faster the reader moves, so the
    /// runway ahead grows when it is needed and the rows behind pay for it.
    /// The anchor sits within a quarter viewport of the position, either
    /// side, so covering the viewport takes at least 1 half above and 3 below.
    pub fn window_shape(lead: i64) -> (u64, u64) {
        match lead.clamp(-2, 2) {
            2 => (1, 10),
            1 => (2, 7),
            -1 => (6, 3),
            -2 => (9, 3),
            _ => (3, 5),
        }
    }

    /// The anchor ROUNDS the position to half-viewport steps, so the margin
    /// is never less than a quarter viewport short of the shape above, and
    /// the fingerprint only changes when the anchor or the lead moves.
    pub fn slice(&self, y: u64, vh: u64, lead: i64) -> Slice {
        let half = (vh / 2).max(1);
        let step = half.max(200);
        let anchor = ((y + step / 2) / step) * step;
        let (above, below) = Self::window_shape(lead);
        let top = anchor.saturating_sub(above * half);
        let bot = (anchor + below * half).min(self.total);
        Slice { top, bot, secs: self.visible(top, bot) }
    }

    pub fn fingerprint(&self, s: &Slice) -> String {
        let mut fp = format!("v{}@{}", self.version, s.top);
        for sec in &s.secs {
            fp.push_str(&format!(",{}:{}-{}", sec.f, sec.a, sec.b));
        }
        fp
    }
}

/// FNV-1a over a list of byte strings, with a separator so ("ab","c") and
/// ("a","bc") differ.
pub fn content_key(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for p in parts {
        for b in *p {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
