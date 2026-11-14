// Keeps the site honest: the figures it shows must be in the repo's docs.
// Run with: node --test site/test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { findHits, parts } from "./demo/live.js";

const read = (p) => readFileSync(new URL(p, import.meta.url), "utf8");
const html = read("index.html");
const llms = read("llms.txt");
const results = read("../docs/RESULTS.md");
const readme = read("../README.md");
const iface = read("../docs/AGENT-INTERFACE.md");
const install = "cargo install --locked nupdf-cli";

test("the agent eval rows are the results document's", () => {
  const rows = [...html.matchAll(/--v:(\d+);--max:(\d+)"><\/div><\/div><span class="n">(\d+) \/ (\d+)<\/span><span class="n">([\d,]+)<\/span><span class="n t">(\d+) s</g)];
  assert.equal(rows.length, 8); // the agent eval's rows carry whole seconds
  for (const [, v, max, passed, total, tokens, secs] of rows) {
    assert.deepEqual([v, max], [passed, total]);
    assert.match(results, new RegExp(`\\| ${passed} / ${total} \\| ${tokens} \\| ${secs} s \\|`), `${passed} / ${total}, ${tokens}, ${secs} s`);
  }
});

test("every other figure is in the results document or the README", () => {
  for (const s of ["5,142", "8,988", "1,045", "99.88%", "100,000"]) {
    assert.ok(results.includes(s), `${s} not in RESULTS.md`);
    assert.ok(html.includes(s.replace(/(\d)x$/, "$1×")), `${s} not on the page`);
  }
  assert.ok(results.includes("19 fuzz targets") && html.includes("<strong>19</strong><span>fuzz targets"));
  assert.ok(results.includes("1,500,000 runs") && html.includes("1.5 million runs"));
  assert.ok(readme.includes("solved every task in every run") && html.includes("150 / 150"));
});

for (const [set, id] of [["typical", "one-call"], ["long", "long-document"]]) {
  test(`the ${set} table is the benchmark run's summary`, () => {
    const sum = JSON.parse(read(`../bench/results/2026-11-14-calls/${set}-summary.json`));
    const block = html.slice(html.indexOf(`id="${id}"`), html.indexOf("</div>\n\n", html.indexOf(`id="${id}"`)));
    const rows = [...block.matchAll(/data-tool="([\w-]+)">.*?--v:([\d.]+);--max:([\d.]+)"><\/div><\/div><span class="n">([\d,.]+)[^<]*<\/span><span class="n">([\d.]+) MB<\/span><span class="n t">(\d+) \/ (\d+)</g)];
    assert.equal(rows.length, sum.tools.length);
    for (const [, tool, v, , shown, peak, got, of] of rows) {
      const t = sum.tools.find((x) => x.tool === tool);
      const value = set === "typical" ? t.median_ms : t.pages_per_s;
      assert.equal(Number(v), value, tool);
      assert.equal(Number(shown.replace(/,/g, "")), value, tool);
      assert.equal(peak, t.median_peak_mb.toFixed(1), tool);
      assert.deepEqual([Number(got), Number(of)], set === "typical" ? [t.right, sum.gradable] : [t.completed, sum.files], tool);
    }
    // nupdf does not have the lowest memory, and the page says so
    const lowest = [...sum.tools].sort((x, y) => x.median_peak_mb - y.median_peak_mb)[0].tool;
    assert.equal(lowest, "mutool");
    assert.ok(html.includes("MuPDF's own tool uses the least memory in both tables"));
    assert.ok(html.includes(set === "typical" ? `median over ${sum.files} test files` : `over ${sum.common} documents`));
  });
}

test("every command the page names is in the agent interface", () => {
  const named = [...html.matchAll(/<div><b>0\d<\/b>[\s\S]*?<\/div>/g)].flatMap((g) => [...g[0].matchAll(/<code>(\w+)<\/code>/g)].map((m) => m[1]));
  assert.ok(named.length >= 28);
  for (const c of named) assert.ok(iface.includes("`" + c + "`") || iface.includes("`" + c + " "), c);
});

test("the install tabs give the README's own commands", () => {
  for (const s of [install, "claude mcp add nupdf -- nupdf mcp", "nupdf guide --skill > ~/.claude/skills/nupdf/SKILL.md", 'nupdf = "0.1"', "nupdf verify clean.pdf --preset email --preset phone"]) {
    assert.ok(readme.includes(s), `${s} not in README`);
    assert.ok(html.includes(s.replace(">", "&gt;")) || html.includes(s), `${s} not on the page`);
  }
});

test("every tab has its panel", () => {
  const tabs = [...html.matchAll(/data-tab="([\w-]+)"/g)].map((m) => m[1]);
  const panels = [...html.matchAll(/data-panel="([\w-]+)"/g)].map((m) => m[1]);
  assert.deepEqual(tabs, panels);
});

test("what the agent button copies names the install line and llms.txt", () => {
  const prompt = html.match(/id="agent-prompt">(.*?)<\/p>/)[1];
  assert.ok(prompt.includes(install) && prompt.includes("https://nupdf.dev/llms.txt"));
  assert.equal((html.match(/<button[^>]*data-copy-agent/g) || []).length, 1);
});

test("the page and llms.txt give the same install line and exit codes", () => {
  assert.ok(html.includes(`$ ${install}`) && llms.includes(install));
  assert.match(llms, /0 success, 1 internal error, 2 usage error, 3 input error.*4 verification failed, 5 limit exceeded/);
});

test("it links to the docs, GitHub and crates.io", () => {
  for (const u of ["https://nupdf.dev/docs/", "https://github.com/raspao-studio/nupdf", "https://crates.io/crates/nupdf-cli"]) assert.ok(html.includes(`href="${u}"`), u);
});

test("structured data parses and names the project", () => {
  const ld = JSON.parse(html.match(/<script type="application\/ld\+json">([\s\S]*?)<\/script>/)[1]);
  assert.deepEqual(ld["@graph"].map((n) => n["@type"]), ["SoftwareApplication", "FAQPage"]);
  assert.equal(ld["@graph"][0].name, "nupdf");
});

test("local files the page links to exist", () => {
  for (const f of ["llms.txt", "favicon.svg", "robots.txt", "sitemap.xml", "demo/page.png"]) assert.ok(readFileSync(new URL(f, import.meta.url)).length > 0, f);
});

test("the page stays short", () => {
  const body = html.slice(html.indexOf("<body>"));
  const words = body.replace(/<script[\s\S]*?<\/script>/g, "").replace(/<pre[\s\S]*?<\/pre>/g, "").replace(/<[^>]+>/g, " ").split(/\s+/).filter(Boolean);
  assert.ok(words.length < 950, `${words.length} words`);
});

const page = JSON.parse(read("demo/page.json"));
const real = JSON.parse(read("demo/searches.json"));

for (const [needle, ids] of Object.entries(real)) {
  test(`the page's search finds "${needle}" where nupdf search does`, () => {
    // nupdf also finds hits that run over a line break, which have no single id; the page leaves those out.
    assert.deepEqual(findHits(page, needle).map((h) => h.id), ids.filter(Boolean));
  });
}

test("the address the page opens on is a real hit, split into its five parts", () => {
  const first = html.match(/data-first="([^"]+)"/)[1];
  assert.ok(real["free culture"].includes(first));
  assert.deepEqual(parts(first), { page: "p15", block: ".b1", line: ".l3", span: ".s1", chars: ":17-29" });
  for (const part of Object.values(parts(first))) assert.ok(html.includes(`>${part}<small>`), part);
  assert.ok(html.includes(`--id ${first}`));
});

test("every character of every span has a left edge, and one more for the right", () => {
  for (const line of page.lines) for (const s of line.spans) assert.equal(s.x.length, [...s.text].length + 1, s.id);
});

const PAGES = ["mcp", "redact", "compare"];
const inner = Object.fromEntries(PAGES.map((p) => [p, read(`${p}/index.html`)]));

for (const [name, page] of Object.entries(inner)) {
  test(`/${name}/ has its address, the shared stylesheet and links that lead somewhere`, () => {
    assert.ok(page.includes(`<link rel="canonical" href="https://nupdf.dev/${name}/">`));
    assert.ok(page.includes('<link rel="stylesheet" href="/style.css">') && read("style.css").includes(".page h1"));
    for (const [, to] of page.matchAll(/href="\/(\w+)\/"/g)) assert.ok(PAGES.includes(to), to);
    assert.ok(read("sitemap.xml").includes(`https://nupdf.dev/${name}/`) && llms.includes(`https://nupdf.dev/${name}/`));
    assert.ok(html.includes(`href="/${name}/"`), "the landing page links to it");
  });

  test(`/${name}/ gives the agent eval as the results document has it`, () => {
    const rows = [...page.matchAll(/--v:(\d+);--max:(\d+)"><\/div><\/div><span class="n">(\d+) \/ (\d+)<\/span><span class="n">([\d,]+)<\/span><span class="n t">(\d+) s</g)];
    assert.equal(rows.length, { mcp: 3, redact: 0, compare: 8 }[name]);
    for (const [, v, max, passed, total, tokens, secs] of rows) {
      assert.deepEqual([v, max], [passed, total]);
      assert.match(results, new RegExp(`\\| ${passed} / ${total} \\| ${tokens} \\| ${secs} s \\|`));
    }
  });
}

test("the comparison page carries the landing page's two speed tables unchanged", () => {
  for (const id of ["one-call", "long-document"]) {
    const cut = (s) => s.slice(s.indexOf(`id="${id}"`), s.indexOf("\n  </div>\n", s.indexOf(`id="${id}"`)));
    assert.equal(cut(inner.compare), cut(html), id);
  }
});

test("the redaction page's figures and options are the docs'", () => {
  assert.ok(inner.redact.includes("5,052 of 5,066 files, 99.72%") && results.includes("5,052 of 5,066") && results.includes("99.72%"));
  for (const flag of ["--text", "--regex", "--preset", "--region", "--id", "--hits", "--dry-run", "--preview", "--boxes"]) {
    assert.ok(inner.redact.includes(flag) && iface.includes("`" + flag), flag);
  }
  assert.ok(readme.includes("nupdf redact statement.pdf -o clean.pdf --preset email --preset phone"));
});

test("the MCP page's commands and limits are the docs'", () => {
  for (const s of ["claude mcp add nupdf -- nupdf mcp", "nupdf guide --skill > ~/.claude/skills/nupdf/SKILL.md"]) assert.ok(inner.mcp.includes(s.replace(">", "&gt;")) || inner.mcp.includes(s), s);
  assert.ok(inner.mcp.includes("300 seconds and 8 MB") && iface.includes("`--call-timeout` (300 s)") && iface.includes("`--max-output-bytes` (8 MB)"));
  assert.ok(inner.mcp.includes("structuredContent") && iface.includes("structuredContent"));
});
