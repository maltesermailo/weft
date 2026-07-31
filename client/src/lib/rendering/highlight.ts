// Syntax highlighting for fenced code blocks (§9.4 `fmt=md`).
//
// Uses highlight.js core with a curated language set — full `highlight.js`
// auto-bundles ~190 grammars, so we register only the languages a chat is
// likely to paste, keeping the client bundle small. Output is HTML-escaped by
// highlight.js itself, so the result is safe to splice into a `{@html}` render.

import hljs from "highlight.js/lib/core";

import rust from "highlight.js/lib/languages/rust";
import javascript from "highlight.js/lib/languages/javascript";
import typescript from "highlight.js/lib/languages/typescript";
import python from "highlight.js/lib/languages/python";
import bash from "highlight.js/lib/languages/bash";
import shell from "highlight.js/lib/languages/shell";
import json from "highlight.js/lib/languages/json";
import go from "highlight.js/lib/languages/go";
import c from "highlight.js/lib/languages/c";
import cpp from "highlight.js/lib/languages/cpp";
import csharp from "highlight.js/lib/languages/csharp";
import java from "highlight.js/lib/languages/java";
import kotlin from "highlight.js/lib/languages/kotlin";
import swift from "highlight.js/lib/languages/swift";
import ruby from "highlight.js/lib/languages/ruby";
import php from "highlight.js/lib/languages/php";
import sql from "highlight.js/lib/languages/sql";
import xml from "highlight.js/lib/languages/xml";
import css from "highlight.js/lib/languages/css";
import markdown from "highlight.js/lib/languages/markdown";
import yaml from "highlight.js/lib/languages/yaml";
import toml from "highlight.js/lib/languages/ini";
import diff from "highlight.js/lib/languages/diff";
import dockerfile from "highlight.js/lib/languages/dockerfile";
import lua from "highlight.js/lib/languages/lua";

hljs.registerLanguage("rust", rust);
hljs.registerLanguage("javascript", javascript);
hljs.registerLanguage("typescript", typescript);
hljs.registerLanguage("python", python);
hljs.registerLanguage("bash", bash);
hljs.registerLanguage("shell", shell);
hljs.registerLanguage("json", json);
hljs.registerLanguage("go", go);
hljs.registerLanguage("c", c);
hljs.registerLanguage("cpp", cpp);
hljs.registerLanguage("csharp", csharp);
hljs.registerLanguage("java", java);
hljs.registerLanguage("kotlin", kotlin);
hljs.registerLanguage("swift", swift);
hljs.registerLanguage("ruby", ruby);
hljs.registerLanguage("php", php);
hljs.registerLanguage("sql", sql);
hljs.registerLanguage("xml", xml);
hljs.registerLanguage("css", css);
hljs.registerLanguage("markdown", markdown);
hljs.registerLanguage("yaml", yaml);
hljs.registerLanguage("toml", toml);
hljs.registerLanguage("diff", diff);
hljs.registerLanguage("dockerfile", dockerfile);
hljs.registerLanguage("lua", lua);

// Common fence-tag aliases → registered grammar names.
const ALIASES: Record<string, string> = {
  rs: "rust",
  js: "javascript",
  jsx: "javascript",
  ts: "typescript",
  tsx: "typescript",
  py: "python",
  sh: "shell",
  zsh: "shell",
  console: "shell",
  yml: "yaml",
  "c++": "cpp",
  cs: "csharp",
  "c#": "csharp",
  kt: "kotlin",
  rb: "ruby",
  html: "xml",
  svg: "xml",
  htm: "xml",
  docker: "dockerfile",
  ini: "toml",
  patch: "diff",
  golang: "go",
};

const escapeHtml = (s: string) =>
  s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");

// Highlight `code`, returning HTML for the inside of a `<code>` element. When a
// language tag is present and known we highlight against that grammar; an empty
// or unknown tag falls back to plain (escaped) text — matching Discord, which
// only colours a block when it is tagged with a recognised language.
export function highlightCode(code: string, lang: string): string {
  const key = ALIASES[lang.toLowerCase()] ?? lang.toLowerCase();

  if (key && hljs.getLanguage(key)) {
    try {
      return hljs.highlight(code, { language: key, ignoreIllegals: true }).value;
    } catch {
      // Fall through to plain rendering on any grammar error.
    }
  }

  return escapeHtml(code);
}
