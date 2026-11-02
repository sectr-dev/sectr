/*
  Generates the installable agent skill from the docs MDX:
  one skill named `sectr` (SKILL.md + references/), produced by a
  remark-based MDX→MD transformer — never hand-authored, docs keep
  evolving and the skill must follow them.

  Pipeline:
    1. `scripts/skill.tmpl.md` holds SKILL.md's lean body (mental model,
       weave directives `{{weave <page>}}` (`{{weave <page> ##}}` sets the
       woven title's level; `{{weave <page> content}}` weaves the page's
       sections without its title or lead-in), `{{include <page>#<section>}}`
       (one section of a page that stays in references/), and
       `{{references}}`, which
       becomes a table of the references/ files from each page's
       frontmatter title and description).
    2. Woven pages (WOVEN: first-agent, approvals) are inlined
       into SKILL.md, nested under the template heading before their weave
       directive (title one level deeper, the page's headings below it).
    3. Every other docs page becomes one MD file under `references/`,
       unless it's marked `skill: skip file` (see applySkipMarkers): a new
       docs page joins the skill with no change here.
    4. Transformer: strips MDX imports (resolving `?raw` imports of the
       generated CLI blocks so `<Code code={var} lang="text" />` becomes a
       plain fenced block), strips frontmatter (title becomes the woven
       heading), converts Starlight asides (`:::note[Title]`) to
       `> **Title**:` blockquotes, drops HTML-commented content (the 🚧
       under-construction wrappers) and sections marked
       `skill: skip section` (see skipMarkedSections), and rewrites
       cross-links to sibling
       `references/` files / SKILL.md anchors / absolute docs URLs, keeping
       any `#anchor`. A relative link the bundle can't resolve fails the
       run.
    5. Pages left (nearly) empty by the comment-stripping are dropped
       entirely so half-written pages don't pollute the skill.
    5b. The bundled OpenAPI spec (public/spec/public-api.yaml, from
       gen-spec.mjs) ships as references/api/openapi.json, whole: JSON so
       the reading agent can look up one operation with `jq` (more often
       installed than `yq`) instead of reading it all; the lookup hint
       follows SKILL.md's references table, and replaces the one
       `<!-- skill: spec lookup -->` marker in the docs (on the API
       overview, where the agent reads the API pages).
       A link to an operation (`/reference/api-spec/#api-1/POST/sessions`)
       becomes its name plus the `jq` command that shows it (in a table,
       just the name); it must name an operation the spec has, or the run
       fails. A bare /reference/api-spec/ link points at the file.
    6. `--out <dir>` writes the skill there (replacing the dir).
       scripts/publish-skill.sh writes into a clone of the public
       github.com/sectr-dev/skills repo (`sectr/`) and pushes it, so users
       install with `npx skills add sectr-dev/skills`; `npm run gen:skill`
       writes a local preview to dist-skill/sectr/ (gitignored). Nothing is
       committed in this repo.

  `--check` runs everything but the write: it fails on what would fail a
  publish (an unsupported MDX construct, a dangling link). Wired as
  `check:skill` into `just check` (docs area) and the release-time
  `check-docs`, so a docs edit that breaks the skill fails before release.
*/
import {
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, resolve } from "node:path";
import { posix as posixPath } from "node:path";
import { fileURLToPath } from "node:url";
import { exit } from "node:process";
import yaml from "js-yaml";
import { unified } from "unified";
import remarkParse from "remark-parse";
import remarkMdx from "remark-mdx";
import remarkGfm from "remark-gfm";
import remarkDirective from "remark-directive";
import remarkStringify from "remark-stringify";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const DOCS = join(ROOT, "src/content/docs");
const TEMPLATE = join(ROOT, "scripts/skill.tmpl.md");
const SPEC = join(ROOT, "public/spec/public-api.yaml");
const SPEC_OUT = "references/api/openapi.json";
const spec = yaml.load(readFileSync(SPEC, "utf8"));
// How to read the spec: after SKILL.md's references table, and in place of
// the one `skill: spec lookup` marker in the docs.
const SPEC_LOOKUP =
  "Look up what you need rather than reading it whole: " +
  `\`jq '.paths | keys' ${SPEC_OUT}\` lists the endpoints, ` +
  `\`jq '.paths["/sessions/{sessionId}/approval"].post' ${SPEC_OUT}\` shows one, and ` +
  `\`jq '.components.schemas.ApprovalDecision' ${SPEC_OUT}\` resolves a \`$ref\` it names.`;

// Pages woven inline into SKILL.md, in this order (the order of the
// template's weave directives).
const WOVEN = [
  "getting-started/first-agent",
  "guides/approvals",
];

// Every docs page's slug (`reference/sdk/app`), sorted.
const ALL_PAGES = listFiles(DOCS)
  .filter((p) => /\.mdx?$/.test(p))
  .map((p) => p.replace(/\.mdx?$/, ""))
  .sort();
