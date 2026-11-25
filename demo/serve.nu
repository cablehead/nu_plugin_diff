# A diff in a page: <diff-files> beside <diff-file>, on the plugin. The
# page holds only the rows near the view.
#
#   DIFF=changes.patch http-nu --datastar-rocket --plugin ./target/release/nu_plugin_diff 127.0.0.1:3021 demo/serve.nu
#
# /      the whole diff in one scroller; the list jumps within it
# /one   one file at a time; the list picks the file

use http-nu/datastar *

const STATIC = path self ../static

let d = (diff open $env.DIFF)

# Syntax colors: syntect's GitHub theme, and a dark one for dark mode.
let theme = $"(.highlight theme GitHub)\n@media \(prefers-color-scheme: dark\) {\n(.highlight theme 'Monokai Extended')\n}"

def page [one: bool] {
  let imports = {imports: {datastar: $DATASTAR_ROCKET_JS_PATH}} | to json -r
  let first = if $one { $d | diff files | get 0?.path? | default "" } else { null }
  let view = if $one { $"<diff-file id=view src=\"/d\" file=\"($first | str replace -a '"' '&quot;')\"></diff-file>" } else { "<diff-file id=view src=\"/d\"></diff-file>" }
  $"<!doctype html>
<html><head><meta charset=utf-8><meta name=viewport content=\"width=device-width, initial-scale=1\"><title>nu_plugin_diff</title>
<style>($theme)</style>
<link rel=stylesheet href=\"/static/diff.css\">
<script type=importmap>($imports)</script>
<script type=module src=\"($DATASTAR_ROCKET_JS_PATH)\"></script>
<script type=module src=\"/static/diff-file.js\"></script>
<script type=module src=\"/static/diff-files.js\"></script>
<style>
html, body { margin: 0; height: 100%; }
body { display: grid; grid-template-columns: minmax\(200px, 320px\) 1fr; }
diff-files { border-right: 1px solid #8884; min-height: 0; }
@media \(max-width: 700px\) { body { grid-template-columns: 1fr; grid-template-rows: 30vh 1fr; } }
</style>
</head><body>
<diff-files src=\"/d\" for=view></diff-files>
($view)
</body></html>"
}

def json-resp [] { to json -r | metadata set { merge {'http.response': {headers: {content-type: "application/json"}}} } }

{|req|
  match $req.path {
    "/" => { page false }
    "/one" => { page true }
    "/d/live" => {
      $d | diff changes | each {|c| {event: "tick", data: ($c | to json -r)} } | to sse
    }
    "/d/files" => { $d | diff files --html }
    "/d/win" => {
      let q = $req.query
      try {
        $d | diff window ($q.y? | default 0 | into int) ($q.vh? | default 900 | into int) --file ($q.file? | default "") --fp ($q.fp? | default "") --lead ($q.lead? | default 0 | into int) | json-resp
      } catch {|e| {error: $e.msg} | json-resp }
    }
    _ => {
      if ($req.path | str starts-with "/static/") {
        let rel = $req.path | str replace '/static/' '' | path split | where {|p| $p != ".." }
        let f = ($STATIC | path join ...$rel)
        if ($f | path exists) {
          let ct = if ($f | str ends-with ".js") { "text/javascript" } else if ($f | str ends-with ".css") { "text/css" } else { "application/octet-stream" }
          open --raw $f | metadata set { merge {'http.response': {headers: {content-type: $ct}}} }
        } else { "not found" | metadata set { merge {'http.response': {status: 404}} } }
      } else { "not found" | metadata set { merge {'http.response': {status: 404}} } }
    }
  }
}
