// §9.4 message markdown rendering (Phase 4 · Tier 1). Pure given an `MdContext`:
// the only ambient inputs are the active server's mention/emoji data, which the
// caller supplies so this stays decoupled from the container's state. The
// escaping is XSS-critical (see `escapeHtml`) and unit-testable in isolation.
import { highlightCode } from "$lib/highlight";
import { shortcodeToChar } from "$lib/shortcodes";
import * as weft from "$lib/weft";
import type { Role } from "$lib/models/role.svelte";

/** The per-render ambient data: mention highlighting + custom emoji for the
 *  server the message belongs to (usually the active one). */
export interface MdContext {
  account: string;
  activeServer: string;
  /** Pingable roles at `ns:activeServer` (for role-mention pills). */
  pingable: Role[];
  /** Role ids the account holds at `ns:activeServer` (highlights a mention of one). */
  myRoleIds: Set<string>;
  /** Resolve a `:shortcode:` to a custom-emoji media ref for the active server. */
  emoji: (name: string) => string | undefined;
}

// Escape-first: safe to feed {@html} because HTML is neutralised before any
// markdown token is turned back into a tag. Quotes are escaped too — the link
// rewriters interpolate a captured URL into `href="${url}"`, and a URL char
// class permits `"`, so without this a body like `https://x/"onfocus="…` would
// break out of the attribute and inject an event handler (attribute-injection
// XSS → the Tauri command bridge on desktop). Escaping here fixes it at the
// root, for every attribute interpolation, not just links.
export const escapeHtml = (s: string): string =>
  s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");