for (const slug of WOVEN) {
  if (!ALL_PAGES.includes(slug)) {
    console.error(`gen-skill: ${slug} is in WOVEN but isn't a docs page`);
    exit(1);
  }
}
// Every page that isn't woven goes to references/, unless it's marked
// `skill: skip file`.
const REFERENCE = ALL_PAGES.filter((slug) => !WOVEN.includes(slug));

const stringifyOptions = { bullet: "-", fences: true, listItemIndent: "one", rule: "-" };
// .mdx pages parse as MDX; .md pages as plain markdown, as Starlight
// renders them (MDX would reject their HTML comments).
const proc = unified()
  .use(remarkParse)
  .use(remarkMdx)
  .use(remarkGfm)
  .use(remarkDirective)
  .use(remarkStringify, stringifyOptions);
const procMd = unified()
  .use(remarkParse)
  .use(remarkGfm)
  .use(remarkDirective)
  .use(remarkStringify, stringifyOptions);

const FRONTMATTER_RE = /^---\r?\n([\s\S]*?)\r?\n---\s*(?:\r?\n|$)/;

function readDoc(slug) {
  for (const ext of [".mdx", ".md"]) {
    const abs = join(DOCS, slug + ext);
    if (existsSync(abs)) return abs;
  }
  throw new Error(`docs page not found: ${slug}`);
}

function parseFrontmatter(raw) {
  const match = raw.match(FRONTMATTER_RE);
  if (!match) throw new Error("missing frontmatter");
  const meta = yaml.load(match[1]);
  return { meta, body: raw.slice(match[0].length) };
}

// GitHub-style slug, used for anchor links into woven sections.
function slugify(text) {
  return text
    .trim()
    .toLowerCase()
    .replace(/[^\w\s-]/g, "")
    .replace(/\s+/g, "-");
}

/**
 * MDX→MD tree transform, phase 1 (everything link-independent):
 *  - mdxjsEsm imports dropped; `?raw` specifiers resolved (relative to the
 *    source MDX) so <Code code={var} lang="text"/> can inline the content
 *    as a plain fenced code block.
 *  - HTML-comment blocks (the 🚧 under-construction wrappers) dropped.
 *  - Starlight directive containers → blockquotes with a **Title** lead.
 *  - Any remaining MDX JSX/expression node fails loudly — the docs should
 *    not grow skill-opaque syntax unnoticed.
 */
// `forInclude`: transforming a page for `{{include}}`, which takes its
// sections even if the page is `skill: skip file` or they're included.
function transformTree(slug, raw, { forInclude = false } = {}) {
  const { meta, body } = parseFrontmatter(raw);
  if (!meta?.title) throw new Error(`${slug}: frontmatter without title`);
  const docDir = dirname(readDoc(slug));
  const varMap = new Map();
  const tree = (readDoc(slug).endsWith(".mdx") ? proc : procMd).parse(body);

  const collectRawImports = (value) => {
    for (const m of value.matchAll(
      /import\s+([A-Za-z_$][\w$]*)\s+from\s*["']([^"']+)["']\s*;?/g,
    )) {
      const [, name, specifier] = m;
      if (!specifier.endsWith("?raw")) continue;
      const abs = resolve(docDir, specifier.slice(0, -"?raw".length));
      varMap.set(name, readFileSync(abs, "utf8"));
    }
  };

  const codeFromJsx = (node) => {
    if (node.name !== "Code") {
      throw new Error(
        `${slug}: unexpected <${node.name}> element — the skill transformer only handles <Code>`,
      );
    }
    const attrs = Object.fromEntries(
      (node.attributes ?? []).map((a) => [a.name, a.value]),
    );
    const varName =
      typeof attrs.code === "object" && attrs.code !== null
        ? attrs.code.value.trim()
        : attrs.code;
    if (!varName || !varMap.has(varName)) {
      throw new Error(`${slug}: <Code code={${varName}}> — unresolved variable`);
    }
    return {
      type: "code",
      lang: typeof attrs.lang === "string" ? attrs.lang : "text",
      value: varMap.get(varName),
    };
  };

  const directiveToQuote = (node) => {
    // remark-directive: `:::note[Title]` puts the label on a first-child
    // paragraph flagged with data.directiveLabel.
    let title;
    let children = node.children ?? [];
    if (children[0]?.data?.directiveLabel) {
      title = children[0].children.map((c) => c.value ?? "").join("").trim();
      children = children.slice(1);
    }
    title ??= node.name
      ? node.name[0].toUpperCase() + node.name.slice(1)
      : "Note";
    const head = {
      type: "paragraph",
      children: [
        { type: "strong", children: [{ type: "text", value: title }] },
        { type: "text", value: ": " },
      ],
    };
    let rest = children;
    if (children[0]?.type === "paragraph") {
      head.children.push(...children[0].children);
      rest = children.slice(1);
    }
    if (head.children.length === 2) {
      head.children.pop(); // empty aside body: drop the dangling ": "
    }
    return { type: "blockquote", children: [head, ...rest] };
  };

  const walk = (parent) => {
    parent.children = parent.children.flatMap((child) => {
      if ((child.type === "html" || child.type === "mdxFlowExpression") && SPEC_LOOKUP_MARKER.test(child.value.trim())) {
        if (!forInclude) specLookupPages.push(slug);
        return proc.parse(
          `The full reference is \`${SPEC_OUT}\`: every operation's exact request and response schemas. ${SPEC_LOOKUP}`,
        ).children;
      }
      switch (child.type) {
        case "mdxjsEsm":
          collectRawImports(child.value);
          return [];
        case "html":
          return child.value.trim().startsWith("<!--") ? [] : [child];
        case "mdxJsxFlowElement":
        case "mdxJsxTextElement":
          return [codeFromJsx(child)];
        case "containerDirective":
          return [directiveToQuote(child)];
        case "mdxFlowExpression":
        case "mdxTextExpression":
          if (child.value.trim().startsWith("/*")) return []; // {/* MDX comment */}
          throw new Error(
            `${slug}: unexpected MDX expression {${child.value}} — not skill-transformable`,
          );
        default:
          if (child.children) walk(child);
          return [child];
      }
    });
  };
  if (applySkipMarkers(slug, tree, { ignoreFile: forInclude })) return { meta, tree, skipped: true };
  if (!forInclude) omitIncludedSections(slug, tree);
  walk(tree);

  return { meta, tree, skipped: false };
}

