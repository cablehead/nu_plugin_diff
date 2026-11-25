//! Syntax highlighting that stays correct across lines.
//!
//! A side's lines are highlighted in blocks of `BLOCK` lines. Each block
//! starts from the parser state the previous block ended with, so a block
//! reached by scrolling down from the top of a file is exact. A block reached
//! by a jump runs on from the nearest cached block above it, or from the
//! top, so its colors are exact. Every line's HTML is self-contained: the scopes open at the
//! start of the line are re-opened in its prefix and closed in its suffix.
//!
//! Classes match http-nu's `.highlight` (syntect, `ClassStyle::Spaced`), so
//! the page's existing theme CSS applies.

use std::sync::{Arc, OnceLock};

use syntect::html::{line_tokens_to_classed_spans, ClassStyle};
use syntect::parsing::{ParseState, ScopeStack, SyntaxDefinition, SyntaxReference, SyntaxSet};

use crate::doc::Side;

pub const BLOCK: usize = 256;
/// a line longer than this is escaped, not parsed: minified bundles cost
/// seconds per line in the regex engine
const MAX_LINE: usize = 2000;
/// cached blocks per side before the side's cache is dropped and refilled
const MAX_BLOCKS_PER_SIDE: usize = 512;

const NUSHELL_SYNTAX: &str = include_str!("../syntaxes/nushell.sublime-syntax");

pub fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(|| {
        let mut b = SyntaxSet::load_defaults_newlines().into_builder();
        if let Ok(def) = SyntaxDefinition::load_from_str(NUSHELL_SYNTAX, true, Some("nushell")) {
            b.add(def);
        }
        b.build()
    })
}

/// extension aliases for languages the default set lacks
fn alias(ext: &str) -> &str {
    match ext {
        "ts" | "tsx" | "jsx" | "mjs" | "cjs" | "mts" | "cts" => "js",
        "zig" => "rs",
        "jsonc" | "json5" => "json",
        _ => ext,
    }
}

pub fn syntax_for(path: &str) -> Option<&'static SyntaxReference> {
    let ss = syntax_set();
    let name = path.rsplit('/').next().unwrap_or(path);
    let found = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ss.find_syntax_by_extension(alias(ext)),
        _ => None,
    }
    .or_else(|| ss.find_syntax_by_token(name))?;
    // "Plain Text" parses to nothing: skip the work
    if found.name == "Plain Text" {
        None
    } else {
        Some(found)
    }
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

#[derive(Clone)]
struct State {
    parse: ParseState,
    scopes: ScopeStack,
}

impl State {
    fn fresh(syntax: &SyntaxReference) -> State {
        State { parse: ParseState::new(syntax), scopes: ScopeStack::new() }
    }
}

pub struct HlBlock {
    pub html: Vec<String>,
    end: State,
}

#[derive(Default)]
pub struct HlCache {
    blocks: Vec<Option<Arc<HlBlock>>>,
    live: usize,
}

fn highlight_line(line: &str, st: &mut State, ss: &SyntaxSet) -> String {
    if line.len() > MAX_LINE {
        return escape(line);
    }
    let with_nl = format!("{line}\n");
    let Ok(ops) = st.parse.parse_line(&with_nl, ss) else { return escape(line) };
    let mut out = String::with_capacity(line.len() * 2);
    let depth = st.scopes.as_slice().len() as isize;
    for scope in st.scopes.as_slice() {
        out.push_str("<span class=\"");
        out.push_str(&scope.build_string().replace('.', " "));
        out.push_str("\">");
    }
    let Ok((html, delta)) = line_tokens_to_classed_spans(&with_nl, &ops, ClassStyle::Spaced, &mut st.scopes) else {
        return escape(line);
    };
    // the row is one fixed-height line: drop the newline syntect echoes
    out.push_str(&html.replace('\n', ""));
    for _ in 0..(depth + delta).max(0) {
        out.push_str("</span>");
    }
    out
}

fn run_block(side: &Side, k: usize, mut st: State, syntax: &SyntaxReference, keep_html: bool) -> HlBlock {
    let ss = syntax_set();
    let start = k * BLOCK;
    let end = (start + BLOCK).min(side.lines.len());
    let mut html = Vec::with_capacity(if keep_html { end - start } else { 0 });
    for i in start..end {
        if side.resets.binary_search(&(i as u32)).is_ok() {
            st = State::fresh(syntax);
        }
        let h = highlight_line(side.line(i as u32), &mut st, ss);
        if keep_html {
            html.push(h);
        }
    }
    HlBlock { html, end: st }
}

