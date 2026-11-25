// <diff-file src="/d" file="src/retry.rs">: one file of a nu_plugin_diff diff,
// or with no `file`, every file in one scroller, one after another.
//
// The page holds only the rows near the view. It asks the server for the
// window of rows at its scroll position and places the HTML it gets; the
// diff, its colors and its layout stay in the plugin.
//
// The server under `src` answers:
//   GET {src}/live                 SSE, event "tick": the diff's info, at once
//                                  and each time it changes (as it loads)
//   GET {src}/win?y&vh&file&fp&lead
//                                  {fp, top, bot, total, cur, head, html}, or
//                                  {unchanged: true}. Pixels: y is the top of
//                                  the view; the window covers top..bot.
//
// The scroll area is squeezed, as in nu_plugin_ghostty's <ghostty-term> and
// largediff's /anchor/ viewer (after CodeMirror 6's BigScaler): a browser
// can't make an element millions of pixels tall, so the spacer is at most
// `cap` pixels. The window sits at its real height and only the gaps above
// and below it are squeezed, so scrolling within it is the browser's own,
// 1:1. A window that lands mid-gesture goes where the row at the top of the
// view already is, so nothing moves; at rest the gaps go back to the even
// squeeze.
//
// Props:
//   src   the server path above
//   file  a file's path or number; empty for the whole diff
//   cap   the most pixels the scroll area takes (default 4,000,000)
//
// Host API:
//   jump(y)  scroll to pixel y of the diff (a file's `data-top` in <diff-files>)
// Events on the host:
//   diff-at  {detail: {file}}: the file at the top of the view changed
//
// The page brings Datastar with Rocket and names it in an import map:
//   <script type=importmap>{"imports": {"datastar": "/datastar-rocket@1.0.4.js"}}</script>

import { rocket } from "datastar";