// Docs content that makes no sense in the skill is marked in the page,
// with an optional reason: `{/* skill: skip … — why */}` in .mdx,
// `<!-- skill: skip … — why -->` in .md. Both render as nothing on the
// site.
//  - `skill: skip file`, before the page's first heading, leaves the whole
//    page out (e.g. the docs landing page).
//  - `skill: skip section`, anywhere in a section, drops that section:
//    from the heading before it up to the next heading of the same or a
//    higher level (e.g. how to install the skill, which an agent reading
//    it already has).
//  - `skill: skip next` drops the one block right after it (a paragraph,
//    list or code block, never a heading), for a line that only makes sense
//    on the site (e.g. the tutorial's "we will select…").
// Returns whether the page is skipped.
const SKIP_MARKER = /^(?:\/\*|<!--)\s*skill:\s*skip (section|file|next)\b[\s\S]*(?:\*\/|-->)$/;
// `<!-- skill: spec lookup -->` (or `{/* … */}`): the spec's lookup hint goes
// here in the skill. The docs site shows nothing; exactly one page has it.
const SPEC_LOOKUP_MARKER = /^(?:\/\*|<!--)\s*skill:\s*spec lookup\b[\s\S]*(?:\*\/|-->)$/;
const specLookupPages = []; // slugs, one entry per marker
function applySkipMarkers(slug, tree, { ignoreFile = false } = {}) {
  const kindOf = (node) =>
    (node.type === "mdxFlowExpression" || node.type === "html") &&
    node.value.trim().match(SKIP_MARKER)?.[1];
  const nodes = tree.children;
  const firstHeading = nodes.findIndex((node) => node.type === "heading");
  const fileMarker = nodes.findIndex((node) => kindOf(node) === "file");
  if (fileMarker !== -1 && ignoreFile) {
    nodes.splice(fileMarker, 1);
  } else if (fileMarker !== -1) {
    if (firstHeading !== -1 && fileMarker > firstHeading) {
      throw new Error(`${slug}: "skill: skip file" must come before the page's first heading`);
    }
    return true;
  }
  for (
    let at = nodes.findIndex((node) => kindOf(node) === "section");
    at !== -1;
    at = nodes.findIndex((node) => kindOf(node) === "section")
  ) {
    let start = at;
    while (start >= 0 && nodes[start].type !== "heading") start--;
    if (start < 0) {
      throw new Error(
        `${slug}: "skill: skip section" before any heading — to leave the whole page out, use "skill: skip file"`,
      );
    }
    const depth = nodes[start].depth;
    let end = start + 1;
    while (end < nodes.length && !(nodes[end].type === "heading" && nodes[end].depth <= depth)) end++;
    const dropped = droppedAnchors.get(slug) ?? new Set();
    for (const node of nodes.slice(start, end)) {
      if (node.type === "heading") dropped.add(slugify(headingText(node)));
    }
    droppedAnchors.set(slug, dropped);
    nodes.splice(start, end - start);
  }
  for (
    let at = nodes.findIndex((node) => kindOf(node) === "next");
    at !== -1;
    at = nodes.findIndex((node) => kindOf(node) === "next")
  ) {
    const next = nodes[at + 1];
    if (!next || next.type === "heading") {
      throw new Error(`${slug}: "skill: skip next" must be followed by a block that isn't a heading`);
    }
    nodes.splice(at, 2);
  }
  return false;
}

