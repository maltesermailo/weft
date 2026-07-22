// Live syntax highlighting for the composer textarea.
//
// This is NOT renderMd. renderMd turns markdown into final HTML (dropping the
// `**`/`>`/`#` markers and restructuring). Here we keep the text
// character-for-character and only wrap valid markdown in COLOUR/decoration
// spans — never font-weight/style/size — so a transparent textarea can sit on
// top of this overlay with its caret staying perfectly aligned. The effect:
// valid markdown lights up as you type.

const esc = (s: string) =>
  s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");

// Underscore-italic needs word-boundary checks (so snake_case is left alone), so
// it's referenced by identity in the scan loop below.
const UNDERSCORE_EM = /_[^_\n]+_/y;

// Inline token patterns (sticky). Each captures the FULL literal, markers
// included. Order matters: code first, bold before italic.
const INLINE: [string, RegExp][] = [
  ["hl-code", /`[^`\n]+`/y],
  ["hl-strong", /\*\*[^*\n]+\*\*/y],
  ["hl-strong", /__[^_\n]+__/y],
  ["hl-em", /\*(?!\s)[^*\n]+\*/y],
  ["hl-em", UNDERSCORE_EM],
  ["hl-strike", /~~[^~\n]+~~/y],
  ["hl-spoiler", /\|\|[^\n]+?\|\|/y],
  ["hl-link", /\[[^\]\n]+\]\([^)\n]+\)/y],
  ["hl-url", /https?:\/\/[^\s<]+/y],
  ["hl-mention", /@(?:everyone|here|[a-z0-9][\w.-]*)/iy],
  ["hl-emoji", /:[a-zA-Z0-9_]+:/y],
];

const isWord = (c: string) => /\w/.test(c);

function hlInline(line: string): string {
  let out = "";
  let i = 0;
  const n = line.length;

  while (i < n) {
    let matched = false;
    for (const [cls, re] of INLINE) {
      re.lastIndex = i;
      const m = re.exec(line);
      if (!m || m.index !== i) continue;

      // Underscore italic: skip when glued to word chars (snake_case, a_b).
      if (re === UNDERSCORE_EM) {
        const before = i > 0 ? line[i - 1] : " ";
        const after = line[i + m[0].length] ?? " ";
        if (isWord(before) || isWord(after)) continue;
      }

      out += `<span class="${cls}">${esc(m[0])}</span>`;
      i += m[0].length;
      matched = true;
      break;
    }
    if (!matched) {
      out += esc(line[i]);
      i++;
    }
  }
  return out;
}

// Per-line block markers (heading / quote / list), then inline for the rest.
function hlLine(line: string): string {
  // ATX heading (needs the trailing space to be valid).
  if (/^#{1,3}\s/.test(line)) {
    return `<span class="hl-heading">${esc(line)}</span>`;
  }
  // Block quote `>`, `>>`, `>>>`.
  const q = line.match(/^(>{1,3})(\s?)([\s\S]*)$/);
  if (q) {
    return (
      `<span class="hl-marker">${esc(q[1])}</span>` +
      esc(q[2]) +
      `<span class="hl-quote">${hlInline(q[3])}</span>`
    );
  }
  // Thematic break.
  if (/^\s*([-*_])(?:\s*\1){2,}\s*$/.test(line)) {
    return `<span class="hl-marker">${esc(line)}</span>`;
  }
  // Unordered / ordered list marker.
  const li = line.match(/^(\s*)([-*+]|\d+[.)])(\s+)([\s\S]*)$/);
  if (li) {
    return (
      esc(li[1]) +
      `<span class="hl-marker">${esc(li[2])}</span>` +
      esc(li[3]) +
      hlInline(li[4])
    );
  }
  return hlInline(line);
}

// Highlight the whole composer buffer. Fenced code blocks are coloured as a
// region (tracked across lines); everything else is per-line.
export function highlightComposer(text: string): string {
  const lines = text.split("\n");
  const out: string[] = [];
  let inFence = false;

  for (const line of lines) {
    const isFenceMarker = /^\s*(```|~~~)/.test(line);
    if (isFenceMarker) {
      inFence = !inFence;
      out.push(`<span class="hl-code">${esc(line)}</span>`);
      continue;
    }
    if (inFence) {
      out.push(`<span class="hl-code">${esc(line)}</span>`);
      continue;
    }
    out.push(hlLine(line));
  }

  return out.join("\n");
}
