// The hero: one real page, as nupdf reads it. Point at a line to see its address; search to see hits.
// page.json is `nupdf extract` output for the page (see build.py); nothing here is made up.

/** Hits of a needle within single spans, as `nupdf search --options ignore-case` ids them. */
export function findHits(page, needle) {
  const hits = [];
  const n = needle.toLowerCase();
  if (!n.trim()) return hits;
  for (const line of page.lines) {
    for (const span of line.spans) {
      const hay = span.text.toLowerCase();
      for (let at = hay.indexOf(n); at !== -1; at = hay.indexOf(n, at + n.length)) {
        const end = at + n.length;
        hits.push({ id: `${span.id}:${at}-${end}`, text: span.text.slice(at, end), span, line, at, end });
      }
    }
  }
  return hits;
}

/** `p15.b1.l3.s1:17-29` as its five parts. */
export function parts(id) {
  const [path, chars] = id.split(":");
  const [page, block, line, span] = path.split(".");
  return { page, block: block && "." + block, line: line && "." + line, span: span && "." + span, chars: chars && ":" + chars };
}

if (typeof document !== "undefined") start();

async function start() {
  const root = document.getElementById("live");
  const page = await (await fetch("demo/page.json")).json();
  const [W, H] = page.size;
  const paper = root.querySelector(".paper");
  const out = root.querySelector(".out code");
  const cmd = root.querySelector(".cmd span");
  const count = root.querySelector(".count");
  const input = root.querySelector("input");
  const segs = Object.fromEntries([...root.querySelectorAll("[data-seg]")].map((e) => [e.dataset.seg, e.firstChild]));
  const pct = (v, of) => (v / of) * 100 + "%";
  const place = (el, [x0, y0, x1, y1], pad = 0) =>
    Object.assign(el.style, { left: pct(x0 - pad, W), top: pct(y0 - pad, H), width: pct(x1 - x0 + 2 * pad, W), height: pct(y1 - y0 + 2 * pad, H) });
  const box = (cls, bbox, pad) => {
    const el = document.createElement("div");
    el.className = cls;
    place(el, bbox, pad);
    return el;
  };
  const blocks = {};
  for (const l of page.lines) {
    const b = (blocks[l.block] ??= [Infinity, Infinity, 0, 0]);
    b[0] = Math.min(b[0], l.bbox[0]); b[1] = Math.min(b[1], l.bbox[1]); b[2] = Math.max(b[2], l.bbox[2]); b[3] = Math.max(b[3], l.bbox[3]);
  }

  const marks = paper.appendChild(document.createElement("div"));
  marks.className = "marks";
  const blockBox = paper.appendChild(box("b-block", [0, 0, 0, 0]));
  const lineBox = paper.appendChild(box("b-line", [0, 0, 0, 0]));
  let selected = null; // {id, json, line}

  const show = (id, line) => {
    const p = parts(id);
    for (const k of Object.keys(segs)) {
      segs[k].textContent = p[k] ?? (k === "chars" ? ":…" : "");
      segs[k].parentElement.classList.toggle("off", !p[k]);
    }
    place(blockBox, blocks[line.block], 5);
    place(lineBox, line.bbox, 1.5);
  };
  const select = (sel) => {
    selected = sel;
    show(sel.id, sel.line);
    out.textContent = JSON.stringify(sel.json, null, 2);
    cmd.textContent = `nupdf redact ${page.file} --id ${sel.id}`;
    for (const m of marks.children) m.classList.toggle("on", m.dataset.id === sel.id);
  };
  const spanJson = (s) => ({ id: s.id, text: s.text, font: s.font.replace(/^[A-Z]{6}\+/, ""), size: s.size, bbox: s.bbox });
  const hitJson = (h) => ({ page: page.page, id: h.id, text: h.text });

  for (const line of page.lines) {
    const span = line.spans[0];
    const hot = paper.appendChild(box("hot", line.bbox, 2));
    hot.tabIndex = 0;
    hot.setAttribute("role", "button");
    hot.setAttribute("aria-label", `${span.id}: ${span.text}`);
    const peek = () => show(span.id, line);
    const pick = () => select({ id: span.id, line, json: spanJson(span) });
    hot.addEventListener("pointerenter", peek);
    hot.addEventListener("focus", peek);
    hot.addEventListener("pointerleave", () => selected && show(selected.id, selected.line));
    hot.addEventListener("click", pick);
    hot.addEventListener("keydown", (e) => (e.key === "Enter" || e.key === " ") && (e.preventDefault(), pick()));
  }

  const search = (needle, pickId) => {
    marks.replaceChildren();
    const hits = findHits(page, needle);
    for (const h of hits) {
      const [, y0, , y1] = h.span.bbox;
      const m = marks.appendChild(box("hit", [h.span.x[h.at], y0, h.span.x[h.end], y1]));
      m.dataset.id = h.id;
      m.addEventListener("click", (e) => (e.stopPropagation(), select({ id: h.id, line: h.line, json: hitJson(h) })));
    }
    count.textContent = needle.trim() ? `${hits.length} ${hits.length === 1 ? "place" : "places"} on this page` : "";
    const first = hits.find((h) => h.id === pickId) ?? hits[0];
    if (first) select({ id: first.id, line: first.line, json: hitJson(first) });
  };
  input.addEventListener("input", () => search(input.value));
  for (const chip of root.querySelectorAll("[data-try]")) chip.addEventListener("click", () => { input.value = chip.dataset.try; search(input.value); });
  search(input.value, root.dataset.first);
  root.classList.add("ready");
}