// Anchors of the headings skip markers dropped, per page slug: links to
// them go to the live docs, which still have them.
const droppedAnchors = new Map();

// Anchors of sections moved into SKILL.md by `{{include}}`, per page slug:
// section (or one of its subheadings) anchor → its SKILL.md anchor.
const movedAnchors = new Map();

function omitIncludedSections(slug, tree) {
  const sections = includedSections.get(slug);
  if (!sections) return;
  const nodes = tree.children;
  for (const [section, skillAnchor] of sections) {
    const at = nodes.findIndex((n) => n.type === "heading" && slugify(headingText(n)) === section);
    if (at === -1) continue; // includeSection reports a missing section
    const depth = nodes[at].depth;
    let end = at + 1;
    while (end < nodes.length && !(nodes[end].type === "heading" && nodes[end].depth <= depth)) end++;
    const moved = movedAnchors.get(slug) ?? new Map();
    moved.set(section, skillAnchor);
    for (const node of nodes.slice(at + 1, end)) {
      if (node.type === "heading") moved.set(slugify(headingText(node)), slugify(headingText(node)));
    }
    movedAnchors.set(slug, moved);
    nodes.splice(at, end - at);
  }
}

// The SKILL.md anchor for a page's section, if `{{include}}` put it there.
const sectionInSkill = (pageSlug, anchor) =>
  includedSections.get(pageSlug)?.get(anchor) ?? movedAnchors.get(pageSlug)?.get(anchor);

// A link from `fromFile` to `anchor` in SKILL.md.
const toSkillAnchor = (fromFile, anchor) =>
  fromFile === "SKILL.md" ? `#${anchor}` : `${relFromRoot(fromFile, "SKILL.md")}#${anchor}`;

function headingText(node) {
  let text = "";
  walkNodes(node, (n) => {
    if (n.type === "text" || n.type === "inlineCode") text += n.value;
  });
  return text;
}

// The page's URL on the live docs (a folder's `index` page is the folder).
const liveUrl = (slug) => `https://docs.sectr.dev/${slug.replace(/(^|\/)index$/, "")}/`.replace(/\/\/$/, "/");

// Pages left with no (shallow) heading and only a sliver of text outside
// blockquotes/code after the comment-stripping are dropped entirely —
// half-written pages must not pollute the skill (e.g. a draft page that's
// entirely HTML-commented).
function pageIsDroppable(tree) {
  let hasHeading = false;
  let text = "";
  const rec = (node, inQuote) => {
    if (node.type === "heading" && node.depth <= 3) hasHeading = true;
    if (node.type === "code") return;
    if (node.type === "blockquote") {
      for (const child of node.children) rec(child, true);
      return;
    }
    if (node.type === "text" && !inQuote) text += node.value;
    if (node.children) for (const child of node.children) rec(child, inQuote);
  };
  rec(tree, false);
  return !hasHeading && text.trim().length < 200;
}

function demoteHeadings(node, levels) {
  if (node.type === "heading") node.depth = Math.max(1, Math.min(node.depth + levels, 6));
  if (node.children) for (const child of node.children) demoteHeadings(child, levels);
}

function walkNodes(node, fn) {
  fn(node);
  if (node.children) for (const child of node.children) walkNodes(child, fn);
}

function relFromRoot(fromFile, toFile) {
  const rel = posixPath.relative(posixPath.dirname(fromFile), toFile);
  return rel;
}

function render(tree) {
  return proc.stringify(proc.runSync(tree)).trimEnd();
}

// ---- main -----------------------------------------------------------------

const checkOnly = process.argv.includes("--check");
const outAt = process.argv.indexOf("--out");
const OUT = outAt === -1 ? null : resolve(process.argv[outAt + 1] ?? "");
if (checkOnly === (OUT !== null) || (OUT !== null && !process.argv[outAt + 1])) {
  console.error("usage: gen-skill.mjs --out <dir> | --check");
  exit(1);
}

