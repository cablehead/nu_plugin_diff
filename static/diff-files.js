// <diff-files src="/d" for="view">: the changed files of a nu_plugin_diff
// diff, as a tree. The server draws the tree; this places it, and keeps it
// current while the diff loads.
//
// The server under `src` answers:
//   GET {src}/live    SSE, event "tick": the diff's info, at once and each
//                     time it changes
//   GET {src}/files   the tree as HTML: folders before files, counts on every
//                     folder, each file an <li class="file" data-file
//                     data-top data-path>
//
// Picking a file tells the <diff-file> whose id is `for`: one showing a
// single file is given that file; one showing the whole diff jumps to it.
// The file at the top of that <diff-file> is marked as you scroll.
//
// Props:
//   src   the server path above
//   for   the id of the <diff-file> it drives

import { rocket } from "datastar";

rocket("diff-files", {
  mode: "light",
  props: ({ string }) => ({
    src: string.trim,
    for: string.trim,
  }),
  setup: ({ host, props, cleanup }) => {
    const src = props.src.replace(/\/$/, "");
    const view = () => (props.for ? document.getElementById(props.for) : null);
    let active = null, seq = -1, busy = false, again = false;

    const mark = (key, value) => {
      const li = host.querySelector(`li.file[${key}="${CSS.escape(String(value))}"]`);
      if (li === active) return;
      active?.classList.remove("active");
      active = li;
      if (!li) return;
      li.classList.add("active");
      // keep it in sight without jumping the list about
      const r = li.getBoundingClientRect(), b = host.getBoundingClientRect();
      if (r.top < b.top || r.bottom > b.bottom) li.scrollIntoView({ block: "center" });
    };

    const load = async () => {
      if (busy) { again = true; return; }
      busy = true;
      try {
        const html = await (await fetch(`${src}/files`)).text();
        const key = active ? active.dataset.file : null;
        const top = host.scrollTop;
        host.innerHTML = html;
        host.scrollTop = top;
        active = null;
        if (key != null) mark("data-file", key);
        else if (view()?.getAttribute("file")) mark("data-path", view().getAttribute("file"));
      } catch {}
      busy = false;
      if (again) { again = false; load(); }
    };

    host.addEventListener("click", (ev) => {
      const li = ev.target.closest("li.file");
      if (!li || !host.contains(li)) return;
      const v = view();
      if (!v) return;
      if (v.hasAttribute("file")) v.file = li.dataset.path;
      else v.jump(Number(li.dataset.top));
      mark("data-file", li.dataset.file);
    });

    const onAt = (ev) => { if (ev.target === view()) mark("data-file", ev.detail.file); };
    document.addEventListener("diff-at", onAt);
    cleanup(() => document.removeEventListener("diff-at", onAt));

    const live = new EventSource(`${src}/live`);
    live.addEventListener("tick", (ev) => {
      const t = JSON.parse(ev.data);
      if (t.seq === seq) return;
      seq = t.seq;
      load();
    });
    cleanup(() => live.close());
  },
});
