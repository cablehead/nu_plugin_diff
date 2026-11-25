# nu_plugin_diff

nu_plugin_diff shows a diff in a web page without sending the diff to the browser. The diff stays on the server, in the plugin; the page asks for the rows it is showing and gets them as plain HTML, colored. A diff of a million rows scrolls like a small one, and the page holds only a few screens of it.

## The plugin

[http-nu](https://github.com/cablehead/http-nu) serves the page's requests by calling the plugin from [Nushell](https://www.nushell.sh/):

```nu
let d = diff open changes.patch             # a unified diff, path or text in; returns its handle at once
let d = [{path old new}] | diff open        # or the two sides of each file, diffed here
$d | diff files                             # [{i path old_path status binary adds dels rows top}]
$d | diff files --html                      # the same, as a tree for <diff-files>
$d | diff window 120000 900                 # the rows around pixel 120000 of the whole diff, as HTML
$d | diff window 0 900 --file src/lib.rs    # the same within one file
$d | diff window 0 900 --fp $fp             # {unchanged: true} if the page already has it
$d | diff changes                           # a tick each time the diff changes
$d | diff info                              # {files rows adds dels loaded ...}
```

`diff open` reads a patch on a thread of its own and publishes each batch of files as it goes, so a page shows the first files while the rest load; `loaded` turns true when it is done. A handle names the diff (`--name`, `main` by default), and opening again under the name replaces what every handle to it sees. Rows are 20 px and each file has a 40 px header, so a position is a pixel.

Syntax colors come from [syntect](https://github.com/trishume/syntect), with the classes of http-nu's `.highlight`, so `.highlight theme` styles them. Colors are exact wherever you land: a jump into the middle of a file colors everything above it first, once.

## The components

Two [Datastar Rocket](https://data-star.dev) components show a diff:

```html
<diff-files src="/d" for="view"></diff-files>
<diff-file id="view" src="/d"></diff-file>
<diff-file id="view" src="/d" file="src/lib.rs"></diff-file>
```

`<diff-file>` shows one file, or with no `file` every file in one scroller. It asks `{src}/win` for the rows at its scroll position and listens for ticks on `{src}/live`. A browser can't make an element millions of pixels tall, so its scroll area is squeezed, after [nu_plugin_ghostty](https://github.com/cablehead/nu_plugin_ghostty)'s `<ghostty-term>`: the rows on screen scroll 1:1 and only the gaps above and below them are squeezed.

`<diff-files>` shows the changed files as a tree, from `{src}/files`, with folders before files and counts on every folder. Picking a file gives it to the `<diff-file>` named by `for`, or jumps there when that one shows the whole diff.

## Demo

[`demo/serve.nu`](demo/serve.nu) serves a diff at `/` (every file in one scroller) and `/one` (a file at a time):

```sh
DIFF=changes.patch http-nu --datastar-rocket --plugin ./target/release/nu_plugin_diff 127.0.0.1:3021 demo/serve.nu
```

## Build

```sh
cargo build --release
```

It is built against Nushell 0.117, for http-nu 0.19.

## License

MIT. See [LICENSE](LICENSE).