/// highlighted HTML for the block holding line `i`. The cache lock is held
/// only to look up and to store, never while parsing: a prefetch thread and
/// a render can then work on different blocks of one side at once. Two
/// threads racing on the same block both compute it; the results are equal.
fn block_for(side: &Side, i: usize, syntax: &SyntaxReference) -> Arc<HlBlock> {
    let k = i / BLOCK;
    let nblocks = side.lines.len().div_ceil(BLOCK);
    let start = {
        let mut cache = side.hl.lock().unwrap();
        if cache.blocks.len() != nblocks {
            cache.blocks = vec![None; nblocks];
            cache.live = 0;
        }
        if let Some(b) = &cache.blocks[k] {
            return b.clone();
        }
        if k == 0 {
            Some(State::fresh(syntax))
        } else {
            cache.blocks[k - 1].as_ref().map(|prev| prev.end.clone())
        }
    };
    // Jumped in: run on from the nearest block above that is cached, or
    // from the top, keeping each block on the way. Slower than guessing, but
    // a color shown is always the color the file has.
    let start = match start {
        Some(st) => st,
        None => {
            let (mut j, mut st) = {
                let cache = side.hl.lock().unwrap();
                (0..k).rev().find_map(|j| cache.blocks.get(j).and_then(|b| b.as_ref()).map(|b| (j + 1, b.end.clone())))
            }
            .unwrap_or((0, State::fresh(syntax)));
            while j < k {
                let b = Arc::new(run_block(side, j, st, syntax, true));
                st = b.end.clone();
                let mut cache = side.hl.lock().unwrap();
                if cache.blocks.len() == nblocks && cache.blocks[j].is_none() {
                    cache.live += 1;
                    cache.blocks[j] = Some(b);
                }
                j += 1;
            }
            st
        }
    };
    let block = Arc::new(run_block(side, k, start, syntax, true));
    let mut cache = side.hl.lock().unwrap();
    if cache.live >= MAX_BLOCKS_PER_SIDE {
        cache.blocks.iter_mut().for_each(|b| *b = None);
        cache.live = 0;
    }
    if cache.blocks[k].is_none() {
        cache.live += 1;
    }
    cache.blocks[k] = Some(block.clone());
    block
}

/// make sure the blocks on both sides of lines lo..=hi are cached, so a
/// scroll that continues in either direction finds them ready
pub fn prefetch_around(side: &Side, lo: u32, hi: u32, syntax: &SyntaxReference) {
    if side.lines.is_empty() {
        return;
    }
    let last = side.lines.len() - 1;
    let ahead = (hi as usize / BLOCK + 1) * BLOCK;
    if ahead <= last {
        block_for(side, ahead, syntax);
    }
    if lo as usize >= BLOCK {
        block_for(side, lo as usize - BLOCK, syntax);
    }
}

/// highlighted HTML for one line of a side; `syntax` None means escape only
pub fn line_html(side: &Side, i: u32, syntax: Option<&SyntaxReference>) -> String {
    match syntax {
        None => escape(side.line(i)),
        Some(sx) => {
            let b = block_for(side, i as usize, sx);
            b.html[i as usize % BLOCK].clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side(text: &str) -> Side {
        let mut s = Side::new(Arc::from(text));
        let mut pos = 0;
        for l in text.split('\n') {
            s.push(pos, l.len());
            pos += l.len() + 1;
        }
        s
    }

    fn balanced(html: &str) -> bool {
        html.matches("<span").count() == html.matches("</span>").count()
    }

    #[test]
    fn multi_line_comment_state_carries_and_lines_stay_balanced() {
        let s = side("fn a() {}\n/* open\nstill <comment> here\nclose */ fn b() {}\nlet x = 1;");
        let sx = syntax_for("x.rs").unwrap();
        let lines: Vec<String> = (0..5).map(|i| line_html(&s, i, Some(sx))).collect();
        for l in &lines {
            assert!(balanced(l), "unbalanced: {l}");
            assert!(l.starts_with("<span class=\"source rust\">"), "{l}");
        }
        // the middle line has no comment token of its own: only carried
        // state can know it is a comment
        assert!(lines[2].contains("comment"), "{}", lines[2]);
        assert!(lines[2].contains("&lt;comment&gt;"));
        assert!(!lines[4].contains("comment"), "{}", lines[4]);
    }

    #[test]
    fn resets_restart_the_parser() {
        let mut s = side("/* never closed\nlet y = 2;");
        s.resets = vec![0, 1];
        let sx = syntax_for("x.rs").unwrap();
        assert!(line_html(&s, 0, Some(sx)).contains("comment"));
        assert!(!line_html(&s, 1, Some(sx)).contains("comment"));
    }

    #[test]
    fn jump_into_a_far_block_is_exact_and_caches_the_way_there() {
        let text: String = (0..2000).map(|i| format!("let v{i} = {i};\n")).collect();
        let s = side(text.trim_end());
        let sx = syntax_for("x.rs").unwrap();
        let h = line_html(&s, 1500, Some(sx));
        assert!(h.contains("v1500"));
        let cache = s.hl.lock().unwrap();
        assert_eq!(cache.live, 1500 / BLOCK + 1);
        assert!(cache.blocks[1500 / BLOCK].is_some());
    }

    #[test]
    fn aliases_and_plain_text() {
        assert!(syntax_for("src/app.tsx").is_some());
        assert!(syntax_for("serve.nu").is_some());
        assert!(syntax_for("notes.txt").is_none());
        assert!(syntax_for("Makefile").is_some());
        assert_eq!(escape("<a & \"b\">"), "&lt;a &amp; &quot;b&quot;&gt;");
    }
}