// Inline formatting for a single run of text (no fenced/block constructs).
// Code spans and links are stashed to placeholders BEFORE emphasis runs, so
// markdown characters inside a URL or code span (snake_case, a*b, …) can't be
// mangled into <em>/<strong>. \x00…\x00 is used as the placeholder delimiter
// because a NUL can never occur in a chat line.
export function renderInline(text: string, ctx: MdContext): string {
  const stash: string[] = [];
  const keep = (html: string) => {
    const i = stash.length;
    stash.push(html);
    return `\x00T${i}\x00`;
  };

  let s = escapeHtml(text);

  // Inline code — verbatim, highest precedence.
  s = s.replace(/`([^`]+)`/g, (_m, c: string) => keep(`<code>${c}</code>`));

  // Masked link [text](url) then bare URL — stashed so emphasis can't touch
  // the URL. `data-mdlink` marks them for the click-through confirm guard.
  s = s.replace(
    /\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)/g,
    (_m, txt: string, url: string) =>
      keep(`<a href="${url}" target="_blank" rel="noopener noreferrer" data-mdlink="1">${txt}</a>`),
  );
  s = s.replace(
    /(^|\s)(https?:\/\/[^\s<]+)/g,
    (_m, pre: string, url: string) =>
      pre + keep(`<a href="${url}" target="_blank" rel="noopener noreferrer" data-mdlink="1">${url}</a>`),
  );

  // Emphasis: ***bold-italic*** → **bold** → __bold__ → *italic* → _italic_ → ~~strike~~.
  s = s.replace(/\*\*\*([^*]+)\*\*\*/g, "<strong><em>$1</em></strong>");
  s = s.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
  s = s.replace(/__([^_]+)__/g, "<strong>$1</strong>");
  s = s.replace(/(^|[^*])\*([^*\n]+)\*/g, "$1<em>$2</em>");
  // _italic_ only at word boundaries, so snake_case is left alone.
  s = s.replace(/(^|[^\w])_([^_\n]+)_(?=[^\w]|$)/g, "$1<em>$2</em>");
  s = s.replace(/~~([^~]+)~~/g, "<del>$1</del>");

  // ||spoiler|| → click-to-reveal (revealed by a delegated handler in the list).
  s = s.replace(
    /\|\|([\s\S]+?)\|\|/g,
    '<span class="spoiler" role="button" tabindex="0" title="Spoiler — click to reveal">$1</span>',
  );
  // @mentions → pills; a mention of me / @everyone / @here / a pingable role
  // I hold highlights. Role pills carry the role's color.
  const pingable = ctx.pingable;
  const myRoleIds = ctx.myRoleIds;
  s = s.replace(/@(everyone|here|[a-z0-9][a-z0-9._-]*)/gi, (_full, name: string) => {
    const lower = name.toLowerCase();
    const role = pingable.find((r) => r.name.toLowerCase() === lower);
    const me =
      name === ctx.account ||
      lower === "everyone" ||
      lower === "here" ||
      (!!role && myRoleIds.has(role.id));
    // Colors ride the wire, so only emit ones matching a strict hex pattern —
    // never interpolate arbitrary text into a style attribute.
    const style = role && /^#[0-9a-fA-F]{3,8}$/.test(role.color) ? ` style="color:${role.color}"` : "";
    return `<span class="mention${me ? " me" : ""}"${style}>@${name}</span>`;
  });
  // :name: → this server's custom emoji (an inline image) if it exists, else a
  // standard unicode emoji (`:smile:` → 😄); an unknown shortcode stays literal.
  s = s.replace(/:([a-zA-Z0-9_+-]+):/g, (full, name: string) => {
    const media = ctx.emoji(name);
    if (media) {
      const url = weft.mediaUrl(media).replace(/&/g, "&amp;").replace(/"/g, "&quot;");
      return `<img class="custom-emoji" src="${url}" alt=":${name}:" title=":${name}:" />`;
    }
    return shortcodeToChar(name) ?? full;
  });

  // Restore stashed code spans / links.
  s = s.replace(/\x00T(\d+)\x00/g, (_m, i: string) => stash[+i]);
  return s;
}

// §9.4 rendered-message cache (Discord-style memoized parsing): markdown +
// syntax-highlight is the costly per-message work, so cache HTML by
// (server, body) — the message list remounts on every channel navigation now,
// so it re-renders from cache instead of re-parsing. Custom emoji and
// role-mention styling are ns-scoped, so `activeServer` is part of the key; the
// cache is cleared (`clearMdCache`) when either changes (emoji add/remove, role
// flush). LRU: a Map keeps insertion order, so a hit re-inserts
// (most-recently-used) and eviction drops the oldest (`keys().next()`). Bounds
// memory without dropping the whole cache. Plain Map — not reactive.
const MD_CACHE_MAX = 4000;
const mdCache = new Map<string, string>();

/** Drop the render cache (call on emoji add/remove + role flush). */
export function clearMdCache(): void {
  mdCache.clear();
}

export function renderMd(text: string, ctx: MdContext): string {
  const key = `${ctx.activeServer} ${text}`;
  const hit = mdCache.get(key);
  if (hit !== undefined) {
    mdCache.delete(key);
    mdCache.set(key, hit); // touch → most-recently-used
    return hit;
  }
  const html = renderMdRaw(text, ctx);
  mdCache.set(key, html);
  if (mdCache.size > MD_CACHE_MAX) mdCache.delete(mdCache.keys().next().value!);
  return html;
}

// Full render: lift out ``` / ~~~ fenced code blocks (verbatim, highlighted),
// parse block-level constructs (headings, block quotes, lists, rules) line by
// line, inline-format the rest, then splice the code blocks back in.
export function renderMdRaw(text: string, ctx: MdContext): string {
  const blocks: { lang: string; code: string }[] = [];
  const lifted = text.replace(
    /(?:```|~~~)([a-zA-Z0-9+#.-]*)\n?([\s\S]*?)(?:```|~~~)/g,
    (_m, lang: string, code: string) => {
      const i = blocks.length;
      blocks.push({ lang: lang.trim(), code: code.replace(/\n$/, "") });
      return `\x00CB${i}\x00`;
    },
  );

  const lines = lifted.split("\n");
  const pieces: { block: boolean; html: string }[] = [];
  const cbOnly = /^\s*\x00CB\d+\x00\s*$/;
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];

    // A fenced-code placeholder alone on its line is a block.
    if (cbOnly.test(line)) {
      pieces.push({ block: true, html: line.trim() });
      i++;
      continue;
    }
    // ATX headings # / ## / ### (h1–h3, Discord-style).
    const h = line.match(/^(#{1,3})\s+(.*)$/);
    if (h) {
      const lvl = h[1].length;
      pieces.push({ block: true, html: `<h${lvl} class="md-h md-h${lvl}">${renderInline(h[2], ctx)}</h${lvl}>` });
      i++;
      continue;
    }
    // Thematic break: ---, ***, ___ (three or more).
    if (/^\s*([-*_])(?:\s*\1){2,}\s*$/.test(line)) {
      pieces.push({ block: true, html: `<hr class="md-hr" />` });
      i++;
      continue;
    }
    // Block quote: `>>> ` quotes the rest of the message; `> ` quotes a run.
    const tri = line.match(/^>>>\s?(.*)$/);
    if (tri) {
      const rest = [tri[1], ...lines.slice(i + 1)];
      pieces.push({
        block: true,
        html: `<blockquote class="md-quote">${rest.map((l) => renderInline(l, ctx)).join("<br>")}</blockquote>`,
      });
      break;
    }
    if (/^>\s?/.test(line)) {
      const buf: string[] = [];
      while (i < lines.length && /^>\s?/.test(lines[i])) {
        buf.push(lines[i].replace(/^>\s?/, ""));
        i++;
      }
      pieces.push({
        block: true,
        html: `<blockquote class="md-quote">${buf.map((l) => renderInline(l, ctx)).join("<br>")}</blockquote>`,
      });
      continue;
    }
    // Unordered list: -, *, + .
    if (/^\s*[-*+]\s+/.test(line)) {
      const items: string[] = [];
      while (i < lines.length && /^\s*[-*+]\s+/.test(lines[i])) {
        items.push(lines[i].replace(/^\s*[-*+]\s+/, ""));
        i++;
      }
      pieces.push({
        block: true,
        html: `<ul class="md-list">${items.map((it) => `<li>${renderInline(it, ctx)}</li>`).join("")}</ul>`,
      });
      continue;
    }
    // Ordered list: 1. / 1) .
    if (/^\s*\d+[.)]\s+/.test(line)) {
      const items: string[] = [];
      while (i < lines.length && /^\s*\d+[.)]\s+/.test(lines[i])) {
        items.push(lines[i].replace(/^\s*\d+[.)]\s+/, ""));
        i++;
      }
      pieces.push({
        block: true,
        html: `<ol class="md-list">${items.map((it) => `<li>${renderInline(it, ctx)}</li>`).join("")}</ol>`,
      });
      continue;
    }

    // Plain line.
    pieces.push({ block: false, html: renderInline(line, ctx) });
    i++;
  }

  // Assemble: consecutive plain lines keep their newline (rendered by the
  // container's pre-wrap); block elements bring their own separation.
  let s = "";
  for (let k = 0; k < pieces.length; k++) {
    if (k > 0 && !pieces[k].block && !pieces[k - 1].block) s += "\n";
    s += pieces[k].html;
  }

  // Splice fenced code blocks back in, highlighted.
  s = s.replace(/\x00CB(\d+)\x00/g, (_m, i: string) => {
    const b = blocks[+i];
    const label = b.lang ? `<span class="code-lang">${escapeHtml(b.lang)}</span>` : "";
    return `<pre class="code-block hljs">${label}<code>${highlightCode(b.code, b.lang)}</code></pre>`;
  });
  return s;
}
