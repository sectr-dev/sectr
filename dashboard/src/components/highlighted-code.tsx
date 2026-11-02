import { Fragment } from "react";

// A minimal highlighter for the step modals' snippets (Python, TOML, TS/TSX
// and dotenv),
// ported from marketing's py-highlight.ts so the two sites color code the
// same way: keywords and punctuation muted, comments faint, and strings
// plus Sectr's own symbols in the brand color. Line-based (no multi-line
// strings), and it renders React nodes, never HTML strings.

const KEYWORDS = new Set([
  "False",
  "None",
  "True",
  "and",
  "as",
  "assert",
  "async",
  "await",
  "break",
  "class",
  "continue",
  "def",
  "del",
  "elif",
  "else",
  "except",
  "finally",
  "for",
  "from",
  "global",
  "if",
  "import",
  "in",
  "is",
  "lambda",
  "nonlocal",
  "not",
  "or",
  "pass",
  "raise",
  "return",
  "try",
  "while",
  "with",
  "yield",
]);

const TS_KEYWORDS = new Set([
  "as",
  "async",
  "await",
  "const",
  "else",
  "export",
  "from",
  "function",
  "if",
  "import",
  "let",
  "new",
  "null",
  "return",
  "throw",
  "true",
  "false",
  "type",
]);

/** Sectr touchpoints: the SDK symbol, the adapter factories and `sectr`. */
const TOUCHPOINTS = new Set([
  "sectr",
  "AgentApp",
  "openai_agents",
  "langgraph",
  "pydantic_ai",
  "as_langgraph_tool",
  "SectrAgent",
  "SectrRunner",
]);

type Lang = "python" | "toml" | "ts" | "dotenv";

// 1 comment · 2 string · 3 identifier · 4 number · 5 punctuation ·
// 6 whitespace · 7 stray quote. TS comments are `//` and strings may use
// backticks; the rest use `#` (a `//` there is part of a URL or an operator).
function tokenRe(ts: boolean): RegExp {
  return new RegExp(
    [
      ts ? "(//[^\\n]*)" : "(#[^\\n]*)",
      ts
        ? "('(?:[^'\\\\\\n]|\\\\.)*'|\"(?:[^\"\\\\\\n]|\\\\.)*\"|`(?:[^`\\\\\\n]|\\\\.)*`)"
        : "('(?:[^'\\\\\\n]|\\\\.)*'|\"(?:[^\"\\\\\\n]|\\\\.)*\")",
      "([A-Za-z_][A-Za-z0-9_]*)",
      "(\\d+)",
      ts ? "([^A-Za-z0-9_\\s\"'`]+)" : "([^A-Za-z0-9_\\s\"']+)",
      "(\\s+)",
      ts ? "([\"'`])" : "([\"'])",
    ].join("|"),
    "g",
  );
}
const TOKEN_RE: Record<Lang, RegExp> = {
  python: tokenRe(false),
  toml: tokenRe(false),
  dotenv: tokenRe(false),
  ts: tokenRe(true),
};

function tokenClass(
  m: RegExpExecArray,
  lang: Lang,
  prev: string,
): string | undefined {
  const [, comment, str, ident, num] = m;
  if (comment !== undefined) return "text-faint-foreground";
  if (str !== undefined) return "text-brand";
  if (ident !== undefined) {
    if (lang === "python" && KEYWORDS.has(ident))
      return "text-muted-foreground";
    if (lang === "ts" && TS_KEYWORDS.has(ident)) return "text-muted-foreground";
    if (lang === "ts" && /^[A-Z]/.test(ident) && /<\/?$/.test(prev))
      return "text-brand"; // JSX tag name
    if ((lang === "python" || lang === "ts") && TOUCHPOINTS.has(ident))
      return "text-brand";
    return "text-foreground";
  }
  if (num !== undefined) return "text-foreground";
  if (m[5] !== undefined || m[7] !== undefined) return "text-muted-foreground";
  return undefined; // whitespace
}

function Line({ line, lang }: { line: string; lang: Lang }) {
  const out: React.ReactNode[] = [];
  const re = new RegExp(TOKEN_RE[lang].source, "g");
  let m: RegExpExecArray | null;
  let i = 0;
  let prev = "";
  while ((m = re.exec(line)) !== null) {
    const cls = tokenClass(m, lang, prev);
    prev = m[0];
    out.push(
      cls ? (
        <span key={i++} className={cls}>
          {m[0]}
        </span>
      ) : (
        <Fragment key={i++}>{m[0]}</Fragment>
      ),
    );
  }
  return <>{out}</>;
}

/** `code` as colored lines; put it in a `whitespace-pre` container. */
export function HighlightedCode({ code, lang }: { code: string; lang: Lang }) {
  return (
    <>
      {code.split("\n").map((line, i, all) => (
        <Fragment key={i}>
          <Line line={line.trimEnd()} lang={lang} />
          {i < all.length - 1 && "\n"}
        </Fragment>
      ))}
    </>
  );
}