rocket("diff-file", {
  mode: "light",
  props: ({ string }) => ({
    src: string.trim,
    file: string.trim,
    cap: string.trim,
  }),
  setup: ({ host, props, cleanup, observeProps, defineHostProp }) => {
    const src = props.src.replace(/\/$/, "");
    host.innerHTML = `<div class="dhead"></div><div class="scroll" tabindex="-1"><div class="spacer"></div><div class="win"></div></div>`;
    const headEl = host.querySelector(".dhead");
    const box = host.querySelector(".scroll");
    const spacer = host.querySelector(".spacer");
    const win = host.querySelector(".win");

    // target: a jump's pixel, held until the window that covers it lands, since
    // the squeeze can't place a pixel outside the window exactly
    const st = { total: 0, top: -1, bot: -1, fp: "", cur: -1, busy: false, again: false, target: null };

    // ---- the squeeze: document pixels to scroller pixels and back.
    // g: {o, b, total, span, dTop, dBot}
    const REST = 300;
    let g = null, lastScroll = 0, restTimer = 0, lastWrote = -1, lastY = 0;
    const geo = (dTop) => {
      const total = st.total;
      const span = Math.max(Math.min(total, Number(props.cap) || 4_000_000), 1);
      const o = Math.max(0, st.top), b = Math.max(o, st.bot);
      const wh = b - o;
      if (!(total > span && span > wh)) return { o, b, total, span: Math.max(total, 1), dTop: o, dBot: b };
      const even = Math.round(o * (span - wh) / (total - wh));
      const top = Math.min(Math.max(dTop ?? even, 0), span - wh);
      return { o, b, total, span, dTop: top, dBot: top + wh };
    };
    const fromDOM = (g, n) => {
      if (n < g.dTop) return g.dTop > 0 ? n * g.o / g.dTop : 0;
      if (n <= g.dBot) return g.o + (n - g.dTop);
      const gap = g.span - g.dBot;
      return gap > 0 ? g.b + (n - g.dBot) * (g.total - g.b) / gap : g.b;
    };
    const toDOM = (g, y) => {
      if (y < g.o) return g.o > 0 ? y * g.dTop / g.o : 0;
      if (y <= g.b) return g.dTop + (y - g.o);
      const gap = g.total - g.b;
      return gap > 0 ? g.dBot + (y - g.b) * (g.span - g.dBot) / gap : g.dBot;
    };
    const place = (next, y) => {
      g = next;
      spacer.style.height = g.span + "px";
      win.style.top = g.dTop + "px";
      if (y == null) return;
      const t = toDOM(g, y);
      if (Math.abs(box.scrollTop - t) > 1) { box.scrollTop = t; lastWrote = box.scrollTop; }
    };
    const docY = () => Math.round(fromDOM(g || geo(null), box.scrollTop));

    // ---- asking for rows: one request in flight; a newer need waits for it
    const covers = (y) => st.top >= 0 && y >= st.top && y + box.clientHeight <= st.bot;
    const fetchWindow = async (force) => {
      if (st.busy) { st.again = true; return; }
      const y = st.target ?? docY();
      if (!force && covers(y)) return;
      st.busy = true;
      try {
        // lean the window the way the reader is going
        const dy = y - lastY;
        const lead = Math.abs(dy) < 50 ? 0 : Math.sign(dy) * (Math.abs(dy) > box.clientHeight ? 2 : 1);
        lastY = y;
        const q = new URLSearchParams({ y, vh: box.clientHeight || 600, file: props.file || "", fp: st.fp, lead });
        const r = await (await fetch(`${src}/win?${q}`)).json();
        if (r.error) throw new Error(r.error);
        if (!r.unchanged) {
          const old = g;
          win.innerHTML = r.html;
          if (r.head != null) serverHead = r.head;
          st.fp = r.fp; st.top = r.top; st.bot = r.bot; st.total = r.total;
          if (st.target != null) {
            place(geo(null), st.target);
            st.target = null;
          } else if (performance.now() - lastScroll >= REST || !old) {
            place(geo(null), y);
          } else {
            // mid-gesture: the row at the top of the view stays where it is
            const dTop = y >= st.top && y <= st.bot ? box.scrollTop - (y - st.top) : toDOM(old, st.top);
            place(geo(dTop), y);
          }
          syncHead();
        }
      } catch (e) {
        win.innerHTML = `<div class="derr">${String(e.message || e).replace(/</g, "&lt;")}</div>`;
      }
      st.busy = false;
      if (st.again) { st.again = false; fetchWindow(true); }
    };

    // The header that stays put shows the file at the top of the view, and
    // only once that file's own header has scrolled out of sight; while the
    // own header is in view, one is enough.
    let serverHead = "";
    const syncHead = () => {
      const top = box.getBoundingClientRect().top;
      const sec = [...win.querySelectorAll("section.vfile")].find((e) => {
        const r = e.getBoundingClientRect();
        return r.top <= top + 1 && r.bottom > top + 1;
      });
      if (!sec) { headEl.innerHTML = ""; return; }
      const own = sec.querySelector(":scope > .vhead");
      if (own && own.getBoundingClientRect().top >= top - 1) headEl.innerHTML = "";
      else headEl.innerHTML = own ? own.outerHTML : serverHead;
      const f = Number(sec.dataset.file);
      if (f !== st.cur) {
        st.cur = f;
        host.dispatchEvent(new CustomEvent("diff-at", { bubbles: true, detail: { file: f } }));
      }
    };

    box.addEventListener("scroll", () => {
      syncHead();
      if (box.scrollTop !== lastWrote) {
        lastScroll = performance.now();
        clearTimeout(restTimer);
        restTimer = setTimeout(() => { if (g) place(geo(null), docY()); fetchWindow(true); }, REST);
      }
      if (!covers(docY())) fetchWindow(true);
    }, { passive: true });
    cleanup(() => clearTimeout(restTimer));
    new ResizeObserver(() => fetchWindow(true)).observe(box);

    const jump = (y) => {
      st.fp = "";
      st.target = Math.max(0, Number(y) || 0);
      place(geo(null), st.target);
      st.top = -1;
      fetchWindow(true);
      box.focus({ preventScroll: true });
    };
    defineHostProp("jump", { value: jump });

    // a new file: start at its top
    observeProps(() => {
      st.fp = ""; st.top = -1; st.total = 0; st.cur = -1; st.target = 0;
      headEl.innerHTML = ""; serverHead = "";
      place(geo(null), 0);
      box.scrollTop = 0;
      fetchWindow(true);
    }, "file");

    // ---- the live stream: the diff grew, or was opened again
    const live = new EventSource(`${src}/live`);
    live.addEventListener("tick", (ev) => {
      const t = JSON.parse(ev.data);
      host.classList.toggle("loading", !t.loaded);
      fetchWindow(true);
    });
    cleanup(() => live.close());
  },
});
