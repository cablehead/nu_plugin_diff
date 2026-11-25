//! nu_plugin_diff: holds diffs in this process's memory and projects them
//! into a page as plain HTML, a window of rows at a time.
//!
//! `diff open` starts loading a diff under a name and returns a handle at
//! once. The diff is read on a thread of its own, and each batch of files it
//! finishes is published as a new snapshot, so a page can show the first
//! files while the rest are still being read. A handle always means the
//! newest snapshot under its name; `diff changes` sends a tick for each one.
//! Every http-nu request shares this one process, so every page shares the
//! one copy of the diff.

mod doc;
mod highlight;
mod patch;
mod render;
mod sources;

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use nu_plugin::{serve_plugin, EngineInterface, EvaluatedCall, MsgPackSerializer, Plugin, PluginCommand, SimplePluginCommand};
use nu_protocol::{record, Category, CustomValue, LabeledError, ListStream, PipelineData, ShellError, Signature, Span, SyntaxShape, Type, Value};
use serde::{Deserialize, Serialize};

use doc::{Doc, GAP, HEAD, ROW_H};
use sources::FileInput;

/// What Nushell holds: the name of a live diff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffHandle {
    name: String,
}

#[typetag::serde]
impl CustomValue for DiffHandle {
    fn clone_value(&self, span: Span) -> Value {
        Value::custom(Box::new(self.clone()), span)
    }
    fn type_name(&self) -> String {
        "diff".into()
    }
    fn to_base_value(&self, span: Span) -> Result<Value, ShellError> {
        Ok(Value::record(record! { "diff" => Value::string(&self.name, span) }, span))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_mut_any(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// One name's newest snapshot, whether its loading has finished, and a
/// counter that moves on every publish.
#[derive(Clone)]
struct Live {
    doc: Arc<Doc>,
    loaded: bool,
    seq: u64,
    /// a newer `diff open` under the same name stops an older load
    load: u64,
}

#[derive(Default)]
struct Store {
    live: Mutex<HashMap<String, Live>>,
    changed: Condvar,
}

impl Store {
    fn get(&self, name: &str) -> Option<Live> {
        self.live.lock().unwrap().get(name).cloned()
    }

    /// Start a load under `name`: an empty snapshot, not loaded. Returns the
    /// load's number, which `publish` checks.
    fn begin(&self, name: &str, mode: &'static str) -> u64 {
        let mut live = self.live.lock().unwrap();
        let (seq, load, version) = live.get(name).map(|l| (l.seq, l.load, l.doc.version)).unwrap_or((0, 0, 0));
        let doc = Arc::new(Doc::new(name.into(), version + 1, mode, vec![]));
        live.insert(name.into(), Live { doc, loaded: false, seq: seq + 1, load: load + 1 });
        self.changed.notify_all();
        load + 1
    }

    /// Publish a snapshot, unless a newer load under the name has begun.
    /// Returns false when this load was overtaken.
    fn publish(&self, load: u64, doc: Doc, loaded: bool) -> bool {
        let mut live = self.live.lock().unwrap();
        let Some(l) = live.get_mut(&doc.name) else { return false };
        if l.load != load {
            return false;
        }
        let mut doc = doc;
        doc.version = l.doc.version + if loaded || l.loaded { 1 } else { 0 };
        l.doc = Arc::new(doc);
        l.loaded = loaded;
        l.seq += 1;
        self.changed.notify_all();
        true
    }

    /// Wait until the name's counter passes `seq`, or `timeout` goes by.
    fn wait(&self, name: &str, seq: u64, timeout: Duration) -> Option<Live> {
        let live = self.live.lock().unwrap();
        let (live, _) = self
            .changed
            .wait_timeout_while(live, timeout, |m| m.get(name).map(|l| l.seq <= seq).unwrap_or(true))
            .unwrap();
        live.get(name).cloned()
    }
}

#[derive(Default)]
pub struct DiffPlugin {
    store: Arc<Store>,
}

impl DiffPlugin {
    fn resolve(&self, input: &Value) -> Result<(String, Live), LabeledError> {
        let h = input
            .as_custom_value()
            .ok()
            .and_then(|cv| cv.as_any().downcast_ref::<DiffHandle>())
            .ok_or_else(|| LabeledError::new("expected a diff handle as input, from `diff open`"))?;
        let live = self.store.get(&h.name).ok_or_else(|| LabeledError::new(format!("no diff named {}", h.name)))?;
        Ok((h.name.clone(), live))
    }
}

impl Plugin for DiffPlugin {
    fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").into()
    }
    fn commands(&self) -> Vec<Box<dyn PluginCommand<Plugin = Self>>> {
        vec![Box::new(Open), Box::new(Get), Box::new(Files), Box::new(WindowCmd), Box::new(Changes), Box::new(Info)]
    }
}

fn cat() -> Category {
    Category::Custom("diff".into())
}

fn handle(name: &str, span: Span) -> Value {
    Value::custom(Box::new(DiffHandle { name: name.into() }), span)
}

fn read_text(path: &str) -> Result<String, LabeledError> {
    let bytes = std::fs::read(path).map_err(|e| LabeledError::new(format!("{path}: {e}")))?;
    Ok(String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

fn opt_str(rec: &nu_protocol::Record, key: &str) -> Option<String> {
    rec.get(key).and_then(|v| v.as_str().ok()).map(str::to_string)
}

fn read_side(rec: &nu_protocol::Record, text_key: &str, file_key: &str) -> Result<Option<String>, LabeledError> {
    if let Some(s) = opt_str(rec, text_key) {
        return Ok(Some(s));
    }
    opt_str(rec, file_key).map(|p| read_text(&p)).transpose()
}

/// Split a patch into one piece per file, at each `diff --git` line. A patch
/// without those lines is one piece.
fn split_files(text: &str) -> Vec<&str> {
    let mut starts: Vec<usize> = Vec::new();
    if text.starts_with("diff --git ") {
        starts.push(0);
    }
    starts.extend(text.match_indices("\ndiff --git ").map(|(i, _)| i + 1));
    if starts.is_empty() {
        return vec![text];
    }
    if starts[0] != 0 {
        starts.insert(0, 0);
    }
    starts.iter().enumerate().map(|(k, &a)| &text[a..*starts.get(k + 1).unwrap_or(&text.len())]).collect()
}

/// How often a loading diff publishes what it has so far.
const BATCH: Duration = Duration::from_millis(50);

fn load_patch(store: Arc<Store>, name: String, load: u64, text: String) {
    let mut files = Vec::new();
    let mut last = Instant::now();
    for piece in split_files(&text) {
        match patch::parse_patch(Arc::from(piece)) {
            Ok(mut more) => files.append(&mut more),
            Err(e) => eprintln!("diff open {name}: {e}"),
        }
        if last.elapsed() >= BATCH {
            last = Instant::now();
            if !store.publish(load, Doc::new(name.clone(), 0, "patch", files.clone()), false) {
                return;
            }
        }
    }
    store.publish(load, Doc::new(name, 0, "patch", files), true);
}

struct Open;
impl SimplePluginCommand for Open {
    type Plugin = DiffPlugin;
    fn name(&self) -> &str {
        "diff open"
    }
    fn description(&self) -> &str {
        "Load a diff under a name and return its handle; the diff loads in the background"
    }
    fn extra_description(&self) -> &str {
        "A unified diff comes as a path, or as text in. A list of records in, \
         {path, old_path?, old?, new?, old_file?, new_file?, binary?}, is the two sides of each file, \
         diffed here: `old`/`new` are texts, `old_file`/`new_file` paths read here; a missing side is \
         an added or deleted file, and files unchanged since the last load under the name are reused. \
         Opening again under a name replaces what it held, and every handle to the name sees the new diff."
    }
    fn signature(&self) -> Signature {
        Signature::build("diff open")
            .optional("path", SyntaxShape::String, "a unified diff; read here, never sent through the pipe")
            .named("name", SyntaxShape::String, "the diff's name (default: main)", Some('n'))
            .named("context", SyntaxShape::Int, "lines of context around each change, for two sides (default: 3)", Some('c'))
            .input_output_types(vec![(Type::Nothing, Type::Any), (Type::String, Type::Any), (Type::Any, Type::Any)])
            .category(cat())
    }
    fn run(&self, p: &DiffPlugin, engine: &EngineInterface, call: &EvaluatedCall, input: &Value) -> Result<Value, LabeledError> {
        let name = call.get_flag::<String>("name")?.unwrap_or_else(|| "main".into());
        // the data lives here: never idle-stop this process
        engine.set_gc_disabled(true)?;
        if let Ok(list) = input.as_list() {
            let context = call.get_flag::<i64>("context")?.unwrap_or(3).clamp(0, 1_000_000) as usize;
            let mut inputs = Vec::with_capacity(list.len());
            for v in list {
                let rec = v.as_record().map_err(|_| LabeledError::new("each file must be a record"))?;
                let path = opt_str(rec, "path").ok_or_else(|| LabeledError::new("a file record needs a `path`"))?;
                inputs.push(FileInput {
                    path,
                    old_path: opt_str(rec, "old_path"),
                    old: read_side(rec, "old", "old_file")?,
                    new: read_side(rec, "new", "new_file")?,
                    binary: rec.get("binary").and_then(|v| v.as_bool().ok()).unwrap_or(false),
                });
            }
            let prev = p.store.get(&name).map(|l| l.doc).filter(|d| d.mode == "sources");
            let load = p.store.begin(&name, "sources");
            let (files, reused) =
                sources::diff_files(&inputs, context, prev.as_ref().map(|d| d.files.as_slice())).map_err(LabeledError::new)?;
            let mut doc = Doc::new(name.clone(), 0, "sources", files);
            doc.reused = reused;
            p.store.publish(load, doc, true);
            return Ok(handle(&name, call.head));
        }
        let text = match call.opt::<String>(0)? {
            Some(path) => read_text(&path)?,
            None => input
                .as_str()
                .map_err(|_| LabeledError::new("give a path, or pipe in the diff's text or a list of files"))?
                .to_string(),
        };
        let load = p.store.begin(&name, "patch");
        let store = p.store.clone();
        let n = name.clone();
        std::thread::spawn(move || load_patch(store, n, load, text));
        Ok(handle(&name, call.head))
    }
}

struct Get;
impl SimplePluginCommand for Get {
    type Plugin = DiffPlugin;
    fn name(&self) -> &str {
        "diff get"
    }
    fn description(&self) -> &str {
        "The handle of a diff already open under a name, or nothing"
    }
    fn signature(&self) -> Signature {
        Signature::build("diff get")
            .optional("name", SyntaxShape::String, "the diff's name (default: main)")
            .input_output_type(Type::Nothing, Type::Any)
            .category(cat())
    }
    fn run(&self, p: &DiffPlugin, _e: &EngineInterface, call: &EvaluatedCall, _input: &Value) -> Result<Value, LabeledError> {
        let name = call.opt::<String>(0)?.unwrap_or_else(|| "main".into());
        Ok(if p.store.get(&name).is_some() { handle(&name, call.head) } else { Value::nothing(call.head) })
    }
}

fn info_record(l: &Live, s: Span) -> Value {
    let d = &l.doc;
    Value::record(
        record! {
            "name" => Value::string(&d.name, s),
            "seq" => Value::int(l.seq as i64, s),
            "version" => Value::int(d.version as i64, s),
            "loaded" => Value::bool(l.loaded, s),
            "mode" => Value::string(d.mode, s),
            "files" => Value::int(d.files.len() as i64, s),
            "rows" => Value::int(d.rows as i64, s),
            "adds" => Value::int(d.adds as i64, s),
            "dels" => Value::int(d.dels as i64, s),
            "total" => Value::int(d.total as i64, s),
            "reused" => Value::int(d.reused as i64, s),
        },
        s,
    )
}

struct Info;
impl SimplePluginCommand for Info {
    type Plugin = DiffPlugin;
    fn name(&self) -> &str {
        "diff info"
    }
    fn description(&self) -> &str {
        "A diff's totals, and whether it has finished loading"
    }
    fn extra_description(&self) -> &str {
        "Returns {name, seq, version, loaded, mode, files, rows, adds, dels, total, reused, row_h, head, gap}. \
         `total` is the whole diff's height in pixels; `row_h`, `head` and `gap` are the fixed geometry."
    }
    fn signature(&self) -> Signature {
        Signature::build("diff info").input_output_type(Type::Any, Type::Any).category(cat())
    }
    fn run(&self, p: &DiffPlugin, _e: &EngineInterface, call: &EvaluatedCall, input: &Value) -> Result<Value, LabeledError> {
        let (_, l) = p.resolve(input)?;
        let s = call.head;
        let mut rec = info_record(&l, s).into_record().unwrap();
        rec.push("row_h", Value::int(ROW_H as i64, s));
        rec.push("head", Value::int(HEAD as i64, s));
        rec.push("gap", Value::int(GAP as i64, s));
        Ok(Value::record(rec, s))
    }
}

struct Files;
impl SimplePluginCommand for Files {
    type Plugin = DiffPlugin;
    fn name(&self) -> &str {
        "diff files"
    }
    fn description(&self) -> &str {
        "One record per file: {i, path, old_path, status, binary, adds, dels, rows, top}"
    }
    fn extra_description(&self) -> &str {
        "`i` is the file's number, for `diff window --file`. `top` is where the file starts in the whole diff, in pixels."
    }
    fn signature(&self) -> Signature {
        Signature::build("diff files")
            .switch("html", "the list as HTML: folders before files, with counts on every folder", None)
            .input_output_type(Type::Any, Type::Any)
            .category(cat())
    }
    fn run(&self, p: &DiffPlugin, _e: &EngineInterface, call: &EvaluatedCall, input: &Value) -> Result<Value, LabeledError> {
        let (_, l) = p.resolve(input)?;
        let d = &l.doc;
        let s = call.head;
        if call.has_flag("html")? {
            return Ok(Value::string(render::tree_html(d, l.loaded), s));
        }
        let rows = d
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| {
                Value::record(
                    record! {
                        "i" => Value::int(i as i64, s),
                        "path" => Value::string(&f.path, s),
                        "old_path" => Value::string(&f.old_path, s),
                        "status" => Value::string(f.status, s),
                        "binary" => Value::bool(f.binary, s),
                        "adds" => Value::int(f.adds as i64, s),
                        "dels" => Value::int(f.dels as i64, s),
                        "rows" => Value::int(f.rows.len() as i64, s),
                        "top" => Value::int(d.tops[i] as i64, s),
                    },
                    s,
                )
            })
            .collect();
        Ok(Value::list(rows, s))
    }
}

fn px(call: &EvaluatedCall, i: usize) -> Result<u64, LabeledError> {
    Ok(call.req::<i64>(i)?.max(0) as u64)
}

struct WindowCmd;
impl SimplePluginCommand for WindowCmd {
    type Plugin = DiffPlugin;
    fn name(&self) -> &str {
        "diff window"
    }
    fn description(&self) -> &str {
        "The rows around a scroll position, as HTML: the whole diff, or one file of it"
    }
    fn extra_description(&self) -> &str {
        "Returns {fp, unchanged, top, bot, total, cur, html, rows, render_us}. Positions are pixels: \
         rows are 20 px and each file has a 40 px header. With --file, pixel 0 is that file's top and \
         `total` its height. `html` is placed relative to `top`. `cur` is the file at y. With --fp, a \
         window whose fingerprint still matches comes back as {unchanged: true} and nothing is drawn."
    }
    fn signature(&self) -> Signature {
        Signature::build("diff window")
            .required("y", SyntaxShape::Int, "the pixel at the top of the view")
            .required("vh", SyntaxShape::Int, "the view's height in pixels")
            .named("file", SyntaxShape::Any, "one file: its number in `diff files`, or its path", Some('f'))
            .named("fp", SyntaxShape::String, "the fingerprint of the window the page already has", None)
            .named("lead", SyntaxShape::Int, "lean the window the way the reader is scrolling: -2..2, 0 at rest", None)
            .input_output_type(Type::Any, Type::Any)
            .category(cat())
    }
    fn run(&self, p: &DiffPlugin, _e: &EngineInterface, call: &EvaluatedCall, input: &Value) -> Result<Value, LabeledError> {
        let (_, l) = p.resolve(input)?;
        let s = call.head;
        let t = Instant::now();
        let one: Option<i64> = match call.get_flag_value("file") {
            None => None,
            Some(Value::String { val, .. }) if val.is_empty() => None,
            Some(Value::Int { val, .. }) => Some(val),
            Some(v) => {
                let want = v.coerce_into_string().map_err(|_| LabeledError::new("--file takes a number or a path"))?;
                match want.parse::<i64>() {
                    Ok(i) => Some(i),
                    Err(_) => Some(
                        l.doc.files.iter().position(|f| f.path == want).map(|i| i as i64).ok_or_else(|| {
                            LabeledError::new(format!("no file {want} in the diff ({} files so far)", l.doc.files.len()))
                        })?,
                    ),
                }
            }
        };
        let (d, prefix) = match one {
            None => (l.doc.clone(), String::new()),
            Some(i) => {
                let f = usize::try_from(i).ok().and_then(|i| l.doc.files.get(i)).cloned().ok_or_else(|| {
                    LabeledError::new(format!("no file {i}: the diff has {} so far", l.doc.files.len()))
                })?;
                (Arc::new(Doc::new(l.doc.name.clone(), l.doc.version, l.doc.mode, vec![f])), format!("f{i}:"))
            }
        };
        let vh = px(call, 1)?.clamp(1, 8192);
        let y = px(call, 0)?.min(d.total.saturating_sub(1));
        let slice = d.slice(y, vh, call.get_flag::<i64>("lead")?.unwrap_or(0));
        let fp = format!("{prefix}{}", d.fingerprint(&slice));
        let unchanged = call.get_flag::<String>("fp")?.as_deref() == Some(fp.as_str());
        let mut rec = record! {
            "fp" => Value::string(&fp, s),
            "unchanged" => Value::bool(unchanged, s),
            "top" => Value::int(slice.top as i64, s),
            "bot" => Value::int(slice.bot as i64, s),
            "total" => Value::int(d.total as i64, s),
            "cur" => Value::int(match one {
                Some(i) => i,
                None => d.file_at(y).map(|i| i as i64).unwrap_or(-1),
            }, s),
        };
        if !unchanged {
            // the header of the file at the top of the view, for a header that stays put
            if let Some(i) = d.file_at(y) {
                rec.push("head", Value::string(render::head_html(&d.files[i]), s));
            }
            let w = render::render_window(&d, &slice, one.map(|i| i as usize));
            render::prefetch(&d, &slice);
            rec.push("html", Value::string(w.html, s));
            rec.push("rows", Value::int(w.rows as i64, s));
        }
        rec.push("render_us", Value::int(t.elapsed().as_micros() as i64, s));
        Ok(Value::record(rec, s))
    }
}

struct Changes;
impl PluginCommand for Changes {
    type Plugin = DiffPlugin;
    fn name(&self) -> &str {
        "diff changes"
    }
    fn description(&self) -> &str {
        "A stream of ticks, one each time the diff changes: as it loads, and when it is opened again"
    }
    fn extra_description(&self) -> &str {
        "Each tick is `diff info`'s record without the geometry. The first comes at once. With no change \
         for 15 seconds, the last tick is sent again, so a page that has gone away ends the stream."
    }
    fn signature(&self) -> Signature {
        Signature::build("diff changes").input_output_type(Type::Any, Type::Any).category(cat())
    }
    fn run(&self, p: &DiffPlugin, engine: &EngineInterface, call: &EvaluatedCall, input: PipelineData) -> Result<PipelineData, LabeledError> {
        let v = input.into_value(call.head)?;
        let (name, first) = p.resolve(&v)?;
        let store = p.store.clone();
        let s = call.head;
        let mut seq = None::<u64>;
        let iter = std::iter::from_fn(move || {
            let l = match seq {
                None => Some(first.clone()),
                Some(n) => store.wait(&name, n, Duration::from_secs(15)),
            }?;
            seq = Some(l.seq);
            Some(info_record(&l, s))
        });
        Ok(PipelineData::list_stream(ListStream::new(iter, s, engine.signals().clone()), None))
    }
}

fn main() {
    // the syntax set takes ~100ms to build: start it before the first window asks
    std::thread::spawn(|| {
        highlight::syntax_set();
    });
    serve_plugin(&DiffPlugin::default(), MsgPackSerializer {})
}