// Template: frontmatter verbatim, body split on weave directives.
const tmplRaw = readFileSync(TEMPLATE, "utf8");
const { meta: tmplMeta, body: tmplBody } = parseFrontmatter(tmplRaw);
// `{{weave <page>}}` nests the page under the template heading before it;
// `{{weave <page> ##}}` sets its title's level outright (here H2);
// `{{weave <page> content}}` leaves out the page's title and lead-in (the
// text before its first heading) and puts its sections straight under the
// template heading before it, so the template can title and introduce the
// page its own way (e.g. a docs tutorial presented as an overview).
// tmplTexts[i] is the template text before tmplWeaves[i] (the last one
// follows the last directive).
const tmplSplit = tmplBody.split(/\{\{weave ([\w/-]+)(?: (#{1,6}|content))?\}\}/);
const tmplTexts = [];
const tmplWeaves = []; // { slug, option? }
for (let i = 0; i < tmplSplit.length; i += 3) {
  tmplTexts.push(tmplSplit[i]);
  if (tmplSplit[i + 1] !== undefined) {
    tmplWeaves.push({ slug: tmplSplit[i + 1], option: tmplSplit[i + 2] });
  }
}
if (tmplTexts.filter((text) => text.includes("{{references}}")).length !== 1) {
  console.error("skill.tmpl.md must contain exactly one {{references}} directive");
  exit(1);
}
const weaveSlugs = tmplWeaves.map((weave) => weave.slug);
if (JSON.stringify(weaveSlugs) !== JSON.stringify(WOVEN)) {
  console.error(
    `skill.tmpl.md weave order ${JSON.stringify(weaveSlugs)} does not match WOVEN ${JSON.stringify(WOVEN)}`,
  );
  exit(1);
}

// Where each woven page goes. A page's title sits at its directive's level,
// or else one level below the template heading before it, and its own
// headings (from H2) below the title; in `content` mode its sections sit
// directly under that template heading, which links to the page point at.
// (Headings inside the template's code fences don't count.)
// `{{include <page>#<section>}}` directives, by page: section anchor →
// the SKILL.md anchor of the template heading the include sits under. An
// included section lives in SKILL.md: the page's reference copy leaves it
// out (omitIncludedSections), and links to it go to SKILL.md.
const INCLUDE_RE = /\{\{include ([\w/-]+)#([\w-]+)\}\}/g;
const includedSections = new Map();
{
  let headingText = null;
  let inFence = false;
  for (const text of tmplTexts) {
    for (const line of text.split("\n")) {
      if (/^\s*(```|~~~)/.test(line)) inFence = !inFence;
      const heading = !inFence && line.match(/^(#{1,6})\s+(.*)$/);
      if (heading) headingText = heading[2];
      for (const [, slug, section] of line.matchAll(INCLUDE_RE)) {
        if (headingText === null) {
          console.error(`skill.tmpl.md: {{include ${slug}#${section}}} needs a template heading before it`);
          exit(1);
        }
        const sections = includedSections.get(slug) ?? new Map();
        sections.set(section, slugify(headingText));
        includedSections.set(slug, sections);
      }
    }
  }
}
// Subheadings inside an included section keep their own anchors in
// SKILL.md: record them too, whether or not the page itself is in the skill.
for (const [slug, sections] of includedSections) {
  if (!ALL_PAGES.includes(slug)) {
    console.error(`skill.tmpl.md: {{include ${slug}#…}}: no such docs page`);
    exit(1);
  }
  const { tree } = transformTree(slug, readFileSync(readDoc(slug), "utf8"), { forInclude: true });
  const nodes = tree.children;
  const moved = movedAnchors.get(slug) ?? new Map();
  for (const section of sections.keys()) {
    const at = nodes.findIndex((n) => n.type === "heading" && slugify(headingText(n)) === section);
    if (at === -1) {
      console.error(`skill.tmpl.md: {{include ${slug}#${section}}}: no such section`);
      exit(1);
    }
    let end = at + 1;
    while (end < nodes.length && !(nodes[end].type === "heading" && nodes[end].depth <= nodes[at].depth)) end++;
    for (const node of nodes.slice(at + 1, end)) {
      if (node.type === "heading") moved.set(slugify(headingText(node)), slugify(headingText(node)));
    }
  }
  movedAnchors.set(slug, moved);
}

const weaveLayout = new Map(); // slug → { content, titleDepth?, anchor?, demote }
{
  let depth = 1;
  let headingText = null;
  let inFence = false;
  for (let i = 0; i < tmplTexts.length; i++) {
    for (const line of tmplTexts[i].split("\n")) {
      if (/^\s*(```|~~~)/.test(line)) inFence = !inFence;
      const heading = !inFence && line.match(/^(#{1,6})\s+(.*)$/);
      if (heading) [depth, headingText] = [heading[1].length, heading[2]];
    }
    const weave = tmplWeaves[i];
    if (!weave) continue;
    if (weave.option === "content") {
      if (headingText === null) {
        console.error(`skill.tmpl.md: {{weave ${weave.slug} content}} needs a template heading before it`);
        exit(1);
      }
      weaveLayout.set(weave.slug, { content: true, anchor: slugify(headingText), demote: depth - 1 });
    } else {
      const titleDepth = weave.option ? weave.option.length : Math.min(depth + 1, 6);
      weaveLayout.set(weave.slug, { content: false, titleDepth, demote: titleDepth - 1 });
    }
  }
}

// Phase 1: transform every page (links untouched — the rewrite map isn't
// known until we know which pages survive the drop rule).
const wovenPages = WOVEN.map((slug) => {
  const raw = readFileSync(readDoc(slug), "utf8");
  const page = { slug, ...transformTree(slug, raw) };
  if (page.skipped) {
    console.error(`gen-skill: ${slug} is in WOVEN but marked "skill: skip file"`);
    exit(1);
  }
  return page;
}).filter((page) => {
  const dropped = pageIsDroppable(page.tree);
  if (dropped) console.log(`dropped (empty after transform): ${page.slug}`);
  return !dropped;
});

const refPages = REFERENCE.map((slug) => {
  const raw = readFileSync(readDoc(slug), "utf8");
  return { slug, ...transformTree(slug, raw) };
}).filter((page) => {
  if (page.skipped) return false; // marked `skill: skip file`
  const dropped = pageIsDroppable(page.tree);
  if (dropped) console.log(`dropped (empty after transform): ${page.slug}`);
  return !dropped;
});

if (specLookupPages.length !== 1) {
  console.error(
    `gen-skill: "skill: spec lookup" must mark exactly one page in the skill, found: ${specLookupPages.join(", ") || "none"}`,
  );
  exit(1);
}

// Phase 2: link map. Woven pages → SKILL.md anchors; kept pages → their
// references/ file. Unknown internal paths fall through to the live docs.
const linkMap = new Map();
for (const page of wovenPages) {
  const layout = weaveLayout.get(page.slug);
  linkMap.set(page.slug, { anchor: layout.content ? layout.anchor : slugify(page.meta.title) });
}
// `reference/api/index` is served at /reference/api/: map both.
for (const page of refPages) {
  // `reference/sdk/app` → references/sdk/app.md; other sections keep their
  // folder (`guides/secrets` → references/guides/secrets.md).
  const outPath = `references/${page.slug.replace(/^reference\//, "")}.md`;
  linkMap.set(page.slug, { outPath });
  if (page.slug.endsWith("/index")) linkMap.set(page.slug.slice(0, -"/index".length), { outPath });
}

function rewriteLink(url, fromFile, fromSlug) {
  if (url.startsWith("#")) {
    // Same-page anchor: fine unless a skip marker dropped its section.
    const moved = sectionInSkill(fromSlug, url.slice(1));
    if (moved) return toSkillAnchor(fromFile, moved);
    return droppedAnchors.get(fromSlug)?.has(url.slice(1)) ? liveUrl(fromSlug) + url : url;
  }
  if (!url.startsWith("/")) return url; // external / already-relative
  // The anchor rides along: `/guides/approvals/#marking-as-gated-in-deployment-config`.
  const at = url.indexOf("#");
  const path = at === -1 ? url : url.slice(0, at);
  const hash = at === -1 ? "" : url.slice(at);
  const key = path.replace(/\/+$/, "");
  const moved = hash && sectionInSkill(key.slice(1), hash.slice(1));
  if (moved) return toSkillAnchor(fromFile, moved);
  if (hash && droppedAnchors.get(key.slice(1))?.has(hash.slice(1))) {
    return `https://docs.sectr.dev${key}/${hash}`; // a section the skill left out
  }
  if (key === "/reference/api-spec") {
    // The whole reference (an operation's link was replaced in rewriteLinks).
    return relFromRoot(fromFile, SPEC_OUT);
  }
  const target = linkMap.get(key.slice(1));
  if (target?.anchor) {
    // A woven page is a section of SKILL.md: its own heading, unless the
    // link names a heading inside it.
    const anchor = hash || `#${target.anchor}`;
    return fromFile === "SKILL.md" ? anchor : `${relFromRoot(fromFile, "SKILL.md")}${anchor}`;
  }
  if (target?.outPath) return relFromRoot(fromFile, target.outPath) + hash;
  // Not in the skill (an excluded or dropped page, a static asset): point
  // at the live docs.
  const live = /\.[a-z0-9]+$/i.test(key)
    ? `https://docs.sectr.dev${key}`
    : `https://docs.sectr.dev${key}/`;
  return live + hash;
}

// Links to a spec operation that it doesn't have, or that don't name it.
const badSpecLinks = [];
// A link to one operation in the full reference: Scalar's anchor is
// `#api-1/<METHOD><path>`. Its text must name the operation exactly
// (`POST /sessions/{sessionId}/input`), since the skill keeps only the text.
// Returns { method, path } (method lowercase, as in the spec), or null.
function specOperation(url, fromFile, text) {
  const m = url.match(/^\/reference\/api-spec\/?#api-1\/([A-Z]+)(\/.*)$/);
  if (!m) return null;
  const [, method, path] = m;
  if (!spec.paths[path]?.[method.toLowerCase()]) {
    badSpecLinks.push(`no such operation in the spec: ${fromFile}: ${url}`);
  } else if (text !== `${method} ${path}`) {
    badSpecLinks.push(`a link to ${method} ${path} must say so, not "${text}": ${fromFile}`);
  }
  return { method: method.toLowerCase(), path };
}
// Relative links the bundle emits, checked once every file is known.
const relativeLinks = []; // { fromFile, url }
const rewriteLinks = (tree, fromFile, fromSlug) => {
  const operationLinks = []; // { parent, node, op }
  const walk = (node, parent) => {
    if (node.type === "link" || node.type === "image") {
      const op = specOperation(node.url, fromFile, headingText(node));
      if (op) return operationLinks.push({ parent, node, op });
      node.url = rewriteLink(node.url, fromFile, fromSlug);
      if (!/^([a-z]+:|#)/i.test(node.url)) relativeLinks.push({ fromFile, url: node.url });
    }
    for (const child of node.children ?? []) walk(child, node);
  };
  walk(tree, null);
  // The JSON has no anchors: keep the operation's name, then the command
  // that shows it (a table cell keeps just the name).
  for (const { parent, node, op } of operationLinks) {
    const lookup =
      parent.type === "tableCell"
        ? []
        : [
            { type: "text", value: " (look it up with " },
            { type: "inlineCode", value: `jq '.paths["${op.path}"].${op.method}' ${SPEC_OUT}` },
            { type: "text", value: ")" },
          ];
    parent.children.splice(parent.children.indexOf(node), 1, ...node.children, ...lookup);
  }
};

// Phase 3: assemble the output bundle.
const files = new Map(); // skill-relative path → content

for (const page of wovenPages) {
  const layout = weaveLayout.get(page.slug);
  if (layout.content) {
    // The template titles and introduces it: drop the page's lead-in.
    const first = page.tree.children.findIndex((node) => node.type === "heading");
    page.tree.children.splice(0, first === -1 ? page.tree.children.length : first);
  }
  demoteHeadings(page.tree, layout.demote);
  rewriteLinks(page.tree, "SKILL.md", page.slug);
  page.rendered = render(page.tree);
}
// `{{include <page>#<section>}}` in the template inlines one section of a
// docs page (its body, without its heading), e.g. the support matrix. Unlike
// a woven page, the page stays in references/ too. The section's own
// subheadings go below the template heading the directive sits under.
function includeSection(slug, anchor, parentDepth) {
  if (!ALL_PAGES.includes(slug)) {
    throw new Error(`skill.tmpl.md: {{include ${slug}#${anchor}}}: no such docs page`);
  }
  const { tree } = transformTree(slug, readFileSync(readDoc(slug), "utf8"), { forInclude: true });
  const nodes = tree.children;
  const at = nodes.findIndex((n) => n.type === "heading" && slugify(headingText(n)) === anchor);
  if (at === -1) throw new Error(`skill.tmpl.md: {{include ${slug}#${anchor}}}: no such section`);
  const depth = nodes[at].depth;
  let end = at + 1;
  while (end < nodes.length && !(nodes[end].type === "heading" && nodes[end].depth <= depth)) end++;
  const section = { type: "root", children: nodes.slice(at + 1, end) };
  demoteHeadings(section, parentDepth - depth);
  rewriteLinks(section, "SKILL.md", slug);
  return render(section);
}
// Template text i with its includes expanded, each under the template
// heading before it (headings in code fences don't count).
function expandIncludes(i) {
  const headingDepthIn = (text, start) => {
    let depth = start;
    let inFence = false;
    for (const line of text.split("\n")) {
      if (/^\s*(```|~~~)/.test(line)) inFence = !inFence;
      const heading = !inFence && line.match(/^(#{1,6})\s/);
      if (heading) depth = heading[1].length;
    }
    return depth;
  };
  let depth = 1;
  for (let j = 0; j < i; j++) depth = headingDepthIn(tmplTexts[j], depth);
  const text = tmplTexts[i];
  return text.replace(INCLUDE_RE, (match, slug, anchor, offset) =>
    includeSection(slug, anchor, headingDepthIn(text.slice(0, offset), depth)),
  );
}

let wovenBody = "";
// Append with clean separation: template text, then each surviving woven
// page as `## Title` + demoted content, always blank-line separated.
const appendWoven = (chunk) => {
  wovenBody = `${wovenBody.trimEnd()}\n\n${chunk.trimStart()}`;
};
for (const page of refPages) rewriteLinks(page.tree, linkMap.get(page.slug).outPath, page.slug);
// The `{{references}}` table: one row per references/ file, from the page's
// frontmatter, so a new docs page gets its row with no template edit.
const cell = (text) => String(text).replace(/\|/g, "\\|").replace(/\s+/g, " ").trim();
const referenceRows = refPages.map(
  (page) =>
    `| **${cell(page.meta.title)}**: ${cell(page.meta.description ?? "")} | ${linkMap.get(page.slug).outPath} |`,
);
// The spec's row goes after the API pages.
const lastApiRow = referenceRows.findLastIndex((row) => row.includes("| references/api/"));
referenceRows.splice(
  lastApiRow + 1,
  0,
  `| **OpenAPI spec**: every operation's exact request and response schemas; look up what you need with \`jq\` (below). | ${SPEC_OUT} |`,
);
// The lookup hint follows the table: in a cell, the `|` in a jq filter would
// have to be escaped, and an agent reading the raw file would copy the `\|`.
const referencesTable = [
  "| Page | File |",
  "| --- | --- |",
  ...referenceRows,
  "",
  `The OpenAPI spec is large. ${SPEC_LOOKUP}`,
].join("\n");
for (let i = 0; i < tmplTexts.length; i++) {
  appendWoven(
    expandIncludes(i).replace("{{references}}", referencesTable),
  );
  if (tmplWeaves[i]) {
    const page = wovenPages.find((p) => p.slug === tmplWeaves[i].slug);
    if (!page) continue; // dropped page: skip its weave slot
    const layout = weaveLayout.get(page.slug);
    appendWoven(
      layout.content
        ? page.rendered
        : `${"#".repeat(layout.titleDepth)} ${page.meta.title}\n\n${page.rendered}`,
    );
  }
}
files.set(
  "SKILL.md",
  `---\n${yaml
    .dump(
      {
        name: "sectr",
        description: tmplMeta.description,
      },
      { lineWidth: 0, quotingType: '"' },
    )
    .trimEnd()}\n---\n\n${wovenBody.trimEnd()}\n`,
);

for (const page of refPages) {
  const outPath = linkMap.get(page.slug).outPath;
  files.set(outPath, `# ${page.meta.title}\n\n${render(page.tree)}\n`);
}
files.set(SPEC_OUT, `${JSON.stringify(spec, null, 2)}\n`);

// Every relative link must land on a file in the bundle, and an #anchor on
// one of its headings (slugged as GitHub and Starlight do).
const headingSlugs = new Map(); // bundle path → Set of heading slugs
const anchorsOf = (path) => {
  if (!headingSlugs.has(path)) {
    const slugs = new Set();
    walkNodes(proc.parse(files.get(path)), (node) => {
      if (node.type !== "heading") return;
      slugs.add(slugify(headingText(node)));
    });
    headingSlugs.set(path, slugs);
  }
  return headingSlugs.get(path);
};
const linkTarget = (fromFile, url) => {
  const [path, anchor] = url.startsWith("#") ? ["", url.slice(1)] : url.split("#");
  return [path ? posixPath.join(posixPath.dirname(fromFile), path) : fromFile, anchor];
};
for (const page of wovenPages) {
  walkNodes(page.tree, (node) => {
    if ((node.type === "link" || node.type === "image") && node.url.startsWith("#")) {
      relativeLinks.push({ fromFile: "SKILL.md", url: node.url });
    }
  });
}
for (const page of refPages) {
  const outPath = linkMap.get(page.slug).outPath;
  walkNodes(page.tree, (node) => {
    if ((node.type === "link" || node.type === "image") && node.url.startsWith("#")) {
      relativeLinks.push({ fromFile: outPath, url: node.url });
    }
  });
}
const dangling = relativeLinks.filter(({ fromFile, url }) => {
  const [target, anchor] = linkTarget(fromFile, url);
  return !files.has(target) || (anchor !== undefined && !anchorsOf(target).has(anchor));
});
for (const problem of badSpecLinks) console.error(problem);
if (dangling.length > 0 || badSpecLinks.length > 0) {
  for (const { fromFile, url } of dangling) console.error(`dangling link in ${fromFile}: ${url}`);
  exit(1);
}

// Phase 4: write (or, with --check, stop here: generating it was the check).
if (checkOnly) {
  console.log(`skill generates cleanly (${files.size} files)`);
  exit(0);
}
rmSync(OUT, { recursive: true, force: true });
for (const [rel, content] of files) {
  const abs = join(OUT, rel);
  mkdirSync(dirname(abs), { recursive: true });
  writeFileSync(abs, content);
}
console.log(`wrote the skill (${files.size} files) to ${OUT}`);

function listFiles(dir) {
  if (!existsSync(dir)) return [];
  const out = [];
  const walk = (cur, prefix) => {
    for (const entry of readdirSync(cur)) {
      const abs = join(cur, entry);
      if (statSync(abs).isDirectory()) walk(abs, prefix ? `${prefix}/${entry}` : entry);
      else out.push(prefix ? `${prefix}/${entry}` : entry);
    }
  };
  walk(dir, "");
  return out;
}